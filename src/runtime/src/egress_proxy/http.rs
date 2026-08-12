use std::str::FromStr;
use std::time::Duration;

use http::uri::Authority;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use url::{Host, Url};

const MAX_PROXY_HEADER_BYTES: usize = 16 * 1024;

pub(super) enum ProxyRequest {
    Connect {
        host: String,
        port: u16,
        buffered_tunnel_bytes: Vec<u8>,
    },
    Http {
        host: String,
        port: u16,
        upstream_header: Vec<u8>,
        buffered_body: Vec<u8>,
        remaining_body_bytes: u64,
    },
}

impl ProxyRequest {
    pub(super) fn host(&self) -> &str {
        match self {
            Self::Connect { host, .. } | Self::Http { host, .. } => host,
        }
    }

    pub(super) const fn port(&self) -> u16 {
        match self {
            Self::Connect { port, .. } | Self::Http { port, .. } => *port,
        }
    }
}

#[derive(Debug, Error)]
pub(super) enum ProxyRequestError {
    #[error("proxy request authentication failed")]
    Authentication,
    #[error("proxy request header timed out")]
    Timeout,
    #[error("proxy request header exceeded its byte limit")]
    HeaderTooLarge,
    #[error("proxy request is malformed")]
    Malformed,
    #[error("proxy request I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

pub(super) async fn read_proxy_request<R>(
    reader: &mut R,
    expected_token: &str,
    timeout: Duration,
) -> Result<ProxyRequest, ProxyRequestError>
where
    R: AsyncRead + Unpin,
{
    tokio::time::timeout(timeout, read_proxy_request_inner(reader, expected_token))
        .await
        .map_err(|_| ProxyRequestError::Timeout)?
}

async fn read_proxy_request_inner<R>(
    reader: &mut R,
    expected_token: &str,
) -> Result<ProxyRequest, ProxyRequestError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(2048);
    let header_end = loop {
        let mut chunk = [0_u8; 1024];
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Err(ProxyRequestError::Malformed);
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_header_end(&bytes) {
            if end > MAX_PROXY_HEADER_BYTES {
                return Err(ProxyRequestError::HeaderTooLarge);
            }
            break end;
        }
        if bytes.len() > MAX_PROXY_HEADER_BYTES {
            return Err(ProxyRequestError::HeaderTooLarge);
        }
    };

    let header =
        std::str::from_utf8(&bytes[..header_end]).map_err(|_| ProxyRequestError::Malformed)?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next().ok_or(ProxyRequestError::Malformed)?;
    let mut request_parts = request_line.split(' ');
    let method = request_parts.next().ok_or(ProxyRequestError::Malformed)?;
    let target = request_parts.next().ok_or(ProxyRequestError::Malformed)?;
    let version = request_parts.next().ok_or(ProxyRequestError::Malformed)?;
    if request_parts.next().is_some()
        || !is_http_token(method)
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
    {
        return Err(ProxyRequestError::Malformed);
    }

    let headers = parse_headers(lines)?;
    authenticate(&headers, expected_token)?;
    let extra = bytes[header_end + 4..].to_vec();
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = parse_authority(target, None)?;
        validate_host_header(&headers, &host, port, 443)?;
        return Ok(ProxyRequest::Connect {
            host,
            port,
            buffered_tunnel_bytes: extra,
        });
    }

    let url = Url::parse(target).map_err(|_| ProxyRequestError::Malformed)?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ProxyRequestError::Malformed);
    }
    let host = canonical_url_host(&url)?;
    let port = url
        .port_or_known_default()
        .ok_or(ProxyRequestError::Malformed)?;
    validate_host_header(&headers, &host, port, 80)?;

    let content_length = unique_header(&headers, "content-length")?
        .map(|value| u64::from_str(value).map_err(|_| ProxyRequestError::Malformed))
        .transpose()?
        .unwrap_or(0);
    if unique_header(&headers, "transfer-encoding")?.is_some() {
        return Err(ProxyRequestError::Malformed);
    }
    if extra.len() as u64 > content_length {
        return Err(ProxyRequestError::Malformed);
    }

    let upstream_header = render_upstream_header(method, version, &url, &headers, content_length)?;
    Ok(ProxyRequest::Http {
        host,
        port,
        upstream_header,
        buffered_body: extra.clone(),
        remaining_body_bytes: content_length.saturating_sub(extra.len() as u64),
    })
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn parse_headers<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Result<Vec<(String, String)>, ProxyRequestError> {
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(ProxyRequestError::Malformed);
        }
        let (name, value) = line.split_once(':').ok_or(ProxyRequestError::Malformed)?;
        if !is_http_token(name)
            || value
                .bytes()
                .any(|byte| byte < 0x20 && byte != b'\t' || byte == 0x7f)
        {
            return Err(ProxyRequestError::Malformed);
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(headers)
}

fn authenticate(
    headers: &[(String, String)],
    expected_token: &str,
) -> Result<(), ProxyRequestError> {
    use base64::Engine;

    let value =
        unique_header(headers, "proxy-authorization")?.ok_or(ProxyRequestError::Authentication)?;
    let (scheme, token) = value
        .split_once(' ')
        .ok_or(ProxyRequestError::Authentication)?;
    let valid = if scheme.eq_ignore_ascii_case("bearer") {
        constant_time_equal(token, expected_token)
    } else if scheme.eq_ignore_ascii_case("basic") {
        let expected =
            base64::engine::general_purpose::STANDARD.encode(format!("a3s:{expected_token}"));
        constant_time_equal(token, &expected)
    } else {
        false
    };
    if !valid {
        return Err(ProxyRequestError::Authentication);
    }
    Ok(())
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn unique_header<'a>(
    headers: &'a [(String, String)],
    name: &str,
) -> Result<Option<&'a str>, ProxyRequestError> {
    let mut values = headers
        .iter()
        .filter(|(candidate, _)| candidate == name)
        .map(|(_, value)| value.as_str());
    let first = values.next();
    if values.next().is_some() {
        return Err(ProxyRequestError::Malformed);
    }
    Ok(first)
}

fn parse_authority(
    value: &str,
    default_port: Option<u16>,
) -> Result<(String, u16), ProxyRequestError> {
    if value.contains(['@', '/', '?', '#']) || value.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(ProxyRequestError::Malformed);
    }
    let authority = Authority::from_str(value).map_err(|_| ProxyRequestError::Malformed)?;
    let port = authority
        .port_u16()
        .or(default_port)
        .ok_or(ProxyRequestError::Malformed)?;
    let host = canonical_host(authority.host())?;
    Ok((host, port))
}

fn canonical_host(value: &str) -> Result<String, ProxyRequestError> {
    let value = if value.contains(':') && !value.starts_with('[') {
        format!("[{value}]")
    } else {
        value.to_string()
    };
    match Host::parse(&value).map_err(|_| ProxyRequestError::Malformed)? {
        Host::Domain(domain) => Ok(domain.trim_end_matches('.').to_string()),
        Host::Ipv4(address) => Ok(address.to_string()),
        Host::Ipv6(address) => Ok(format!("[{address}]")),
    }
}

fn canonical_url_host(url: &Url) -> Result<String, ProxyRequestError> {
    match url.host().ok_or(ProxyRequestError::Malformed)? {
        Host::Domain(domain) => Ok(domain.trim_end_matches('.').to_string()),
        Host::Ipv4(address) => Ok(address.to_string()),
        Host::Ipv6(address) => Ok(format!("[{address}]")),
    }
}

fn validate_host_header(
    headers: &[(String, String)],
    target_host: &str,
    target_port: u16,
    default_port: u16,
) -> Result<(), ProxyRequestError> {
    let Some(host_header) = unique_header(headers, "host")? else {
        return Err(ProxyRequestError::Malformed);
    };
    let (host, port) = parse_authority(host_header, Some(default_port))?;
    if host != target_host || port != target_port {
        return Err(ProxyRequestError::Malformed);
    }
    Ok(())
}

fn render_upstream_header(
    method: &str,
    version: &str,
    url: &Url,
    headers: &[(String, String)],
    content_length: u64,
) -> Result<Vec<u8>, ProxyRequestError> {
    let mut path = url.path().to_string();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let host = canonical_url_host(url)?;
    let port = url
        .port_or_known_default()
        .ok_or(ProxyRequestError::Malformed)?;
    let host_header = if port == 80 {
        host
    } else {
        format!("{host}:{port}")
    };

    let mut output = format!("{method} {path} {version}\r\nHost: {host_header}\r\n").into_bytes();
    const STRIPPED: &[&str] = &[
        "host",
        "proxy-authorization",
        "proxy-connection",
        "connection",
        "keep-alive",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
    ];
    for (name, value) in headers {
        if STRIPPED.contains(&name.as_str()) {
            continue;
        }
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(b": ");
        output.extend_from_slice(value.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    if content_length > 0 {
        output.extend_from_slice(format!("content-length: {content_length}\r\n").as_bytes());
    }
    output.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    async fn parse(request: &[u8]) -> Result<ProxyRequest, ProxyRequestError> {
        let (mut client, mut server) = tokio::io::duplex(32 * 1024);
        client.write_all(request).await.unwrap();
        drop(client);
        read_proxy_request(&mut server, "generation-token", Duration::from_secs(1)).await
    }

    #[tokio::test]
    async fn parses_authenticated_connect_without_retaining_credentials() {
        let request = parse(
            b"CONNECT API.EXAMPLE.COM:443 HTTP/1.1\r\nHost: api.example.com:443\r\nProxy-Authorization: Bearer generation-token\r\n\r\nhello",
        )
        .await
        .unwrap();
        let ProxyRequest::Connect {
            host,
            port,
            buffered_tunnel_bytes,
        } = request
        else {
            panic!("expected CONNECT")
        };
        assert_eq!(host, "api.example.com");
        assert_eq!(port, 443);
        assert_eq!(buffered_tunnel_bytes, b"hello");
    }

    #[tokio::test]
    async fn accepts_generation_scoped_basic_proxy_credentials() {
        let request = parse(
            b"CONNECT api.example.com:443 HTTP/1.1\r\nHost: api.example.com:443\r\nProxy-Authorization: Basic YTNzOmdlbmVyYXRpb24tdG9rZW4=\r\n\r\n",
        )
        .await
        .unwrap();

        assert_eq!(request.host(), "api.example.com");
        assert_eq!(request.port(), 443);
    }

    #[tokio::test]
    async fn rewrites_absolute_http_and_strips_proxy_headers() {
        let request = parse(
            b"POST http://example.com:8080/a?q=1 HTTP/1.1\r\nHost: example.com:8080\r\nProxy-Authorization: Bearer generation-token\r\nProxy-Connection: keep-alive\r\nAuthorization: Bearer upstream-secret\r\nContent-Length: 4\r\n\r\ntest",
        )
        .await
        .unwrap();
        let ProxyRequest::Http {
            upstream_header,
            buffered_body,
            remaining_body_bytes,
            ..
        } = request
        else {
            panic!("expected HTTP")
        };
        let header = String::from_utf8(upstream_header).unwrap();
        assert!(header.starts_with("POST /a?q=1 HTTP/1.1\r\n"));
        assert!(header.contains("authorization: Bearer upstream-secret\r\n"));
        assert!(!header.contains("Proxy-Authorization"));
        assert!(!header.contains("generation-token"));
        assert_eq!(buffered_body, b"test");
        assert_eq!(remaining_body_bytes, 0);
    }

    #[tokio::test]
    async fn rejects_auth_failure_host_confusion_and_pipelining() {
        assert!(matches!(
            parse(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n").await,
            Err(ProxyRequestError::Authentication)
        ));
        assert!(matches!(
            parse(b"CONNECT example.com:443 HTTP/1.1\r\nHost: attacker.example:443\r\nProxy-Authorization: Bearer generation-token\r\n\r\n").await,
            Err(ProxyRequestError::Malformed)
        ));
        assert!(matches!(
            parse(b"POST http://example.com/ HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: Bearer generation-token\r\nContent-Length: 0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n").await,
            Err(ProxyRequestError::Malformed)
        ));
    }
}
