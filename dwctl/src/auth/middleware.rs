use crate::{
    api::models::users::CurrentUser,
    db::handlers::Deployments,
    errors::Error,
    types::{Operation, Permission},
    AppState,
};
use anyhow::Context;
use axum::{
    body::Body,
    extract::{FromRequestParts, Request, State},
    http::{HeaderValue, Uri},
    middleware::Next,
    response::Response,
};
use tracing::{debug, trace};

/// Implementation for admin_ai_proxy_middleware. Since we only modify the request, in this
/// middleware we can just return it from the implementation.
pub(crate) async fn admin_ai_proxy(state: AppState, mut request: Request) -> Result<Request, Error> {
    let uri = request.uri().clone();
    let path = uri.path();

    // Only intercept requests to /admin/api/v1/ai/*
    if !path.starts_with("/admin/api/v1/ai/") {
        return Ok(request);
    }
    debug!("Intercepted admin AI proxy request: {}", path);

    // Extract user using the same auth methods as other endpoints
    let (mut parts, body) = request.into_parts();
    let current_user = CurrentUser::from_request_parts(&mut parts, &state).await?;
    let user_email = current_user.email.clone();

    // Reconstruct request for further processing
    request = Request::from_parts(parts, body);
    trace!("Authenticated user: {}", user_email);

    // Extract the request body to parse the model
    let body_bytes = match axum::body::to_bytes(std::mem::take(request.body_mut()), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(Error::BadRequest {
                message: "Failed to read request body".to_string(),
            })
        }
    };

    // Extract the model name from the request using the shared function
    let model_name = onwards::extract_model_from_request(request.headers(), &body_bytes).map_err(|_| Error::BadRequest {
        message: "Could not extract model from request".to_string(),
    })?;

    debug!("Model name extracted from request: {}", model_name);

    let mut deployment_conn = state.db.acquire().await.unwrap();
    let mut deployment_repo = Deployments::new(&mut deployment_conn);
    let access_info = deployment_repo
        .check_user_access(&model_name, &user_email)
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| format!("Failed to check user access for model '{model_name}' and user '{user_email}'"))?
        .ok_or_else(|| Error::InsufficientPermissions {
            required: Permission::Granted,
            action: Operation::ReadAll,
            resource: format!("model '{model_name}'"),
        });

    trace!("Access info for user {}: {:?}", user_email, access_info);

    let access_info = access_info?;

    // Rewrite the path from /admin/api/v1/ai/* to /ai/*
    debug!("User has access to model: {}", model_name);
    let new_path = path.replace("/admin/api/v1/ai", "/ai");

    // Create new URI with rewritten path
    let query_string = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let mut parts = uri.into_parts();
    parts.path_and_query = Some(
        format!("{new_path}{query_string}")
            .parse()
            .with_context(|| format!("Failed to parse rewritten path: {new_path}{query_string}"))?,
    );

    let new_uri = Uri::from_parts(parts).with_context(|| "Failed to construct URI from parts")?;

    // Update the request URI
    *request.uri_mut() = new_uri;

    // Add system API key as Authorization header for the AI proxy (from optimized query)
    let headers = request.headers_mut();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", access_info.system_api_key))
            .with_context(|| "Failed to create authorization header value")?,
    );

    // Restore the body to the request
    *request.body_mut() = Body::from(body_bytes);

    trace!("Rewritten request URI: {}", request.uri());
    trace!("Request headers: {:?}", request.headers());
    // Continue with the modified request
    Ok(request)
}

/// Middleware that routes /admin/api/v1/ai requests to /ai with system authentication
/// Only allows requests if the X-Doubleword-User header is set and the user has access to the requested model
pub async fn admin_ai_proxy_middleware(State(state): State<AppState>, request: Request, next: Next) -> Result<Response, Error> {
    let request = admin_ai_proxy(state, request).await?;
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::{
        api::models::{
            groups::GroupCreate,
            users::{CurrentUser, Role},
        },
        auth::{middleware::admin_ai_proxy, session},
        db::{
            handlers::{Deployments, Groups, InferenceEndpoints, Repository as _},
            models::{
                deployments::DeploymentCreateDBRequest, groups::GroupCreateDBRequest, inference_endpoints::InferenceEndpointCreateDBRequest,
            },
        },
        test_utils::{create_test_config, create_test_user},
    };

    #[sqlx::test]
    async fn test_user_no_access_auth_error(pool: PgPool) {
        let config = create_test_config();
        let mut inference_conn = pool.acquire().await.unwrap();
        let user = create_test_user(&pool, Role::StandardUser).await;
        let mut endpoints = InferenceEndpoints::new(&mut inference_conn);
        let endpoint = endpoints
            .create(&InferenceEndpointCreateDBRequest {
                name: "Test Endpoint".to_string(),
                description: Some("Test endpoint".to_string()),
                url: "http://localhost:8000".parse().unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: None,
                auth_header_prefix: None,
                created_by: user.id,
            })
            .await
            .expect("Failed to create test inference endpoint");

        let mut deployment_conn = pool.acquire().await.unwrap();
        let mut deployments = Deployments::new(&mut deployment_conn);
        let model = deployments
            .create(
                &DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("test_model".to_string())
                    .alias("gpt-4".to_string())
                    .maybe_description(Some("Test deployment".to_string()))
                    .hosted_on(endpoint.id)
                    .build(),
            )
            .await
            .expect("Failed to create test deployment");

        let state = crate::test_utils::create_test_app_state(pool.clone(), config).await;
        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            .header("x-doubleword-user", user.email)
            .body(
                json!({
                    "model": model.alias
                })
                .to_string()
                .into(),
            )
            .unwrap();
        let request = admin_ai_proxy(state, request).await;
        assert_eq!(request.unwrap_err().status_code().as_u16(), 403);
    }

    #[sqlx::test]
    async fn test_user_access_no_auth_error(pool: PgPool) {
        let config = create_test_config();
        let user = create_test_user(&pool, Role::StandardUser).await;
        let mut inference_conn = pool.acquire().await.unwrap();
        let mut endpoints = InferenceEndpoints::new(&mut inference_conn);
        let endpoint = endpoints
            .create(&InferenceEndpointCreateDBRequest {
                name: "Test Endpoint".to_string(),
                description: Some("Test endpoint".to_string()),
                url: "http://localhost:8000".parse().unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: None,
                auth_header_prefix: None,
                created_by: user.id,
            })
            .await
            .expect("Failed to create test inference endpoint");

        let mut deployment_conn = pool.acquire().await.unwrap();
        let mut deployments = Deployments::new(&mut deployment_conn);
        let model = deployments
            .create(
                &DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("test_model".to_string())
                    .alias("gpt-4".to_string())
                    .maybe_description(Some("Test deployment".to_string()))
                    .hosted_on(endpoint.id)
                    .build(),
            )
            .await
            .expect("Failed to create test deployment");

        let mut group_con = pool.acquire().await.unwrap();
        let mut groups = Groups::new(&mut group_con);
        let group = groups
            .create(&GroupCreateDBRequest::new(
                Uuid::nil(),
                GroupCreate {
                    name: "a group".to_string(),
                    description: Some("A test group".to_string()),
                },
            ))
            .await
            .expect("Failed to create test group");

        groups
            .add_user_to_group(user.id, group.id)
            .await
            .expect("Failed to add user to group");
        groups
            .add_deployment_to_group(model.id, group.id, Uuid::nil())
            .await
            .expect("Failed to add deployment to group");

        let state = crate::test_utils::create_test_app_state(pool.clone(), config).await;
        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            .header("x-doubleword-user", user.email)
            .body(
                json!({
                    "model": model.alias
                })
                .to_string()
                .into(),
            )
            .unwrap();
        // No error on making request - user has access
        let request = admin_ai_proxy(state, request).await.unwrap();
        assert_eq!(request.uri().path(), "/ai/v1/chat/completions"); // stripped the
                                                                     // /admin/api/v1/ prefix
        assert!(request.headers().get("authorization").is_some());
    }

    #[sqlx::test]
    async fn test_header_must_be_supplied(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test_utils::create_test_app_state(pool.clone(), config).await;
        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            .body(
                json!({
                    "model": "irrelevant"
                })
                .to_string()
                .into(),
            )
            .unwrap();
        // No error on making request - user has access
        let err = admin_ai_proxy(state, request).await.unwrap_err();
        assert_eq!(err.status_code().as_u16(), 401);
    }

    #[sqlx::test]
    async fn test_unknown_user_no_access(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test_utils::create_test_app_state(pool.clone(), config).await;
        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            .header("x-doubleword-user", "test@example.org")
            .body(
                json!({
                    "model": "irrelevant"
                })
                .to_string()
                .into(),
            )
            .unwrap();
        // No error on making request - user has access
        let err = admin_ai_proxy(state, request).await.unwrap_err();
        assert_eq!(err.status_code().as_u16(), 403);
    }

    #[sqlx::test]
    async fn test_unknown_model_not_found(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test_utils::create_test_app_state(pool.clone(), config).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            .header("x-doubleword-user", user.email)
            .body(
                json!({
                    "model": "nonexistent"
                })
                .to_string()
                .into(),
            )
            .unwrap();
        // No error on making request - user has access
        let err = admin_ai_proxy(state, request).await.unwrap_err();
        assert_eq!(err.status_code().as_u16(), 403);
    }

    #[sqlx::test]
    async fn test_ignored_paths(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test_utils::create_test_app_state(pool.clone(), config).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let request = axum::http::Request::builder()
            .uri("/nonsense/admin/api/v1/ai/v1/chat/completions")
            .header("x-doubleword-user", user.email)
            .body(
                json!({
                    "model": "nonexistent"
                })
                .to_string()
                .into(),
            )
            .unwrap();
        // No error on making request - user has access
        let err = admin_ai_proxy(state, request).await.unwrap();
        assert_eq!(err.uri().path(), "/nonsense/admin/api/v1/ai/v1/chat/completions");
    }

    #[sqlx::test]
    async fn test_user_access_through_everyone_group(pool: PgPool) {
        let config = create_test_config();
        let user = create_test_user(&pool, Role::StandardUser).await;
        let mut inference_conn = pool.acquire().await.unwrap();
        let mut endpoints = InferenceEndpoints::new(&mut inference_conn);
        let endpoint = endpoints
            .create(&InferenceEndpointCreateDBRequest {
                name: "Test Endpoint".to_string(),
                description: Some("Test endpoint".to_string()),
                url: "http://localhost:8000".parse().unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: None,
                auth_header_prefix: None,
                created_by: user.id,
            })
            .await
            .expect("Failed to create test inference endpoint");

        let mut deployment_conn = pool.acquire().await.unwrap();
        let mut deployments = Deployments::new(&mut deployment_conn);
        let model = deployments
            .create(
                &DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("test_model".to_string())
                    .alias("gpt-4-everyone".to_string())
                    .maybe_description(Some("Test deployment for Everyone group".to_string()))
                    .hosted_on(endpoint.id)
                    .build(),
            )
            .await
            .expect("Failed to create test deployment");

        let mut group_conn = pool.acquire().await.unwrap();
        let mut groups = Groups::new(&mut group_conn);

        // Add deployment to Everyone group (nil UUID)
        let everyone_group_id = uuid::Uuid::nil();
        groups
            .add_deployment_to_group(model.id, everyone_group_id, user.id)
            .await
            .expect("Failed to add deployment to Everyone group");

        let state = crate::test_utils::create_test_app_state(pool.clone(), config).await;
        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            .header("x-doubleword-user", user.email)
            .body(
                json!({
                    "model": model.alias
                })
                .to_string()
                .into(),
            )
            .unwrap();

        // User should have access through Everyone group - no error
        let request = admin_ai_proxy(state, request).await.unwrap();
        assert_eq!(request.uri().path(), "/ai/v1/chat/completions");
        assert!(request.headers().get("authorization").is_some());
    }

    #[sqlx::test]
    async fn test_jwt_session_authentication(pool: PgPool) {
        let mut config = create_test_config();
        // Enable native auth for JWT tests
        config.auth.native.enabled = true;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let mut inference_conn = pool.acquire().await.unwrap();
        let mut endpoints = InferenceEndpoints::new(&mut inference_conn);
        let endpoint = endpoints
            .create(&InferenceEndpointCreateDBRequest {
                name: "Test Endpoint".to_string(),
                description: Some("Test endpoint".to_string()),
                url: "http://localhost:8000".parse().unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: None,
                auth_header_prefix: None,
                created_by: user.id,
            })
            .await
            .expect("Failed to create test inference endpoint");

        let mut deployment_conn = pool.acquire().await.unwrap();
        let mut deployments = Deployments::new(&mut deployment_conn);
        let model = deployments
            .create(
                &DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("test_model".to_string())
                    .alias("gpt-4-jwt".to_string())
                    .maybe_description(Some("Test deployment for JWT auth".to_string()))
                    .hosted_on(endpoint.id)
                    .build(),
            )
            .await
            .expect("Failed to create test deployment");

        let mut group_conn = pool.acquire().await.unwrap();
        let mut groups = Groups::new(&mut group_conn);
        let group = groups
            .create(&GroupCreateDBRequest::new(
                Uuid::nil(),
                GroupCreate {
                    name: "jwt group".to_string(),
                    description: Some("A test group for JWT".to_string()),
                },
            ))
            .await
            .expect("Failed to create test group");

        groups
            .add_user_to_group(user.id, group.id)
            .await
            .expect("Failed to add user to group");
        groups
            .add_deployment_to_group(model.id, group.id, Uuid::nil())
            .await
            .expect("Failed to add deployment to group");

        // Create JWT session token
        let current_user = CurrentUser {
            id: user.id,
            username: user.username,
            email: user.email,
            is_admin: user.is_admin,
            roles: user.roles,
            display_name: user.display_name,
            avatar_url: user.avatar_url,
        };
        let jwt_token = session::create_session_token(&current_user, &config).unwrap();

        let state = crate::test_utils::create_test_app_state(pool.clone(), config.clone()).await;

        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            .header("cookie", format!("{}={}", config.auth.native.session.cookie_name, jwt_token))
            .body(
                json!({
                    "model": model.alias
                })
                .to_string()
                .into(),
            )
            .unwrap();

        // User should have access via JWT session
        let request = admin_ai_proxy(state, request).await.unwrap();
        assert_eq!(request.uri().path(), "/ai/v1/chat/completions");
        assert!(request.headers().get("authorization").is_some());
    }

    #[sqlx::test]
    async fn test_auth_method_priority_jwt_over_header(pool: PgPool) {
        let mut config = create_test_config();
        // Enable native auth for JWT priority tests
        config.auth.native.enabled = true;
        let jwt_user = create_test_user(&pool, Role::StandardUser).await;
        let header_user = create_test_user(&pool, Role::StandardUser).await;

        let mut inference_conn = pool.acquire().await.unwrap();
        let mut endpoints = InferenceEndpoints::new(&mut inference_conn);
        let endpoint = endpoints
            .create(&InferenceEndpointCreateDBRequest {
                name: "Test Endpoint".to_string(),
                description: Some("Test endpoint".to_string()),
                url: "http://localhost:8000".parse().unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: None,
                auth_header_prefix: None,
                created_by: jwt_user.id,
            })
            .await
            .expect("Failed to create test inference endpoint");

        let mut deployment_conn = pool.acquire().await.unwrap();
        let mut deployments = Deployments::new(&mut deployment_conn);
        let model = deployments
            .create(
                &DeploymentCreateDBRequest::builder()
                    .created_by(jwt_user.id)
                    .model_name("test_model".to_string())
                    .alias("gpt-4-priority".to_string())
                    .maybe_description(Some("Test deployment for auth priority".to_string()))
                    .hosted_on(endpoint.id)
                    .build(),
            )
            .await
            .expect("Failed to create test deployment");

        let mut group_conn = pool.acquire().await.unwrap();
        let mut groups = Groups::new(&mut group_conn);
        let group = groups
            .create(&GroupCreateDBRequest::new(
                Uuid::nil(),
                GroupCreate {
                    name: "priority group".to_string(),
                    description: Some("A test group for auth priority".to_string()),
                },
            ))
            .await
            .expect("Failed to create test group");

        // Give JWT user access but not header user
        groups
            .add_user_to_group(jwt_user.id, group.id)
            .await
            .expect("Failed to add JWT user to group");
        groups
            .add_deployment_to_group(model.id, group.id, Uuid::nil())
            .await
            .expect("Failed to add deployment to group");

        // Create JWT session token for the JWT user
        let current_user = CurrentUser {
            id: jwt_user.id,
            username: jwt_user.username,
            email: jwt_user.email.clone(),
            is_admin: jwt_user.is_admin,
            roles: jwt_user.roles,
            display_name: jwt_user.display_name,
            avatar_url: jwt_user.avatar_url,
        };
        let jwt_token = session::create_session_token(&current_user, &config).unwrap();

        let state = crate::test_utils::create_test_app_state(pool.clone(), config.clone()).await;

        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            // Both JWT cookie and proxy header present - JWT should take priority
            .header("cookie", format!("{}={}", config.auth.native.session.cookie_name, jwt_token))
            .header("x-doubleword-user", header_user.email) // This user has no access
            .body(
                json!({
                    "model": model.alias
                })
                .to_string()
                .into(),
            )
            .unwrap();

        // Should succeed because JWT auth (which has access) takes priority over header auth
        let request = admin_ai_proxy(state, request).await.unwrap();
        assert_eq!(request.uri().path(), "/ai/v1/chat/completions");
        assert!(request.headers().get("authorization").is_some());
    }

    #[sqlx::test]
    async fn test_disabled_auth_methods(pool: PgPool) {
        let mut config = create_test_config();
        // Disable native auth but keep proxy header enabled
        config.auth.native.enabled = false;

        let user = create_test_user(&pool, Role::StandardUser).await;
        let mut inference_conn = pool.acquire().await.unwrap();
        let mut endpoints = InferenceEndpoints::new(&mut inference_conn);
        let endpoint = endpoints
            .create(&InferenceEndpointCreateDBRequest {
                name: "Test Endpoint".to_string(),
                description: Some("Test endpoint".to_string()),
                url: "http://localhost:8000".parse().unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: None,
                auth_header_prefix: None,
                created_by: user.id,
            })
            .await
            .expect("Failed to create test inference endpoint");

        let mut deployment_conn = pool.acquire().await.unwrap();
        let mut deployments = Deployments::new(&mut deployment_conn);
        let model = deployments
            .create(
                &DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("test_model".to_string())
                    .alias("gpt-4-disabled".to_string())
                    .maybe_description(Some("Test deployment for disabled auth".to_string()))
                    .hosted_on(endpoint.id)
                    .build(),
            )
            .await
            .expect("Failed to create test deployment");

        let mut group_conn = pool.acquire().await.unwrap();
        let mut groups = Groups::new(&mut group_conn);
        let group = groups
            .create(&GroupCreateDBRequest::new(
                Uuid::nil(),
                GroupCreate {
                    name: "disabled auth group".to_string(),
                    description: Some("A test group for disabled auth".to_string()),
                },
            ))
            .await
            .expect("Failed to create test group");

        groups
            .add_user_to_group(user.id, group.id)
            .await
            .expect("Failed to add user to group");
        groups
            .add_deployment_to_group(model.id, group.id, Uuid::nil())
            .await
            .expect("Failed to add deployment to group");

        // Create JWT session token
        let current_user = CurrentUser {
            id: user.id,
            username: user.username.clone(),
            email: user.email.clone(),
            is_admin: user.is_admin,
            roles: user.roles.clone(),
            display_name: user.display_name,
            avatar_url: user.avatar_url,
        };
        let jwt_token = session::create_session_token(&current_user, &config).unwrap();

        let state = crate::test_utils::create_test_app_state(pool.clone(), config.clone()).await;

        // Request with JWT cookie - should be ignored since native auth is disabled
        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            .header("cookie", format!("{}={}", config.auth.native.session.cookie_name, jwt_token))
            .header("x-doubleword-user", user.email) // This should work since proxy header is enabled
            .body(
                json!({
                    "model": model.alias
                })
                .to_string()
                .into(),
            )
            .unwrap();

        // Should succeed via proxy header auth (JWT is ignored)
        let request = admin_ai_proxy(state, request).await.unwrap();
        assert_eq!(request.uri().path(), "/ai/v1/chat/completions");
        assert!(request.headers().get("authorization").is_some());
    }

    #[sqlx::test]
    async fn test_auto_user_creation_via_current_user(pool: PgPool) {
        let config = create_test_config();
        // Config should have auto_create_users = true by default in test config

        let mut tx = pool.begin().await.unwrap();

        let mut endpoints = InferenceEndpoints::new(&mut tx);
        let endpoint = endpoints
            .create(&InferenceEndpointCreateDBRequest {
                name: "Test Endpoint".to_string(),
                description: Some("Test endpoint".to_string()),
                url: "http://localhost:8000".parse().unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: None,
                auth_header_prefix: None,
                created_by: Uuid::nil(), // Use nil for system creation
            })
            .await
            .expect("Failed to create test inference endpoint");

        let mut deployments = Deployments::new(&mut tx);
        let model = deployments
            .create(
                &DeploymentCreateDBRequest::builder()
                    .created_by(Uuid::nil())
                    .model_name("test_model".to_string())
                    .alias("gpt-4-auto-create".to_string())
                    .maybe_description(Some("Test deployment for auto user creation".to_string()))
                    .hosted_on(endpoint.id)
                    .build(),
            )
            .await
            .expect("Failed to create test deployment");

        // Add deployment to Everyone group so auto-created user has access
        let mut groups = Groups::new(&mut tx);
        groups
            .add_deployment_to_group(model.id, Uuid::nil(), Uuid::nil())
            .await
            .expect("Failed to add deployment to Everyone group");

        tx.commit().await.unwrap();

        let state = crate::test_utils::create_test_app_state(pool.clone(), config).await;

        let new_user_email = "auto-created@example.com";

        // Verify user doesn't exist before the request
        let mut user_conn = pool.acquire().await.unwrap();
        let mut users_repo = crate::db::handlers::Users::new(&mut user_conn);
        let existing_user = users_repo.get_user_by_email(new_user_email).await.unwrap();
        assert!(existing_user.is_none());

        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            .header("x-doubleword-user", new_user_email)
            .body(
                json!({
                    "model": model.alias
                })
                .to_string()
                .into(),
            )
            .unwrap();

        // Should succeed and auto-create the user
        let request = admin_ai_proxy(state, request).await.unwrap();
        assert_eq!(request.uri().path(), "/ai/v1/chat/completions");
        assert!(request.headers().get("authorization").is_some());

        // Verify user was actually created
        let created_user = users_repo.get_user_by_email(new_user_email).await.unwrap();
        assert!(created_user.is_some());
        let db_user = created_user.unwrap();
        assert_eq!(db_user.email, new_user_email);
        assert_eq!(db_user.auth_source, "proxy-header");
    }

    #[sqlx::test]
    async fn test_invalid_jwt_fallback_to_header(pool: PgPool) {
        let mut config = create_test_config();
        // Enable native auth for JWT fallback tests
        config.auth.native.enabled = true;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut tx = pool.begin().await.unwrap();

        let mut endpoints = InferenceEndpoints::new(&mut tx);
        let endpoint = endpoints
            .create(&InferenceEndpointCreateDBRequest {
                name: "Test Endpoint".to_string(),
                description: Some("Test endpoint".to_string()),
                url: "http://localhost:8000".parse().unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: None,
                auth_header_prefix: None,
                created_by: user.id,
            })
            .await
            .expect("Failed to create test inference endpoint");

        let mut deployments = Deployments::new(&mut tx);
        let model = deployments
            .create(
                &DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("test_model".to_string())
                    .alias("gpt-4-fallback".to_string())
                    .maybe_description(Some("Test deployment for auth fallback".to_string()))
                    .hosted_on(endpoint.id)
                    .build(),
            )
            .await
            .expect("Failed to create test deployment");

        let mut groups = Groups::new(&mut tx);
        let group = groups
            .create(&GroupCreateDBRequest::new(
                Uuid::nil(),
                GroupCreate {
                    name: "fallback group".to_string(),
                    description: Some("A test group for auth fallback".to_string()),
                },
            ))
            .await
            .expect("Failed to create test group");

        groups
            .add_user_to_group(user.id, group.id)
            .await
            .expect("Failed to add user to group");
        groups
            .add_deployment_to_group(model.id, group.id, Uuid::nil())
            .await
            .expect("Failed to add deployment to group");

        tx.commit().await.unwrap();

        let state = crate::test_utils::create_test_app_state(pool.clone(), config.clone()).await;

        let request = axum::http::Request::builder()
            .uri("/admin/api/v1/ai/v1/chat/completions")
            // Invalid JWT token
            .header("cookie", format!("{}=invalid-jwt-token", config.auth.native.session.cookie_name))
            // Valid proxy header - should fallback to this
            .header("x-doubleword-user", user.email)
            .body(
                json!({
                    "model": model.alias
                })
                .to_string()
                .into(),
            )
            .unwrap();

        // Should succeed via proxy header fallback
        let request = admin_ai_proxy(state, request).await.unwrap();
        assert_eq!(request.uri().path(), "/ai/v1/chat/completions");
        assert!(request.headers().get("authorization").is_some());
    }
}
