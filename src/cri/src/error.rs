//! CRI-specific error conversions.

use a3s_box_core::error::BoxError;
use tonic::Status;

/// Convert a BoxError to a gRPC Status.
pub fn box_error_to_status(err: BoxError) -> Status {
    match err {
        BoxError::BoxBootError { message, hint } => {
            let msg = match hint {
                Some(h) => format!("{} (hint: {})", message, h),
                None => message,
            };
            Status::internal(msg)
        }
        BoxError::SessionError(msg) => Status::not_found(msg),
        BoxError::OciImageError(msg) => Status::not_found(msg),
        BoxError::RegistryError { registry, message } => {
            Status::unavailable(format!("{}: {}", registry, message))
        }
        BoxError::TimeoutError(msg) => Status::deadline_exceeded(msg),
        BoxError::ConfigError(msg) => Status::invalid_argument(msg),
        BoxError::IoError(e) => Status::internal(e.to_string()),
        BoxError::GrpcError(status) => status,
        BoxError::TeeConfig(msg) => Status::failed_precondition(msg),
        BoxError::TeeNotSupported(msg) => Status::failed_precondition(msg),
        other => Status::internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boot_error_maps_to_internal() {
        let err = BoxError::BoxBootError {
            message: "VM failed".to_string(),
            hint: None,
        };
        let status = box_error_to_status(err);
        assert_eq!(status.code(), tonic::Code::Internal);
    }

    #[test]
    fn test_boot_error_with_hint() {
        let err = BoxError::BoxBootError {
            message: "VM failed".to_string(),
            hint: Some("check kvm".to_string()),
        };
        let status = box_error_to_status(err);
        assert!(status.message().contains("hint"));
    }

    #[test]
    fn test_session_error_maps_to_not_found() {
        let err = BoxError::SessionError("not found".to_string());
        let status = box_error_to_status(err);
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[test]
    fn test_oci_image_error_maps_to_not_found() {
        let err = BoxError::OciImageError("bad image".to_string());
        let status = box_error_to_status(err);
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[test]
    fn test_registry_error_maps_to_unavailable() {
        let err = BoxError::RegistryError {
            registry: "ghcr.io".to_string(),
            message: "auth failed".to_string(),
        };
        let status = box_error_to_status(err);
        assert_eq!(status.code(), tonic::Code::Unavailable);
    }

    #[test]
    fn test_timeout_error_maps_to_deadline_exceeded() {
        let err = BoxError::TimeoutError("timed out".to_string());
        let status = box_error_to_status(err);
        assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
    }

    #[test]
    fn test_config_error_maps_to_invalid_argument() {
        let err = BoxError::ConfigError("bad config".to_string());
        let status = box_error_to_status(err);
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}
