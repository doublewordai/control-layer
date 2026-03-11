//! User extraction from request authentication.

use crate::db::errors::DbError;
use crate::{
    AppState,
    api::models::users::{CurrentUser, Role},
    auth::session,
    db::handlers::{Repository, Users},
    errors::{Error, Result},
};
use axum::{extract::FromRequestParts, http::request::Parts};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::{debug, instrument, trace};

/// Result of a successful authentication: the user and their last login timestamp.
type AuthSuccess = (CurrentUser, Option<DateTime<Utc>>);

/// Extract user from JWT session cookie if present and valid
/// Returns:
/// - None: No JWT cookie present
/// - Some(Ok((user, last_login))): Valid JWT found, user fetched from DB with current data
/// - Some(Err(error)): JWT cookie present but invalid/malformed, or user not found/deleted
#[instrument(skip(parts, config, db))]
async fn try_jwt_session_auth(
    parts: &axum::http::request::Parts,
    config: &crate::config::Config,
    db: &PgPool,
) -> Option<Result<AuthSuccess>> {
    let cookie_header = parts.headers.get(axum::http::header::COOKIE)?;

    let cookie_str = match cookie_header.to_str() {
        Ok(s) => s,
        Err(e) => {
            return Some(Err(Error::BadRequest {
                message: format!("Invalid cookie header: {e}"),
            }));
        }
    };
    let cookie_name = &config.auth.native.session.cookie_name;

    for cookie in cookie_str.split(';') {
        let cookie = cookie.trim();
        if let Some((name, value)) = cookie.split_once('=')
            && name == cookie_name
        {
            // Verify the JWT and extract user ID
            let user_id = match session::verify_session_token(value, config) {
                Ok(id) => id,
                Err(_) => {
                    // Invalid/expired token, continue checking other cookies
                    continue;
                }
            };

            // Fetch fresh user data from database
            let mut conn = match db.acquire().await {
                Ok(conn) => conn,
                Err(e) => return Some(Err(DbError::from(e).into())),
            };
            let mut user_repo = Users::new(&mut conn);

            let user = match user_repo.get_by_id(user_id).await {
                Ok(Some(user)) => user,
                Ok(None) => {
                    // User was deleted - invalidate session
                    return Some(Err(Error::Unauthenticated {
                        message: Some("User no longer exists".to_string()),
                    }));
                }
                Err(e) => return Some(Err(Error::Database(e))),
            };

            let last_login = user.last_login;
            return Some(Ok((
                CurrentUser {
                    id: user.id,
                    username: user.username,
                    email: user.email,
                    is_admin: user.is_admin,
                    roles: user.roles,
                    display_name: user.display_name,
                    avatar_url: user.avatar_url,
                    payment_provider_id: user.payment_provider_id,
                    organizations: vec![],
                    active_organization: None,
                },
                last_login,
            )));
        }
    }
    None
}

/// Extract user from proxy header if present and valid
/// Returns:
/// - None: No proxy header present
/// - Some(Ok((user, last_login))): Valid proxy header found and user authenticated
/// - Some(Err(error)): Proxy header present but user lookup/creation failed
#[instrument(skip(parts, state), level = "TRACE")]
async fn try_proxy_header_auth<P: sqlx_pool_router::PoolProvider + Clone + Send + Sync + 'static>(
    parts: &axum::http::request::Parts,
    state: &crate::AppState<P>,
) -> Option<Result<AuthSuccess>> {
    let config = &state.config;
    let db: &PgPool = state.db.write();
    tracing::trace!("Trying proxy header auth, config: {:?}", config.auth.proxy_header);
    // Extract external_user_id from header_name (required)
    let external_user_id = parts
        .headers
        .get(&config.auth.proxy_header.header_name)
        .and_then(|h| h.to_str().ok())?;

    // Extract email from email_header_name, or fall back to external_user_id for backwards compatibility
    // This allows old deployments (single header with email) to continue working
    // while new deployments can send both headers to distinguish federated users
    let user_email = parts
        .headers
        .get(&config.auth.proxy_header.email_header_name)
        .and_then(|h| h.to_str().ok())
        .unwrap_or(external_user_id);

    // Extract groups and provider if import_idp_groups is enabled
    let groups_and_provider = if config.auth.proxy_header.import_idp_groups {
        parts
            .headers
            .get(&config.auth.proxy_header.groups_field_name)
            .and_then(|h| h.to_str().ok())
            .and_then(|group_string| {
                let groups: Vec<String> = group_string
                    .split(',')
                    .map(|g| g.trim().to_string())
                    .filter(|g| !config.auth.proxy_header.blacklisted_sso_groups.contains(g))
                    .collect();

                if groups.is_empty() {
                    None
                } else {
                    let provider = parts
                        .headers
                        .get(&config.auth.proxy_header.provider_field_name)
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("unknown");
                    Some((groups, provider))
                }
            })
    } else {
        None
    };
    tracing::trace!(
        "Proxy header auth: external_user_id='{}', email='{}', groups_and_provider={:?}",
        external_user_id,
        user_email,
        groups_and_provider
    );

    let mut tx = match db.begin().await {
        Ok(tx) => tx,
        Err(e) => return Some(Err(DbError::from(e).into())),
    };
    let mut user_repo = Users::new(&mut tx);
    let mut should_create_sample_files = false;

    // Get or create user with group sync (only if auto_create is enabled)
    let user_result = if config.auth.proxy_header.auto_create_users {
        match user_repo
            .get_or_create_proxy_header_user(external_user_id, user_email, groups_and_provider, &config.auth.default_user_roles)
            .await
        {
            Ok((user, was_created)) => {
                // Grant initial credits for newly created users
                if was_created {
                    should_create_sample_files = true;
                    let initial_credits = config.credits.initial_credits_for_standard_users;
                    if initial_credits > rust_decimal::Decimal::ZERO && user.roles.contains(&Role::StandardUser) {
                        use crate::db::handlers::credits::Credits;
                        use crate::db::models::credits::CreditTransactionCreateDBRequest;

                        let mut credits_repo = Credits::new(&mut tx);
                        let request = CreditTransactionCreateDBRequest::admin_grant(
                            user.id,
                            uuid::Uuid::nil(), // System ID for initial credits
                            initial_credits,
                            Some("Initial credits on account creation".to_string()),
                        );
                        if let Err(e) = credits_repo.create_transaction(&request).await {
                            return Some(Err(Error::Database(e)));
                        }
                    }
                }

                let last_login = user.last_login;
                Some((
                    CurrentUser {
                        id: user.id,
                        username: user.username,
                        email: user.email,
                        is_admin: user.is_admin,
                        roles: user.roles,
                        display_name: user.display_name,
                        avatar_url: user.avatar_url,
                        payment_provider_id: user.payment_provider_id,
                        organizations: vec![],
                        active_organization: None,
                    },
                    last_login,
                ))
            }
            Err(e) => return Some(Err(Error::Database(e))),
        }
    } else {
        // auto_create disabled - just lookup by external_user_id
        debug!("Auto-create disabled, looking up existing user");
        match user_repo.get_user_by_external_user_id(external_user_id).await {
            Ok(Some(user)) => {
                debug!("Found existing user");
                let last_login = user.last_login;
                Some((
                    CurrentUser {
                        id: user.id,
                        username: user.username,
                        email: user.email,
                        is_admin: user.is_admin,
                        roles: user.roles,
                        display_name: user.display_name,
                        avatar_url: user.avatar_url,
                        payment_provider_id: user.payment_provider_id,
                        organizations: vec![],
                        active_organization: None,
                    },
                    last_login,
                ))
            }
            Ok(None) => {
                debug!("User not found and auto-create disabled");
                None
            }
            Err(e) => return Some(Err(Error::Database(e))),
        }
    };

    // Commit transaction
    match tx.commit().await {
        Ok(_) => {}
        Err(e) => return Some(Err(DbError::from(e).into())),
    }

    // Create sample files after commit so the user and API keys are persisted
    if should_create_sample_files
        && config.sample_files.enabled
        && config.batches.enabled
        && let Some((ref user, _)) = user_result
    {
        let state_clone = state.clone();
        let user_id = user.id;
        tokio::spawn(async move {
            if let Err(e) = crate::api::handlers::auth::create_sample_files_for_new_user(&state_clone, user_id).await {
                tracing::warn!(user_id = %user_id, error = %e, "Failed to create sample files for new user");
            }
        });
    }

    user_result.map(Ok)
}

/// Extract user from API key in Authorization header if present and valid
/// Returns:
/// - None: No Authorization header or not a Bearer token
/// - Some(Ok((user, last_login))): Valid API key found and user authenticated
/// - Some(Err(error)): Bearer token present but invalid or insufficient permissions
#[instrument(skip(parts, db))]
async fn try_api_key_auth(parts: &axum::http::request::Parts, db: &PgPool) -> Option<Result<AuthSuccess>> {
    // Extract Authorization header
    let auth_header = match parts.headers.get(axum::http::header::AUTHORIZATION) {
        Some(header) => header,
        None => return None,
    };

    let auth_str = match auth_header.to_str() {
        Ok(s) => s,
        Err(e) => {
            return Some(Err(Error::BadRequest {
                message: format!("Invalid authorization header: {e}"),
            }));
        }
    };

    // Check for Bearer token format
    let api_key = match auth_str.strip_prefix("Bearer ") {
        Some(key) => key,
        None => return None, // Not a Bearer token, try other auth methods
    };

    // Look up API key in database
    let mut conn = match db.acquire().await {
        Ok(conn) => conn,
        Err(e) => return Some(Err(DbError::from(e).into())),
    };
    let api_key_result = match sqlx::query!(
        r#"
        SELECT ak.user_id, ak.purpose, u.username, u.email, u.is_admin, u.display_name, u.avatar_url, u.payment_provider_id, u.last_login
        FROM api_keys ak
        INNER JOIN users u ON ak.user_id = u.id
        WHERE ak.secret = $1 AND ak.is_deleted = false
        "#,
        api_key
    )
    .fetch_optional(&mut *conn)
    .await
    {
        Ok(result) => result,
        Err(e) => return Some(Err(DbError::from(e).into())),
    };

    let api_key_data = match api_key_result {
        Some(data) => data,
        None => {
            return Some(Err(Error::Unauthenticated {
                message: Some("Invalid API key".to_string()),
            }));
        }
    };

    // Check purpose matches the endpoint path
    // Use OriginalUri to get the full path before axum nest() stripping
    let path = parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .map(|uri| uri.path().to_owned());
    let path = path.as_deref().unwrap_or_else(|| parts.uri.path());
    let purpose_str = &api_key_data.purpose;

    // Validate purpose for the endpoint
    debug!(path, purpose = purpose_str.as_str(), "API key purpose check");
    let is_valid = if path.starts_with("/admin/api/") {
        // Platform endpoints require platform keys
        purpose_str == "platform"
    } else if path.starts_with("/ai/") {
        // AI inference endpoints accept any inference-type key
        matches!(purpose_str.as_str(), "realtime" | "batch" | "playground")
    } else {
        // For other paths, allow any purpose
        true
    };

    if !is_valid {
        return Some(Err(Error::InsufficientPermissions {
            required: crate::types::Permission::Granted,
            action: crate::types::Operation::ReadAll,
            resource: format!("endpoint {} with API key purpose '{}'", path, purpose_str),
        }));
    }

    // Get user roles
    let roles = match sqlx::query_scalar!(
        r#"
        SELECT role as "role!: Role"
        FROM user_roles
        WHERE user_id = $1
        "#,
        api_key_data.user_id
    )
    .fetch_all(&mut *conn)
    .await
    {
        Ok(roles) => roles,
        Err(e) => return Some(Err(DbError::from(e).into())),
    };

    Some(Ok((
        CurrentUser {
            id: api_key_data.user_id,
            username: api_key_data.username,
            email: api_key_data.email,
            is_admin: api_key_data.is_admin,
            roles,
            display_name: api_key_data.display_name,
            avatar_url: api_key_data.avatar_url,
            payment_provider_id: api_key_data.payment_provider_id,
            organizations: vec![],
            active_organization: None,
        },
        api_key_data.last_login,
    )))
}

/// Extractor that checks if the request contains an API key in the Authorization header.
///
/// Used to determine the request source:
/// - API key present → request came from API client
/// - No API key → request came from frontend (cookie auth)
#[derive(Debug, Clone)]
pub struct HasApiKey(pub bool);

impl FromRequestParts<AppState> for HasApiKey {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &AppState) -> std::result::Result<Self, Self::Rejection> {
        let has_api_key = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .map(|s| s.starts_with("Bearer "))
            .unwrap_or(false);

        Ok(HasApiKey(has_api_key))
    }
}

/// The HTTP header name used to specify the active organization context.
/// Clients (browser via localStorage, CLI via flag) send this header to indicate
/// which organization the request should operate in.
pub const ORGANIZATION_HEADER: &str = "x-organization-id";

/// Populate organization context on an authenticated user.
/// Reads the `X-Organization-Id` header and queries the user's org memberships.
async fn populate_org_context(user: &mut CurrentUser, parts: &Parts, db: &PgPool) {
    // Query all organizations the user belongs to
    let mut conn = match db.acquire().await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to acquire connection for org context");
            return;
        }
    };

    let mut org_repo = crate::db::handlers::Organizations::new(&mut conn);

    // Get the user's organization memberships
    match org_repo.list_user_organizations(user.id).await {
        Ok(memberships) => {
            user.organizations = memberships
                .into_iter()
                .map(|m| crate::api::models::users::UserOrganizationContext {
                    id: m.organization_id,
                    name: String::new(), // Will be populated below
                    role: m.role,
                })
                .collect();
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to load user organizations");
            return;
        }
    }

    // Populate organization names by fetching org user records
    if !user.organizations.is_empty() {
        let org_ids: Vec<uuid::Uuid> = user.organizations.iter().map(|o| o.id).collect();
        match sqlx::query!(r#"SELECT id, username FROM users WHERE id = ANY($1)"#, &org_ids)
            .fetch_all(&mut *conn)
            .await
        {
            Ok(rows) => {
                for org in &mut user.organizations {
                    if let Some(row) = rows.iter().find(|r| r.id == org.id) {
                        org.name = row.username.clone();
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load organization names");
            }
        }
    }

    // Read active organization from dw_active_org cookie, falling back to X-Organization-Id header.
    // The cookie is set by POST /session/organization and sent automatically by the browser.
    // The header is used by CLI tools and API clients without access to an API key (likely none).
    let org_id_str = parts
        .headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .find_map(|cookie| {
            let cookie = cookie.trim();
            cookie.strip_prefix("dw_active_org=").filter(|v| !v.is_empty())
        })
        .map(String::from)
        .or_else(|| {
            parts
                .headers
                .get(ORGANIZATION_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        });

    if let Some(ref value) = org_id_str
        && let Ok(org_id) = value.parse::<uuid::Uuid>()
    {
        if user.organizations.iter().any(|o| o.id == org_id) {
            user.active_organization = Some(org_id);
        } else {
            tracing::debug!(
                org_id = %org_id,
                "Active organization references org user is not a member of"
            );
        }
    }
}

/// Spawn a background task to update `last_login` if it is null or older than 5 minutes.
fn maybe_update_last_login(user_id: crate::types::UserId, last_login: Option<DateTime<Utc>>, db: &PgPool) {
    let should_update = match last_login {
        None => true,
        Some(ts) => Utc::now() - ts > chrono::Duration::minutes(5),
    };
    if should_update {
        let pool = db.clone();
        tokio::spawn(async move {
            if let Err(e) = sqlx::query!("UPDATE users SET last_login = NOW() WHERE id = $1", user_id)
                .execute(&pool)
                .await
            {
                tracing::warn!(user_id = %user_id, error = %e, "Failed to update last_login");
            }
        });
    }
}

impl<P: sqlx_pool_router::PoolProvider + Clone + Send + Sync> FromRequestParts<crate::AppState<P>> for CurrentUser {
    type Rejection = Error;

    #[instrument(skip(parts, state))]
    async fn from_request_parts(parts: &mut Parts, state: &crate::AppState<P>) -> Result<Self> {
        // Try all authentication methods and accumulate results
        // Each method returns Option<Result<AuthSuccess>>:
        // - None means the auth method is not applicable (no credentials present)
        // - Some(Ok((user, last_login))) means successful authentication
        // - Some(Err(error)) means auth credentials were present but invalid
        //
        // Strategy: Try ALL methods and return the first successful one.
        // Only fail if ALL methods either weren't present or failed.
        // This allows a user with a valid session cookie + invalid API key to still authenticate.

        let mut auth_errors = Vec::new();
        let mut any_auth_attempted = false;

        // Try API key authentication first (most specific)
        match try_api_key_auth(parts, state.db.read()).await {
            Some(Ok((mut user, last_login))) => {
                debug!("Authentication successful via API key");
                trace!("Authenticated user: {}", user.id);
                populate_org_context(&mut user, parts, state.db.read()).await;
                maybe_update_last_login(user.id, last_login, state.db.write());
                return Ok(user);
            }
            Some(Err(e)) => {
                trace!("API key authentication failed: {:?}", e);
                any_auth_attempted = true;
                auth_errors.push(("API key", e));
            }
            None => {
                trace!("No API key authentication attempted");
            }
        }

        // Native authentication (JWT sessions)
        if state.config.auth.native.enabled {
            match try_jwt_session_auth(parts, &state.config, state.db.read()).await {
                Some(Ok((mut user, last_login))) => {
                    debug!("Authentication successful via JWT session");
                    trace!("Authenticated user: {}", user.id);
                    populate_org_context(&mut user, parts, state.db.read()).await;
                    maybe_update_last_login(user.id, last_login, state.db.write());
                    return Ok(user);
                }
                Some(Err(e)) => {
                    trace!("JWT session authentication failed: {:?}", e);
                    any_auth_attempted = true;
                    auth_errors.push(("JWT session", e));
                }
                None => {
                    trace!("No JWT session authentication attempted");
                }
            }
        }

        // Fall back to proxy header authentication
        if state.config.auth.proxy_header.enabled {
            match try_proxy_header_auth(parts, state).await {
                Some(Ok((mut user, last_login))) => {
                    debug!("Authentication successful via proxy header");
                    trace!("Authenticated user: {}", user.id);
                    populate_org_context(&mut user, parts, state.db.read()).await;
                    maybe_update_last_login(user.id, last_login, state.db.write());
                    return Ok(user);
                }
                Some(Err(e)) => {
                    trace!("Proxy header authentication failed: {:?}", e);
                    any_auth_attempted = true;
                    auth_errors.push(("Proxy header", e));
                }
                None => {
                    trace!("No proxy header authentication attempted");
                }
            }
        }

        // If we get here, no auth method succeeded
        if !any_auth_attempted {
            debug!("Authentication failed: no credentials provided");
            trace!("No authentication credentials found in request");
            Err(Error::Unauthenticated { message: None })
        } else {
            debug!("Authentication failed: invalid credentials");
            trace!("All authentication attempts failed ({}): {:?}", auth_errors.len(), auth_errors);
            Err(Error::Unauthenticated { message: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        api::models::{
            transactions::TransactionFilters,
            users::{CurrentUser, Role},
        },
        db::handlers::{Users, repository::Repository},
        errors::Error,
        test::utils::create_test_config,
        test::utils::require_admin,
    };
    use axum::{extract::FromRequestParts as _, http::request::Parts};
    use sqlx::PgPool;

    fn create_test_parts_with_auth(external_user_id: &str, email: &str) -> Parts {
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", external_user_id)
            .header("x-doubleword-email", email)
            .body(())
            .unwrap();

        let (parts, _body) = request.into_parts();
        parts
    }

    #[sqlx::test]
    async fn test_existing_user_extraction(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Create a test user first
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Test extracting existing user
        let external_user_id = test_user.external_user_id.as_ref().unwrap();
        let mut parts = create_test_parts_with_auth(external_user_id, &test_user.email);

        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_ok());

        let current_user = result.unwrap();
        assert_eq!(current_user.email, test_user.email);
        assert_eq!(current_user.username, test_user.username);
        assert!(current_user.roles.contains(&Role::StandardUser));
    }

    #[sqlx::test]
    async fn test_auto_create_nonexistent_user(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let new_email = "newuser@example.com";
        let new_external_id = "auth0|newuser123";
        let mut parts = create_test_parts_with_auth(new_external_id, new_email);

        // Verify user doesn't exist initially
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let existing = users_repo.get_user_by_email(new_email).await.unwrap();
        assert!(existing.is_none());

        // Extract should auto-create the user
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_ok());

        let current_user = result.unwrap();
        assert_eq!(current_user.email, new_email);
        assert_eq!(current_user.username, new_external_id); // Username is set to external_user_id for uniqueness
        assert!(current_user.roles.contains(&Role::StandardUser));
        assert!(current_user.display_name.is_some(), "Display name should be auto-generated");

        // Verify user was actually created in database with display name
        let created_user = users_repo.get_user_by_email(new_email).await.unwrap();
        assert!(created_user.is_some());
        let db_user = created_user.unwrap();
        assert_eq!(db_user.auth_source, "proxy-header");
        assert!(db_user.display_name.is_some(), "Database user should have display name");

        // Verify display name format (should match pattern: "{adjective} {noun} {4-digit number}")
        let display_name = db_user.display_name.unwrap();
        let parts: Vec<&str> = display_name.split_whitespace().collect();
        assert_eq!(parts.len(), 3, "Display name should have 3 parts");
        assert!(
            parts[2].len() == 4 && parts[2].parse::<u32>().is_ok(),
            "Third part should be a 4-digit number"
        );
    }

    #[sqlx::test]
    async fn test_missing_header_returns_unauthorized(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Create parts without x-doubleword-user header
        let request = axum::http::Request::builder().uri("http://localhost/test").body(()).unwrap();

        let (mut parts, _body) = request.into_parts();

        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_err());

        let error = result.unwrap_err();
        assert_eq!(error.status_code(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_backwards_compatibility_single_header(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Old deployment behavior: single header with email value
        // Should use it as both external_user_id and email
        let email = "legacy-user@example.com";
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", email)
            // Intentionally NOT sending x-doubleword-email header
            .body(())
            .unwrap();

        let (mut parts, _body) = request.into_parts();

        // Should succeed and auto-create user with email as external_user_id
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_ok());

        let current_user = result.unwrap();
        assert_eq!(current_user.email, email);
        assert_eq!(current_user.username, email); // Username set to external_user_id
        assert!(current_user.roles.contains(&Role::StandardUser));

        // Verify user was created in database with correct external_user_id
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let db_user = users_repo.get_user_by_email(email).await.unwrap().unwrap();

        // external_user_id in database should match the email value sent in header_name
        assert_eq!(db_user.external_user_id, Some(email.to_string()));
    }

    #[sqlx::test]
    async fn test_multiple_federated_identities_same_email(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let shared_email = "user@example.com";

        // First login via GitHub
        let github_external_id = "github|user123";
        let request1 = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", github_external_id)
            .header("x-doubleword-email", shared_email)
            .body(())
            .unwrap();

        let (mut parts1, _) = request1.into_parts();
        let result1 = CurrentUser::from_request_parts(&mut parts1, &state).await;
        assert!(result1.is_ok(), "First identity should succeed");

        let user1 = result1.unwrap();
        assert_eq!(user1.email, shared_email);
        assert_eq!(user1.username, github_external_id);

        // Second login via Google (same email, different external_user_id)
        let google_external_id = "google|user456";
        let request2 = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", google_external_id)
            .header("x-doubleword-email", shared_email)
            .body(())
            .unwrap();

        let (mut parts2, _) = request2.into_parts();
        let result2 = CurrentUser::from_request_parts(&mut parts2, &state).await;

        // EXPECTED: Should create a second user with different external_user_id but same email
        // CURRENT: Fails due to email UNIQUE constraint in database
        assert!(
            result2.is_ok(),
            "Second identity should create separate user. Error: {:?}",
            result2.as_ref().err()
        );

        let user2 = result2.unwrap();
        assert_eq!(user2.email, shared_email);
        assert_eq!(user2.username, google_external_id);
        assert_ne!(user1.id, user2.id, "Should be different users");
    }

    #[sqlx::test]
    async fn test_migration_backfill_external_user_id(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let email = "legacy-user@example.com";

        // Simulate a user created before external_user_id feature (external_user_id = NULL)
        // This is what existing users will look like after upgrading
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let legacy_user = users_repo
            .create(&crate::db::models::users::UserCreateDBRequest {
                username: "legacyuser".to_string(),
                email: email.to_string(),
                display_name: None,
                avatar_url: None,
                is_admin: false,
                roles: vec![Role::StandardUser],
                auth_source: "proxy-header".to_string(),
                password_hash: None,
                external_user_id: None, // NULL - no external_user_id set
            })
            .await
            .unwrap();

        let legacy_user_id = legacy_user.id;
        drop(pool_conn); // Release connection for state.db

        // Now user logs in with federated auth for the first time
        let federated_external_id = "auth0|github|user123";
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", federated_external_id)
            .header("x-doubleword-email", email)
            .body(())
            .unwrap();

        let (mut parts, _) = request.into_parts();
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;

        // Should backfill the external_user_id on the existing user
        assert!(result.is_ok(), "Should backfill external_user_id for existing user");
        let user = result.unwrap();
        assert_eq!(user.id, legacy_user_id, "Should use the same existing user");
        assert_eq!(user.email, email);

        // Verify external_user_id was backfilled in database
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let db_user = users_repo
            .get_user_by_external_user_id(federated_external_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(db_user.id, legacy_user_id);
        assert_eq!(db_user.external_user_id, Some(federated_external_id.to_string()));
    }

    #[sqlx::test]
    async fn test_backwards_compat_no_backfill(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let email = "legacy-user@example.com";

        // Create legacy user with NULL external_user_id (pre-migration state)
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let legacy_user = users_repo
            .create(&crate::db::models::users::UserCreateDBRequest {
                username: "legacyuser".to_string(),
                email: email.to_string(),
                display_name: None,
                avatar_url: None,
                is_admin: false,
                roles: vec![Role::StandardUser],
                auth_source: "proxy-header".to_string(),
                password_hash: None,
                external_user_id: None,
            })
            .await
            .unwrap();

        let legacy_user_id = legacy_user.id;
        drop(pool_conn);

        // Login with old proxy (single header) - backwards compatibility mode
        // This simulates upgrading dwctl but NOT upgrading proxy yet
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", email) // Only one header sent
            // x-doubleword-email is NOT sent (old proxy behavior)
            .body(())
            .unwrap();

        let (mut parts, _) = request.into_parts();
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;

        assert!(result.is_ok(), "Should work in backwards compat mode");
        let user = result.unwrap();
        assert_eq!(user.id, legacy_user_id, "Should use existing user");
        assert_eq!(user.email, email);

        // CRITICAL: external_user_id should remain NULL (not backfilled)
        // Because external_user_id == email in this case (backwards compat mode)
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let db_user = users_repo.get_user_by_email(email).await.unwrap().unwrap();
        assert_eq!(db_user.external_user_id, None, "Should NOT backfill in backwards compat mode");

        // Now upgrade proxy to send both headers
        drop(pool_conn);
        let federated_external_id = "github|user123";
        let request2 = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", federated_external_id)
            .header("x-doubleword-email", email)
            .body(())
            .unwrap();

        let (mut parts2, _) = request2.into_parts();
        let result2 = CurrentUser::from_request_parts(&mut parts2, &state).await;

        assert!(result2.is_ok(), "Should work with both headers");
        let user2 = result2.unwrap();
        assert_eq!(user2.id, legacy_user_id, "Should still use same user");

        // NOW it should backfill because external_user_id != email
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let db_user_after = users_repo.get_user_by_email(email).await.unwrap().unwrap();
        assert_eq!(
            db_user_after.external_user_id,
            Some(federated_external_id.to_string()),
            "Should backfill now that proxy sends both headers"
        );
    }

    #[sqlx::test]
    async fn test_only_email_header_sent_fails(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Send only email header, not user header - this is invalid
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-email", "user@example.com")
            // Missing x-doubleword-user header
            .body(())
            .unwrap();

        let (mut parts, _) = request.into_parts();
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;

        // Should fail - user header is required
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status_code(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_auto_create_disabled_existing_user(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.proxy_header.auto_create_users = false;

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Create a user first
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_user_id = test_user.external_user_id.as_ref().unwrap();

        // Try to login with auto_create disabled - should work because user exists
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", external_user_id)
            .header("x-doubleword-email", &test_user.email)
            .body(())
            .unwrap();

        let (mut parts, _) = request.into_parts();
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;

        assert!(result.is_ok(), "Should succeed for existing user even with auto_create disabled");
        let current_user = result.unwrap();
        assert_eq!(current_user.email, test_user.email);
    }

    #[sqlx::test]
    async fn test_auto_create_disabled_new_user_fails(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.proxy_header.auto_create_users = false;

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Try to login as new user with auto_create disabled
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", "github|newuser789")
            .header("x-doubleword-email", "newuser@example.com")
            .body(())
            .unwrap();

        let (mut parts, _) = request.into_parts();
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;

        // Should fail - user doesn't exist and auto_create is disabled
        assert!(result.is_err(), "Should fail for new user when auto_create disabled");
        assert_eq!(result.unwrap_err().status_code(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_existing_user_email_update(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let external_user_id = "github|user123";
        let old_email = "old@example.com";
        let new_email = "new@example.com";

        // First login with original email
        let request1 = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", external_user_id)
            .header("x-doubleword-email", old_email)
            .body(())
            .unwrap();

        let (mut parts1, _) = request1.into_parts();
        let result1 = CurrentUser::from_request_parts(&mut parts1, &state).await;
        assert!(result1.is_ok());
        let user1 = result1.unwrap();
        let user_id = user1.id;
        assert_eq!(user1.email, old_email);

        // Second login with same external_user_id but different email
        let request2 = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", external_user_id)
            .header("x-doubleword-email", new_email)
            .body(())
            .unwrap();

        let (mut parts2, _) = request2.into_parts();
        let result2 = CurrentUser::from_request_parts(&mut parts2, &state).await;
        assert!(result2.is_ok(), "Should update email for existing user");

        let user2 = result2.unwrap();
        assert_eq!(user2.id, user_id, "Should be same user");
        assert_eq!(user2.email, new_email, "Email should be updated");

        // Verify in database
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let db_user = users_repo.get_user_by_external_user_id(external_user_id).await.unwrap().unwrap();
        assert_eq!(db_user.email, new_email);
    }

    #[sqlx::test]
    async fn test_idempotent_logins(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let external_user_id = "auth0|user456";
        let email = "user@example.com";

        // Login multiple times with identical credentials
        for i in 0..3 {
            let request = axum::http::Request::builder()
                .uri("http://localhost/test")
                .header("x-doubleword-user", external_user_id)
                .header("x-doubleword-email", email)
                .body(())
                .unwrap();

            let (mut parts, _) = request.into_parts();
            let result = CurrentUser::from_request_parts(&mut parts, &state).await;

            assert!(result.is_ok(), "Login attempt {} should succeed", i + 1);
            let user = result.unwrap();
            assert_eq!(user.email, email);
        }

        // Verify only one user was created
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let db_user = users_repo.get_user_by_external_user_id(external_user_id).await.unwrap().unwrap();
        assert_eq!(db_user.email, email);
    }

    #[sqlx::test]
    async fn test_special_characters_in_external_user_id(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Test various special characters that might appear in IdP identifiers
        let test_cases = vec![
            "auth0|google-oauth2|123456789",
            "okta|user@domain.com",
            "azure-ad|user_with_underscores",
            "github|user-with-dashes",
        ];

        for external_user_id in test_cases {
            let email = format!("{}@example.com", external_user_id.replace(['|', '@'], "_"));

            let request = axum::http::Request::builder()
                .uri("http://localhost/test")
                .header("x-doubleword-user", external_user_id)
                .header("x-doubleword-email", &email)
                .body(())
                .unwrap();

            let (mut parts, _) = request.into_parts();
            let result = CurrentUser::from_request_parts(&mut parts, &state).await;

            assert!(result.is_ok(), "Should handle external_user_id: {}", external_user_id);
            let user = result.unwrap();
            assert_eq!(user.username, external_user_id);
        }
    }

    #[test]
    fn test_username_extraction_from_email() {
        // Test various email formats for username extraction
        let test_cases = vec![
            ("simple@example.com", "simple"),
            ("user.name@domain.co.uk", "user.name"),
            ("test+tag@gmail.com", "test+tag"),
            ("no-at-sign", "no-at-sign"), // no @ sign case
            ("@domain.com", "user"),      // edge case - empty username
        ];

        for (email, expected_username) in test_cases {
            let username = email.split('@').next().unwrap_or("user");
            let username = if username.is_empty() { "user" } else { username }.to_string();
            assert_eq!(username, expected_username, "Failed for email: {email}");
        }
    }

    #[test]
    fn test_require_admin_function() {
        // Test with admin user
        let admin_user = CurrentUser {
            id: uuid::Uuid::new_v4(),
            username: "admin".to_string(),
            email: "admin@example.com".to_string(),
            is_admin: true,
            roles: vec![Role::PlatformManager],
            display_name: None,
            avatar_url: None,
            payment_provider_id: None,
            organizations: vec![],
            active_organization: None,
        };

        let result = require_admin(admin_user);
        assert!(result.is_ok());

        // Test with regular user
        let regular_user = CurrentUser {
            id: uuid::Uuid::new_v4(),
            username: "user".to_string(),
            email: "user@example.com".to_string(),
            is_admin: false,
            roles: vec![Role::StandardUser],
            display_name: None,
            avatar_url: None,
            payment_provider_id: None,
            organizations: vec![],
            active_organization: None,
        };

        let result = require_admin(regular_user);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.status_code(), axum::http::StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn test_jwt_reflects_current_user_state(pool: PgPool) {
        use crate::auth::session;

        let mut config = create_test_config();
        config.auth.native.enabled = true;

        // Create a user with StandardUser role
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Create a JWT token
        let current_user = CurrentUser {
            id: user.id,
            username: user.username.clone(),
            email: user.email.clone(),
            is_admin: user.is_admin,
            roles: user.roles.clone(),
            display_name: user.display_name.clone(),
            avatar_url: user.avatar_url.clone(),
            payment_provider_id: None,
            organizations: vec![],
            active_organization: None,
        };
        let jwt_token = session::create_session_token(&current_user, &config).unwrap();

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config.clone()).await;

        // Create request with JWT
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("cookie", format!("{}={}", config.auth.native.session.cookie_name, jwt_token))
            .body(())
            .unwrap();
        let (mut parts, _body) = request.into_parts();

        // First extraction should succeed with StandardUser role
        let extracted_user = CurrentUser::from_request_parts(&mut parts, &state).await.unwrap();
        assert_eq!(extracted_user.id, user.id);
        assert_eq!(extracted_user.roles, vec![Role::StandardUser]);
        assert!(!extracted_user.is_admin);

        // Now update the user's roles in the database
        let mut conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut conn);
        let update = crate::db::models::users::UserUpdateDBRequest {
            display_name: None,
            avatar_url: None,
            roles: Some(vec![Role::StandardUser, Role::PlatformManager]),
            password_hash: None,
            batch_notifications_enabled: None,
            low_balance_threshold: None,
            auto_topup_amount: None,
            auto_topup_threshold: None,
            auto_topup_monthly_limit: None,
        };
        users_repo.update(user.id, &update).await.unwrap();

        // Create a NEW request with the SAME JWT token
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("cookie", format!("{}={}", config.auth.native.session.cookie_name, jwt_token))
            .body(())
            .unwrap();
        let (mut parts, _body) = request.into_parts();

        // Extraction should now show updated roles (fetched fresh from DB)
        let extracted_user = CurrentUser::from_request_parts(&mut parts, &state).await.unwrap();
        assert_eq!(extracted_user.id, user.id);
        assert!(extracted_user.roles.contains(&Role::StandardUser));
        assert!(extracted_user.roles.contains(&Role::PlatformManager));
    }

    #[sqlx::test]
    async fn test_jwt_invalidated_when_user_deleted(pool: PgPool) {
        use crate::auth::session;

        let mut config = create_test_config();
        config.auth.native.enabled = true;

        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Create a JWT token
        let current_user = CurrentUser {
            id: user.id,
            username: user.username.clone(),
            email: user.email.clone(),
            is_admin: user.is_admin,
            roles: user.roles.clone(),
            display_name: user.display_name.clone(),
            avatar_url: user.avatar_url.clone(),
            payment_provider_id: None,
            organizations: vec![],
            active_organization: None,
        };
        let jwt_token = session::create_session_token(&current_user, &config).unwrap();

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config.clone()).await;

        // First extraction should succeed
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("cookie", format!("{}={}", config.auth.native.session.cookie_name, jwt_token))
            .body(())
            .unwrap();
        let (mut parts, _body) = request.into_parts();
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_ok());

        // Delete the user
        let mut conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut conn);
        users_repo.delete(user.id).await.unwrap();

        // Try to use the same JWT token after user deletion
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("cookie", format!("{}={}", config.auth.native.session.cookie_name, jwt_token))
            .body(())
            .unwrap();
        let (mut parts, _body) = request.into_parts();
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;

        // Should fail with Unauthenticated error - user no longer exists
        // The important security property is that authentication fails,
        // not the specific error message (which may be aggregated)
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(matches!(error, Error::Unauthenticated { .. }));
    }

    #[sqlx::test]
    async fn test_proxy_header_user_receives_initial_credits(pool: PgPool) {
        use crate::db::handlers::credits::Credits;
        use crate::db::models::credits::CreditTransactionType;

        let mut config = create_test_config();
        config.auth.proxy_header.enabled = true;
        config.auth.proxy_header.auto_create_users = true;
        // Set initial credits for standard users
        config.credits.initial_credits_for_standard_users = rust_decimal::Decimal::new(10000, 2); // 100.00 credits

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let new_email = "proxy-user@example.com";
        let new_external_id = "auth0|proxyuser123";
        let mut parts = create_test_parts_with_auth(new_external_id, new_email);

        // Verify user doesn't exist initially
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut pool_conn);
        let existing = users_repo.get_user_by_email(new_email).await.unwrap();
        assert!(existing.is_none());
        drop(pool_conn);

        // Extract should auto-create the user
        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_ok(), "Should successfully create user via proxy header");

        let current_user = result.unwrap();
        assert_eq!(current_user.email, new_email);
        assert!(current_user.roles.contains(&Role::StandardUser));

        // Verify the user got initial credits
        let mut conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut conn);

        let balance = credits_repo.get_user_balance(current_user.id).await.unwrap();
        assert_eq!(
            balance,
            rust_decimal::Decimal::new(10000, 2),
            "User should have initial credits balance of 100.00"
        );

        // Verify the transaction exists with correct details
        let transactions = credits_repo
            .list_user_transactions(current_user.id, 0, 10, &TransactionFilters::default())
            .await
            .unwrap();

        assert_eq!(transactions.len(), 1, "Should have exactly one transaction");
        assert_eq!(transactions[0].amount, rust_decimal::Decimal::new(10000, 2));
        assert_eq!(transactions[0].transaction_type, CreditTransactionType::AdminGrant);
        assert!(transactions[0].description.as_ref().unwrap().contains("Initial credits"));

        // Verify balance is correct via get_user_balance
        let balance = credits_repo.get_user_balance(current_user.id).await.unwrap();
        assert_eq!(balance, rust_decimal::Decimal::new(10000, 2));
    }

    #[sqlx::test]
    async fn test_proxy_header_existing_user_no_duplicate_credits(pool: PgPool) {
        use crate::db::handlers::credits::Credits;

        let mut config = create_test_config();
        config.auth.proxy_header.enabled = true;
        config.auth.proxy_header.auto_create_users = true;
        config.credits.initial_credits_for_standard_users = rust_decimal::Decimal::new(10000, 2); // 100.00 credits

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let email = "existing-proxy@example.com";
        let external_id = "auth0|existing123";

        // First login - creates user and grants credits
        let mut parts1 = create_test_parts_with_auth(external_id, email);
        let result1 = CurrentUser::from_request_parts(&mut parts1, &state).await;
        assert!(result1.is_ok());
        let user = result1.unwrap();

        // Verify initial credits were granted
        let mut conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut conn);
        let balance = credits_repo.get_user_balance(user.id).await.unwrap();
        assert_eq!(balance, rust_decimal::Decimal::new(10000, 2));
        drop(conn);

        // Second login with same user - should NOT grant credits again
        let mut parts2 = create_test_parts_with_auth(external_id, email);
        let result2 = CurrentUser::from_request_parts(&mut parts2, &state).await;
        assert!(result2.is_ok());

        // Verify balance is still the same (no duplicate credits)
        let mut conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut conn);
        let balance_after = credits_repo.get_user_balance(user.id).await.unwrap();
        assert_eq!(
            balance_after,
            rust_decimal::Decimal::new(10000, 2),
            "Balance should remain the same on subsequent logins"
        );

        // Verify still only one transaction
        let transactions = credits_repo
            .list_user_transactions(user.id, 0, 10, &TransactionFilters::default())
            .await
            .unwrap();
        assert_eq!(transactions.len(), 1, "Should still have exactly one transaction");
    }

    // ── X-Organization-Id header tests ───────────────────────────────────

    #[sqlx::test]
    async fn test_org_context_from_header(pool: PgPool) {
        use crate::db::handlers::Organizations;
        use crate::db::models::organizations::OrganizationCreateDBRequest;

        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Create a user
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_user_id = test_user.external_user_id.as_ref().unwrap();

        // Create an org with this user as owner
        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        let org = orgs
            .create(&OrganizationCreateDBRequest {
                name: "test-org-header".to_string(),
                email: "org@example.com".to_string(),
                display_name: Some("Test Org".to_string()),
                avatar_url: None,
                created_by: test_user.id,
            })
            .await
            .unwrap();
        drop(conn);

        // Request with X-Organization-Id header set to the org
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", external_user_id)
            .header("x-doubleword-email", &test_user.email)
            .header(super::ORGANIZATION_HEADER, org.id.to_string())
            .body(())
            .unwrap();
        let (mut parts, _body) = request.into_parts();

        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_ok());

        let current_user = result.unwrap();
        assert_eq!(current_user.active_organization, Some(org.id));
        assert!(!current_user.organizations.is_empty());
        assert!(current_user.organizations.iter().any(|o| o.id == org.id));
    }

    #[sqlx::test]
    async fn test_org_context_invalid_org_id_ignored(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_user_id = test_user.external_user_id.as_ref().unwrap();

        // Request with X-Organization-Id header set to a random UUID (not a member)
        let fake_org_id = uuid::Uuid::new_v4();
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", external_user_id)
            .header("x-doubleword-email", &test_user.email)
            .header(super::ORGANIZATION_HEADER, fake_org_id.to_string())
            .body(())
            .unwrap();
        let (mut parts, _body) = request.into_parts();

        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_ok());

        let current_user = result.unwrap();
        // active_organization should be None because user is not a member
        assert_eq!(current_user.active_organization, None);
    }

    #[sqlx::test]
    async fn test_org_context_no_header_means_personal(pool: PgPool) {
        use crate::db::handlers::Organizations;
        use crate::db::models::organizations::OrganizationCreateDBRequest;

        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_user_id = test_user.external_user_id.as_ref().unwrap();

        // Create an org (user is a member)
        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        orgs.create(&OrganizationCreateDBRequest {
            name: "test-org-no-header".to_string(),
            email: "org@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            created_by: test_user.id,
        })
        .await
        .unwrap();
        drop(conn);

        // Request WITHOUT X-Organization-Id header
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", external_user_id)
            .header("x-doubleword-email", &test_user.email)
            .body(())
            .unwrap();
        let (mut parts, _body) = request.into_parts();

        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_ok());

        let current_user = result.unwrap();
        // active_organization should be None (personal context)
        assert_eq!(current_user.active_organization, None);
        // But organizations list should still contain the org
        assert_eq!(current_user.organizations.len(), 1);
    }

    #[sqlx::test]
    async fn test_org_context_malformed_header_ignored(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_user_id = test_user.external_user_id.as_ref().unwrap();

        // Request with malformed X-Organization-Id header (not a UUID)
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header("x-doubleword-user", external_user_id)
            .header("x-doubleword-email", &test_user.email)
            .header(super::ORGANIZATION_HEADER, "not-a-uuid")
            .body(())
            .unwrap();
        let (mut parts, _body) = request.into_parts();

        let result = CurrentUser::from_request_parts(&mut parts, &state).await;
        assert!(result.is_ok());

        let current_user = result.unwrap();
        // Should silently ignore the malformed header
        assert_eq!(current_user.active_organization, None);
    }

    #[sqlx::test]
    async fn test_last_login_updated_on_first_auth(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.proxy_header.enabled = true;
        config.auth.proxy_header.auto_create_users = true;

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let email = "new-last-login@example.com";
        let external_id = "auth0|lastlogin123";

        // First auth creates the user — last_login should be null initially
        let mut parts = create_test_parts_with_auth(external_id, email);
        let user = CurrentUser::from_request_parts(&mut parts, &state).await.unwrap();

        // Verify last_login was null at creation
        let row = sqlx::query!("SELECT last_login, created_at FROM users WHERE id = $1", user.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(row.last_login.is_none() || {
            // Background task may have already completed — if so, it should
            // be within a few seconds of created_at
            let ll = row.last_login.unwrap();
            (ll - row.created_at).num_seconds().abs() < 10
        }, "On first auth, last_login should be null or just set by background task");

        // Poll until the background task updates last_login
        let mut last_login = None;
        for _ in 0..50 {
            let row = sqlx::query!("SELECT last_login FROM users WHERE id = $1", user.id)
                .fetch_one(&pool)
                .await
                .unwrap();
            if row.last_login.is_some() {
                last_login = row.last_login;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(last_login.is_some(), "Background task should have set last_login");
    }

    #[sqlx::test]
    async fn test_last_login_not_updated_when_recent(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.proxy_header.enabled = true;

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let external_user_id = test_user.external_user_id.as_ref().unwrap();

        // Set last_login to 1 minute ago (within the 5-minute threshold)
        let recent = chrono::Utc::now() - chrono::Duration::minutes(1);
        sqlx::query!("UPDATE users SET last_login = $1 WHERE id = $2", recent, test_user.id)
            .execute(&pool)
            .await
            .unwrap();

        // Authenticate — should NOT update last_login since it's recent
        let mut parts = create_test_parts_with_auth(external_user_id, &test_user.email);
        let _ = CurrentUser::from_request_parts(&mut parts, &state).await.unwrap();

        // Give the background task a chance to run (it shouldn't)
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let row = sqlx::query!("SELECT last_login FROM users WHERE id = $1", test_user.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let actual = row.last_login.unwrap();
        let diff = (actual - recent).num_seconds().abs();
        assert!(diff < 2, "last_login should not have been updated (diff: {diff}s)");
    }
}
