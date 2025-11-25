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
use sqlx::PgPool;
use tracing::{debug, instrument, trace};

/// Extract user from JWT session cookie if present and valid
/// Returns:
/// - None: No JWT cookie present
/// - Some(Ok(user)): Valid JWT found, user fetched from DB with current data
/// - Some(Err(error)): JWT cookie present but invalid/malformed, or user not found/deleted
#[instrument(skip(parts, config, db))]
async fn try_jwt_session_auth(
    parts: &axum::http::request::Parts,
    config: &crate::config::Config,
    db: &PgPool,
) -> Option<Result<CurrentUser>> {
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

                return Some(Ok(CurrentUser {
                    id: user.id,
                    username: user.username,
                    email: user.email,
                    is_admin: user.is_admin,
                    roles: user.roles,
                    display_name: user.display_name,
                    avatar_url: user.avatar_url,
                    payment_provider_id: user.payment_provider_id,
                }));
            }
        }
    }
    None
}

/// Extract user from proxy header if present and valid
/// Returns:
/// - None: No proxy header present
/// - Some(Ok(user)): Valid proxy header found and user authenticated
/// - Some(Err(error)): Proxy header present but user lookup/creation failed
#[instrument(skip(parts, config, db), level = "TRACE")]
async fn try_proxy_header_auth(
    parts: &axum::http::request::Parts,
    config: &crate::config::Config,
    db: &PgPool,
) -> Option<Result<CurrentUser>> {
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

    // Get or create user with group sync (only if auto_create is enabled)
    let user_result = if config.auth.proxy_header.auto_create_users {
        match user_repo
            .get_or_create_proxy_header_user(external_user_id, user_email, groups_and_provider)
            .await
        {
            Ok(user) => Some(CurrentUser {
                id: user.id,
                username: user.username,
                email: user.email,
                is_admin: user.is_admin,
                roles: user.roles,
                display_name: user.display_name,
                avatar_url: user.avatar_url,
                payment_provider_id: user.payment_provider_id,
            }),
            Err(e) => return Some(Err(Error::Database(e))),
        }
    } else {
        // auto_create disabled - just lookup by external_user_id
        debug!("Auto-create disabled, looking up existing user");
        match user_repo.get_user_by_external_user_id(external_user_id).await {
            Ok(Some(user)) => {
                debug!("Found existing user");
                Some(CurrentUser {
                    id: user.id,
                    username: user.username,
                    email: user.email,
                    is_admin: user.is_admin,
                    roles: user.roles,
                    display_name: user.display_name,
                    avatar_url: user.avatar_url,
                    payment_provider_id: user.payment_provider_id,
                })
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
    user_result.map(Ok)
}

/// Extract user from API key in Authorization header if present and valid
/// Returns:
/// - None: No Authorization header or not a Bearer token
/// - Some(Ok(user)): Valid API key found and user authenticated
/// - Some(Err(error)): Bearer token present but invalid or insufficient permissions
#[instrument(skip(parts, db))]
async fn try_api_key_auth(parts: &axum::http::request::Parts, db: &PgPool) -> Option<Result<CurrentUser>> {
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
        SELECT ak.user_id, ak.purpose, u.username, u.email, u.is_admin, u.display_name, u.avatar_url, u.payment_provider_id
        FROM api_keys ak
        INNER JOIN users u ON ak.user_id = u.id
        WHERE ak.secret = $1
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
    let path = parts.uri.path();
    let purpose_str = &api_key_data.purpose;

    let expected_purpose = if path.starts_with("/admin/api/") {
        "platform"
    } else if path.starts_with("/ai/") {
        "inference"
    } else {
        // For other paths, allow any purpose
        purpose_str.as_str()
    };

    if purpose_str != expected_purpose {
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

    Some(Ok(CurrentUser {
        id: api_key_data.user_id,
        username: api_key_data.username,
        email: api_key_data.email,
        is_admin: api_key_data.is_admin,
        roles,
        display_name: api_key_data.display_name,
        avatar_url: api_key_data.avatar_url,
        payment_provider_id: api_key_data.payment_provider_id,
    }))
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = Error;

    #[instrument(skip(parts, state))]
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        // Try all authentication methods and accumulate results
        // Each method returns Option<Result<CurrentUser>>:
        // - None means the auth method is not applicable (no credentials present)
        // - Some(Ok(user)) means successful authentication
        // - Some(Err(error)) means auth credentials were present but invalid
        //
        // Strategy: Try ALL methods and return the first successful one.
        // Only fail if ALL methods either weren't present or failed.
        // This allows a user with a valid session cookie + invalid API key to still authenticate.

        let mut auth_errors = Vec::new();
        let mut any_auth_attempted = false;

        // Try API key authentication first (most specific)
        match try_api_key_auth(parts, &state.db).await {
            Some(Ok(user)) => {
                debug!("Authentication successful via API key");
                trace!("Authenticated user: {}", user.id);
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
            match try_jwt_session_auth(parts, &state.config, &state.db).await {
                Some(Ok(user)) => {
                    debug!("Authentication successful via JWT session");
                    trace!("Authenticated user: {}", user.id);
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
            match try_proxy_header_auth(parts, &state.config, &state.db).await {
                Some(Ok(user)) => {
                    debug!("Authentication successful via proxy header");
                    trace!("Authenticated user: {}", user.id);
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
        AppState,
        api::models::users::{CurrentUser, Role},
        db::handlers::{Users, repository::Repository},
        errors::Error,
        test_utils::create_test_config,
        test_utils::require_admin,
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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

        // Create a test user first
        let test_user = crate::test_utils::create_test_user(&pool, Role::StandardUser).await;

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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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

        // Verify user was actually created in database
        let created_user = users_repo.get_user_by_email(new_email).await.unwrap();
        assert!(created_user.is_some());
        let db_user = created_user.unwrap();
        assert_eq!(db_user.auth_source, "proxy-header");
    }

    #[sqlx::test]
    async fn test_missing_header_returns_unauthorized(pool: PgPool) {
        let config = create_test_config();
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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

        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

        // Create a user first
        let test_user = crate::test_utils::create_test_user(&pool, Role::StandardUser).await;
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

        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

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
        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config)
                .request_manager(request_manager)
                .build()
        };

        // Test various special characters that might appear in IdP identifiers
        let test_cases = vec![
            "auth0|google-oauth2|123456789",
            "okta|user@domain.com",
            "azure-ad|user_with_underscores",
            "github|user-with-dashes",
        ];

        for external_user_id in test_cases {
            let email = format!("{}@example.com", external_user_id.replace('|', "_").replace('@', "_"));

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
        let user = crate::test_utils::create_test_user(&pool, Role::StandardUser).await;

        // Create a JWT token
        let current_user = CurrentUser {
            id: user.id,
            username: user.username.clone(),
            email: user.email.clone(),
            is_admin: user.is_admin,
            roles: user.roles.clone(),
            display_name: user.display_name.clone(),
            avatar_url: user.avatar_url.clone(),
            payment_provider_id: user.payment_provider_id.clone(),
        };
        let jwt_token = session::create_session_token(&current_user, &config).unwrap();

        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config.clone())
                .request_manager(request_manager)
                .build()
        };

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

        let user = crate::test_utils::create_test_user(&pool, Role::StandardUser).await;

        // Create a JWT token
        let current_user = CurrentUser {
            id: user.id,
            username: user.username.clone(),
            email: user.email.clone(),
            is_admin: user.is_admin,
            roles: user.roles.clone(),
            display_name: user.display_name.clone(),
            avatar_url: user.avatar_url.clone(),
            payment_provider_id: user.payment_provider_id.clone(),
        };
        let jwt_token = session::create_session_token(&current_user, &config).unwrap();

        let state = {
            let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
            AppState::builder()
                .db(pool.clone())
                .config(config.clone())
                .request_manager(request_manager)
                .build()
        };

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
}
