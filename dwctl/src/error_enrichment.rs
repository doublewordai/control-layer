//! Error enrichment middleware for AI proxy requests.
//!
//! This module provides middleware that intercepts error responses from the onwards
//! routing layer and enriches them with helpful context. The primary use case is
//! transforming generic 403 errors (insufficient credits) into detailed responses
//! that explain the issue and suggest remediation.
//!
//! ## Architecture
//!
//! The middleware sits in the response path after onwards but before outlet:
//! ```text
//! Request → onwards (routing) → error_enrichment (this) → outlet (logging) → Response
//! ```
//!
//! ## Error Cases Enriched
//!
//! 1. **403 Forbidden - Insufficient Credits**: User's balance < 0 for paid models
//!    - Shows current balance
//! 2. **403 Forbidden - Model Access Denied**: User is not a member of a group with access to the requested model
//!    - Shows which model was requested
//! 3. **403 Forbidden - Modality Blocked**: A traffic routing rule denies the API key's
//!    purpose (realtime/batch/playground) for the requested model
//!    - Shows which modality and model are blocked

use crate::{
    db::errors::DbError,
    db::handlers::{Credits, api_keys::ApiKeys},
    errors::Error,
    types::UserId,
};
use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use sqlx::PgPool;
use tracing::{debug, instrument};

/// Request body structure for extracting model name
#[derive(Debug, Deserialize)]
struct ChatRequest {
    model: String,
}

/// Middleware that enriches error responses from the AI proxy with helpful context
///
/// Currently handles:
/// - 403 Forbidden errors (likely insufficient credits) → enriched with balance
/// - 403 Forbidden errors (likely model access denied) → enriched with model name
/// - 403 Forbidden errors (likely modality blocked by routing rule) → enriched with modality + model
#[instrument(name = "dwctl.error_enrichment", skip_all, fields(http.request.method = %request.method(), url.path = %request.uri().path(), url.query = request.uri().query().unwrap_or("")))]
pub async fn error_enrichment_middleware(State(pool): State<PgPool>, request: Request<Body>, next: Next) -> Response<Body> {
    // Extract API key from request headers before passing to onwards
    let api_key = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")))
        .map(|token| token.trim().to_string());

    // Extract request body to get model name for potential error enrichment
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            // If we can't read the body, just pass through
            let reconstructed = Request::from_parts(parts, Body::empty());
            return next.run(reconstructed).await;
        }
    };

    let model_name = serde_json::from_slice::<ChatRequest>(&bytes).ok().map(|req| req.model);

    // Reconstruct the request with the body
    let reconstructed = Request::from_parts(parts, Body::from(bytes));

    // Let the request proceed through onwards
    let response = next.run(reconstructed).await;

    // Only enrich 403 errors when we have an API key
    // Note: This middleware is applied only to the onwards router (AI proxy paths),
    // so no path filtering is needed here
    if response.status() == StatusCode::FORBIDDEN
        && let Some(key) = api_key
    {
        debug!("Intercepted 403 response on AI proxy path, attempting enrichment");

        // Order matters: the more fundamental the failure, the earlier it runs, so
        // when several conditions could explain the 403 we surface the one that's
        // most useful to act on.
        //   1. Model access (group membership) — without this the user can't reach
        //      the model at all, so report it first.
        //   2. Modality (traffic routing rule) — user has the model but their key
        //      kind (batch/realtime/playground) is denied.
        //   3. Insufficient balance — onwards excludes keys with balance ≤ 0; this
        //      is the catch-all if neither of the above explains the 403.

        // 1. Model access via group membership
        if let Some(model) = &model_name
            && let Ok(user_id) = get_user_id_of_api_key(pool.clone(), &key).await
            && let Ok(has_access) = check_user_has_model_access(pool.clone(), user_id, model).await
            && !has_access
        {
            return Error::ModelAccessDenied {
                model_name: model.clone(),
                message: format!(
                    "You do not have access to '{}'. Please contact your administrator to request access.",
                    model
                ),
            }
            .into_response();
        }

        // 2. Modality blocked by a traffic routing rule on this model.
        if let Some(model) = &model_name
            && let Ok(Some(purpose)) = check_modality_blocked(pool.clone(), &key, model).await
        {
            return Error::ModalityAccessDenied {
                model_name: model.clone(),
                purpose: purpose.clone(),
                message: modality_blocked_message(&purpose, model),
            }
            .into_response();
        }

        // 3. Insufficient balance.
        if let Ok(balance) = get_balance_of_api_key(pool.clone(), &key).await
            && balance <= Decimal::ZERO
        {
            return Error::InsufficientCredits {
                current_balance: balance,
                message: "Account balance too low. Please add credits to continue.".to_string(),
            }
            .into_response();
        }
    }

    response
}

/// Render an `api_keys.purpose` value as a user-facing modality label.
///
/// Returns owned `String` so unknown purposes can fall back to a capitalised
/// form of the raw value (rather than a generic placeholder), keeping the
/// message accurate if new purposes are added without updating this match.
fn modality_label(purpose: &str) -> String {
    match purpose {
        "batch" => "Batch".to_string(),
        "realtime" => "Real-time".to_string(),
        "playground" => "Playground".to_string(),
        "platform" => "Platform".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => "This".to_string(),
            }
        }
    }
}

/// Check whether a traffic routing rule denies this API key's purpose for the given model.
///
/// Returns `Ok(Some(purpose))` if a matching `deny` rule exists, where `purpose` is the
/// `api_key_purpose` string stored on the rule (e.g. "batch"). Returns `Ok(None)` otherwise.
#[instrument(skip(pool, api_key), name = "dwctl.check_modality_blocked")]
pub async fn check_modality_blocked(pool: PgPool, api_key: &str, model_alias: &str) -> Result<Option<String>, DbError> {
    let mut conn = pool.acquire().await?;

    let purpose = sqlx::query_scalar!(
        r#"
        SELECT mtr.api_key_purpose
        FROM model_traffic_rules mtr
        JOIN deployed_models dm ON dm.id = mtr.deployed_model_id
        JOIN api_keys ak ON ak.purpose = mtr.api_key_purpose
        WHERE dm.alias = $1
          AND dm.deleted = false
          AND ak.secret = $2
          AND ak.is_deleted = false
          AND mtr.action = 'deny'
        LIMIT 1
        "#,
        model_alias,
        api_key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(purpose)
}

#[instrument(skip_all, name = "dwctl.get_user_id_of_api_key")]
pub async fn get_user_id_of_api_key(pool: PgPool, api_key: &str) -> Result<UserId, DbError> {
    let mut conn = pool.acquire().await?;
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    api_keys_repo
        .get_user_id_by_secret(api_key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("API key not found or associated user doesn't exist").into())
}

#[instrument(skip_all, name = "dwctl.get_balance_of_api_key")]
pub async fn get_balance_of_api_key(pool: PgPool, api_key: &str) -> Result<Decimal, DbError> {
    // Look up user_id from API key
    let user_id = get_user_id_of_api_key(pool.clone(), api_key).await?;

    debug!("Found user_id for API key: {}", user_id);

    // Query user's current balance
    let mut conn = pool.acquire().await?;
    let mut credits_repo = Credits::new(&mut conn);
    credits_repo.get_user_balance(user_id).await
}

#[instrument(skip_all, name = "dwctl.check_user_has_model_access")]
pub async fn check_user_has_model_access(pool: PgPool, user_id: UserId, model_alias: &str) -> Result<bool, DbError> {
    let mut conn = pool.acquire().await?;

    // Query to check if user has access to this deployment through group membership
    let result = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM deployed_models d
            JOIN deployment_groups dg ON dg.deployment_id = d.id
            WHERE d.alias = $1
              AND d.deleted = false
              AND dg.group_id IN (
                  SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = $2
                  UNION
                  SELECT '00000000-0000-0000-0000-000000000000'::uuid
                  WHERE $2 != '00000000-0000-0000-0000-000000000000'
              )
        ) as "has_access!"
        "#,
        model_alias,
        user_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(result)
}

/// Validate that the bearer token's user is allowed to call the specified model.
///
/// Checks both group-based access and modality (traffic routing rule) restrictions.
/// Returns `Ok(())` if the request should be allowed, or a user-facing error message
/// if not.
///
/// Used by the responses middleware to fail fast on Flex requests that bypass
/// `onwards` entirely — without the modality check here, a Batch-purpose key
/// could send a Flex request and skip a deny rule onwards would have enforced.
pub async fn validate_api_key_model_access(pool: PgPool, api_key: &str, model: &str) -> Result<(), String> {
    let user_id = get_user_id_of_api_key(pool.clone(), api_key)
        .await
        .map_err(|_| "Invalid API key".to_string())?;

    let has_access = check_user_has_model_access(pool.clone(), user_id, model)
        .await
        .map_err(|e| format!("Failed to check model access: {e}"))?;

    if !has_access {
        return Err(format!(
            "You do not have access to '{}'. Please contact your administrator to request access.",
            model
        ));
    }

    if let Some(purpose) = check_modality_blocked(pool, api_key, model)
        .await
        .map_err(|e| format!("Failed to check modality routing rules: {e}"))?
    {
        return Err(modality_blocked_message(&purpose, model));
    }

    Ok(())
}

/// Build the user-facing message returned when a routing rule denies the API
/// key's purpose for the requested model. Shared by the proxy enrichment
/// middleware and the Flex validation path so both surface identical wording.
fn modality_blocked_message(purpose: &str, model: &str) -> String {
    format!(
        "{} access to '{}' is blocked by a routing rule. Please contact your administrator to request access.",
        modality_label(purpose),
        model
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        handlers::{Credits, Repository as _, api_keys::ApiKeys},
        models::{
            api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose},
            credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
        },
    };
    use crate::{api::models::users::Role, test::utils::create_test_user};
    use rust_decimal::Decimal;

    /// Pure unit test: `modality_label` produces user-friendly labels for known
    /// purposes and capitalises unknown values rather than emitting a placeholder.
    #[test]
    fn test_modality_label_known_and_unknown_purposes() {
        assert_eq!(modality_label("batch"), "Batch");
        assert_eq!(modality_label("realtime"), "Real-time");
        assert_eq!(modality_label("playground"), "Playground");
        assert_eq!(modality_label("platform"), "Platform");

        // Unknown purposes capitalise the raw value so messages stay accurate
        // if a new purpose ships before this match is updated.
        assert_eq!(modality_label("evaluation"), "Evaluation");

        // Empty string falls back to a generic label rather than producing
        // a sentence starting with " access to ...".
        assert_eq!(modality_label(""), "This");
    }

    /// Integration test: Error enrichment middleware enriches 403 with balance info
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_enriches_403_with_balance(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        // Create test user with an API key
        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
            })
            .await
            .unwrap();

        // Give user some credits
        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(5000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Initial credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Create a test app with just the error enrichment middleware
        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Simulate onwards returning 403 (insufficient credits)
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Forbidden"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

        // Make request with API key in Authorization header
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", api_key.secret))
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Test that response is 403 and enriched with model access denied (user has positive balance but no model access)
        assert_eq!(response.status_code().as_u16(), 403);
        let body = response.text();
        assert!(body.contains("do not have access to 'test-model'"));

        // Now, deduct all credits to simulate insufficient balance
        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::new(10000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Usage".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // We need to ensure user is part of a group with access to the deployment to avoid 403
        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id =
            crate::test::utils::create_test_model(&pool, "authorized-model-name", "authorized-model", endpoint_id, user.id).await;

        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        // Grant access to the group
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        // Make request with API key in Authorization header, using the model the user has access to
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", api_key.secret))
            .json(&serde_json::json!({
                "model": "authorized-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Test that response is 402 and has been enriched
        assert_eq!(response.status_code().as_u16(), 402);
        let body = response.text();
        println!("Enriched response body: {}", body);
        assert!(body.contains("balance too low"));
    }

    /// Integration test: Error enrichment middleware passes through 403 when user has access
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_passes_through_legitimate_403(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        // Create test user with positive balance and model access
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a group and add the user to it
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create API key
        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
            })
            .await
            .unwrap();

        // Give user positive balance
        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(5000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Initial credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Create a deployment with 'authorized-model' alias and grant access to the group
        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id =
            crate::test::utils::create_test_model(&pool, "authorized-model-name", "authorized-model", endpoint_id, user.id).await;

        // Grant access to the group
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        // Create a test app with middleware that returns 403 for a different reason (e.g., rate limit)
        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Simulate a legitimate 403 error (not credits/access related)
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Rate limit exceeded"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

        // Make request with API key and a model the user has access to
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", api_key.secret))
            .json(&serde_json::json!({
                "model": "authorized-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should pass through the original 403 without enrichment
        assert_eq!(response.status_code().as_u16(), 403);
        let body = response.text();
        assert!(body.contains("Rate limit exceeded"));
        assert!(!body.contains("balance"));
        assert!(!body.contains("access"));
    }

    /// Integration test: Error enrichment middleware only affects /ai/ paths
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_ignores_non_ai_paths(pool: PgPool) {
        let router = axum::Router::new()
            .route(
                "/admin/api/v1/users",
                axum::routing::get(|| async {
                    // Return 403 on admin path
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Admin Forbidden"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");
        let response = server.get("/admin/api/v1/users").await;

        // Should remain 403, not enriched
        assert_eq!(response.status_code().as_u16(), 403);
        assert_eq!(response.text(), "Admin Forbidden");
    }

    /// Integration test: Error enrichment middleware ignores non-403 responses
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_ignores_non_403_errors(pool: PgPool) {
        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Return 404 instead of 403
                    axum::response::Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .header("authorization", "Bearer dummy-key")
                        .body(axum::body::Body::from("Not Found"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", "Bearer test-key")
            .json(&serde_json::json!({"model": "test"}))
            .await;

        // Should remain 404, not enriched
        assert_eq!(response.status_code().as_u16(), 404);
        assert_eq!(response.text(), "Not Found");
    }

    /// Integration test: Error enrichment middleware enriches 403 when a routing
    /// rule denies the API key's purpose (e.g. batch/realtime/playground) for the model.
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_enriches_403_with_modality_block(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        // User with positive balance, model access, and a Batch-purpose API key.
        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Batch Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Batch,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
            })
            .await
            .unwrap();

        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(5000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Initial credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Deploy a model the user can access via group membership.
        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id = crate::test::utils::create_test_model(&pool, "blocked-model-name", "blocked-model", endpoint_id, user.id).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        // Add a deny rule for batch-purpose keys on this model.
        sqlx::query!(
            "INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action) VALUES ($1, 'batch', 'deny')",
            deployment_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Simulate onwards returning 403 because a deny rule matched.
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Forbidden"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", api_key.secret))
            .json(&serde_json::json!({
                "model": "blocked-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code().as_u16(), 403);
        let body = response.text();
        assert!(
            body.contains("Batch") && body.contains("blocked-model") && body.contains("administrator"),
            "expected modality-blocked message, got: {body}"
        );
    }

    /// Direct unit test: `validate_api_key_model_access` rejects requests when a routing
    /// rule denies the API key's purpose. Flex requests on `/v1/responses` bypass onwards
    /// and use this function to enforce auth, so it must reject modality-blocked keys
    /// — otherwise a Batch key could reach a model where Batch is denied.
    #[sqlx::test]
    #[test_log::test]
    async fn test_validate_api_key_model_access_rejects_modality_blocked(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Batch Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Batch,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
            })
            .await
            .unwrap();

        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id = crate::test::utils::create_test_model(&pool, "blocked-model-name", "blocked-model", endpoint_id, user.id).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        sqlx::query!(
            "INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action) VALUES ($1, 'batch', 'deny')",
            deployment_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = validate_api_key_model_access(pool.clone(), &api_key.secret, "blocked-model").await;
        let err = result.expect_err("expected modality-blocked rejection");
        assert!(
            err.contains("Batch") && err.contains("blocked-model") && err.contains("administrator"),
            "expected modality-blocked message, got: {err}"
        );

        // Sanity: a model without the deny rule still passes.
        let other_deployment_id = crate::test::utils::create_test_model(&pool, "open-model-name", "open-model", endpoint_id, user.id).await;
        add_deployment_to_group(&pool, other_deployment_id, group.id, user.id).await;
        validate_api_key_model_access(pool.clone(), &api_key.secret, "open-model")
            .await
            .expect("expected access to be granted on a model without a deny rule");
    }

    /// Integration test: Error enrichment middleware passes through when no auth header
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_without_auth_header(pool: PgPool) {
        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Return 403 without auth header
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("No Auth"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");
        let response = server
            .post("/ai/v1/chat/completions")
            .json(&serde_json::json!({"model": "test"}))
            .await;

        // Should pass through original 403 without enrichment
        assert_eq!(response.status_code().as_u16(), 403);
        assert_eq!(response.text(), "No Auth");
    }
}
