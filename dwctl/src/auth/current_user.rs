//! User extraction from request authentication.

use crate::db::errors::DbError;
use crate::db::handlers::Groups;
use crate::{
    api::models::users::{CurrentUser, Role},
    auth::session,
    db::{
        handlers::{Repository, Users}, // Add Repository here
        models::users::UserCreateDBRequest,
    },
    errors::{Error, Result},
    AppState,
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
            }))
        }
    };
    let cookie_name = &config.auth.native.session.cookie_name;

    for cookie in cookie_str.split(';') {
        let cookie = cookie.trim();
        if let Some((name, value)) = cookie.split_once('=') {
            if name == cookie_name {
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
#[instrument(skip(parts, config, db))]
async fn try_proxy_header_auth(
    parts: &axum::http::request::Parts,
    config: &crate::config::Config,
    db: &PgPool,
) -> Option<Result<CurrentUser>> {
    let user_email = match parts
        .headers
        .get(&config.auth.proxy_header.header_name)
        .and_then(|h| h.to_str().ok())
    {
        Some(email) => email,
        None => return None,
    };

    let mut tx = match db.begin().await {
        Ok(tx) => tx,
        Err(e) => return Some(Err(DbError::from(e).into())),
    };
    let mut user_repo = Users::new(&mut tx);

    let user_result = match user_repo.get_user_by_email(user_email).await {
        Ok(Some(user)) => Some(CurrentUser {
            id: user.id,
            username: user.username,
            email: user.email,
            is_admin: user.is_admin,
            roles: user.roles,
            display_name: user.display_name,
            avatar_url: user.avatar_url,
        }),
        Ok(None) => {
            if config.auth.proxy_header.auto_create_users {
                let create_request = UserCreateDBRequest {
                    username: user_email.to_string(),
                    email: user_email.to_string(),
                    display_name: None,
                    avatar_url: None,
                    is_admin: false,
                    roles: vec![Role::StandardUser],
                    auth_source: "proxy-header".to_string(),
                    password_hash: None,
                };

                match user_repo.create(&create_request).await {
                    Ok(new_user) => Some(CurrentUser {
                        id: new_user.id,
                        username: new_user.username,
                        email: new_user.email,
                        is_admin: new_user.is_admin,
                        roles: new_user.roles,
                        display_name: new_user.display_name,
                        avatar_url: new_user.avatar_url,
                    }),
                    Err(e) => return Some(Err(Error::Database(e))),
                }
            } else {
                None
            }
        }
        Err(e) => return Some(Err(Error::Database(e))),
    };

    // If we found a user, check their oauth groups match their db ones.
    if let Some(user) = &user_result {
        if config.auth.proxy_header.import_idp_groups {
            let user_groups: Option<Vec<&str>> = match parts
                .headers
                .get(&config.auth.proxy_header.groups_field_name)
                .and_then(|h| h.to_str().ok())
            {
                Some(group_string) => {
                    let groups: Vec<&str> = group_string
                        .split(",")
                        .map(|g| g.trim())
                        .filter(|g| !config.auth.proxy_header.blacklisted_sso_groups.contains(&g.to_string()))
                        .collect();
                    if groups.is_empty() {
                        None
                    } else {
                        Some(groups)
                    }
                }
                None => None,
            };

            let source = parts
                .headers
                .get(&config.auth.proxy_header.provider_field_name) // &String works as &str
                .and_then(|h| h.to_str().ok()) // convert HeaderValue → &str
                .unwrap_or("unknown"); // default if header missing or invalid UTF-8
            if let Some(groups) = user_groups {
                let mut group_repo = Groups::new(&mut tx);
                if let Err(e) = group_repo
                    .sync_groups_with_sso(
                        user.id,
                        groups.into_iter().map(|s| s.to_string()).collect(),
                        source,
                        &format!("A group provisioned by the {source} SSO source."),
                    )
                    .await
                {
                    return Some(Err(Error::Database(e)));
                }
            }
        }
    }

    // Only commit if both user and group operations succeeded
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
            }))
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
        SELECT ak.user_id, ak.purpose, u.username, u.email, u.is_admin, u.display_name, u.avatar_url
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
            }))
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
                debug!("Found API key authenticated user: {}", user.id);
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
                    debug!("Found JWT session authenticated user: {}", user.id);
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
                    debug!("Found proxy header authenticated user: {}", user.id);
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
            trace!("No authentication credentials found in request");
            Err(Error::Unauthenticated { message: None })
        } else {
            trace!("All authentication attempts failed ({}): {:?}", auth_errors.len(), auth_errors);
            Err(Error::Unauthenticated { message: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        api::models::users::{CurrentUser, Role},
        db::handlers::{Repository, Users}, // Add Repository here
        errors::Error,                     // Add Error here
        test_utils::create_test_config,
        test_utils::require_admin,
        AppState,
    };
    use axum::{extract::FromRequestParts as _, http::request::Parts};
    use sqlx::PgPool;

    fn create_test_parts_with_header(header_name: &str, header_value: &str) -> Parts {
        let request = axum::http::Request::builder()
            .uri("http://localhost/test")
            .header(header_name, header_value)
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
        let mut parts = create_test_parts_with_header("x-doubleword-user", &test_user.email);

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
        let mut parts = create_test_parts_with_header("x-doubleword-user", new_email);

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
        assert_eq!(current_user.username, new_email); // Username should be the email for uniqueness
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
