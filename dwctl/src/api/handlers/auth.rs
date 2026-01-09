//! HTTP handlers for authentication endpoints.

use axum::{
    Json,
    extract::{Path, State},
};
use uuid::Uuid;

use crate::{
    AppState,
    api::models::{
        auth::{
            AuthResponse, AuthSuccessResponse, ChangePasswordRequest, LoginInfo, LoginRequest, LoginResponse, LogoutResponse,
            PasswordResetConfirmRequest, PasswordResetRequest, PasswordResetResponse, RegisterRequest, RegisterResponse, RegistrationInfo,
        },
        users::{CurrentUser, Role, UserResponse},
    },
    auth::{password, session},
    db::{
        handlers::{Deployments, PasswordResetTokens, Repository, Users, api_keys::ApiKeys, credits::Credits},
        models::{
            api_keys::ApiKeyPurpose, credits::CreditTransactionCreateDBRequest, deployments::ModelStatus, users::UserCreateDBRequest,
        },
    },
    email::EmailService,
    errors::Error,
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
    let argon2_params = password::Argon2Params {
        memory_kib: password_config.argon2_memory_kib,
        iterations: password_config.argon2_iterations,
        parallelism: password_config.argon2_parallelism,
    };
    let password_hash = tokio::task::spawn_blocking(move || password::hash_string_with_params(&password, Some(argon2_params)))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("spawn password hashing task: {e}"),
        })??;
    // Generate a random display name if not provided
    let display_name = if request.display_name.is_none() {
        Some(crate::auth::utils::generate_random_display_name())
    } else {
        request.display_name
    };

    let create_request = UserCreateDBRequest {
        username: request.username,
        email: request.email,
        display_name,
        avatar_url: None,
        is_admin: false,
        roles: state.config.auth.default_user_roles.clone(),
        auth_source: "native".to_string(),
        password_hash: Some(password_hash),
        external_user_id: None,
    };

    let created_user = user_repo.create(&create_request).await?;

    // Give initial credits to standard users if configured
    let initial_credits = state.config.credits.initial_credits_for_standard_users;
    if initial_credits > rust_decimal::Decimal::ZERO && create_request.roles.contains(&Role::StandardUser) {
        let mut credits_repo = Credits::new(&mut tx);
        let request = CreditTransactionCreateDBRequest::admin_grant(
            created_user.id,
            uuid::Uuid::nil(), // System ID for initial credits
            initial_credits,
            Some("Initial credits on account creation".to_string()),
        );
        credits_repo.create_transaction(&request).await?;
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    // Create sample files for new user if enabled (non-blocking, failures are logged)
    if state.config.sample_files.enabled && state.config.batches.enabled {
        let user_id = created_user.id;
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = create_sample_files_for_new_user(&state_clone, user_id).await {
                tracing::warn!(user_id = %user_id, error = %e, "Failed to create sample files for new user");
            }
        });
    }

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

/// Helper function to create sample files for a newly registered user.
///
/// This function is called asynchronously after user registration to avoid
/// blocking the registration response. Failures are logged but don't affect
/// the user creation.
async fn create_sample_files_for_new_user(state: &AppState, user_id: Uuid) -> Result<(), Error> {
    use crate::db::handlers::deployments::DeploymentFilter;
    use crate::sample_files;

    let mut conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Get the user's batch API key
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    let api_key = api_keys_repo
        .get_or_create_hidden_key(user_id, ApiKeyPurpose::Batch)
        .await
        .map_err(Error::Database)?;

    // Get deployments accessible to this user
    let mut deployments_repo = Deployments::new(&mut conn);
    let filter = DeploymentFilter::new(0, i64::MAX)
        .with_accessible_to(user_id)
        .with_statuses(vec![ModelStatus::Active])
        .with_deleted(false);
    let accessible_deployments = deployments_repo.list(&filter).await.map_err(Error::Database)?;

    // Construct batch execution endpoint
    let endpoint = format!("http://{}:{}/ai", state.config.host, state.config.port);

    // Create sample files using the sample_files module
    let created_files = sample_files::create_sample_files_for_user(
        state.request_manager.as_ref(),
        user_id,
        &api_key,
        &endpoint,
        &accessible_deployments,
        &state.config.sample_files,
    )
    .await?;

    tracing::info!(
        user_id = %user_id,
        file_count = created_files.len(),
        "Created sample files for new user"
    );

    Ok(())
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

    if let Some(user) = user
        && user.password_hash.is_some()
    {
        // Only send reset email for native auth users (have password_hash)
        // Create reset token
        let (raw_token, token) = token_repo.create_for_user(user.id, &state.config).await?;

        // Send email with token ID
        let email_service = EmailService::new(&state.config)?;
        email_service
            .send_password_reset_email(&user.email, user.display_name.as_deref(), &token.id, &raw_token)
            .await?;
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
        let argon2_params = password::Argon2Params {
            memory_kib: password_config.argon2_memory_kib,
            iterations: password_config.argon2_iterations,
            parallelism: password_config.argon2_parallelism,
        };
        move || password::hash_string_with_params(&password, Some(argon2_params))
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
        let argon2_params = password::Argon2Params {
            memory_kib: password_config.argon2_memory_kib,
            iterations: password_config.argon2_iterations,
            parallelism: password_config.argon2_parallelism,
        };
        move || password::hash_string_with_params(&password, Some(argon2_params))
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
    use crate::{db::models::credits::CreditTransactionType, test::utils::create_test_config};
    use axum_test::TestServer;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_register_success(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

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
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

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
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

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
            .db(crate::db::DbPools::new(pool.clone()))
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
        let transactions = credits_repo
            .list_user_transactions(body.user.id, 0, 10, &TransactionFilters::default())
            .await
            .unwrap();

        assert_eq!(transactions.len(), 1, "Should have exactly one transaction");
        assert_eq!(transactions[0].amount, rust_decimal::Decimal::new(10000, 2));
        assert_eq!(transactions[0].transaction_type, CreditTransactionType::AdminGrant);
        assert!(transactions[0].description.as_ref().unwrap().contains("Initial credits"));

        // Verify balance is correct via get_user_balance
        let balance = credits_repo.get_user_balance(body.user.id).await.unwrap();
        assert_eq!(balance, rust_decimal::Decimal::new(10000, 2));
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
            .db(crate::db::DbPools::new(pool.clone()))
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

    #[sqlx::test]
    async fn test_register_auto_generates_display_name(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        // Register without providing a display name
        let request = RegisterRequest {
            username: "autogen_user".to_string(),
            email: "autogen@example.com".to_string(),
            password: "password123".to_string(),
            display_name: None, // No display name provided
        };

        let response = server.post("/auth/register").json(&request).await;
        response.assert_status(axum::http::StatusCode::CREATED);

        let body: AuthResponse = response.json();
        assert_eq!(body.user.email, "autogen@example.com");

        // Verify display name was auto-generated
        assert!(body.user.display_name.is_some(), "Display name should be auto-generated");
        let display_name = body.user.display_name.unwrap();

        // Verify format: "{adjective} {noun} {4-digit number}"
        let parts: Vec<&str> = display_name.split_whitespace().collect();
        assert_eq!(parts.len(), 3, "Display name should have 3 parts, got: {}", display_name);
        assert!(
            parts[2].len() == 4 && parts[2].parse::<u32>().is_ok(),
            "Third part should be a 4-digit number, got: {}",
            parts[2]
        );
    }

    #[sqlx::test]
    async fn test_get_registration_info_enabled(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/register-info", axum::routing::get(get_registration_info))
            .with_state(state);

        let server = TestServer::new(app).unwrap();
        let response = server.get("/auth/register-info").await;

        response.assert_status(axum::http::StatusCode::OK);
        let body: RegistrationInfo = response.json();
        assert!(body.enabled);
        assert_eq!(body.message, "Registration is enabled");
    }

    #[sqlx::test]
    async fn test_get_registration_info_disabled_native_auth(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = false;
        config.auth.native.allow_registration = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/register-info", axum::routing::get(get_registration_info))
            .with_state(state);

        let server = TestServer::new(app).unwrap();
        let response = server.get("/auth/register-info").await;

        response.assert_status(axum::http::StatusCode::OK);
        let body: RegistrationInfo = response.json();
        assert!(!body.enabled);
        assert_eq!(body.message, "Registration is disabled");
    }

    #[sqlx::test]
    async fn test_get_registration_info_disabled_allow_registration(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = false;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/register-info", axum::routing::get(get_registration_info))
            .with_state(state);

        let server = TestServer::new(app).unwrap();
        let response = server.get("/auth/register-info").await;

        response.assert_status(axum::http::StatusCode::OK);
        let body: RegistrationInfo = response.json();
        assert!(!body.enabled);
        assert_eq!(body.message, "Registration is disabled");
    }

    #[sqlx::test]
    async fn test_get_login_info_enabled(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/login-info", axum::routing::get(get_login_info))
            .with_state(state);

        let server = TestServer::new(app).unwrap();
        let response = server.get("/auth/login-info").await;

        response.assert_status(axum::http::StatusCode::OK);
        let body: LoginInfo = response.json();
        assert!(body.enabled);
        assert_eq!(body.message, "Native login is enabled");
    }

    #[sqlx::test]
    async fn test_get_login_info_disabled(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = false;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/login-info", axum::routing::get(get_login_info))
            .with_state(state);

        let server = TestServer::new(app).unwrap();
        let response = server.get("/auth/login-info").await;

        response.assert_status(axum::http::StatusCode::OK);
        let body: LoginInfo = response.json();
        assert!(!body.enabled);
        assert_eq!(body.message, "Native login is disabled");
    }

    #[sqlx::test]
    async fn test_login_success(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        // Create a user using the repository
        // Use weak params for fast testing
        let test_params = password::Argon2Params {
            memory_kib: 128,
            iterations: 1,
            parallelism: 1,
        };
        let password_hash = password::hash_string_with_params("testpassword", Some(test_params)).unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "loginuser".to_string(),
            email: "login@example.com".to_string(),
            display_name: Some("Login User".to_string()),
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "native".to_string(),
            password_hash: Some(password_hash),
            external_user_id: None,
        };

        let created_user = user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let app = axum::Router::new()
            .route("/auth/login", axum::routing::post(login))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = LoginRequest {
            email: "login@example.com".to_string(),
            password: "testpassword".to_string(),
        };

        let response = server.post("/auth/login").json(&request).await;

        response.assert_status(axum::http::StatusCode::OK);
        assert!(response.headers().get("set-cookie").is_some());

        let body: AuthResponse = response.json();
        assert_eq!(body.user.email, "login@example.com");
        assert_eq!(body.user.id, created_user.id);
        assert_eq!(body.message, "Login successful");
    }

    #[sqlx::test]
    async fn test_login_disabled(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = false;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/login", axum::routing::post(login))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = LoginRequest {
            email: "test@example.com".to_string(),
            password: "password".to_string(),
        };

        let response = server.post("/auth/login").json(&request).await;
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_login_invalid_email(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/login", axum::routing::post(login))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = LoginRequest {
            email: "nonexistent@example.com".to_string(),
            password: "password".to_string(),
        };

        let response = server.post("/auth/login").json(&request).await;
        response.assert_status(axum::http::StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_login_invalid_password(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        // Create a user using the repository
        let password_hash = password::hash_string_with_params(
            "correctpassword",
            Some(password::Argon2Params {
                memory_kib: 128,
                iterations: 1,
                parallelism: 1,
            }),
        )
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "wrongpwuser".to_string(),
            email: "wrongpw@example.com".to_string(),
            display_name: Some("Wrong Password User".to_string()),
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "native".to_string(),
            password_hash: Some(password_hash),
            external_user_id: None,
        };

        user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let app = axum::Router::new()
            .route("/auth/login", axum::routing::post(login))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = LoginRequest {
            email: "wrongpw@example.com".to_string(),
            password: "wrongpassword".to_string(),
        };

        let response = server.post("/auth/login").json(&request).await;
        response.assert_status(axum::http::StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_login_user_without_password(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        // Create a user without password_hash (e.g., SSO user)
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "ssouser".to_string(),
            email: "sso@example.com".to_string(),
            display_name: Some("SSO User".to_string()),
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "proxy".to_string(),
            password_hash: None,
            external_user_id: None,
        };

        user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let app = axum::Router::new()
            .route("/auth/login", axum::routing::post(login))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = LoginRequest {
            email: "sso@example.com".to_string(),
            password: "anypassword".to_string(),
        };

        let response = server.post("/auth/login").json(&request).await;
        response.assert_status(axum::http::StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_logout(pool: PgPool) {
        let config = create_test_config();
        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/logout", axum::routing::post(logout))
            .with_state(state);

        let server = TestServer::new(app).unwrap();
        let response = server.post("/auth/logout").await;

        response.assert_status(axum::http::StatusCode::OK);

        // Verify cookie is set to expire
        let cookie_header = response.headers().get("set-cookie");
        assert!(cookie_header.is_some());
        let cookie_str = cookie_header.unwrap().to_str().unwrap();
        assert!(cookie_str.contains("Max-Age=0"));

        let body: AuthSuccessResponse = response.json();
        assert_eq!(body.message, "Logout successful");
    }

    #[sqlx::test]
    async fn test_register_duplicate_email(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        // Create a user first
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "existinguser".to_string(),
            email: "duplicate@example.com".to_string(),
            display_name: Some("Existing User".to_string()),
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "native".to_string(),
            password_hash: Some(
                password::hash_string_with_params(
                    "password",
                    Some(password::Argon2Params {
                        memory_kib: 128,
                        iterations: 1,
                        parallelism: 1,
                    }),
                )
                .unwrap(),
            ),
            external_user_id: None,
        };

        user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        // Try to register with the same email
        let request = RegisterRequest {
            username: "newuser".to_string(),
            email: "duplicate@example.com".to_string(),
            password: "password123".to_string(),
            display_name: None,
        };

        let response = server.post("/auth/register").json(&request).await;
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_register_password_too_long(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;
        config.auth.native.password.max_length = 20;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = RegisterRequest {
            username: "testuser".to_string(),
            email: "test@example.com".to_string(),
            password: "thispasswordiswaytoolongandexceedsthelimit".to_string(),
            display_name: None,
        };

        let response = server.post("/auth/register").json(&request).await;
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_register_registration_disabled(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = false;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

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
    async fn test_request_password_reset_disabled(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = false;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/password-reset", axum::routing::post(request_password_reset))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = PasswordResetRequest {
            email: "test@example.com".to_string(),
        };

        let response = server.post("/auth/password-reset").json(&request).await;
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_request_password_reset_nonexistent_user(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/password-reset", axum::routing::post(request_password_reset))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = PasswordResetRequest {
            email: "nonexistent@example.com".to_string(),
        };

        let response = server.post("/auth/password-reset").json(&request).await;

        // Should return success to prevent email enumeration
        response.assert_status(axum::http::StatusCode::OK);
        let body: PasswordResetResponse = response.json();
        assert!(body.message.contains("If an account with that email exists"));
    }

    #[sqlx::test]
    async fn test_request_password_reset_sso_user(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        // Create an SSO user (no password_hash)
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "ssouser".to_string(),
            email: "sso@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "proxy".to_string(),
            password_hash: None,
            external_user_id: None,
        };

        user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let app = axum::Router::new()
            .route("/auth/password-reset", axum::routing::post(request_password_reset))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = PasswordResetRequest {
            email: "sso@example.com".to_string(),
        };

        let response = server.post("/auth/password-reset").json(&request).await;

        // Should return success even though no email will be sent
        response.assert_status(axum::http::StatusCode::OK);
        let body: PasswordResetResponse = response.json();
        assert!(body.message.contains("If an account with that email exists"));
    }

    #[sqlx::test]
    async fn test_confirm_password_reset_disabled(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = false;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route(
                "/auth/password-reset/{token_id}/confirm",
                axum::routing::post(confirm_password_reset),
            )
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let token_id = Uuid::new_v4();
        let request = PasswordResetConfirmRequest {
            token: "sometoken".to_string(),
            new_password: "newpassword123".to_string(),
        };

        let response = server
            .post(&format!("/auth/password-reset/{}/confirm", token_id))
            .json(&request)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_confirm_password_reset_invalid_token(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route(
                "/auth/password-reset/{token_id}/confirm",
                axum::routing::post(confirm_password_reset),
            )
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let token_id = Uuid::new_v4();
        let request = PasswordResetConfirmRequest {
            token: "invalidtoken".to_string(),
            new_password: "newpassword123".to_string(),
        };

        let response = server
            .post(&format!("/auth/password-reset/{}/confirm", token_id))
            .json(&request)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_confirm_password_reset_password_too_short(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.password.min_length = 10;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route(
                "/auth/password-reset/{token_id}/confirm",
                axum::routing::post(confirm_password_reset),
            )
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let token_id = Uuid::new_v4();
        let request = PasswordResetConfirmRequest {
            token: "sometoken".to_string(),
            new_password: "short".to_string(),
        };

        let response = server
            .post(&format!("/auth/password-reset/{}/confirm", token_id))
            .json(&request)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_confirm_password_reset_password_too_long(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.password.max_length = 20;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route(
                "/auth/password-reset/{token_id}/confirm",
                axum::routing::post(confirm_password_reset),
            )
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let token_id = Uuid::new_v4();
        let request = PasswordResetConfirmRequest {
            token: "sometoken".to_string(),
            new_password: "thispasswordiswaytoolongandexceedsthelimit".to_string(),
        };

        let response = server
            .post(&format!("/auth/password-reset/{}/confirm", token_id))
            .json(&request)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_password_reset_full_flow(pool: PgPool) {
        use crate::test::utils::create_test_config;

        // Create a custom config with native auth enabled
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        // Save the email path before moving config
        let email_path = if let crate::config::EmailTransportConfig::File { path } = &config.auth.native.email.transport {
            path.clone()
        } else {
            panic!("Expected File transport in test config");
        };

        let app = crate::Application::new_with_pool(config, Some(pool.clone()))
            .await
            .expect("Failed to create application");

        let (app, _bg_services) = app.into_test_server();

        // Create a user with a password
        let old_password_hash = password::hash_string_with_params(
            "oldpassword123",
            Some(password::Argon2Params {
                memory_kib: 128,
                iterations: 1,
                parallelism: 1,
            }),
        )
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "resetuser".to_string(),
            email: "reset@example.com".to_string(),
            display_name: Some("Reset User".to_string()),
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "native".to_string(),
            password_hash: Some(old_password_hash.clone()),
            external_user_id: None,
        };

        let _created_user = user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        // Step 1: Request password reset
        let reset_request = PasswordResetRequest {
            email: "reset@example.com".to_string(),
        };

        let response = app.post("/authentication/password-resets").json(&reset_request).await;

        response.assert_status(axum::http::StatusCode::OK);
        let body: PasswordResetResponse = response.json();
        assert!(body.message.contains("If an account with that email exists"));

        // Step 2: Extract the reset link from the email file (simulating clicking email link)
        // Use the email path we saved earlier
        let emails_dir = std::path::Path::new(&email_path);

        // Find the most recent email file
        let mut email_files: Vec<_> = std::fs::read_dir(emails_dir).unwrap().filter_map(|e| e.ok()).collect();
        email_files.sort_by_key(|e| e.metadata().unwrap().modified().unwrap());

        let email_file = email_files.last().expect("No email file found");
        let email_content = std::fs::read_to_string(email_file.path()).unwrap();

        // Decode quoted-printable encoding (=3D is =, = at end of line is continuation)
        let decoded_content = email_content.replace("=\r\n", "").replace("=\n", "").replace("=3D", "=");

        // Parse the reset link from the email content
        // Format: {base_url}/reset-password?id={token_id}&token={raw_token}
        let reset_link_start = decoded_content.find("/reset-password?id=").expect("Reset link not found");
        let link_portion = &decoded_content[reset_link_start..];

        // Find the end of the URL (could be whitespace, quote, or bracket)
        let link_end = link_portion
            .find(&[' ', '\n', '\r', '"', '<', '>'][..])
            .unwrap_or(link_portion.len());
        let reset_link = &link_portion[..link_end];

        // Extract token_id and token from URL
        let url_parts: Vec<&str> = reset_link.split(&['?', '&'][..]).collect();
        let token_id_str = url_parts
            .iter()
            .find(|s| s.starts_with("id="))
            .and_then(|s| s.strip_prefix("id="))
            .expect("token_id not found in reset link");
        let token_str = url_parts
            .iter()
            .find(|s| s.starts_with("token="))
            .and_then(|s| s.strip_prefix("token="))
            .expect("token not found in reset link");

        let token_id = Uuid::parse_str(token_id_str).unwrap();
        let raw_token = token_str.to_string();

        // Step 3: Confirm password reset with the token
        let confirm_request = PasswordResetConfirmRequest {
            token: raw_token.clone(),
            new_password: "newpassword456".to_string(),
        };

        let response = app
            .post(&format!("/authentication/password-resets/{}/confirm", token_id))
            .json(&confirm_request)
            .await;

        response.assert_status(axum::http::StatusCode::OK);
        let body: PasswordResetResponse = response.json();
        assert_eq!(body.message, "Password has been reset successfully");

        // Step 4: Verify the password was actually changed by trying to login
        let login_old_password = LoginRequest {
            email: "reset@example.com".to_string(),
            password: "oldpassword123".to_string(),
        };

        let response = app.post("/authentication/login").json(&login_old_password).await;

        // Old password should not work
        response.assert_status(axum::http::StatusCode::UNAUTHORIZED);

        // Step 5: Login with new password should work
        let login_new_password = LoginRequest {
            email: "reset@example.com".to_string(),
            password: "newpassword456".to_string(),
        };

        let response = app.post("/authentication/login").json(&login_new_password).await;

        response.assert_status(axum::http::StatusCode::OK);
        let body: AuthResponse = response.json();
        assert_eq!(body.user.email, "reset@example.com");
        assert_eq!(body.message, "Login successful");

        // Step 6: Verify the token was invalidated and cannot be reused
        let reuse_request = PasswordResetConfirmRequest {
            token: raw_token,
            new_password: "anotherpassword789".to_string(),
        };

        let response = app
            .post(&format!("/authentication/password-resets/{}/confirm", token_id))
            .json(&reuse_request)
            .await;

        // Token should be invalid now
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);

        // Cleanup: remove the email file
        std::fs::remove_file(email_file.path()).ok();
    }

    #[sqlx::test]
    async fn test_change_password_success_full(pool: PgPool) {
        use crate::test::utils::create_test_config;

        // Create a custom config with native auth enabled
        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let app = crate::Application::new_with_pool(config, Some(pool.clone()))
            .await
            .expect("Failed to create application");

        let (app, _bg_services) = app.into_test_server();

        // Create a user with a password
        let old_password_hash = password::hash_string_with_params(
            "oldpassword123",
            Some(password::Argon2Params {
                memory_kib: 128,
                iterations: 1,
                parallelism: 1,
            }),
        )
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "changepassworduser".to_string(),
            email: "changepassword@example.com".to_string(),
            display_name: Some("Change Password User".to_string()),
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "native".to_string(),
            password_hash: Some(old_password_hash.clone()),
            external_user_id: Some("changepassworduser".to_string()),
        };

        let created_user = user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let user_response = UserResponse::from(created_user);

        // Change password
        let change_request = ChangePasswordRequest {
            current_password: "oldpassword123".to_string(),
            new_password: "newpassword456".to_string(),
        };

        let auth_headers = crate::test::utils::add_auth_headers(&user_response);
        let response = app
            .post("/authentication/password-change")
            .json(&change_request)
            .add_header(&auth_headers[0].0, &auth_headers[0].1)
            .add_header(&auth_headers[1].0, &auth_headers[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::OK);
        let body: AuthSuccessResponse = response.json();
        assert_eq!(body.message, "Password changed successfully");

        // Verify old password doesn't work
        let login_old = LoginRequest {
            email: "changepassword@example.com".to_string(),
            password: "oldpassword123".to_string(),
        };

        let response = app.post("/authentication/login").json(&login_old).await;
        response.assert_status(axum::http::StatusCode::UNAUTHORIZED);

        // Verify new password works
        let login_new = LoginRequest {
            email: "changepassword@example.com".to_string(),
            password: "newpassword456".to_string(),
        };

        let response = app.post("/authentication/login").json(&login_new).await;
        response.assert_status(axum::http::StatusCode::OK);
    }

    #[sqlx::test]
    async fn test_change_password_wrong_current(pool: PgPool) {
        use crate::test::utils::create_test_config;

        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let app = crate::Application::new_with_pool(config, Some(pool.clone()))
            .await
            .expect("Failed to create application");

        let (app, _bg_services) = app.into_test_server();

        // Create a user with a password
        let password_hash = password::hash_string_with_params(
            "correctpassword",
            Some(password::Argon2Params {
                memory_kib: 128,
                iterations: 1,
                parallelism: 1,
            }),
        )
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "wrongcurrentuser".to_string(),
            email: "wrongcurrent@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "native".to_string(),
            password_hash: Some(password_hash),
            external_user_id: Some("wrongcurrentuser".to_string()),
        };

        let created_user = user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let user_response = UserResponse::from(created_user);

        let change_request = ChangePasswordRequest {
            current_password: "wrongpassword".to_string(),
            new_password: "newpassword456".to_string(),
        };

        let auth_headers = crate::test::utils::add_auth_headers(&user_response);
        let response = app
            .post("/authentication/password-change")
            .json(&change_request)
            .add_header(&auth_headers[0].0, &auth_headers[0].1)
            .add_header(&auth_headers[1].0, &auth_headers[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_change_password_sso_user_cannot_change(pool: PgPool) {
        use crate::test::utils::create_test_config;

        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let app = crate::Application::new_with_pool(config, Some(pool.clone()))
            .await
            .expect("Failed to create application");

        let (app, _bg_services) = app.into_test_server();

        // Create an SSO user (no password_hash)
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "ssochangeuser".to_string(),
            email: "ssochange@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "proxy".to_string(),
            password_hash: None,
            external_user_id: Some("ssochangeuser".to_string()),
        };

        let created_user = user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let user_response = UserResponse::from(created_user);

        let change_request = ChangePasswordRequest {
            current_password: "anypassword".to_string(),
            new_password: "newpassword456".to_string(),
        };

        let auth_headers = crate::test::utils::add_auth_headers(&user_response);
        let response = app
            .post("/authentication/password-change")
            .json(&change_request)
            .add_header(&auth_headers[0].0, &auth_headers[0].1)
            .add_header(&auth_headers[1].0, &auth_headers[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_change_password_too_short(pool: PgPool) {
        use crate::test::utils::create_test_config;

        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.password.min_length = 10;

        let app = crate::Application::new_with_pool(config, Some(pool.clone()))
            .await
            .expect("Failed to create application");

        let (app, _bg_services) = app.into_test_server();

        // Create a user with a password
        let password_hash = password::hash_string_with_params(
            "oldpassword123",
            Some(password::Argon2Params {
                memory_kib: 128,
                iterations: 1,
                parallelism: 1,
            }),
        )
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "shortpwchangeuser".to_string(),
            email: "shortpwchange@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "native".to_string(),
            password_hash: Some(password_hash),
            external_user_id: Some("shortpwchangeuser".to_string()),
        };

        let created_user = user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let user_response = UserResponse::from(created_user);

        let change_request = ChangePasswordRequest {
            current_password: "oldpassword123".to_string(),
            new_password: "short".to_string(),
        };

        let auth_headers = crate::test::utils::add_auth_headers(&user_response);
        let response = app
            .post("/authentication/password-change")
            .json(&change_request)
            .add_header(&auth_headers[0].0, &auth_headers[0].1)
            .add_header(&auth_headers[1].0, &auth_headers[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_change_password_too_long(pool: PgPool) {
        use crate::test::utils::create_test_config;

        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.password.max_length = 20;

        let app = crate::Application::new_with_pool(config, Some(pool.clone()))
            .await
            .expect("Failed to create application");

        let (app, _bg_services) = app.into_test_server();

        // Create a user with a password
        let password_hash = password::hash_string_with_params(
            "oldpassword",
            Some(password::Argon2Params {
                memory_kib: 128,
                iterations: 1,
                parallelism: 1,
            }),
        )
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "longpwchangeuser".to_string(),
            email: "longpwchange@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "native".to_string(),
            password_hash: Some(password_hash),
            external_user_id: Some("longpwchangeuser".to_string()),
        };

        let created_user = user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let user_response = UserResponse::from(created_user);

        let change_request = ChangePasswordRequest {
            current_password: "oldpassword".to_string(),
            new_password: "thispasswordiswaytoolongandexceedsthelimit".to_string(),
        };

        let auth_headers = crate::test::utils::add_auth_headers(&user_response);
        let response = app
            .post("/authentication/password-change")
            .json(&change_request)
            .add_header(&auth_headers[0].0, &auth_headers[0].1)
            .add_header(&auth_headers[1].0, &auth_headers[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_change_password_when_disabled(pool: PgPool) {
        use crate::test::utils::create_test_config;

        let mut config = create_test_config();
        config.auth.native.enabled = false; // Disabled!

        let app = crate::Application::new_with_pool(config, Some(pool.clone()))
            .await
            .expect("Failed to create application");

        let (app, _bg_services) = app.into_test_server();

        // Create a user with a password
        let password_hash = password::hash_string_with_params(
            "oldpassword",
            Some(password::Argon2Params {
                memory_kib: 128,
                iterations: 1,
                parallelism: 1,
            }),
        )
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest {
            username: "disabledchangeuser".to_string(),
            email: "disabledchange@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            is_admin: false,
            roles: vec![Role::StandardUser],
            auth_source: "native".to_string(),
            password_hash: Some(password_hash),
            external_user_id: Some("disabledchangeuser".to_string()),
        };

        let created_user = user_repo.create(&user_create).await.unwrap();
        drop(user_repo);
        drop(conn);

        let user_response = UserResponse::from(created_user);

        let change_request = ChangePasswordRequest {
            current_password: "oldpassword".to_string(),
            new_password: "newpassword".to_string(),
        };

        let auth_headers = crate::test::utils::add_auth_headers(&user_response);
        let response = app
            .post("/authentication/password-change")
            .json(&change_request)
            .add_header(&auth_headers[0].0, &auth_headers[0].1)
            .add_header(&auth_headers[1].0, &auth_headers[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_register_with_configured_default_roles(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;
        // Configure default roles to include RequestViewer in addition to StandardUser
        config.auth.default_user_roles = vec![Role::StandardUser, Role::RequestViewer];

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

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

        let body: AuthResponse = response.json();
        assert_eq!(body.user.email, "test@example.com");

        // Verify the user has both StandardUser and RequestViewer roles
        assert_eq!(body.user.roles.len(), 2);
        assert!(body.user.roles.contains(&Role::StandardUser));
        assert!(body.user.roles.contains(&Role::RequestViewer));
    }

    #[sqlx::test]
    async fn test_register_standard_user_role_always_present(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;
        // Configure default roles without StandardUser - it should still be added
        config.auth.default_user_roles = vec![Role::RequestViewer];

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let request = RegisterRequest {
            username: "testuser2".to_string(),
            email: "test2@example.com".to_string(),
            password: "password123".to_string(),
            display_name: Some("Test User 2".to_string()),
        };

        let response = server.post("/auth/register").json(&request).await;

        response.assert_status(axum::http::StatusCode::CREATED);

        let body: AuthResponse = response.json();

        // Verify StandardUser was automatically added even though not in config
        assert!(body.user.roles.contains(&Role::StandardUser));
        assert!(body.user.roles.contains(&Role::RequestViewer));
    }
}
