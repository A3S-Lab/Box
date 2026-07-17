use futures::StreamExt;
use oci_distribution::manifest::OciDescriptor;
use oci_distribution::RegistryOperation;
use tokio::io::AsyncWriteExt;

use super::{BlobPullTransport, HashingFileWriter};
use crate::oci::registry::progress::ProgressReporter;
use crate::oci::registry::{
    registry_base_url, registry_blob_url, registry_error_summary,
};

pub(super) struct AttemptFailure {
    pub(super) message: String,
    pub(super) retryable: bool,
    pub(super) reset_partial: bool,
}

impl AttemptFailure {
    pub(super) fn retryable(message: String) -> Self {
        Self {
            message,
            retryable: true,
            reset_partial: false,
        }
    }

    pub(super) fn retryable_reset(message: String) -> Self {
        Self {
            message,
            retryable: true,
            reset_partial: true,
        }
    }

    pub(super) fn fatal(message: String, reset_partial: bool) -> Self {
        Self {
            message,
            retryable: false,
            reset_partial,
        }
    }
}

pub(super) async fn transfer_attempt(
    transport: &BlobPullTransport<'_>,
    descriptor: &OciDescriptor,
    writer: &mut HashingFileWriter,
    mut progress: Option<&mut ProgressReporter>,
    attempt: usize,
    expected_size: u64,
) -> std::result::Result<(), AttemptFailure> {
    let offset = writer.bytes_written();
    let response = request_blob(transport, descriptor, offset).await?;
    prepare_response(&response, writer, offset, expected_size).await?;

    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::time::timeout(
            transport.policy.no_progress_timeout(),
            stream.next(),
        )
        .await
        .map_err(|_| {
            AttemptFailure::retryable(format!(
                "no byte progress for {:?}",
                transport.policy.no_progress_timeout()
            ))
        })?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            AttemptFailure::retryable(format!("response body read failed: {error}"))
        })?;
        tokio::time::timeout(
            transport.policy.no_progress_timeout(),
            writer.write_all(&chunk),
        )
        .await
        .map_err(|_| {
            AttemptFailure::retryable(format!(
                "no file-write progress for {:?}",
                transport.policy.no_progress_timeout()
            ))
        })?
        .map_err(|error| AttemptFailure::fatal(format!("blob file write failed: {error}"), false))?;

        if writer.bytes_written() > expected_size {
            return Err(AttemptFailure::fatal(
                format!(
                    "response exceeded declared size {expected_size} bytes (received {})",
                    writer.bytes_written()
                ),
                true,
            ));
        }
        if let Some(reporter) = progress.as_deref_mut() {
            reporter.downloading(writer.bytes_written(), attempt, false);
        }
    }
    Ok(())
}

async fn request_blob(
    transport: &BlobPullTransport<'_>,
    descriptor: &OciDescriptor,
    offset: u64,
) -> std::result::Result<oci_reqwest::Response, AttemptFailure> {
    let base = registry_base_url(transport.protocol, transport.image_ref)
        .map_err(|error| AttemptFailure::fatal(error.to_string(), false))?;
    let url = registry_blob_url(&base, &transport.image_ref.repository, &descriptor.digest)
        .map_err(|error| AttemptFailure::fatal(error.to_string(), false))?;

    let token = if transport.force_basic {
        None
    } else {
        let auth = transport.auth.to_oci_auth();
        Some(
            tokio::time::timeout(
                transport.policy.no_progress_timeout(),
                transport
                    .client
                    .auth(transport.oci_ref, &auth, RegistryOperation::Pull),
            )
            .await
            .map_err(|_| AttemptFailure::retryable("registry authentication timed out".to_string()))?
            .map_err(|error| {
                AttemptFailure::retryable(format!(
                    "registry authentication failed: {}",
                    registry_error_summary(&error, transport.auth)
                ))
            })?,
        )
        .flatten()
    };

    let mut response = send_request(
        transport,
        url,
        offset,
        token.as_deref(),
        transport.force_basic,
    )
    .await?;
    if response.status() == oci_reqwest::StatusCode::UNAUTHORIZED
        && !transport.force_basic
        && transport.auth.basic_credentials().is_some()
    {
        let base = registry_base_url(transport.protocol, transport.image_ref)
            .map_err(|error| AttemptFailure::fatal(error.to_string(), false))?;
        let url = registry_blob_url(&base, &transport.image_ref.repository, &descriptor.digest)
            .map_err(|error| AttemptFailure::fatal(error.to_string(), false))?;
        response = send_request(transport, url, offset, None, true).await?;
    }

    if !response.status().is_success() {
        if let Some(urls) = &descriptor.urls {
            for candidate in urls {
                let Ok(url) = oci_reqwest::Url::parse(candidate) else {
                    continue;
                };
                if !matches!(url.scheme(), "http" | "https") {
                    continue;
                }
                let external = send_request(transport, url, offset, None, false).await?;
                if external.status().is_success() {
                    return Ok(external);
                }
                response = external;
            }
        }
    }

    classify_response(response, offset)
}

async fn send_request(
    transport: &BlobPullTransport<'_>,
    url: oci_reqwest::Url,
    offset: u64,
    bearer_token: Option<&str>,
    basic: bool,
) -> std::result::Result<oci_reqwest::Response, AttemptFailure> {
    let mut request = transport.http.get(url);
    if offset > 0 {
        request = request.header(oci_reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    if let Some(token) = bearer_token {
        request = request.bearer_auth(token);
    } else if basic {
        let (username, password) = transport.auth.basic_credentials().ok_or_else(|| {
            AttemptFailure::fatal(
                "preemptive Basic blob pull requires non-empty credentials".to_string(),
                false,
            )
        })?;
        request = request.basic_auth(username, Some(password));
    }

    tokio::time::timeout(transport.policy.no_progress_timeout(), request.send())
        .await
        .map_err(|_| AttemptFailure::retryable("registry response headers timed out".to_string()))?
        .map_err(|error| AttemptFailure::retryable(format!("registry request failed: {error}")))
}

fn classify_response(
    response: oci_reqwest::Response,
    offset: u64,
) -> std::result::Result<oci_reqwest::Response, AttemptFailure> {
    let status = response.status();
    if status == oci_reqwest::StatusCode::OK
        || status == oci_reqwest::StatusCode::PARTIAL_CONTENT
    {
        return Ok(response);
    }
    if status == oci_reqwest::StatusCode::RANGE_NOT_SATISFIABLE && offset > 0 {
        return Err(AttemptFailure::retryable_reset(
            "registry rejected the requested resume offset".to_string(),
        ));
    }
    let retryable = status == oci_reqwest::StatusCode::REQUEST_TIMEOUT
        || status == oci_reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error();
    let message = format!("registry returned HTTP {}", status.as_u16());
    if retryable {
        Err(AttemptFailure::retryable(message))
    } else {
        Err(AttemptFailure::fatal(message, false))
    }
}

async fn prepare_response(
    response: &oci_reqwest::Response,
    writer: &mut HashingFileWriter,
    requested_offset: u64,
    expected_size: u64,
) -> std::result::Result<(), AttemptFailure> {
    if requested_offset > 0 && response.status() == oci_reqwest::StatusCode::OK {
        writer
            .reset()
            .await
            .map_err(|error| AttemptFailure::fatal(format!("failed to restart blob file: {error}"), true))?;
    } else if response.status() == oci_reqwest::StatusCode::PARTIAL_CONTENT {
        validate_content_range(response, requested_offset, expected_size)?;
    }

    if response.status() == oci_reqwest::StatusCode::OK {
        if let Some(length) = response.content_length() {
            if length != expected_size {
                return Err(AttemptFailure::fatal(
                    format!(
                        "registry Content-Length mismatch: expected {expected_size}, received {length}"
                    ),
                    true,
                ));
            }
        }
    }
    Ok(())
}

fn validate_content_range(
    response: &oci_reqwest::Response,
    requested_offset: u64,
    expected_size: u64,
) -> std::result::Result<(), AttemptFailure> {
    let value = response
        .headers()
        .get(oci_reqwest::header::CONTENT_RANGE)
        .ok_or_else(|| AttemptFailure::fatal("206 response omitted Content-Range".to_string(), false))?
        .to_str()
        .map_err(|error| AttemptFailure::fatal(format!("invalid Content-Range header: {error}"), false))?;
    let value = value
        .strip_prefix("bytes ")
        .ok_or_else(|| AttemptFailure::fatal(format!("invalid Content-Range header: {value}"), false))?;
    let (range, total) = value
        .split_once('/')
        .ok_or_else(|| AttemptFailure::fatal(format!("invalid Content-Range header: {value}"), false))?;
    let (start, end) = range
        .split_once('-')
        .ok_or_else(|| AttemptFailure::fatal(format!("invalid Content-Range header: {value}"), false))?;
    let start = start
        .parse::<u64>()
        .map_err(|_| AttemptFailure::fatal(format!("invalid Content-Range start: {value}"), false))?;
    let end = end
        .parse::<u64>()
        .map_err(|_| AttemptFailure::fatal(format!("invalid Content-Range end: {value}"), false))?;
    let total = total
        .parse::<u64>()
        .map_err(|_| AttemptFailure::fatal(format!("invalid Content-Range total: {value}"), false))?;
    if start != requested_offset || end < start || total != expected_size || end >= total {
        return Err(AttemptFailure::fatal(
            format!(
                "Content-Range {value} does not match requested offset {requested_offset} and declared size {expected_size}"
            ),
            false,
        ));
    }
    if let Some(length) = response.content_length() {
        let range_length = end.saturating_sub(start).saturating_add(1);
        if length != range_length {
            return Err(AttemptFailure::fatal(
                format!(
                    "partial Content-Length mismatch: range contains {range_length} bytes, header says {length}"
                ),
                false,
            ));
        }
    }
    Ok(())
}
