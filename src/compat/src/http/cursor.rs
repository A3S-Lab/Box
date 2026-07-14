use thiserror::Error;

use crate::control::SandboxCursor;

#[derive(Debug, Error)]
pub enum CursorError {
    #[error("sandbox cursor is invalid")]
    Invalid,
    #[error("sandbox cursor provider is unavailable: {0}")]
    Unavailable(String),
}

pub type CursorResult<T> = std::result::Result<T, CursorError>;

pub trait CursorDecoder: Send + Sync {
    fn decode(&self, value: &str) -> CursorResult<Option<SandboxCursor>>;
}

#[derive(Debug, Default)]
pub struct RejectingCursorDecoder;

impl CursorDecoder for RejectingCursorDecoder {
    fn decode(&self, _value: &str) -> CursorResult<Option<SandboxCursor>> {
        Err(CursorError::Invalid)
    }
}
