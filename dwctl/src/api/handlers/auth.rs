use axum::{
    extract::{Path, State},
    Json,
};
use uuid::Uuid;

use crate::{
    api::models::{
        auth::{
            AuthResponse, AuthSuccessResponse, ChangePasswordRequest, LoginInfo, LoginRequest, LoginResponse, LogoutResponse,
            PasswordResetConfirmRequest, PasswordResetRequest, PasswordResetResponse, RegisterRequest, RegisterResponse, RegistrationInfo,
        },
        users::{CurrentUser, Role, UserResponse},
    },
    auth::{password, session},
    db::{
        handlers::{credits::Credits, PasswordResetTokens, Repository, Users},
        models::{
            credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
            users::UserCreateDBRequest,
        },
    },
    email::EmailService,
    errors::Error,
    AppState,
};

/// Get registration information
#[utoipa::path(
    get,
    path = "/authentication/register",
    tag = "authentication",
    responses(
        (status = 200, description = "Registration info", body = RegistrationInfo),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_registration_info(State(state): State<AppState>) -> Result<Json<RegistrationInfo>, Error> {
    Ok(Json(RegistrationInfo {
        enabled: state.config.auth.native.enabled && state.config.auth.native.allow_registration,
        message: if state.config.auth.native.enabled && state.config.auth.native.allow_registration {
            "Registration is enabled".to_string()
        } else {
            "Registration is disabled".to_string()
        },
    }))
}

/// Register a new user account
#[utoipa::path(
    post,
    path = "/authentication/register",
    request_body = RegisterRequest,
    tag = "authentication",
    responses(
        (status = 201, description = "User registered successfully", body = AuthResponse),
        (status = 400, description = "Invalid input"),
        (status = 409, description = "User already exists"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn register(State(state): State<AppState>, Json(request): Json<RegisterRequest>) -> Result<RegisterResponse, Error> {
    // Check if native auth is enabled
    if !state.config.auth.native.enabled {
        return Err(Error::BadRequest {
            message: "Native authentication is disabled".to_string(),
        });
    }

    // Check if registration is allowed
    if !state.config.auth.native.allow_registration {
        return Err(Error::BadRequest {
            message: "User registration is disabled".to_string(),
        });
    }

    // Validate password length
    let password_config = &state.config.auth.native.password;
    if request.password.len() < password_config.min_length {
        return Err(Error::BadRequest {
            message: format!("Password must be at least {} characters", password_config.min_length),
        });
    }
    if request.password.len() > password_config.max_length {
        return Err(Error::BadRequest {
            message: format!("Password must be no more than {} characters", password_config.max_length),
        });
    }

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Check if user with this email already exists
    let mut user_repo = Users::new(&mut tx);
    if user_repo.get_user_by_email(&request.email).await?.is_some() {
        return Err(Error::BadRequest {
            message: "An account with this email address already exists".to_string(),
        });
    }

    // Hash the password on a blocking thread to avoid blocking async runtime
    let password = request.password.clone();
    let password_hash = tokio::task::spawn_blocking(move || password::hash_string(&password))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("spawn password hashing task: {e}"),
        })??;
    let create_request = UserCreateDBRequest {
        username: request.username,
        email: request.email,
        display_name: request.display_name,
        avatar_url: None,
        is_admin: false,
        roles: vec![Role::StandardUser],
        auth_source: "native".to_string(),
        password_hash: Some(password_hash),
    };

    let created_user = user_repo.create(&create_request).await?;

    // Give initial credits to standard users if configured
    let initial_credits = state.config.credits.initial_credits_for_standard_users;
    if initial_credits > rust_decimal::Decimal::ZERO && create_request.roles.contains(&Role::StandardUser) {
        let mut credits_repo = Credits::new(&mut tx);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: created_user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: initial_credits,
                source_id: uuid::Uuid::nil().to_string(), // System ID for initial credits
                description: Some("Initial credits on account creation".to_string()),
            })
            .await?;
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;
    let user_response = UserResponse::from(created_user);

    // Create session token
    let current_user = user_response.clone().into();
    let token = session::create_session_token(&current_user, &state.config)?;

    // Set session cookie
    let cookie = create_session_cookie(&token, &state.config);

    let auth_response = AuthResponse {
        user: user_response,
        message: "Registration successful".to_string(),
    };

    Ok(RegisterResponse { auth_response, cookie })
}

/// Get login information
#[utoipa::path(
    get,
    path = "/authentication/login",
    tag = "authentication",
    responses(
        (status = 200, description = "Login info", body = LoginInfo),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_login_info(State(state): State<AppState>) -> Result<Json<LoginInfo>, Error> {
    Ok(Json(LoginInfo {
        enabled: state.config.auth.native.enabled,
        message: if state.config.auth.native.enabled {
            "Native login is enabled".to_string()
        } else {
            "Native login is disabled".to_string()
        },
    }))
}

/// Login with email and password
#[utoipa::path(
    post,
    path = "/authentication/login",
    request_body = LoginRequest,
    tag = "authentication",
    responses(
        (status = 200, description = "Login successful", body = AuthResponse),
        (status = 401, description = "Invalid credentials"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn login(State(state): State<AppState>, Json(request): Json<LoginRequest>) -> Result<LoginResponse, Error> {
    // Check if native auth is enabled
    if !state.config.auth.native.enabled {
        return Err(Error::BadRequest {
            message: "Native authentication is disabled".to_string(),
        });
    }
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;

    let mut user_repo = Users::new(&mut pool_conn);

    // Find user by email
    let user = user_repo
        .get_user_by_email(&request.email)
        .await?
        .ok_or_else(|| Error::Unauthenticated {
            message: Some("Invalid email or password".to_string()),
        })?;

    // Check if user has a password (native auth)
    let password_hash = user.password_hash.as_ref().ok_or_else(|| Error::Unauthenticated {
        message: Some("Invalid email or password".to_string()),
    })?;

    // Verify password on a blocking thread to avoid blocking async runtime
    let password = request.password.clone();
    let hash = password_hash.clone();
    let is_valid = tokio::task::spawn_blocking(move || password::verify_string(&password, &hash))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("spawn password verification task: {e}"),
        })??;

    if !is_valid {
        return Err(Error::Unauthenticated {
            message: Some("Invalid email or password".to_string()),
        });
    }

    let user_response = UserResponse::from(user);

    // Create session token
    let current_user = user_response.clone().into();
    let token = session::create_session_token(&current_user, &state.config)?;

    // Set session cookie
    let cookie = create_session_cookie(&token, &state.config);

    let auth_response = AuthResponse {
        user: user_response,
        message: "Login successful".to_string(),
    };

    Ok(LoginResponse { auth_response, cookie })
}

/// Logout (clear session)
#[utoipa::path(
    post,
    path = "/authentication/logout",
    tag = "authentication",
    responses(
        (status = 200, description = "Logout successful", body = AuthSuccessResponse),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn logout(State(state): State<AppState>) -> Result<LogoutResponse, Error> {
    // Create expired cookie to clear session
    let cookie = format!(
        "{}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
        state.config.auth.native.session.cookie_name
    );

    let auth_response = AuthSuccessResponse {
        message: "Logout successful".to_string(),
    };

    Ok(LogoutResponse { auth_response, cookie })
}

/// Request password reset (send email)
#[utoipa::path(
    post,
    path = "/authentication/password-resets",
    request_body = PasswordResetRequest,
    tag = "authentication",
    responses(
        (status = 200, description = "Password reset email sent", body = PasswordResetResponse),
        (status = 400, description = "Invalid request"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn request_password_reset(
    State(state): State<AppState>,
    Json(request): Json<PasswordResetRequest>,
) -> Result<Json<PasswordResetResponse>, Error> {
    // Check if native auth is enabled
    if !state.config.auth.native.enabled {
        return Err(Error::BadRequest {
            message: "Native authentication is disabled".to_string(),
        });
    }
    let mut tx = state.db.begin().await.unwrap();

    let mut user_repo = Users::new(&mut tx);

    // Return success response to avoid email enumeration attacks
    // Only send email if user actually exists
    let user = user_repo.get_user_by_email(&request.email).await?;

    let mut token_repo = PasswordResetTokens::new(&mut tx);

    if let Some(user) = user {
        if user.password_hash.is_some() {
            // Only send reset email for native auth users (have password_hash)
            // Create reset token
            let (raw_token, token) = token_repo.create_for_user(user.id, &state.config).await?;

            // Send email with token ID
            let email_service = EmailService::new(&state.config)?;
            email_service
                .send_password_reset_email(&user.email, user.display_name.as_deref(), &token.id, &raw_token)
                .await?;
        }
    }
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(PasswordResetResponse {
        message: "If an account with that email exists, a password reset link has been sent.".to_string(),
    }))
}

/// Confirm password reset with token
#[utoipa::path(
    post,
    path = "/authentication/password-resets/{token_id}/confirm",
    request_body = PasswordResetConfirmRequest,
    tag = "authentication",
    responses(
        (status = 200, description = "Password reset successful", body = PasswordResetResponse),
        (status = 400, description = "Invalid or expired token"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn confirm_password_reset(
    State(state): State<AppState>,
    Path(token_id): Path<Uuid>,
    Json(request): Json<PasswordResetConfirmRequest>,
) -> Result<Json<PasswordResetResponse>, Error> {
    // Check if native auth is enabled
    if !state.config.auth.native.enabled {
        return Err(Error::BadRequest {
            message: "Native authentication is disabled".to_string(),
        });
    }

    // Validate password length
    let password_config = &state.config.auth.native.password;
    if request.new_password.len() < password_config.min_length {
        return Err(Error::BadRequest {
            message: format!("Password must be at least {} characters", password_config.min_length),
        });
    }
    if request.new_password.len() > password_config.max_length {
        return Err(Error::BadRequest {
            message: format!("Password must be no more than {} characters", password_config.max_length),
        });
    }

    // Hash new password
    let new_password_hash = tokio::task::spawn_blocking({
        let password = request.new_password.clone();
        move || password::hash_string(&password)
    })
    .await
    .map_err(|e| Error::Internal {
        operation: format!("spawn password hashing task: {e}"),
    })??;

    let update_request = crate::db::models::users::UserUpdateDBRequest {
        display_name: None,
        avatar_url: None,
        roles: None,
        password_hash: Some(new_password_hash),
    };

    let mut tx = state.db.begin().await.unwrap();
    let token;
    {
        let mut token_repo = PasswordResetTokens::new(&mut tx);

        // Find and validate token by ID
        token = token_repo
            .find_valid_token_by_id(token_id, &request.token)
            .await?
            .ok_or_else(|| Error::BadRequest {
                message: "Invalid or expired reset token".to_string(),
            })?;
    }

    {
        let mut user_repo = Users::new(&mut tx);

        // Update user password using repository
        let _user = user_repo.get_by_id(token.user_id).await?.ok_or_else(|| Error::BadRequest {
            message: "User not found".to_string(),
        })?;

        user_repo.update(token.user_id, &update_request).await?;
    }

    {
        // Invalidate all tokens for this user (including the current one) atomically
        // We do this after password update to ensure consistency
        let mut token_repo = PasswordResetTokens::new(&mut tx);
        token_repo.invalidate_for_user(token.user_id).await?;
    }
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(PasswordResetResponse {
        message: "Password has been reset successfully".to_string(),
    }))
}

/// Change password for authenticated user
#[utoipa::path(
    post,
    path = "/authentication/password-change",
    request_body = ChangePasswordRequest,
    tag = "authentication",
    responses(
        (status = 200, description = "Password changed successfully", body = AuthSuccessResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Current password is incorrect"),
    ),
    security(
        ("session_token" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn change_password(
    State(state): State<AppState>,
    current_user: CurrentUser,
    Json(request): Json<ChangePasswordRequest>,
) -> Result<Json<AuthSuccessResponse>, Error> {
    // Check if native auth is enabled
    if !state.config.auth.native.enabled {
        return Err(Error::BadRequest {
            message: "Native authentication is disabled".to_string(),
        });
    }

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut user_repo = Users::new(&mut pool_conn);

    // Get the user from database
    let user = user_repo.get_by_id(current_user.id).await?.ok_or_else(|| Error::Unauthenticated {
        message: Some("User not found".to_string()),
    })?;

    // Check if user has a password (native auth only)
    let password_hash = user.password_hash.as_ref().ok_or_else(|| Error::BadRequest {
        message: "Cannot change password for non-native authentication users".to_string(),
    })?;

    // Verify current password
    let current_password = request.current_password.clone();
    let hash = password_hash.clone();
    let is_valid = tokio::task::spawn_blocking(move || password::verify_string(&current_password, &hash))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("spawn password verification task: {e}"),
        })??;

    if !is_valid {
        return Err(Error::Unauthenticated {
            message: Some("Current password is incorrect".to_string()),
        });
    }

    // Validate new password length
    let password_config = &state.config.auth.native.password;
    if request.new_password.len() < password_config.min_length {
        return Err(Error::BadRequest {
            message: format!("Password must be at least {} characters", password_config.min_length),
        });
    }
    if request.new_password.len() > password_config.max_length {
        return Err(Error::BadRequest {
            message: format!("Password must be no more than {} characters", password_config.max_length),
        });
    }

    // Hash new password
    let new_password_hash = tokio::task::spawn_blocking({
        let password = request.new_password.clone();
        move || password::hash_string(&password)
    })
    .await
    .map_err(|e| Error::Internal {
        operation: format!("spawn password hashing task: {e}"),
    })??;

    // Update password
    let update_request = crate::db::models::users::UserUpdateDBRequest {
        display_name: None,
        avatar_url: None,
        roles: None,
        password_hash: Some(new_password_hash),
    };

    user_repo.update(current_user.id, &update_request).await?;

    Ok(Json(AuthSuccessResponse {
        message: "Password changed successfully".to_string(),
    }))
}

/// Helper function to create a session cookie
fn create_session_cookie(token: &str, config: &crate::config::Config) -> String {
    let session_config = &config.auth.native.session;
    let max_age = session_config.timeout.as_secs();

    format!(
        "{}={}; Path=/; HttpOnly; Secure={}; SameSite={}; Max-Age={}",
        session_config.cookie_name, token, session_config.cookie_secure, session_config.cookie_same_site, max_age
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_config;
    use axum_test::TestServer;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_register_success(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder().db(pool).config(config).request_manager(request_manager).build();

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = RegisterRequest {
            username: "testuser".to_string(),
            email: "test@example.com".to_string(),
            password: "password123".to_string(),
            display_name: Some("Test User".to_string()),
        };

        let response = server.post("/auth/register").json(&request).await;

        response.assert_status(axum::http::StatusCode::CREATED);
        assert!(response.headers().get("set-cookie").is_some());

        let body: AuthResponse = response.json();
        assert_eq!(body.user.email, "test@example.com");
        assert_eq!(body.message, "Registration successful");
    }

    #[sqlx::test]
    async fn test_register_disabled(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = false;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder().db(pool).config(config).request_manager(request_manager).build();

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = RegisterRequest {
            username: "testuser".to_string(),
            email: "test@example.com".to_string(),
            password: "password123".to_string(),
            display_name: None,
        };

        let response = server.post("/auth/register").json(&request).await;
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_password_validation(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.password.min_length = 10;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder().db(pool).config(config).request_manager(request_manager).build();

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = RegisterRequest {
            username: "testuser".to_string(),
            email: "test@example.com".to_string(),
            password: "short".to_string(), // Too short
            display_name: None,
        };

        let response = server.post("/auth/register").json(&request).await;
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_register_with_initial_credits(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;
        // Set initial credits for standard users
        config.credits.initial_credits_for_standard_users = rust_decimal::Decimal::new(10000, 2); // 100.00 credits

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(pool.clone())
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = RegisterRequest {
            username: "testuser_credits".to_string(),
            email: "credits@example.com".to_string(),
            password: "password123".to_string(),
            display_name: Some("Credits Test User".to_string()),
        };

        let response = server.post("/auth/register").json(&request).await;
        response.assert_status(axum::http::StatusCode::CREATED);

        let body: AuthResponse = response.json();
        assert_eq!(body.user.email, "credits@example.com");

        // Verify the user got initial credits
        let mut conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut conn);

        let balance = credits_repo.get_user_balance(body.user.id).await.unwrap();
        assert_eq!(
            balance,
            rust_decimal::Decimal::new(10000, 2),
            "User should have initial credits balance of 100.00"
        );

        // Verify the transaction exists with correct details
        let transactions = sqlx::query!(
            r#"SELECT amount, balance_after, transaction_type as "transaction_type: CreditTransactionType", source_id, description
               FROM credits_transactions
               WHERE user_id = $1"#,
            body.user.id
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(transactions.len(), 1, "Should have exactly one transaction");
        assert_eq!(transactions[0].amount, rust_decimal::Decimal::new(10000, 2));
        assert_eq!(transactions[0].balance_after, rust_decimal::Decimal::new(10000, 2));
        assert_eq!(transactions[0].transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transactions[0].source_id, Uuid::nil().to_string());
        assert!(transactions[0].description.as_ref().unwrap().contains("Initial credits"));
    }

    #[sqlx::test]
    async fn test_register_without_initial_credits_when_zero(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;
        // Set initial credits to zero (default)
        config.credits.initial_credits_for_standard_users = rust_decimal::Decimal::ZERO;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(pool.clone())
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = RegisterRequest {
            username: "testuser_nocredits".to_string(),
            email: "nocredits@example.com".to_string(),
            password: "password123".to_string(),
            display_name: Some("No Credits Test User".to_string()),
        };

        let response = server.post("/auth/register").json(&request).await;
        response.assert_status(axum::http::StatusCode::CREATED);

        let body: AuthResponse = response.json();
        assert_eq!(body.user.email, "nocredits@example.com");

        // Verify no credit transactions were created
        let transactions = sqlx::query!(r#"SELECT id FROM credits_transactions WHERE user_id = $1"#, body.user.id)
            .fetch_all(&pool)
            .await
            .unwrap();

        assert_eq!(
            transactions.len(),
            0,
            "Should have no credit transactions when initial credits is zero"
        );
    }
}
