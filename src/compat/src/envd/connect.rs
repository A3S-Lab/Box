//! Connect JSON and Protobuf framing for the pinned E2B envd clients.

use std::marker::PhantomData;

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, Request, Response, StatusCode};
use hyper::body::{Bytes, HttpBody};
use prost::Message;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

const CONTENT_TYPE_UNARY_JSON: &str = "application/json";
const CONTENT_TYPE_UNARY_PROTO: &str = "application/proto";
const CONTENT_TYPE_STREAM_JSON: &str = "application/connect+json";
const CONTENT_TYPE_STREAM_PROTO: &str = "application/connect+proto";
const END_STREAM_FLAG: u8 = 0x02;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectEncoding {
    Json,
    Protobuf,
}

impl ConnectEncoding {
    fn unary_content_type(self) -> &'static str {
        match self {
            Self::Json => CONTENT_TYPE_UNARY_JSON,
            Self::Protobuf => CONTENT_TYPE_UNARY_PROTO,
        }
    }

    fn stream_content_type(self) -> &'static str {
        match self {
            Self::Json => CONTENT_TYPE_STREAM_JSON,
            Self::Protobuf => CONTENT_TYPE_STREAM_PROTO,
        }
    }
}

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

    pub(super) fn unsupported_media_type(message: impl Into<String>) -> Self {
        Self::new(
            "invalid_argument",
            message,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        )
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

pub(super) fn stream_encoding(request: &Request<Body>) -> Result<ConnectEncoding, ConnectFailure> {
    match content_type(request) {
        Some(actual) if actual.eq_ignore_ascii_case(CONTENT_TYPE_STREAM_JSON) => {
            Ok(ConnectEncoding::Json)
        }
        Some(actual) if actual.eq_ignore_ascii_case(CONTENT_TYPE_STREAM_PROTO) => {
            Ok(ConnectEncoding::Protobuf)
        }
        _ => Err(ConnectFailure::unsupported_media_type(format!(
            "expected Content-Type {CONTENT_TYPE_STREAM_JSON} or {CONTENT_TYPE_STREAM_PROTO}"
        ))),
    }
}

pub(super) async fn decode_unary<T>(
    request: Request<Body>,
) -> Result<(T, ConnectEncoding), ConnectFailure>
where
    T: DeserializeOwned + Message + Default,
{
    let encoding = match content_type(&request) {
        Some(actual) if actual.eq_ignore_ascii_case(CONTENT_TYPE_UNARY_JSON) => {
            ConnectEncoding::Json
        }
        Some(actual) if actual.eq_ignore_ascii_case(CONTENT_TYPE_UNARY_PROTO) => {
            ConnectEncoding::Protobuf
        }
        _ => {
            return Err(ConnectFailure::unsupported_media_type(format!(
                "expected Content-Type {CONTENT_TYPE_UNARY_JSON} or {CONTENT_TYPE_UNARY_PROTO}"
            )))
        }
    };
    let bytes = read_body(request.into_body()).await?;
    decode_payload(&bytes, encoding).map(|message| (message, encoding))
}

pub(super) async fn decode_stream<T>(
    request: Request<Body>,
    encoding: ConnectEncoding,
) -> Result<T, ConnectFailure>
where
    T: DeserializeOwned + Message + Default,
{
    let mut stream = decode_client_stream(request, encoding);
    let message = stream.next().await?.ok_or_else(|| {
        ConnectFailure::invalid_argument("Connect stream request is missing its envelope")
    })?;
    if stream.next().await?.is_some() {
        return Err(ConnectFailure::invalid_argument(
            "Connect procedure expects exactly one request envelope",
        ));
    }
    Ok(message)
}

pub(super) fn decode_client_stream<T>(
    request: Request<Body>,
    encoding: ConnectEncoding,
) -> ConnectStream<T>
where
    T: DeserializeOwned + Message + Default,
{
    ConnectStream {
        body: request.into_body(),
        chunk: Bytes::new(),
        header: [0; 5],
        header_len: 0,
        encoding,
        message: PhantomData,
    }
}

pub(super) struct ConnectStream<T> {
    body: Body,
    chunk: Bytes,
    header: [u8; 5],
    header_len: usize,
    encoding: ConnectEncoding,
    message: PhantomData<T>,
}

impl<T> ConnectStream<T>
where
    T: DeserializeOwned + Message + Default,
{
    /// Decode one envelope without buffering the complete request stream.
    ///
    /// Only the current transport chunk and one bounded message payload are
    /// retained. Awaiting each message before reading the next one propagates
    /// backpressure to long-lived stdin streams.
    pub(super) async fn next(&mut self) -> Result<Option<T>, ConnectFailure> {
        while self.header_len < self.header.len() {
            if self.chunk.is_empty() {
                self.chunk = match self.body.data().await {
                    Some(Ok(chunk)) if !chunk.is_empty() => chunk,
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => {
                        return Err(ConnectFailure::invalid_argument(format!(
                            "failed to read request body: {error}"
                        )))
                    }
                    None if self.header_len == 0 => return Ok(None),
                    None => {
                        return Err(ConnectFailure::invalid_argument(
                            "Connect request ended inside an envelope header",
                        ))
                    }
                };
            }
            let count = (self.header.len() - self.header_len).min(self.chunk.len());
            self.header[self.header_len..self.header_len + count]
                .copy_from_slice(&self.chunk[..count]);
            self.header_len += count;
            self.chunk = self.chunk.slice(count..);
        }

        let flags = self.header[0];
        if flags & END_STREAM_FLAG != 0 {
            self.header_len = 0;
            return Err(ConnectFailure::invalid_argument(
                "Connect request streams cannot contain end-stream envelopes",
            ));
        }
        if flags != 0 {
            self.header_len = 0;
            return Err(ConnectFailure::invalid_argument(format!(
                "unsupported Connect request envelope flags: {flags:#04x}"
            )));
        }
        let length = u32::from_be_bytes([
            self.header[1],
            self.header[2],
            self.header[3],
            self.header[4],
        ]) as usize;
        self.header_len = 0;
        if length > MAX_REQUEST_BYTES {
            return Err(ConnectFailure::invalid_argument(format!(
                "Connect request envelope exceeds the {MAX_REQUEST_BYTES}-byte limit"
            )));
        }

        let mut payload = Vec::with_capacity(length);
        while payload.len() < length {
            if self.chunk.is_empty() {
                self.chunk = match self.body.data().await {
                    Some(Ok(chunk)) if !chunk.is_empty() => chunk,
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => {
                        return Err(ConnectFailure::invalid_argument(format!(
                            "failed to read request body: {error}"
                        )))
                    }
                    None => {
                        return Err(ConnectFailure::invalid_argument(
                            "Connect request ended inside an envelope payload",
                        ))
                    }
                };
            }
            let count = (length - payload.len()).min(self.chunk.len());
            payload.extend_from_slice(&self.chunk[..count]);
            self.chunk = self.chunk.slice(count..);
        }

        decode_payload(&payload, self.encoding).map(Some)
    }
}

pub(super) fn unary_ok<T>(value: &T, encoding: ConnectEncoding) -> Response<Body>
where
    T: Serialize + Message,
{
    match encode_payload(value, encoding) {
        Ok(body) => response(
            StatusCode::OK,
            encoding.unary_content_type(),
            Body::from(body),
        ),
        Err(error) => error.unary_response(),
    }
}

pub(super) fn stream_response(body: Body, encoding: ConnectEncoding) -> Response<Body> {
    response(StatusCode::OK, encoding.stream_content_type(), body)
}

pub(super) fn stream_error(error: &ConnectFailure, encoding: ConnectEncoding) -> Response<Body> {
    stream_response(Body::from(error.end_stream_frame()), encoding)
}

pub(super) fn stream_unary_ok<T>(value: &T, encoding: ConnectEncoding) -> Response<Body>
where
    T: Serialize + Message,
{
    match data_frame(value, encoding) {
        Ok(frame) => {
            let mut body = frame.to_vec();
            body.extend_from_slice(&success_end_stream_frame());
            stream_response(Body::from(body), encoding)
        }
        Err(error) => stream_error(&error, encoding),
    }
}

pub(super) fn data_frame<T>(value: &T, encoding: ConnectEncoding) -> Result<Bytes, ConnectFailure>
where
    T: Serialize + Message,
{
    let payload = encode_payload(value, encoding)?;
    let length = u32::try_from(payload.len()).map_err(|_| {
        ConnectFailure::internal("Connect response exceeds the 32-bit envelope limit")
    })?;
    let mut frame = Vec::with_capacity(payload.len().saturating_add(5));
    frame.push(0);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(Bytes::from(frame))
}

pub(super) fn success_end_stream_frame() -> Bytes {
    Bytes::from(encode_json_frame(END_STREAM_FLAG, &json!({})))
}

fn decode_payload<T>(bytes: &[u8], encoding: ConnectEncoding) -> Result<T, ConnectFailure>
where
    T: DeserializeOwned + Message + Default,
{
    match encoding {
        ConnectEncoding::Json => serde_json::from_slice(bytes).map_err(|error| {
            ConnectFailure::invalid_argument(format!("invalid Connect JSON request: {error}"))
        }),
        ConnectEncoding::Protobuf => T::decode(bytes).map_err(|error| {
            ConnectFailure::invalid_argument(format!("invalid Connect Protobuf request: {error}"))
        }),
    }
}

fn encode_payload<T>(value: &T, encoding: ConnectEncoding) -> Result<Vec<u8>, ConnectFailure>
where
    T: Serialize + Message,
{
    match encoding {
        ConnectEncoding::Json => serde_json::to_vec(value).map_err(|error| {
            ConnectFailure::internal(format!(
                "failed to serialize Connect JSON response: {error}"
            ))
        }),
        ConnectEncoding::Protobuf => Ok(value.encode_to_vec()),
    }
}

fn encode_json_frame(flags: u8, value: &serde_json::Value) -> Vec<u8> {
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

fn content_type(request: &Request<Body>) -> Option<&str> {
    request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
}

fn response(status: StatusCode, content_type: &'static str, body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}
