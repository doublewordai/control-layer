//! Error types and HTTP response conversion.
//!
//! This module defines the application's error hierarchy and implements conversion
//! to HTTP responses with appropriate status codes and JSON payloads.
//!
//! # Error Hierarchy
//!
//! The main [`Error`] enum covers all application error cases:
//!
//! - **Authentication Errors**: `Unauthenticated` (401)
//! - **Authorization Errors**: `InsufficientPermissions` (403)
//! - **Validation Errors**: `BadRequest` (400)
//! - **Not Found Errors**: `NotFound` (404)
//! - **Conflict Errors**: `Conflict` (409) for unique constraint violations
//! - **Database Errors**: Wraps [`DbError`] with appropriate status codes
//! - **Internal Errors**: Generic server errors (500)
//!
//! # HTTP Response Conversion
//!
//! All errors implement [`IntoResponse`] for automatic conversion to HTTP responses
//! with JSON bodies:
//!
//! ```json
//! {
//!   "error": "Not Found",
//!   "message": "User with ID abc123 not found"
//! }
//! ```
//!
//! # Usage in Handlers
//!
//! Handlers can return `Result<T, Error>` and errors will automatically convert
//! to appropriate HTTP responses:
//!
//! ```ignore
//! use dwctl::errors::Error;
//!
//! async fn handler() -> Result<String, Error> {
//!     Err(Error::BadRequest {
//!         message: "Invalid input".to_string()
//!     })
//! }
//! ```
//!
//! # Error Construction Helpers
//!
//! The module provides convenience methods for common error types:
//!
//! ```ignore
//! // Not found error
//! return Err(Error::NotFound {
//!     resource: "User".to_string(),
//!     id: user_id.to_string(),
//! });
//!
//! // Permission error
//! return Err(Error::InsufficientPermissions {
//!     required: Permission::Admin,
//!     action: Operation::Delete,
//!     resource: "deployment".to_string(),
//! });
//! ```

use crate::db::errors::DbError;
use crate::types::{Operation, Permission};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

/// Retry-After header value (in seconds) for 503 Service Unavailable responses
/// when the database connection pool is exhausted.
const POOL_EXHAUSTED_RETRY_AFTER_SECS: &str = "30";

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

    /// Payload exceeds maximum allowed size
    #[error("Payload too large: {message}")]
    PayloadTooLarge { message: String },

    /// Insufficient credits to perform the requested operation
    #[error("Insufficient credits: {message}")]
    InsufficientCredits { current_balance: Decimal, message: String },

    /// User does not have access to the requested model
    #[error("Model access denied: {message}")]
    ModelAccessDenied { model_name: String, message: String },

    /// Too many concurrent requests - rate limiting
    #[error("Too many requests: {message}")]
    TooManyRequests { message: String },
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
                DbError::InvalidModelField { .. } => StatusCode::BAD_REQUEST,
                DbError::PoolExhausted => StatusCode::SERVICE_UNAVAILABLE,
                DbError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
            },
            Error::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Error::Conflict { .. } => StatusCode::CONFLICT,
            Error::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Error::InsufficientCredits { .. } => StatusCode::PAYMENT_REQUIRED,
            Error::ModelAccessDenied { .. } => StatusCode::FORBIDDEN,
            Error::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
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
            Error::PayloadTooLarge { message } => message.clone(),
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
                DbError::InvalidModelField { field } => format!("Field '{field}' must not be empty or whitespace"),
                DbError::PoolExhausted => "Service temporarily overloaded, please retry".to_string(),
                DbError::Other(_) => "Database error occurred".to_string(),
            },
            Error::Other(_) => "Internal server error".to_string(),
            Error::Conflict { message, conflicts } => {
                if let Some(conflicts) = conflicts {
                    let aliases: Vec<String> = conflicts.iter().map(|c| c.attempted_alias.to_string()).collect();
                    format!("{message}: {}", aliases.join(", "))
                } else {
                    message.clone()
                }
            }
            Error::InsufficientCredits { message, .. } => message.clone(),
            Error::ModelAccessDenied { message, .. } => message.clone(),
            Error::TooManyRequests { message } => message.clone(),
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
            Error::Database(DbError::PoolExhausted) => {
                tracing::warn!("Database connection pool exhausted - service overloaded");
            }
            Error::Database(_) => {
                tracing::warn!("Database constraint error: {}", self);
            }
            Error::Unauthenticated { .. } | Error::InsufficientPermissions { .. } => {
                tracing::info!("Authorization error: {}", self);
            }
            Error::BadRequest { .. } | Error::NotFound { .. } | Error::PayloadTooLarge { .. } => {
                tracing::debug!("Client error: {}", self);
            }
            Error::Conflict { .. } => {
                tracing::warn!("Conflict error: {}", self);
            }
            Error::InsufficientCredits { .. } => {
                tracing::info!("Insufficient credits error: {}", self);
            }
            Error::ModelAccessDenied { .. } => {
                tracing::info!("Model access denied error: {}", self);
            }
            Error::TooManyRequests { .. } => {
                tracing::info!("Rate limit exceeded: {}", self);
            }
        }

        let status = self.status_code();

        // Handle structured JSON responses for specific error types
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
            // Handle pool exhaustion with Retry-After header
            Error::Database(DbError::PoolExhausted) => {
                use axum::http::header::RETRY_AFTER;
                use serde_json::json;
                let body = json!({
                    "error": "service_unavailable",
                    "message": self.user_message(),
                    "retry_after_seconds": 30
                });
                (status, [(RETRY_AFTER, POOL_EXHAUSTED_RETRY_AFTER_SECS)], axum::response::Json(body)).into_response()
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
            Error::TooManyRequests { message } => {
                use axum::http::header::RETRY_AFTER;
                use serde_json::json;

                // Suggest retry after 60 seconds for capacity-based rejections
                let retry_after_secs = "60";

                let body = json!({
                    "error": "too_many_requests",
                    "message": message,
                    "retry_after_seconds": 30
                });

                (status, [(RETRY_AFTER, retry_after_secs)], axum::response::Json(body)).into_response()
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
