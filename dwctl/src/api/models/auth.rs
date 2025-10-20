use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::models::users::UserResponse;

/// Registration information
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RegistrationInfo {
    /// Whether registration is enabled
    pub enabled: bool,
    /// Status message
    pub message: String,
}

/// Login information
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct LoginInfo {
    /// Whether native login is enabled
    pub enabled: bool,
    /// Status message
    pub message: String,
}

/// Request to register a new user
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RegisterRequest {
    /// Username (must be unique)
    pub username: String,
    /// Email address (must be unique)
    pub email: String,
    /// Password (will be hashed)
    pub password: String,
    /// Optional display name
    pub display_name: Option<String>,
}

/// Request to login
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct LoginRequest {
    /// Email address
    pub email: String,
    /// Password
    pub password: String,
}

/// Response after successful login or registration
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AuthResponse {
    /// User information
    pub user: UserResponse,
    /// Success message
    pub message: String,
}

/// Generic success response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AuthSuccessResponse {
    pub message: String,
}

/// Response models that implement IntoResponse for cleaner handler code
use axum::{
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

/// Structured response for successful registration
pub struct RegisterResponse {
    pub auth_response: AuthResponse,
    pub cookie: String,
}

impl IntoResponse for RegisterResponse {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(header::SET_COOKIE, self.cookie.parse().unwrap());
        (StatusCode::CREATED, headers, Json(self.auth_response)).into_response()
    }
}

/// Structured response for successful login
pub struct LoginResponse {
    pub auth_response: AuthResponse,
    pub cookie: String,
}

impl IntoResponse for LoginResponse {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(header::SET_COOKIE, self.cookie.parse().unwrap());
        (StatusCode::OK, headers, Json(self.auth_response)).into_response()
    }
}

/// Structured response for successful logout
pub struct LogoutResponse {
    pub auth_response: AuthSuccessResponse,
    pub cookie: String,
}

/// Request to initiate password reset
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PasswordResetRequest {
    /// Email address to send reset link to
    pub email: String,
}

/// Request to confirm password reset with token
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PasswordResetConfirmRequest {
    /// Reset token from email
    pub token: String,
    /// New password
    pub new_password: String,
}

/// Response for password reset operations
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PasswordResetResponse {
    /// Success message
    pub message: String,
}

/// Request to change password (for authenticated users)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    /// Current password (for verification)
    pub current_password: String,
    /// New password
    pub new_password: String,
}

impl IntoResponse for LogoutResponse {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(header::SET_COOKIE, self.cookie.parse().unwrap());
        (StatusCode::OK, headers, Json(self.auth_response)).into_response()
    }
}
