//! HTTP handlers for authentication endpoints.

use axum::{
    Json,
    extract::{Path, State},
};
use uuid::Uuid;

use sqlx_pool_router::PoolProvider;

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
    summary = "Check registration availability",
    description = "Returns whether user registration is enabled on this instance. \
        Use this before displaying a registration form to determine if self-registration \
        is allowed.",
    responses(
        (status = 200, description = "Registration info", body = RegistrationInfo),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_registration_info<P: PoolProvider>(State(state): State<AppState<P>>) -> Result<Json<RegistrationInfo>, Error> {
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
    summary = "Register new account",
    description = "Create a new user account with email and password. On success, returns the \
        created user and sets a session cookie for immediate login. Registration must be enabled \
        in the instance configuration. New users receive default roles and initial credits if configured.",
    responses(
        (status = 201, description = "User registered successfully", body = AuthResponse),
        (status = 400, description = "Invalid input"),
        (status = 409, description = "User already exists"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn register<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Json(request): Json<RegisterRequest>,
) -> Result<RegisterResponse, Error> {
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

    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;

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

    let user_response = UserResponse::from(created_user.clone());
    let current_user = CurrentUser::from(created_user);

    // Create session token
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
pub async fn create_sample_files_for_new_user<P: PoolProvider>(state: &AppState<P>, user_id: Uuid) -> Result<(), Error> {
    use crate::db::handlers::deployments::DeploymentFilter;
    use crate::sample_files;

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Get the user's batch API key
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    let api_key = api_keys_repo
        .get_or_create_hidden_key(user_id, ApiKeyPurpose::Batch, user_id)
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

    tracing::debug!(
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
    summary = "Check login availability",
    description = "Returns whether native (email/password) login is enabled on this instance. \
        Use this before displaying a login form. If disabled, users should authenticate via \
        configured SSO providers instead.",
    responses(
        (status = 200, description = "Login info", body = LoginInfo),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_login_info<P: PoolProvider>(State(state): State<AppState<P>>) -> Result<Json<LoginInfo>, Error> {
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
    summary = "Login with credentials",
    description = "Authenticate with email and password. On success, returns the user details \
        and sets a session cookie. Native authentication must be enabled in the instance \
        configuration. The session cookie can be used for subsequent authenticated requests.",
    responses(
        (status = 200, description = "Login successful", body = AuthResponse),
        (status = 401, description = "Invalid credentials"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn login<P: PoolProvider>(State(state): State<AppState<P>>, Json(request): Json<LoginRequest>) -> Result<LoginResponse, Error> {
    // Check if native auth is enabled
    if !state.config.auth.native.enabled {
        return Err(Error::BadRequest {
            message: "Native authentication is disabled".to_string(),
        });
    }
    let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;

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

    let user_response = UserResponse::from(user.clone());
    let current_user = CurrentUser::from(user);

    // Create session token
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
    summary = "End session",
    description = "Log out the current user by clearing the session cookie. After calling this \
        endpoint, subsequent requests will require re-authentication.",
    responses(
        (status = 200, description = "Logout successful", body = AuthSuccessResponse),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn logout<P: PoolProvider>(State(state): State<AppState<P>>) -> Result<LogoutResponse, Error> {
    let session_config = &state.config.auth.native.session;
    let secure = if session_config.cookie_secure { "; Secure" } else { "" };

    // Domain attribute for cross-subdomain cookies
    let domain = session_config
        .cookie_domain
        .as_ref()
        .map(|d| format!("; Domain={d}"))
        .unwrap_or_default();

    // Clear session cookie
    let cookie = format!(
        "{}=; Path=/; HttpOnly{}{}; SameSite={}; Max-Age=0",
        session_config.cookie_name, secure, domain, session_config.cookie_same_site
    );

    // Also clear the active organization cookie
    let org_cookie = format!(
        "dw_active_org=; Path=/; HttpOnly{}{}; SameSite={}; Max-Age=0",
        secure, domain, session_config.cookie_same_site
    );

    let auth_response = AuthSuccessResponse {
        message: "Logout successful".to_string(),
    };

    Ok(LogoutResponse {
        auth_response,
        cookie,
        extra_cookies: vec![org_cookie],
    })
}

/// Request password reset (send email)
#[utoipa::path(
    post,
    path = "/authentication/password-resets",
    request_body = PasswordResetRequest,
    tag = "authentication",
    summary = "Request password reset",
    description = "Request a password reset email for the specified email address. For security, \
        this endpoint always returns success even if the email doesn't exist (to prevent email \
        enumeration). If the email is valid and associated with a native auth account, a reset \
        link will be sent.",
    responses(
        (status = 200, description = "Password reset email sent", body = PasswordResetResponse),
        (status = 400, description = "Invalid request"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn request_password_reset<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Json(request): Json<PasswordResetRequest>,
) -> Result<Json<PasswordResetResponse>, Error> {
    // Check if native auth is enabled
    if !state.config.auth.native.enabled {
        return Err(Error::BadRequest {
            message: "Native authentication is disabled".to_string(),
        });
    }
    let mut tx = state.db.write().begin().await.unwrap();

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
    summary = "Complete password reset",
    description = "Set a new password using the token received via email. The token_id is from the \
        URL and the raw token is included in the request body. Tokens expire after a configured \
        period and can only be used once.",
    responses(
        (status = 200, description = "Password reset successful", body = PasswordResetResponse),
        (status = 400, description = "Invalid or expired token"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn confirm_password_reset<P: PoolProvider>(
    State(state): State<AppState<P>>,
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
        batch_notifications_enabled: None,
        low_balance_threshold: None,
        auto_topup_amount: None,
        auto_topup_threshold: None,
        auto_topup_monthly_limit: None,
    };

    let mut tx = state.db.write().begin().await.unwrap();
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
pub async fn change_password<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: CurrentUser,
    Json(request): Json<ChangePasswordRequest>,
) -> Result<Json<AuthSuccessResponse>, Error> {
    // Check if native auth is enabled
    if !state.config.auth.native.enabled {
        return Err(Error::BadRequest {
            message: "Native authentication is disabled".to_string(),
        });
    }

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
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
        batch_notifications_enabled: None,
        low_balance_threshold: None,
        auto_topup_amount: None,
        auto_topup_threshold: None,
        auto_topup_monthly_limit: None,
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

    let secure = if session_config.cookie_secure { "; Secure" } else { "" };
    let domain = session_config
        .cookie_domain
        .as_ref()
        .map(|d| format!("; Domain={d}"))
        .unwrap_or_default();
    format!(
        "{}={}; Path=/; HttpOnly{}{}; SameSite={}; Max-Age={}",
        session_config.cookie_name, token, secure, domain, session_config.cookie_same_site, max_age
    )
}

// ---------------------------------------------------------------------------
// CLI login endpoints (two-step code exchange)
// ---------------------------------------------------------------------------
//
// Step 1: GET /authentication/cli-callback
//   - Browser redirects here after SSO authentication
//   - Creates API keys + a short-lived one-time code
//   - Redirects to localhost with the code (no secrets in URL)
//
// Step 2: POST /authentication/cli-exchange
//   - CLI sends the code from step 1
//   - Server returns the API key secrets in the response body
//   - Code is deleted (single-use)
//
// This two-step pattern keeps API key secrets out of browser history,
// server logs, and referrer headers.

/// Query params for the CLI callback (step 1).
#[derive(Debug, serde::Deserialize)]
pub struct CliCallbackQuery {
    /// Localhost port where the CLI is listening (1024–65535).
    pub port: u16,
    /// CSRF state token generated by the CLI.
    pub state: String,
    /// Optional org slug or display name to create org-scoped keys.
    pub org: Option<String>,
}

/// Request body for the CLI code exchange (step 2).
#[derive(Debug, serde::Deserialize)]
pub struct CliExchangeRequest {
    /// The one-time code received in the callback redirect.
    pub code: String,
}

/// Response from the CLI code exchange (step 2).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CliExchangeResponse {
    pub inference_key: String,
    pub inference_key_id: String,
    pub platform_key: String,
    pub platform_key_id: String,
    pub user_id: String,
    pub email: String,
    pub display_name: String,
    /// Unique account identifier for the CLI context switcher.
    /// Format: "username" for personal, "username@org-slug" for org.
    pub account_name: String,
    /// "personal" or "organization"
    pub account_type: String,
    /// Org display name (only present for org accounts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
}

/// CLI login callback (step 1).
///
/// Called after the user authenticates via SSO. Creates a pair of API keys
/// and a short-lived one-time authorization code. Redirects to the CLI's
/// localhost server with the code (no secrets in the URL).
///
/// This endpoint accepts SSO cookie, proxy-header, or native JWT session
/// authentication. API key Bearer auth is explicitly rejected to prevent
/// privilege escalation (a realtime key minting a platform key).
// Not included in OpenAPI spec — internal endpoint for CLI login flow.
#[tracing::instrument(skip_all)]
pub async fn cli_callback<P: PoolProvider>(
    State(state): State<AppState<P>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<CliCallbackQuery>,
    current_user: CurrentUser,
) -> Result<axum::response::Response, Error> {
    use crate::db::handlers::organizations::Organizations;
    use axum::response::IntoResponse;

    // Reject Bearer token authentication — only SSO cookie/proxy-header allowed.
    // This prevents a realtime key holder from minting a platform key.
    // Other Authorization schemes (e.g., ID tokens from SSO proxies) are allowed.
    if let Some(auth_header) = headers.get(axum::http::header::AUTHORIZATION)
        && auth_header.to_str().is_ok_and(|s| s.starts_with("Bearer "))
    {
        return Err(Error::Unauthenticated {
            message: Some("CLI callback must be accessed via browser SSO, not API keys.".to_string()),
        });
    }

    // Validate port: reject 0 and privileged ports
    if query.port < 1024 {
        return Err(Error::BadRequest {
            message: format!("Invalid port: {}. Must be between 1024 and 65535.", query.port),
        });
    }

    let user_id = current_user.id;
    let user_username = current_user.username.clone();

    // Resolved account context for key creation
    struct AccountContext {
        target_user_id: crate::types::UserId,
        account_name: String,
        org_id: Option<String>,
    }

    // Determine target user for key creation (personal or org-scoped)
    let ctx = if let Some(ref org_slug) = query.org {
        // Use primary pool for strongly consistent reads — org membership is
        // security-sensitive, so we can't risk stale replica data.
        let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let mut org_repo = Organizations::new(&mut pool_conn);
        let memberships = org_repo.list_user_organizations(user_id).await.map_err(Error::Database)?;

        let org_ids: Vec<crate::types::UserId> = memberships.iter().map(|m| m.organization_id).collect();
        let mut users_repo = crate::db::handlers::Users::new(&mut pool_conn);
        let org_map = users_repo.get_bulk(org_ids).await.map_err(Error::Database)?;

        let matched_org = memberships.iter().find_map(|m| {
            org_map.get(&m.organization_id).and_then(|org| {
                let matches = org.username.eq_ignore_ascii_case(org_slug)
                    || org.display_name.as_deref().is_some_and(|dn| dn.eq_ignore_ascii_case(org_slug));
                if matches {
                    Some((m.organization_id, org.username.clone(), org.display_name.clone()))
                } else {
                    None
                }
            })
        });

        match matched_org {
            Some((org_id, org_username, _org_display_name)) => AccountContext {
                target_user_id: org_id,
                // "hamish@acme-corp" — unique, human-readable
                account_name: format!("{}@{}", user_username, org_username),
                org_id: Some(org_id.to_string()),
            },
            None => {
                return Err(Error::BadRequest {
                    message: format!("Organization '{}' not found or you are not a member.", org_slug),
                });
            }
        }
    } else {
        AccountContext {
            target_user_id: user_id,
            account_name: user_username.clone(),
            org_id: None,
        }
    };

    // Create API keys + auth code in a single transaction
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;

    // Include a timestamp in key names to avoid unique constraint conflicts
    // when a user logs in from multiple machines or re-logs on the same machine.
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");

    let inference_key = {
        let mut repo = ApiKeys::new(&mut tx);
        repo.create(&crate::db::models::api_keys::ApiKeyCreateDBRequest {
            user_id: ctx.target_user_id,
            name: format!("DW CLI inference ({})", timestamp),
            description: Some("Created by dw login".to_string()),
            purpose: ApiKeyPurpose::Realtime,
            requests_per_second: None,
            burst_size: None,
            created_by: user_id,
        })
        .await
        .map_err(Error::Database)?
    };

    let platform_key = {
        let mut repo = ApiKeys::new(&mut tx);
        repo.create(&crate::db::models::api_keys::ApiKeyCreateDBRequest {
            user_id: ctx.target_user_id,
            name: format!("DW CLI platform ({})", timestamp),
            description: Some("Created by dw login".to_string()),
            purpose: ApiKeyPurpose::Platform,
            requests_per_second: None,
            burst_size: None,
            created_by: user_id,
        })
        .await
        .map_err(Error::Database)?
    };

    // Generate a cryptographically random one-time code
    let code = crate::crypto::generate_api_key();

    // Store the code with 60-second expiry.
    // user_id = target_user_id (org id in org context, personal id otherwise).
    // This matches the api_keys.user_id — the account that owns the keys and
    // gets billed. The individual creator is tracked via api_keys.created_by.
    let org_uuid = ctx.org_id.as_ref().and_then(|id| Uuid::parse_str(id).ok());
    sqlx::query(
        "INSERT INTO cli_auth_codes (code, inference_key_id, platform_key_id, user_id, account_name, org_id, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, NOW() + INTERVAL '60 seconds')",
    )
    .bind(&code)
    .bind(inference_key.id)
    .bind(platform_key.id)
    .bind(ctx.target_user_id)
    .bind(&ctx.account_name)
    .bind(org_uuid)
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(e.into()))?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    // Redirect to localhost with only the code and state — no secrets in URL
    let mut redirect_url = url::Url::parse(&format!("http://127.0.0.1:{}/callback", query.port)).map_err(|e| Error::Internal {
        operation: format!("build redirect URL: {e}"),
    })?;

    redirect_url
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &query.state);

    Ok((
        axum::http::StatusCode::FOUND,
        [
            ("location", redirect_url.as_str()),
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("referrer-policy", "no-referrer"),
        ],
    )
        .into_response())
}

/// CLI code exchange (step 2).
///
/// The CLI sends the one-time code received in the callback redirect.
/// Server looks up the code, returns the API key secrets in the response body,
/// and deletes the code (single-use). Codes expire after 60 seconds.
///
/// The entire operation (code lookup, key fetch, code delete) runs in a
/// single transaction so that if any step fails, the code is not consumed
/// and the CLI can retry.
// Not included in OpenAPI spec — internal endpoint for CLI login flow.
#[tracing::instrument(skip_all)]
pub async fn cli_exchange<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Json(request): Json<CliExchangeRequest>,
) -> Result<axum::response::Response, Error> {
    use axum::response::IntoResponse;

    // Run everything in a transaction — if any read fails after we find the
    // code, the delete rolls back and the CLI can retry.
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;

    // Opportunistically clean up expired codes to prevent unbounded table growth
    sqlx::query("DELETE FROM cli_auth_codes WHERE expires_at <= NOW()")
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(e.into()))?;

    // Look up and delete the valid code (single-use).
    // Type overrides needed because sqlx infers RETURNING columns from DELETE as nullable.
    let row = sqlx::query!(
        r#"
        DELETE FROM cli_auth_codes
        WHERE code = $1 AND expires_at > NOW()
        RETURNING
            inference_key_id as "inference_key_id!",
            platform_key_id as "platform_key_id!",
            user_id as "user_id!",
            account_name as "account_name!",
            org_id
        "#,
        request.code,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| Error::Database(e.into()))?
    .ok_or_else(|| Error::BadRequest {
        message: "Invalid or expired code.".to_string(),
    })?;

    // Fetch the API key secrets
    let inference_key = sqlx::query!(
        "SELECT id, secret FROM api_keys WHERE id = $1 AND is_deleted = false",
        row.inference_key_id,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| Error::Database(e.into()))?;

    let platform_key = sqlx::query!(
        "SELECT id, secret FROM api_keys WHERE id = $1 AND is_deleted = false",
        row.platform_key_id,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| Error::Database(e.into()))?;

    // Fetch user/org info for the response.
    // row.user_id = target_user_id (org id in org context, personal id otherwise).
    // Scope each repo usage in a block to avoid borrow conflicts on tx.
    let is_org = row.org_id.is_some();
    let (email, display_name, org_name) = if is_org {
        // Org context: get the individual's info (from api_keys.created_by)
        // and the org's display name
        let key_row = sqlx::query!("SELECT created_by FROM api_keys WHERE id = $1", row.inference_key_id,)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| Error::Database(e.into()))?;

        let individual = {
            let mut repo = crate::db::handlers::Users::new(&mut tx);
            repo.get_by_id(key_row.created_by)
                .await
                .map_err(Error::Database)?
                .ok_or_else(|| Error::Internal {
                    operation: "CLI exchange: creator not found".to_string(),
                })?
        };

        let org = {
            let mut repo = crate::db::handlers::Users::new(&mut tx);
            repo.get_by_id(row.user_id)
                .await
                .map_err(Error::Database)?
                .ok_or_else(|| Error::Internal {
                    operation: "CLI exchange: org not found".to_string(),
                })?
        };

        let org_display = org.display_name.unwrap_or(org.username);
        (
            individual.email,
            individual.display_name.unwrap_or(individual.username),
            Some(org_display),
        )
    } else {
        // Personal context: user_id is the individual
        let user = {
            let mut repo = crate::db::handlers::Users::new(&mut tx);
            repo.get_by_id(row.user_id)
                .await
                .map_err(Error::Database)?
                .ok_or_else(|| Error::Internal {
                    operation: "CLI exchange: user not found".to_string(),
                })?
        };
        (user.email, user.display_name.unwrap_or(user.username), None)
    };

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    let body = CliExchangeResponse {
        inference_key: inference_key.secret,
        inference_key_id: inference_key.id.to_string(),
        platform_key: platform_key.secret,
        platform_key_id: platform_key.id.to_string(),
        user_id: row.user_id.to_string(),
        email,
        display_name,
        account_name: row.account_name,
        account_type: if is_org {
            "organization".to_string()
        } else {
            "personal".to_string()
        },
        org_name,
        org_id: row.org_id.map(|id| id.to_string()),
    };

    // Return with anti-caching headers — response contains long-lived secrets
    Ok((
        axum::http::StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
        ],
        axum::Json(body),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::transactions::TransactionFilters, db::models::credits::CreditTransactionType, test::utils::create_test_config,
    };
    use axum_test::TestServer;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_register_success(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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
        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

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
        let email_path = if let crate::config::EmailTransportConfig::File { path } = &config.email.transport {
            path.clone()
        } else {
            panic!("Expected File transport in test config");
        };

        let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
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

        let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
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

        let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
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

        let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
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

        let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
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

        let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
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

        let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

    #[sqlx::test]
    async fn test_session_cookie_includes_domain_when_configured(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;
        config.auth.native.session.cookie_domain = Some(".example.com".to_string());

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let response = server
            .post("/auth/register")
            .json(&RegisterRequest {
                username: "domaintest".to_string(),
                email: "domain@example.com".to_string(),
                password: "password123".to_string(),
                display_name: None,
            })
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let cookie = response.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(cookie.contains("Domain=.example.com"), "cookie should include Domain: {cookie}");
    }

    #[sqlx::test]
    async fn test_session_cookie_omits_domain_when_not_configured(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.enabled = true;
        config.auth.native.allow_registration = true;
        config.auth.native.session.cookie_domain = None;

        let state = crate::test::utils::create_test_app_state_with_config(pool, config).await;

        let app = axum::Router::new()
            .route("/auth/register", axum::routing::post(register))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let response = server
            .post("/auth/register")
            .json(&RegisterRequest {
                username: "nodomaintest".to_string(),
                email: "nodomain@example.com".to_string(),
                password: "password123".to_string(),
                display_name: None,
            })
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let cookie = response.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(!cookie.contains("Domain="), "cookie should not include Domain: {cookie}");
    }

    // -----------------------------------------------------------------------
    // CLI login (callback + exchange) tests
    // -----------------------------------------------------------------------

    /// Helper: perform step 1 (callback) and extract the code from the redirect.
    async fn cli_callback_get_code(
        server: &TestServer,
        external_id: &str,
        email: &str,
        port: u16,
        state: &str,
        org: Option<&str>,
    ) -> (axum_test::TestResponse, Option<String>) {
        let mut url_builder = url::Url::parse("http://localhost/authentication/cli-callback").unwrap();
        url_builder
            .query_pairs_mut()
            .append_pair("port", &port.to_string())
            .append_pair("state", state);
        if let Some(org_slug) = org {
            url_builder.query_pairs_mut().append_pair("org", org_slug);
        }
        // axum_test expects a path+query, not a full URL
        let url = format!("{}?{}", url_builder.path(), url_builder.query().unwrap_or(""));

        let response = server
            .get(&url)
            .add_header("x-doubleword-user", external_id)
            .add_header("x-doubleword-email", email)
            .await;

        let code = response
            .headers()
            .get("location")
            .and_then(|loc| loc.to_str().ok())
            .and_then(|loc| {
                url::Url::parse(loc)
                    .ok()?
                    .query_pairs()
                    .find(|(k, _)| k == "code")
                    .map(|(_, v)| v.into_owned())
            });

        (response, code)
    }

    #[sqlx::test]
    async fn test_cli_login_personal_success(pool: PgPool) {
        let (server, _bg) = crate::test::utils::create_test_app(pool.clone(), false).await;
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_id = user.external_user_id.unwrap();

        // Step 1: callback — should redirect with code (no secrets)
        let (response, code) = cli_callback_get_code(&server, &external_id, &user.email, 12345, "test-state", None).await;

        response.assert_status(axum::http::StatusCode::FOUND);

        let location = response.headers().get("location").unwrap().to_str().unwrap();
        assert!(
            location.starts_with("http://127.0.0.1:12345/callback?"),
            "unexpected redirect: {location}"
        );
        assert!(location.contains("state=test-state"), "missing state: {location}");
        assert!(location.contains("code="), "missing code: {location}");
        // Secrets must NOT be in the redirect URL
        assert!(!location.contains("inference_key="), "secrets should not be in URL: {location}");
        assert!(!location.contains("platform_key="), "secrets should not be in URL: {location}");
        // Security headers
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        assert_eq!(response.headers().get("pragma").unwrap(), "no-cache");
        assert_eq!(response.headers().get("referrer-policy").unwrap(), "no-referrer");

        // Step 2: exchange — should return keys in response body
        let code = code.expect("code should be present in redirect");
        let exchange_response = server
            .post("/authentication/cli-exchange")
            .json(&serde_json::json!({ "code": code }))
            .await;
        exchange_response.assert_status(axum::http::StatusCode::OK);

        let body: CliExchangeResponse = exchange_response.json();
        assert!(!body.inference_key.is_empty(), "inference_key should not be empty");
        assert!(!body.platform_key.is_empty(), "platform_key should not be empty");
        assert_eq!(body.user_id, user.id.to_string());
        assert_eq!(body.account_name, user.username, "personal account_name should be the username");
        assert_eq!(body.account_type, "personal");
        assert!(body.org_id.is_none());
        assert!(body.org_name.is_none());

        // Step 3: code should be single-use — second exchange fails
        let replay_response = server
            .post("/authentication/cli-exchange")
            .json(&serde_json::json!({ "code": code }))
            .await;
        replay_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_cli_login_org_success(pool: PgPool) {
        let (server, _bg) = crate::test::utils::create_test_app(pool.clone(), false).await;
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_id = user.external_user_id.clone().unwrap();
        let org = crate::test::utils::create_test_org(&pool, user.id).await;

        let (response, code) = cli_callback_get_code(&server, &external_id, &user.email, 54321, "org-state", Some(&org.username)).await;

        response.assert_status(axum::http::StatusCode::FOUND);
        let code = code.expect("code should be present");

        let exchange_response = server
            .post("/authentication/cli-exchange")
            .json(&serde_json::json!({ "code": code }))
            .await;
        exchange_response.assert_status(axum::http::StatusCode::OK);

        let body: CliExchangeResponse = exchange_response.json();
        let org_id_str = org.id.to_string();
        assert_eq!(body.user_id, org_id_str, "user_id should be org id");
        assert_eq!(body.org_id.as_deref(), Some(org_id_str.as_str()));
        assert_eq!(body.account_type, "organization");
        assert!(body.org_name.is_some(), "org_name should be present");
        // account_name should be "username@org-slug"
        assert!(
            body.account_name.contains('@'),
            "org account_name should be username@org-slug, got: {}",
            body.account_name
        );
    }

    #[sqlx::test]
    async fn test_cli_callback_unknown_org(pool: PgPool) {
        let (server, _bg) = crate::test::utils::create_test_app(pool.clone(), false).await;
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_id = user.external_user_id.unwrap();

        let (response, _) = cli_callback_get_code(&server, &external_id, &user.email, 12345, "s", Some("nonexistent-org")).await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);

        // Verify no keys were created (transaction should have rolled back)
        let mut conn = pool.acquire().await.unwrap();
        let key_count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM api_keys WHERE created_by = $1 AND name LIKE 'DW CLI%' AND is_deleted = false",
            user.id
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap()
        .unwrap_or(0);

        assert_eq!(key_count, 0, "no CLI keys should be created when org lookup fails");
    }

    #[sqlx::test]
    async fn test_cli_callback_rejects_api_key_auth(pool: PgPool) {
        let (server, _bg) = crate::test::utils::create_test_app(pool.clone(), false).await;
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = crate::db::handlers::api_keys::ApiKeys::new(&mut conn);
        let key = repo
            .create(&crate::db::models::api_keys::ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "test-key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
            })
            .await
            .unwrap();
        drop(conn);

        let response = server
            .get("/authentication/cli-callback?port=12345&state=s")
            .add_header("authorization", &format!("Bearer {}", key.secret))
            .await;

        response.assert_status(axum::http::StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_cli_callback_rejects_invalid_port(pool: PgPool) {
        let (server, _bg) = crate::test::utils::create_test_app(pool.clone(), false).await;
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_id = user.external_user_id.unwrap();

        for port in [0, 80, 443, 1023] {
            let (response, _) = cli_callback_get_code(&server, &external_id, &user.email, port, "s", None).await;
            response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        }
    }

    #[sqlx::test]
    async fn test_cli_exchange_expired_code(pool: PgPool) {
        let (server, _bg) = crate::test::utils::create_test_app(pool.clone(), false).await;

        // Try to exchange a code that doesn't exist
        let response = server
            .post("/authentication/cli-exchange")
            .json(&serde_json::json!({ "code": "nonexistent-code" }))
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }
}
