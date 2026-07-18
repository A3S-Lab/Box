//! Minimal Connect JSON framing for the pinned E2B envd clients.

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, Request, Response, StatusCode};
use hyper::body::{Bytes, HttpBody};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

const CONTENT_TYPE_UNARY_JSON: &str = "application/json";
const CONTENT_TYPE_STREAM_JSON: &str = "application/connect+json";
const END_STREAM_FLAG: u8 = 0x02;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub(super) struct ConnectFailure {
    code: &'static str,
    message: String,
    status: StatusCode,
}

impl ConnectFailure {
    pub(super) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new("invalid_argument", message, StatusCode::BAD_REQUEST)
    }

    pub(super) fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message, StatusCode::NOT_FOUND)
    }

    pub(super) fn failed_precondition(message: impl Into<String>) -> Self {
        Self::new("failed_precondition", message, StatusCode::BAD_REQUEST)
    }

    pub(super) fn unimplemented(message: impl Into<String>) -> Self {
        Self::new("unimplemented", message, StatusCode::NOT_IMPLEMENTED)
    }

    pub(super) fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::new("resource_exhausted", message, StatusCode::TOO_MANY_REQUESTS)
    }

    pub(super) fn unavailable(message: impl Into<String>) -> Self {
        Self::new("unavailable", message, StatusCode::SERVICE_UNAVAILABLE)
    }

    pub(super) fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message, StatusCode::INTERNAL_SERVER_ERROR)
    }

    fn new(code: &'static str, message: impl Into<String>, status: StatusCode) -> Self {
        Self {
            code,
            message: message.into(),
            status,
        }
    }

    pub(super) fn unary_response(&self) -> Response<Body> {
        response(
            self.status,
            CONTENT_TYPE_UNARY_JSON,
            Body::from(json!({ "code": self.code, "message": self.message }).to_string()),
        )
    }

    pub(super) fn end_stream_frame(&self) -> Vec<u8> {
        encode_json_frame(
            END_STREAM_FLAG,
            &json!({
                "error": {
                    "code": self.code,
                    "message": self.message,
                }
            }),
        )
    }
}

pub(super) async fn decode_unary<T>(request: Request<Body>) -> Result<T, ConnectFailure>
where
    T: DeserializeOwned,
{
    require_content_type(&request, CONTENT_TYPE_UNARY_JSON)?;
    let bytes = read_body(request.into_body()).await?;
    serde_json::from_slice(&bytes).map_err(|error| {
        ConnectFailure::invalid_argument(format!("invalid Connect JSON request: {error}"))
    })
}

pub(super) async fn decode_stream<T>(request: Request<Body>) -> Result<T, ConnectFailure>
where
    T: DeserializeOwned,
{
    require_content_type(&request, CONTENT_TYPE_STREAM_JSON)?;
    let bytes = read_body(request.into_body()).await?;
    if bytes.len() < 5 {
        return Err(ConnectFailure::invalid_argument(
            "Connect stream request is missing its envelope",
        ));
    }
    let flags = bytes[0];
    if flags != 0 {
        return Err(ConnectFailure::invalid_argument(format!(
            "unsupported Connect request envelope flags: {flags:#04x}"
        )));
    }
    let length = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    if length != bytes.len() - 5 {
        return Err(ConnectFailure::invalid_argument(
            "Connect request envelope length does not match its payload",
        ));
    }
    serde_json::from_slice(&bytes[5..]).map_err(|error| {
        ConnectFailure::invalid_argument(format!("invalid Connect JSON request: {error}"))
    })
}

pub(super) fn unary_ok<T>(value: &T) -> Response<Body>
where
    T: Serialize,
{
    match serde_json::to_vec(value) {
        Ok(body) => response(StatusCode::OK, CONTENT_TYPE_UNARY_JSON, Body::from(body)),
        Err(error) => ConnectFailure::internal(format!(
            "failed to serialize Connect JSON response: {error}"
        ))
        .unary_response(),
    }
}

pub(super) fn stream_response(body: Body) -> Response<Body> {
    response(StatusCode::OK, CONTENT_TYPE_STREAM_JSON, body)
}

pub(super) fn stream_error(error: &ConnectFailure) -> Response<Body> {
    stream_response(Body::from(error.end_stream_frame()))
}

pub(super) fn data_frame(value: &Value) -> Bytes {
    Bytes::from(encode_json_frame(0, value))
}

pub(super) fn success_end_stream_frame() -> Bytes {
    Bytes::from(encode_json_frame(END_STREAM_FLAG, &json!({})))
}

fn encode_json_frame(flags: u8, value: &Value) -> Vec<u8> {
    // serde_json::Value serialization is infallible for values constructed by
    // this module. Keep the fallback a valid Connect error envelope anyway.
    let payload = serde_json::to_vec(value).unwrap_or_else(|_| {
        br#"{"error":{"code":"internal","message":"response serialization failed"}}"#.to_vec()
    });
    let length = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut frame = Vec::with_capacity(payload.len().saturating_add(5));
    frame.push(flags);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

async fn read_body(mut body: Body) -> Result<Vec<u8>, ConnectFailure> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|error| {
            ConnectFailure::invalid_argument(format!("failed to read request body: {error}"))
        })?;
        if bytes.len().saturating_add(chunk.len()) > MAX_REQUEST_BYTES {
            return Err(ConnectFailure::invalid_argument(format!(
                "Connect request exceeds the {MAX_REQUEST_BYTES}-byte limit"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn require_content_type(
    request: &Request<Body>,
    expected: &'static str,
) -> Result<(), ConnectFailure> {
    let actual = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(ConnectFailure::invalid_argument(format!(
            "expected Content-Type {expected}"
        )))
    }
}

fn response(status: StatusCode, content_type: &'static str, body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}
