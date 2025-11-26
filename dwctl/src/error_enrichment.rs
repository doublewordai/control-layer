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
//! 1. **403 Forbidden - Insufficient Credits**: User's balance ≤ 0 for paid models
//!    - Shows current balance

use crate::{
    db::errors::DbError,
    db::handlers::{Credits, api_keys::ApiKeys},
    errors::Error,
};
use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use rust_decimal::Decimal;
use sqlx::PgPool;
use tracing::{debug, instrument};

/// Middleware that enriches error responses from the AI proxy with helpful context
///
/// Currently handles:
/// - 403 Forbidden errors (likely insufficient credits) → enriched with balance
#[instrument(skip_all, fields(path = %request.uri().path(), method = %request.method()))]
pub async fn error_enrichment_middleware(State(pool): State<PgPool>, request: Request<Body>, next: Next) -> Response<Body> {
    // Extract API key from request headers before passing to onwards
    let api_key = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")))
        .map(|token| token.trim().to_string());

    // Let the request proceed through onwards
    let response = next.run(request).await;

    // Only enrich 403 errors when we have an API key
    // Note: This middleware is applied only to the onwards router (AI proxy paths),
    // so no path filtering is needed here
    if response.status() == StatusCode::FORBIDDEN {
        if let Some(key) = api_key {
            debug!("Intercepted 403 response on AI proxy path, attempting enrichment");
            if let Ok(balance) = get_balance_of_api_key(pool, &key).await {
                // Only enrich if balance is negative (user is in debt)
                // If balance is exactly 0, the 403 might be due to model access rather than credits
                if balance < Decimal::ZERO {
                    return Error::InsufficientCredits {
                        current_balance: balance,
                        message: "Account balance too low. Please add credits to continue.".to_string(),
                    }
                    .into_response();
                }
            }
        }
    }

    response
}

#[instrument(skip_all)]
pub async fn get_balance_of_api_key(pool: PgPool, api_key: &str) -> Result<Decimal, DbError> {
    // Look up user_id from API key
    let mut conn = pool.acquire().await?;
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    let user_id = api_keys_repo
        .get_user_id_by_secret(api_key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("API key not found or associated user doesn't exist"))?;

    debug!("Found user_id for API key: {}", user_id);

    // Query user's current balance
    let mut credits_repo = Credits::new(&mut conn);
    credits_repo.get_user_balance(user_id).await
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
