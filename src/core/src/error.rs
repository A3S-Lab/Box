use thiserror::Error;

/// A3S Box error types
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
    ContextOverflowError { used: usize, max: usize },

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

    /// Context provider error
    #[error("Context error: {provider} - {message}")]
    ContextError { provider: String, message: String },

    /// TEE configuration error
    #[error("TEE configuration error: {0}")]
    TeeConfig(String),

    /// TEE hardware not available
    #[error("TEE hardware not available: {0}")]
    TeeNotSupported(String),

    /// OCI image error
    #[error("OCI image error: {0}")]
    OciImageError(String),

    /// Container registry error
    #[error("Registry error: {registry} - {message}")]
    RegistryError { registry: String, message: String },

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

/// Result type alias for A3S Box operations
pub type Result<T> = std::result::Result<T, BoxError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_box_boot_error_display() {
        let error = BoxError::BoxBootError {
            message: "Failed to start VM".to_string(),
            hint: Some("Check virtualization support".to_string()),
        };
        assert_eq!(error.to_string(), "VM boot failed: Failed to start VM");
    }

    #[test]
    fn test_box_boot_error_without_hint() {
        let error = BoxError::BoxBootError {
            message: "No kernel found".to_string(),
            hint: None,
        };
        assert_eq!(error.to_string(), "VM boot failed: No kernel found");
    }

    #[test]
    fn test_session_error_display() {
        let error = BoxError::SessionError("Session not found".to_string());
        assert_eq!(error.to_string(), "Session error: Session not found");
    }

    #[test]
    fn test_tool_download_error_display() {
        let error = BoxError::ToolDownloadError {
            url: "https://example.com/tool".to_string(),
            status_code: 404,
            message: "Not Found".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "Tool download failed: https://example.com/tool -> 404"
        );
    }

    #[test]
    fn test_context_overflow_error_display() {
        let error = BoxError::ContextOverflowError {
            used: 150000,
            max: 128000,
        };
        assert_eq!(error.to_string(), "Context overflow: 150000/128000 tokens");
    }

    #[test]
    fn test_model_error_display() {
        let error = BoxError::ModelError {
            provider: "anthropic".to_string(),
            status_code: 429,
            message: "Rate limit exceeded".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "Model error: anthropic 429 - Rate limit exceeded"
        );
    }

    #[test]
    fn test_timeout_error_display() {
        let error = BoxError::TimeoutError("Operation timed out after 30s".to_string());
        assert_eq!(error.to_string(), "Timeout: Operation timed out after 30s");
    }

    #[test]
    fn test_io_error_conversion() {
        let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let box_error: BoxError = io_error.into();
        assert!(matches!(box_error, BoxError::IoError(_)));
        assert!(box_error.to_string().contains("file not found"));
    }

    #[test]
    fn test_serialization_error_display() {
        let error = BoxError::SerializationError("Invalid JSON".to_string());
        assert_eq!(error.to_string(), "Serialization error: Invalid JSON");
    }

    #[test]
    fn test_config_error_display() {
        let error = BoxError::ConfigError("Missing required field".to_string());
        assert_eq!(
            error.to_string(),
            "Configuration error: Missing required field"
        );
    }

    #[test]
    fn test_queue_error_display() {
        let error = BoxError::QueueError("Lane not found".to_string());
        assert_eq!(error.to_string(), "Queue error: Lane not found");
    }

    #[test]
    fn test_skill_error_display() {
        let error = BoxError::SkillError("Skill parsing failed".to_string());
        assert_eq!(error.to_string(), "Skill error: Skill parsing failed");
    }

    #[test]
    fn test_context_error_display() {
        let error = BoxError::ContextError {
            provider: "openviking".to_string(),
            message: "Connection refused".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "Context error: openviking - Connection refused"
        );
    }

    #[test]
    fn test_other_error_display() {
        let error = BoxError::Other("Unknown error occurred".to_string());
        assert_eq!(error.to_string(), "Unknown error occurred");
    }

    #[test]
    fn test_tee_config_error_display() {
        let error = BoxError::TeeConfig("Failed to set TEE config file".to_string());
        assert_eq!(
            error.to_string(),
            "TEE configuration error: Failed to set TEE config file"
        );
    }

    #[test]
    fn test_tee_not_supported_error_display() {
        let error = BoxError::TeeNotSupported("AMD SEV-SNP not available".to_string());
        assert_eq!(
            error.to_string(),
            "TEE hardware not available: AMD SEV-SNP not available"
        );
    }

    #[test]
    fn test_oci_image_error_display() {
        let error = BoxError::OciImageError("Invalid manifest".to_string());
        assert_eq!(error.to_string(), "OCI image error: Invalid manifest");
    }

    #[test]
    fn test_registry_error_display() {
        let error = BoxError::RegistryError {
            registry: "ghcr.io".to_string(),
            message: "Authentication failed".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "Registry error: ghcr.io - Authentication failed"
        );
    }

    #[test]
    fn test_serde_json_error_conversion() {
        let json_str = "{ invalid json }";
        let result: std::result::Result<serde_json::Value, _> = serde_json::from_str(json_str);
        let json_error = result.unwrap_err();
        let box_error: BoxError = json_error.into();
        assert!(matches!(box_error, BoxError::SerializationError(_)));
    }

    #[test]
    fn test_serde_yaml_error_conversion() {
        let yaml_str = "invalid: yaml: content:";
        let result: std::result::Result<serde_yaml::Value, _> = serde_yaml::from_str(yaml_str);
        let yaml_error = result.unwrap_err();
        let box_error: BoxError = yaml_error.into();
        assert!(matches!(box_error, BoxError::SerializationError(_)));
    }

    #[test]
    fn test_result_type_alias() {
        fn returns_ok() -> Result<i32> {
            Ok(42)
        }

        fn returns_err() -> Result<i32> {
            Err(BoxError::Other("test error".to_string()))
        }

        assert_eq!(returns_ok().unwrap(), 42);
        assert!(returns_err().is_err());
    }

    #[test]
    fn test_error_is_debug() {
        let error = BoxError::SessionError("test".to_string());
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("SessionError"));
    }
}
