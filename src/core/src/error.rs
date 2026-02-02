use thiserror::Error;

/// BoxLite error types
#[derive(Error, Debug)]
pub enum BoxError {
    /// VM failed to start
    #[error("VM boot failed: {message}")]
    BoxBootError {
        message: String,
        hint: Option<String>,
    },

    /// Session-related error
    #[error("Session error: {0}")]
    SessionError(String),

    /// Skill tool download failed
    #[error("Tool download failed: {url} -> {status_code}")]
    ToolDownloadError {
        url: String,
        status_code: u16,
        message: String,
    },

    /// Context window overflow
    #[error("Context overflow: {used}/{max} tokens")]
    ContextOverflowError {
        used: usize,
        max: usize,
    },

    /// LLM API error
    #[error("Model error: {provider} {status_code} - {message}")]
    ModelError {
        provider: String,
        status_code: u16,
        message: String,
    },

    /// Timeout error
    #[error("Timeout: {0}")]
    TimeoutError(String),

    /// gRPC communication error
    #[error("gRPC error: {0}")]
    GrpcError(#[from] tonic::Status),

    /// I/O error
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    /// Serialization error
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    ConfigError(String),

    /// Queue error
    #[error("Queue error: {0}")]
    QueueError(String),

    /// Skill error
    #[error("Skill error: {0}")]
    SkillError(String),

    /// Generic error
    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for BoxError {
    fn from(err: serde_json::Error) -> Self {
        BoxError::SerializationError(err.to_string())
    }
}

impl From<serde_yaml::Error> for BoxError {
    fn from(err: serde_yaml::Error) -> Self {
        BoxError::SerializationError(err.to_string())
    }
}

/// Result type alias for BoxLite operations
pub type Result<T> = std::result::Result<T, BoxError>;
