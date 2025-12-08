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
#[instrument(skip_all, fields(path = %request.uri().path(), method = %request.method()))]
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

        // Check balance first - if negative, it's a credits issue
        if let Ok(balance) = get_balance_of_api_key(pool.clone(), &key).await {
            if balance < Decimal::ZERO {
                return Error::InsufficientCredits {
                    current_balance: balance,
                    message: "Account balance too low. Please add credits to continue.".to_string(),
                }
                .into_response();
            }
        }

        // If balance is OK but we have a 403, check if it's a model access issue
        if let Some(model) = model_name {
            if let Ok(user_id) = get_user_id_of_api_key(pool.clone(), &key).await {
                if let Ok(has_access) = check_user_has_model_access(pool, user_id, &model).await {
                    if !has_access {
                        return Error::ModelAccessDenied {
                            model_name: model.clone(),
                            message: format!(
                                "You do not have access to '{}'. Please contact your administrator to request access.",
                                model
                            ),
                        }
                        .into_response();
                    }
                }
            }
        }
    }

    response
}

#[instrument(skip_all)]
pub async fn get_user_id_of_api_key(pool: PgPool, api_key: &str) -> Result<UserId, DbError> {
    let mut conn = pool.acquire().await?;
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    api_keys_repo
        .get_user_id_by_secret(api_key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("API key not found or associated user doesn't exist").into())
}

#[instrument(skip_all)]
pub async fn get_balance_of_api_key(pool: PgPool, api_key: &str) -> Result<Decimal, DbError> {
    // Look up user_id from API key
    let user_id = get_user_id_of_api_key(pool.clone(), api_key).await?;

    debug!("Found user_id for API key: {}", user_id);

    // Query user's current balance
    let mut conn = pool.acquire().await?;
    let mut credits_repo = Credits::new(&mut conn);
    credits_repo.get_user_balance(user_id).await
}

#[instrument(skip_all)]
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
    use crate::{api::models::users::Role, test_utils::create_test_user};
    use rust_decimal::Decimal;

    /// Integration test: Error enrichment middleware enriches 403 with balance info
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_enriches_403_with_balance(pool: PgPool) {
        // Create test user with an API key
        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
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

        // Test that response is 403 and not enriched
        assert_eq!(response.status_code().as_u16(), 403);
        let body = response.text();
        assert!(body.contains("Forbidden"));

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
            })
            .await
            .unwrap();

        // Make request with API key in Authorization header
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", api_key.secret))
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Test that response is 402 and has been enriched
        assert_eq!(response.status_code().as_u16(), 402);
        let body = response.text();
        println!("Enriched response body: {}", body);
        assert!(body.contains("balance too low"));
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
