use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::control::{ControlServiceError, RepositoryError, TemplateProviderError};
use crate::volume::VolumeServiceError;

use super::{AuthenticationError, CursorError};

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: "Sandbox not found".to_string(),
        }
    }

    pub fn volume_not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: "Volume not found".to_string(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    pub fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "Internal server error".to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    code: u16,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            code: self.status.as_u16(),
            message: self.message,
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(error: JsonRejection) -> Self {
        Self::bad_request(format!("Invalid JSON request: {}", error.body_text()))
    }
}

impl From<AuthenticationError> for ApiError {
    fn from(error: AuthenticationError) -> Self {
        match error {
            AuthenticationError::Missing | AuthenticationError::Invalid => {
                Self::unauthorized("Invalid authentication credentials")
            }
            AuthenticationError::Unavailable(_) => Self::internal(),
        }
    }
}

impl From<CursorError> for ApiError {
    fn from(error: CursorError) -> Self {
        match error {
            CursorError::Invalid => Self::bad_request("Invalid pagination cursor"),
            CursorError::Unavailable(_) => Self::internal(),
        }
    }
}

impl From<ControlServiceError> for ApiError {
    fn from(error: ControlServiceError) -> Self {
        match error {
            ControlServiceError::InvalidRequest(message) => Self::bad_request(message),
            ControlServiceError::NotFound(_) => Self::not_found(),
            ControlServiceError::Conflict(_) => Self {
                status: StatusCode::CONFLICT,
                message: "Sandbox lifecycle conflict".to_string(),
            },
            ControlServiceError::Template(TemplateProviderError::NotFound(_)) => Self::not_found(),
            ControlServiceError::Template(TemplateProviderError::Invalid(message)) => {
                Self::bad_request(message)
            }
            ControlServiceError::Volume(VolumeServiceError::InvalidRequest(message)) => {
                Self::bad_request(message)
            }
            ControlServiceError::Volume(
                VolumeServiceError::NotFound
                | VolumeServiceError::Duplicate
                | VolumeServiceError::Conflict,
            ) => Self::bad_request("Volume mount is unavailable"),
            ControlServiceError::Repository(RepositoryError::Duplicate(_)) => Self {
                status: StatusCode::CONFLICT,
                message: "Sandbox already exists".to_string(),
            },
            ControlServiceError::Execution(a3s_box_core::ExecutionManagerError::NotFound(_)) => {
                Self::not_found()
            }
            ControlServiceError::Execution(a3s_box_core::ExecutionManagerError::Conflict {
                ..
            })
            | ControlServiceError::Lifecycle(_) => Self {
                status: StatusCode::CONFLICT,
                message: "Sandbox lifecycle conflict".to_string(),
            },
            ControlServiceError::Repository(_)
            | ControlServiceError::Execution(_)
            | ControlServiceError::Identity(_)
            | ControlServiceError::Template(_)
            | ControlServiceError::Credential(_)
            | ControlServiceError::Volume(_) => Self::internal(),
        }
    }
}

impl From<VolumeServiceError> for ApiError {
    fn from(error: VolumeServiceError) -> Self {
        match error {
            VolumeServiceError::InvalidRequest(message) => Self::bad_request(message),
            VolumeServiceError::NotFound => Self::volume_not_found(),
            VolumeServiceError::Duplicate => Self::conflict("Volume already exists"),
            VolumeServiceError::Conflict => Self::conflict("Volume is in use"),
            VolumeServiceError::Forbidden => Self::unauthorized("Invalid volume token"),
            VolumeServiceError::Repository(_)
            | VolumeServiceError::Runtime(_)
            | VolumeServiceError::Credential(_)
            | VolumeServiceError::Model(_)
            | VolumeServiceError::Content(_) => Self::internal(),
        }
    }
}
