use crate::db::errors::DbError;
use crate::types::{Operation, Permission};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

#[derive(ThisError, Debug)]
pub enum Error {
    /// Authentication required but not provided
    #[error("Not authenticated")]
    Unauthenticated { message: Option<String> },

    /// User lacks required permissions for the operation
    #[error("Insufficient permissions to {action:?} {resource}")]
    InsufficientPermissions {
        required: Permission,
        action: Operation,
        resource: String,
    },

    /// Invalid request data or business rule violation
    #[error("{message}")]
    BadRequest { message: String },

    /// Requested resource not found
    #[error("{resource} with ID {id} not found")]
    NotFound { resource: String, id: String },

    /// Generic internal service error
    #[error("Failed to {operation}")]
    Internal { operation: String },

    /// Database operation error
    #[error(transparent)]
    Database(#[from] DbError),

    /// Unexpected error with full context chain
    #[error(transparent)]
    Other(#[from] anyhow::Error),

    /// Conflict error, e.g., for unique constraint violations
    #[error("Conflict: {message}")]
    Conflict {
        message: String,
        conflicts: Option<Vec<AliasConflict>>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AliasConflict {
    pub model_name: String,
    pub attempted_alias: String,
}

impl Error {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Error::Unauthenticated { .. } => StatusCode::UNAUTHORIZED,
            Error::InsufficientPermissions { .. } => StatusCode::FORBIDDEN,
            Error::BadRequest { .. } => StatusCode::BAD_REQUEST,
            Error::NotFound { .. } => StatusCode::NOT_FOUND,
            Error::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Error::Database(db_err) => match db_err {
                DbError::NotFound => StatusCode::NOT_FOUND,
                DbError::UniqueViolation { .. } => StatusCode::CONFLICT,
                DbError::ForeignKeyViolation { .. } => StatusCode::BAD_REQUEST,
                DbError::CheckViolation { .. } => StatusCode::BAD_REQUEST,
                DbError::ProtectedEntity { .. } => StatusCode::FORBIDDEN,
                DbError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
            },
            Error::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Error::Conflict { .. } => StatusCode::CONFLICT,
        }
    }

    /// Returns a user-safe error message, without leaking internal implementation details
    pub fn user_message(&self) -> String {
        match self {
            Error::Unauthenticated { message } => message.clone().unwrap_or_else(|| "Authentication required".to_string()),
            Error::InsufficientPermissions { action, resource, .. } => {
                format!("Insufficient permissions to {action} {resource}")
            }
            Error::BadRequest { message } => message.clone(),
            Error::NotFound { resource, id } => {
                format!("{resource} with ID {id} not found")
            }
            Error::Internal { .. } => "Internal server error".to_string(),
            Error::Database(db_err) => match db_err {
                DbError::NotFound => "Resource not found".to_string(),
                DbError::UniqueViolation { constraint, table, .. } => {
                    // Provide user-friendly messages for common unique constraint violations
                    match (table.as_deref(), constraint.as_deref()) {
                        (Some("users"), Some(c)) if c.contains("email") => "An account with this email address already exists".to_string(),
                        (Some("users"), Some(c)) if c.contains("username") => "This username is already taken".to_string(),
                        (Some("deployed_models"), Some("deployed_models_alias_unique")) => {
                            "The specified alias is already in use. Please choose a different alias.".to_string()
                        }
                        _ => "Resource already exists".to_string(),
                    }
                }
                DbError::ForeignKeyViolation { .. } => "Invalid reference to related resource".to_string(),
                DbError::CheckViolation { .. } => "Invalid data provided".to_string(),
                DbError::ProtectedEntity {
                    operation,
                    entity_type,
                    reason,
                    ..
                } => {
                    format!("Cannot {operation:?} {entity_type}: {reason}")
                }
                DbError::Other(_) => "Database error occurred".to_string(),
            },
            Error::Other(_) => "Internal server error".to_string(),
            Error::Conflict { message, conflicts } => {
                if let Some(conflicts) = conflicts {
                    format!("{message}: {:?}", conflicts)
                } else {
                    message.clone()
                }
            }
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // Log full error details for debugging - different log levels based on severity
        match &self {
            Error::Database(DbError::Other(_)) | Error::Internal { .. } | Error::Other(_) => {
                tracing::error!("Internal service error: {:#}", self);
            }
            Error::Database(_) => {
                tracing::warn!("Database constraint error: {}", self);
            }
            Error::Unauthenticated { .. } | Error::InsufficientPermissions { .. } => {
                tracing::info!("Authorization error: {}", self);
            }
            Error::BadRequest { .. } | Error::NotFound { .. } => {
                tracing::debug!("Client error: {}", self);
            }
            Error::Conflict { .. } => {
                tracing::warn!("Conflict error: {}", self);
            }
        }

        let status = self.status_code();

        // Handle conflict errors with structured JSON response
        match &self {
            Error::Conflict { message, conflicts } => {
                use serde_json::json;
                let body = if let Some(conflicts) = conflicts {
                    json!({
                        "message": message,
                        "conflicts": conflicts
                    })
                } else {
                    json!({ "message": message })
                };

                (status, axum::response::Json(body)).into_response()
            }
            // Handle database unique violations with minimal structured JSON
            Error::Database(DbError::UniqueViolation { constraint, table, .. }) => {
                use serde_json::json;

                // Determine the resource and message only
                let (message, resource) = match (table.as_deref(), constraint.as_deref()) {
                    (Some("users"), Some(c)) if c.contains("email") => {
                        ("An account with this email address already exists".to_string(), "user")
                    }
                    (Some("users"), Some(c)) if c.contains("username") => ("This username is already taken".to_string(), "user"),
                    (Some("deployed_models"), Some("deployed_models_alias_unique")) => (
                        "The specified alias is already in use. Please choose a different alias.".to_string(),
                        "deployment",
                    ),
                    (Some("inference_endpoints"), Some(c)) if c.contains("name") => {
                        ("An endpoint with this name already exists".to_string(), "endpoint")
                    }
                    (Some("inference_endpoints"), Some(c)) if c.contains("url") => {
                        ("An endpoint with this URL already exists".to_string(), "endpoint")
                    }
                    _ => ("Resource already exists".to_string(), "unknown"),
                };

                let body = json!({
                    "message": message,
                    "resource": resource
                });

                (status, axum::response::Json(body)).into_response()
            }
            _ => {
                // For all other errors, return simple text message (unchanged)
                let user_message = self.user_message();
                (status, user_message).into_response()
            }
        }
    }
}

/// Convert from String errors (e.g., from external functions)
impl From<String> for Error {
    fn from(msg: String) -> Self {
        Error::Internal { operation: msg }
    }
}

/// Type alias for service operation results
pub type Result<T> = std::result::Result<T, Error>;
