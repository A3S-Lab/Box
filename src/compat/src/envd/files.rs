//! Bounded legacy envd file upload and download support.

use std::collections::HashSet;
use std::sync::Arc;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionSessionManager, FileOp,
    FileRequest, FileResponse,
};
use axum::body::Body;
use axum::extract::{FromRequest, Multipart};
use axum::http::header::{
    ACCEPT_ENCODING, ALLOW, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE,
    RANGE, VARY,
};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures::StreamExt;
use hyper::body::HttpBody;
use serde::Serialize;

const DEFAULT_FILE_USER: &str = "user";
// File requests are base64-encoded inside a 16 MiB guest transport frame. An
// 11 MiB raw cap leaves more than 1 MiB for base64 expansion and JSON framing.
const MAX_FILE_BYTES: usize = 11 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_UPLOAD_FILES: usize = 128;

#[derive(Clone)]
pub(super) struct FileBroker {
    sessions: Arc<dyn ExecutionSessionManager>,
}

impl FileBroker {
    pub(super) fn new(sessions: Arc<dyn ExecutionSessionManager>) -> Self {
        Self { sessions }
    }

    pub(super) async fn handle(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Response<Body> {
        let query = match FileQuery::parse(request.uri().query()) {
            Ok(query) => query,
            Err(error) => return error.response(),
        };

        match *request.method() {
            Method::GET => {
                self.download(request, execution_id, generation, query)
                    .await
            }
            Method::POST => self.upload(request, execution_id, generation, query).await,
            _ => {
                let mut response = FileFailure::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method not allowed; expected GET or POST",
                )
                .response();
                response
                    .headers_mut()
                    .insert(ALLOW, HeaderValue::from_static("GET, POST"));
                response
            }
        }
    }

    async fn download(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        query: FileQuery,
    ) -> Response<Body> {
        if request.headers().contains_key(RANGE) {
            return FileFailure::new(
                StatusCode::NOT_IMPLEMENTED,
                "range downloads are not supported by the broker file transport",
            )
            .response();
        }
        if !identity_encoding_is_acceptable(request.headers()) {
            return FileFailure::new(
                StatusCode::NOT_ACCEPTABLE,
                "identity content encoding is not acceptable",
            )
            .response();
        }
        let path =
            match normalize_posix_path(query.path.as_deref().unwrap_or_default(), &query.user) {
                Ok(path) => path,
                Err(error) => return error.response(),
            };
        let response = self
            .sessions
            .transfer_file(
                execution_id,
                generation,
                FileRequest {
                    op: FileOp::Download,
                    guest_path: path.clone(),
                    data: None,
                    user: Some(query.user),
                },
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => return manager_failure(error).response(),
        };
        let (encoded, expected_size) = match successful_download(response) {
            Ok(download) => download,
            Err(error) => return error.response(),
        };
        let data = match STANDARD.decode(encoded) {
            Ok(data) => data,
            Err(error) => {
                return FileFailure::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("guest returned invalid base64 file data: {error}"),
                )
                .response()
            }
        };
        if data.len() as u64 != expected_size {
            return FileFailure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "guest returned {} downloaded bytes, expected {expected_size}",
                    data.len()
                ),
            )
            .response();
        }

        let mut response = Response::new(Body::from(data.clone()));
        *response.status_mut() = StatusCode::OK;
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        response
            .headers_mut()
            .insert(VARY, HeaderValue::from_static("Accept-Encoding"));
        response.headers_mut().insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&data.len().to_string()).expect("decimal length is a header"),
        );
        if let Some(value) = content_disposition(&path) {
            response.headers_mut().insert(CONTENT_DISPOSITION, value);
        }
        response
    }

    async fn upload(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        query: FileQuery,
    ) -> Response<Body> {
        if let Some(encoding) = request.headers().get(CONTENT_ENCODING) {
            let encoding = match encoding.to_str() {
                Ok(encoding) => encoding,
                Err(_) => {
                    return FileFailure::bad_request("Content-Encoding is not UTF-8").response()
                }
            };
            if !encoding.eq_ignore_ascii_case("identity") {
                return FileFailure::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "compressed uploads are not supported by the broker file transport",
                )
                .response();
            }
        }
        if request
            .headers()
            .keys()
            .any(|name| name.as_str().starts_with("x-metadata-"))
        {
            return FileFailure::new(
                StatusCode::NOT_IMPLEMENTED,
                "file metadata is not supported by the broker file transport",
            )
            .response();
        }

        let media_type = match request.headers().get(CONTENT_TYPE) {
            Some(value) => match value.to_str() {
                Ok(value) => value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase(),
                Err(_) => return FileFailure::bad_request("Content-Type is not UTF-8").response(),
            },
            None => String::new(),
        };

        let result = if media_type == "application/octet-stream" {
            self.upload_raw(request, execution_id, generation, query)
                .await
        } else if media_type.starts_with("multipart/") {
            self.upload_multipart(request, execution_id, generation, query)
                .await
        } else {
            Err(FileFailure::bad_request(format!(
                "unsupported content type: {}; expected multipart/form-data or application/octet-stream",
                request
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
            )))
        };

        match result {
            Ok(files) => json_response(StatusCode::OK, &files),
            Err(error) => error.response(),
        }
    }

    async fn upload_raw(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        query: FileQuery,
    ) -> Result<Vec<UploadedFile>, FileFailure> {
        let path = query.path.as_deref().ok_or_else(|| {
            FileFailure::bad_request("path query parameter is required for raw body upload")
        })?;
        let path = normalize_posix_path(path, &query.user)?;
        let data = read_body_limited(request.into_body()).await?;
        let entry = self
            .upload_one(execution_id, generation, &query.user, path, data)
            .await?;
        Ok(vec![entry])
    }

    async fn upload_multipart(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        query: FileQuery,
    ) -> Result<Vec<UploadedFile>, FileFailure> {
        let mut multipart = Multipart::from_request(request, &())
            .await
            .map_err(|error| {
                FileFailure::bad_request(format!("error parsing multipart form: {error}"))
            })?;
        let mut uploaded = Vec::new();
        let mut paths = HashSet::new();

        while let Some(mut field) = multipart.next_field().await.map_err(|error| {
            FileFailure::bad_request(format!("error reading multipart form: {error}"))
        })? {
            if field.name() != Some("file") {
                read_field_limited(&mut field).await?;
                continue;
            }
            if uploaded.len() >= MAX_UPLOAD_FILES {
                return Err(FileFailure::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("an upload may contain at most {MAX_UPLOAD_FILES} files"),
                ));
            }
            let part_path = match query.path.as_deref() {
                Some(path) => path.to_string(),
                None => field.file_name().map(str::to_string).ok_or_else(|| {
                    FileFailure::bad_request(
                        "multipart file part requires a filename when path is omitted",
                    )
                })?,
            };
            let path = normalize_posix_path(&part_path, &query.user)?;
            if !paths.insert(path.clone()) {
                return Err(FileFailure::bad_request(format!(
                    "you cannot upload multiple files to the same path '{path}' in one request"
                )));
            }
            let data = read_field_limited(&mut field).await?;
            let entry = self
                .upload_one(execution_id, generation, &query.user, path, data)
                .await?;
            uploaded.push(entry);
        }

        Ok(uploaded)
    }

    async fn upload_one(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        user: &str,
        path: String,
        data: Vec<u8>,
    ) -> Result<UploadedFile, FileFailure> {
        let expected_size = data.len() as u64;
        let response = self
            .sessions
            .transfer_file(
                execution_id,
                generation,
                FileRequest {
                    op: FileOp::Upload,
                    guest_path: path.clone(),
                    data: Some(STANDARD.encode(data)),
                    user: Some(user.to_string()),
                },
            )
            .await
            .map_err(manager_failure)?;
        if !response.success {
            return Err(guest_failure(response));
        }
        if response.size != expected_size {
            return Err(FileFailure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "guest reported {} uploaded bytes, expected {expected_size}",
                    response.size
                ),
            ));
        }
        Ok(UploadedFile::new(path))
    }
}

#[derive(Debug)]
struct FileQuery {
    path: Option<String>,
    user: String,
}

impl FileQuery {
    fn parse(raw: Option<&str>) -> Result<Self, FileFailure> {
        let mut path = None;
        let mut user = None;
        for (name, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
            match name.as_ref() {
                "path" => set_once(&mut path, value.into_owned(), "path")?,
                "username" => set_once(&mut user, value.into_owned(), "username")?,
                _ => {}
            }
        }
        let user = user.unwrap_or_else(|| DEFAULT_FILE_USER.to_string());
        validate_user(&user)?;
        Ok(Self { path, user })
    }
}

fn set_once(
    target: &mut Option<String>,
    value: String,
    name: &'static str,
) -> Result<(), FileFailure> {
    if target.replace(value).is_some() {
        return Err(FileFailure::bad_request(format!(
            "query parameter {name} must be provided at most once"
        )));
    }
    Ok(())
}

fn validate_user(user: &str) -> Result<(), FileFailure> {
    if user.is_empty()
        || user.len() > 128
        || !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(FileFailure::bad_request(
            "username must contain 1 to 128 ASCII letters, digits, '.', '_' or '-'",
        ));
    }
    Ok(())
}

fn normalize_posix_path(path: &str, user: &str) -> Result<String, FileFailure> {
    if path.contains('\0') {
        return Err(FileFailure::bad_request("file path contains NUL"));
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(FileFailure::bad_request(format!(
            "file path exceeds {MAX_PATH_BYTES} bytes"
        )));
    }
    let home = if user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{user}")
    };
    let expanded = if path.is_empty() || path == "~" {
        home
    } else if let Some(relative) = path.strip_prefix("~/") {
        format!("{home}/{relative}")
    } else if path.starts_with('~') {
        return Err(FileFailure::bad_request(
            "user-specific home expansion is not supported",
        ));
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{home}/{path}")
    };

    let mut components = Vec::new();
    for component in expanded.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            component => components.push(component),
        }
    }
    let normalized = format!("/{}", components.join("/"));
    if normalized.len() > MAX_PATH_BYTES {
        return Err(FileFailure::bad_request(format!(
            "resolved file path exceeds {MAX_PATH_BYTES} bytes"
        )));
    }
    Ok(normalized)
}

async fn read_body_limited(mut body: Body) -> Result<Vec<u8>, FileFailure> {
    if body.size_hint().lower() > MAX_FILE_BYTES as u64 {
        return Err(file_too_large());
    }
    let capacity = body
        .size_hint()
        .upper()
        .unwrap_or_default()
        .min(MAX_FILE_BYTES as u64) as usize;
    let mut data = Vec::with_capacity(capacity);
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|error| {
            FileFailure::bad_request(format!("error reading upload body: {error}"))
        })?;
        if data.len().saturating_add(chunk.len()) > MAX_FILE_BYTES {
            return Err(file_too_large());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

async fn read_field_limited(
    field: &mut axum::extract::multipart::Field<'_>,
) -> Result<Vec<u8>, FileFailure> {
    let mut data = Vec::new();
    while let Some(chunk) = field.next().await {
        let chunk = chunk.map_err(|error| {
            FileFailure::bad_request(format!("error reading multipart file: {error}"))
        })?;
        if data.len().saturating_add(chunk.len()) > MAX_FILE_BYTES {
            return Err(file_too_large());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

fn file_too_large() -> FileFailure {
    FileFailure::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        format!("file exceeds the broker limit of {MAX_FILE_BYTES} bytes"),
    )
}

fn successful_download(response: FileResponse) -> Result<(String, u64), FileFailure> {
    if !response.success {
        return Err(guest_failure(response));
    }
    let data = response.data.ok_or_else(|| {
        FileFailure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "guest returned a successful download without file data",
        )
    })?;
    Ok((data, response.size))
}

fn guest_failure(response: FileResponse) -> FileFailure {
    let message = response
        .error
        .unwrap_or_else(|| "guest file operation failed without an error".to_string());
    let lower = message.to_ascii_lowercase();
    let status = if lower.contains("not found") || lower.contains("does not exist") {
        StatusCode::NOT_FOUND
    } else if lower.contains("user") && (lower.contains("invalid") || lower.contains("unknown")) {
        StatusCode::UNAUTHORIZED
    } else if lower.contains("directory")
        || lower.contains("path is a file")
        || lower.contains("file path")
        || lower.contains("base64")
        || lower.contains("upload request")
        || lower.contains("home expansion")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    FileFailure::new(status, message)
}

fn manager_failure(error: ExecutionManagerError) -> FileFailure {
    match error {
        ExecutionManagerError::InvalidRequest(message) => FileFailure::bad_request(message),
        ExecutionManagerError::NotFound(execution_id) => FileFailure::new(
            StatusCode::NOT_FOUND,
            format!("execution {execution_id} not found"),
        ),
        ExecutionManagerError::Conflict { message, .. } => {
            FileFailure::new(StatusCode::CONFLICT, message)
        }
        ExecutionManagerError::Unavailable(message) => {
            FileFailure::new(StatusCode::SERVICE_UNAVAILABLE, message)
        }
        ExecutionManagerError::Internal(message) => {
            FileFailure::new(StatusCode::INTERNAL_SERVER_ERROR, message)
        }
    }
}

fn identity_encoding_is_acceptable(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let mut wildcard = None;
    for item in value.split(',') {
        let mut parts = item.trim().split(';');
        let encoding = parts.next().unwrap_or_default().trim();
        let quality = parts
            .find_map(|part| part.trim().strip_prefix("q="))
            .and_then(|quality| quality.parse::<f32>().ok())
            .unwrap_or(1.0);
        if encoding.eq_ignore_ascii_case("identity") {
            return quality > 0.0;
        }
        if encoding == "*" {
            wildcard = Some(quality);
        }
    }
    wildcard != Some(0.0)
}

fn content_disposition(path: &str) -> Option<HeaderValue> {
    let name = posix_basename(path);
    if name.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    HeaderValue::from_str(&format!("inline; filename=\"{escaped}\"")).ok()
}

fn posix_basename(path: &str) -> &str {
    path.rsplit('/')
        .find(|component| !component.is_empty())
        .unwrap_or("/")
}

#[derive(Debug, Serialize)]
struct UploadedFile {
    path: String,
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
}

impl UploadedFile {
    fn new(path: String) -> Self {
        let name = posix_basename(&path).to_string();
        Self {
            path,
            name,
            kind: "file",
        }
    }
}

#[derive(Debug)]
struct FileFailure {
    status: StatusCode,
    message: String,
}

impl FileFailure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn response(self) -> Response<Body> {
        json_response(
            self.status,
            &ErrorResponse {
                code: self.status.as_u16(),
                message: self.message,
            },
        )
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    code: u16,
    message: String,
}

fn json_response(status: StatusCode, value: &impl Serialize) -> Response<Body> {
    let body = serde_json::to_vec(value).expect("envd file response is serializable");
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}
