use crate::db::errors::DbError;
use crate::db::handlers::Groups;
use crate::{
    api::models::users::{CurrentUser, Role},
    auth::session,
    db::{
        handlers::{Repository, Users},
        models::users::UserCreateDBRequest,
    },
    errors::{Error, Result},
    AppState,
};
use axum::{extract::FromRequestParts, http::request::Parts};
use sqlx::PgPool;

/// Extract user from JWT session cookie if present and valid
fn try_jwt_session_auth(parts: &axum::http::request::Parts, config: &crate::config::Config) -> Result<Option<CurrentUser>> {
    let cookie_header = match parts.headers.get(axum::http::header::COOKIE) {
        Some(header) => header,
        None => return Ok(None),
    };

    let cookie_str = cookie_header.to_str().map_err(|e| Error::BadRequest {
        message: format!("Invalid cookie header: {e}"),
    })?;
    let cookie_name = &config.auth.native.session.cookie_name;

    for cookie in cookie_str.split(';') {
        let cookie = cookie.trim();
        if let Some((name, value)) = cookie.split_once('=') {
            if name == cookie_name {
                // Try to verify the JWT session token
                match session::verify_session_token(value, config) {
                    Ok(user) => return Ok(Some(user)),
                    Err(_) => {
                        // Invalid/expired token, continue checking other cookies or return None
                        // We don't propagate JWT verification errors as they're expected for expired tokens
                        continue;
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Extract user from proxy header if present and valid
async fn try_proxy_header_auth(
    parts: &axum::http::request::Parts,
    config: &crate::config::Config,
    db: &PgPool,
) -> Result<Option<CurrentUser>> {
    let user_email = match parts
        .headers
        .get(&config.auth.proxy_header.header_name)
        .and_then(|h| h.to_str().ok())
    {
        Some(email) => email,
        None => return Ok(None),
    };

    let mut tx = db.begin().await.unwrap();
    let mut user_repo = Users::new(&mut tx);

    let user_result = match user_repo.get_user_by_email(user_email).await? {
        Some(user) => Some(CurrentUser {
            id: user.id,
            username: user.username,
            email: user.email,
            is_admin: user.is_admin,
            roles: user.roles,
            display_name: user.display_name,
            avatar_url: user.avatar_url,
        }),
        None => {
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

                let new_user = user_repo.create(&create_request).await?;
                Some(CurrentUser {
                    id: new_user.id,
                    username: new_user.username,
                    email: new_user.email,
                    is_admin: new_user.is_admin,
                    roles: new_user.roles,
                    display_name: new_user.display_name,
                    avatar_url: new_user.avatar_url,
                })
            } else {
                None
            }
        }
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
                group_repo
                    .sync_groups_with_sso(
                        user.id,
                        groups.into_iter().map(|s| s.to_string()).collect(),
                        source,
                        &format!("A group provisioned by the {source} SSO source."),
                    )
                    .await?;
            }
        }
    }

    // Only commit if both user and group operations succeeded
    tx.commit().await.map_err(DbError::from)?;
    Ok(user_result)
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        // Try authentication methods in order, returning the first successful one
        // Native authentication first (JWT sessions)
        if state.config.auth.native.enabled {
            if let Some(user) = try_jwt_session_auth(parts, &state.config)? {
                return Ok(user);
            }
        }

        // Fall back to proxy header authentication
        if state.config.auth.proxy_header.enabled {
            if let Some(user) = try_proxy_header_auth(parts, &state.config, &state.db).await? {
                return Ok(user);
            }
        }

        Err(Error::Unauthenticated { message: None })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        api::models::users::{CurrentUser, Role},
        db::handlers::Users,
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
        let state = AppState::builder().db(pool.clone()).config(config).build();

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
        let state = AppState::builder().db(pool.clone()).config(config).build();

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
        let state = AppState::builder().db(pool.clone()).config(config).build();

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
}
