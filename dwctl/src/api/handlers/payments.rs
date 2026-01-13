//! HTTP handlers for payment processing endpoints.
//!
//! # Payment Flow
//!
//! The payment system supports multiple payment providers (Stripe, PayPal, etc.) through
//! a unified abstraction layer. The flow works as follows:
//!
//! ## 1. Checkout Session Creation
//!
//! **Endpoint**: `POST /admin/api/v1/payments`
//!
//! - User initiates payment from the frontend
//! - Backend creates a checkout session with the configured payment provider
//! - Returns a checkout URL for the frontend to redirect the user to
//! - Requires configured `host_url` in payment config for building redirect URLs
//!
//! ## 2. User Completes Payment
//!
//! - User is redirected to payment provider (e.g., Stripe Checkout)
//! - User completes payment on provider's secure page
//! - Provider redirects user back to success or cancel URL
//!
//! ## 3. Payment Confirmation
//!
//! ### Path A: Webhook (Primary, Automatic)
//!
//! **Endpoint**: `POST /admin/api/v1/webhooks/payments`
//!
//! - Payment provider sends webhook event when payment completes
//! - Backend validates webhook signature
//! - Processes payment and credits user account
//! - Returns 200 OK (even on processing errors to prevent retries)
//!
//! ### Path B: Manual Processing (Fallback)
//!
//! **Endpoint**: `PATCH /admin/api/v1/payments/{session_id}`
//!
//! - Frontend can trigger payment processing manually using session ID
//! - Useful when webhooks fail or for immediate confirmation
//! - Idempotent - safe to call multiple times
//! - Returns 402 if payment not yet completed by provider
//!
//! ## Idempotency
//!
//! Payment processing is idempotent - processing the same session multiple times
//! (via webhooks or manual triggers) will not create duplicate transactions.
//!
//! ## Frontend Integration
//!
//! The frontend payment flow:
//!
//! 1. **Initiate Payment**: Call `POST /admin/api/v1/payments` to get checkout URL
//! 2. **Redirect**: Navigate user to the returned checkout URL (payment provider page)
//! 3. **Handle Return**: Payment provider redirects back with query parameters:
//!    - Success: `?payment=success&session_id={SESSION_ID}`
//!    - Cancelled: `?payment=cancelled&session_id={SESSION_ID}`
//! 4. **Process Payment**: On success, call `PATCH /admin/api/v1/payments/{session_id}`
//!    to confirm and apply payment to account
//! 5. **Show Feedback**: Display appropriate UI based on result:
//!    - Success: "Payment processed successfully"
//!    - Error: "Payment captured but not yet applied. Will update automatically."
//! 6. **Clean URL**: Remove query parameters from URL after processing
//!
//! The frontend should handle errors gracefully - if manual processing fails,
//! the webhook will eventually process the payment automatically.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{AppState, api::models::users::CurrentUser, payment_providers};

#[derive(Debug, Deserialize, Serialize)]
pub struct PaymentQuery {
    pub creditee_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/payments",
    tag = "payments",
    summary = "Create payment",
    description = "Creates a payment checkout session with the payment provider. Returns a JSON object with the checkout URL for the client to handle navigation (better for SPAs). Optionally accepts a creditee_id query parameter to credit another user (admin feature).",
    params(
        ("creditee_id" = Option<String>, Query, description = "Optional user ID to credit (for admin granting credits to another user)")
    ),
    responses(
        (status = 200, description = "Payment session created successfully. Returns JSON with checkout URL.", body = inline(Object)),
        (status = 501, description = "No payment provider configured"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_payment(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Query(query): axum::extract::Query<PaymentQuery>,
) -> Result<Response, StatusCode> {
    // Get payment provider from config (generic - works for any provider)
    let payment_config = match state.config.payment.clone() {
        Some(config) => config,
        None => {
            tracing::warn!("Checkout requested but no payment provider is configured");
            let error_response = Json(json!({
                "message": "Payment processing is currently unavailable. Please contact support."
            }));
            return Ok((StatusCode::SERVICE_UNAVAILABLE, error_response).into_response());
        }
    };

    // Build redirect URLs from configured host URL
    let origin = match payment_config.host_url() {
        Some(configured_host) => configured_host.to_string(),
        None => {
            tracing::error!("No host_url configured in payment config - this is required for payment processing");
            let error_response = Json(json!({
                "message": "Payment processing is currently unavailable. Please contact support."
            }));
            return Ok((StatusCode::SERVICE_UNAVAILABLE, error_response).into_response());
        }
    };

    // Build success/cancel URLs, preserving the user query parameter if present
    let base_path = if let Some(creditee_id) = &query.creditee_id {
        format!("/cost-management?user={}", creditee_id)
    } else {
        "/cost-management".to_string()
    };

    let success_url = format!(
        "{}{}payment=success&session_id={{CHECKOUT_SESSION_ID}}",
        origin,
        if query.creditee_id.is_some() {
            format!("{}&", base_path)
        } else {
            format!("{}?", base_path)
        }
    );
    let cancel_url = format!(
        "{}{}payment=cancelled&session_id={{CHECKOUT_SESSION_ID}}",
        origin,
        if query.creditee_id.is_some() {
            format!("{}&", base_path)
        } else {
            format!("{}?", base_path)
        }
    );

    let provider = payment_providers::create_provider(payment_config);

    // Create checkout session using the provider trait
    let checkout_url = provider
        .create_checkout_session(&state.db, &user, query.creditee_id.as_deref(), &cancel_url, &success_url)
        .await
        .map_err(|e| {
            tracing::error!("Failed to create checkout session: {:?}", e);
            StatusCode::from(e)
        })?;

    // Return the checkout URL as JSON for the frontend to navigate to
    Ok(Json(json!({
        "url": checkout_url
    }))
    .into_response())
}

/// Process a payment
/// This endpoint allows the frontend to trigger payment processing for a specific payment ID.
/// Useful as a fallback when webhooks fail or for immediate payment confirmation.
#[utoipa::path(
    patch,
    path = "/payments/{id}",
    tag = "payments",
    summary = "Process payment",
    description = "Processes a completed payment session and credits the user account. This is idempotent.",
    responses(
        (status = 200, description = "Payment processed successfully"),
        (status = 402, description = "Payment not completed yet"),
        (status = 400, description = "Invalid payment ID or missing data"),
        (status = 501, description = "Payment provider not configured"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn process_payment(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    _user: CurrentUser,
) -> Result<Response, StatusCode> {
    // Get payment provider from config (generic - works for any provider)
    let provider = match state.config.payment.clone() {
        Some(payment_config) => payment_providers::create_provider(payment_config),
        None => {
            tracing::warn!("Payment processing requested but no payment provider is configured");
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "message": "Payment processing is currently unavailable. Please contact support."
                })),
            )
                .into_response());
        }
    };

    // Process the payment session using the provider trait
    match provider.process_payment_session(&state.db, &id).await {
        Ok(()) => Ok(Json(json!({
            "message": "Payment processed successfully"
        }))
        .into_response()),
        Err(e) => match e {
            payment_providers::PaymentError::PaymentNotCompleted => {
                Ok((
                    StatusCode::PAYMENT_REQUIRED,
                    Json(json!({
                        "message": "Payment is still processing. Please check back in a moment."
                    })),
                )
                    .into_response())
            }
            payment_providers::PaymentError::AlreadyProcessed => {
                tracing::trace!("Transaction already processed (idempotent)");
                Ok(Json(json!({
                    "message": "Payment processed successfully"
                }))
                .into_response())
            }
            _ => {
                tracing::error!("Failed to process payment session: {:?}", e);
                Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "message": "Unable to process payment. Please contact support."
                    })),
                )
                    .into_response())
            }
        }
    }
}

/// Generic webhook handler that works with any payment provider
///
/// This endpoint receives webhook events from payment providers and routes them
/// to the appropriate provider implementation for validation and processing.
#[utoipa::path(
    post,
    path = "/webhooks/payments",
    tag = "payments",
    summary = "Payment webhook",
    description = "Receives webhook events from payment providers (Stripe, PayPal, etc.) and processes them.",
    responses(
        (status = 200, description = "Webhook processed successfully"),
        (status = 400, description = "Invalid webhook signature or malformed data"),
        (status = 501, description = "Payment provider not configured or doesn't support webhooks"),
    ),
)]
#[tracing::instrument(skip_all)]
pub async fn webhook_handler(State(state): State<AppState>, headers: axum::http::HeaderMap, body: String) -> StatusCode {
    // Get payment provider from config
    let provider = match state.config.payment.clone() {
        Some(payment_config) => payment_providers::create_provider(payment_config),
        None => {
            tracing::warn!("Webhook received but no payment provider configured");
            return StatusCode::NOT_IMPLEMENTED;
        }
    };

    // Validate the webhook with the provider
    let event = match provider.validate_webhook(&headers, &body).await {
        Ok(Some(event)) => event,
        Ok(None) => {
            tracing::info!("Provider doesn't support webhooks");
            return StatusCode::NOT_IMPLEMENTED;
        }
        Err(e) => {
            tracing::error!("Webhook validation failed: {:?}", e);
            return StatusCode::BAD_REQUEST;
        }
    };

    tracing::trace!("Received webhook event: {}", event.event_type);

    // Process the webhook event
    match provider.process_webhook_event(&state.db, &event).await {
        Ok(()) => {
            tracing::trace!("Successfully processed webhook event: {}", event.event_type);
            StatusCode::OK
        }
        Err(e) => {
            tracing::error!("Failed to process webhook event: {:?}", e);
            // Always return 200 to prevent provider retries for events we've already seen
            StatusCode::OK
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DummyConfig;
    use crate::{config::PaymentConfig, test::utils::create_test_config};
    use axum::Router;
    use axum::routing::{patch, post};
    use axum_test::TestServer;
    use rust_decimal::Decimal;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_dummy_payment_flow(pool: PgPool) {
        // Setup config with dummy payment provider
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            host_url: Some("http://localhost:3001".to_string()),
            amount: Decimal::new(100, 0), // $100
        }));

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        // Create a test user
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let auth_headers = crate::test::utils::add_auth_headers(&user);

        let app = Router::new()
            .route("/payments", post(create_payment))
            .route("/payments/{id}", patch(process_payment))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        // Step 1: Create checkout session
        let mut request = server.post("/payments");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);
        let checkout_response: serde_json::Value = response.json();
        let checkout_url = checkout_response["url"].as_str().unwrap();

        // Verify URL contains session_id
        assert!(checkout_url.contains("session_id="));
        assert!(checkout_url.contains("payment=success"));

        // Extract session_id from URL
        let url = url::Url::parse(checkout_url).unwrap();
        let query_pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        let session_id = query_pairs.get("session_id").unwrap();

        // Step 2: Verify NO transaction was created yet (matches real payment flow)
        let count_before = sqlx::query!(
            r#"
            SELECT COUNT(*) as count
            FROM credits_transactions
            WHERE source_id = $1
            "#,
            session_id.to_string()
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count_before.count.unwrap(), 0, "Transaction should not exist before processing");

        // Step 3: Process payment to create transaction
        let mut request = server.patch(&format!("/payments/{}", session_id));
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);
        let process_response: serde_json::Value = response.json();
        assert_eq!(process_response["message"], "Payment processed successfully");

        // Step 4: Verify transaction was created
        let transaction = sqlx::query!(
            r#"
            SELECT amount, user_id, source_id
            FROM credits_transactions
            WHERE source_id = $1
            "#,
            session_id.to_string()
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(transaction.amount, Decimal::new(100, 0));
        assert_eq!(transaction.user_id, user.id);

        // Step 5: Process again to verify idempotency
        let mut request = server.patch(&format!("/payments/{}", session_id));
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);

        // Step 6: Verify no duplicate transactions
        let count = sqlx::query!(
            r#"
            SELECT COUNT(*) as count
            FROM credits_transactions
            WHERE source_id = $1
            "#,
            session_id.to_string()
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(count.count.unwrap(), 1, "Should only have one transaction (idempotent)");
    }

    #[sqlx::test]
    async fn test_payment_no_provider_configured(pool: PgPool) {
        // Setup config WITHOUT payment provider
        let config = create_test_config();

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let auth_headers = crate::test::utils::add_auth_headers(&user);

        let app = Router::new().route("/payments", post(create_payment)).with_state(state);

        let server = TestServer::new(app).unwrap();

        let mut request = server.post("/payments");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        let error_response: serde_json::Value = response.json();
        assert!(error_response["message"].as_str().unwrap().contains("unavailable"));
    }

    #[sqlx::test]
    async fn test_payment_no_host_url(pool: PgPool) {
        // Setup config with dummy provider but NO host_url
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            host_url: None,
            amount: Decimal::new(50, 0),
        }));

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let auth_headers = crate::test::utils::add_auth_headers(&user);

        let app = Router::new().route("/payments", post(create_payment)).with_state(state);

        let server = TestServer::new(app).unwrap();

        let mut request = server.post("/payments");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        let error_response: serde_json::Value = response.json();
        assert!(error_response["message"].as_str().unwrap().contains("unavailable"));
    }

    #[sqlx::test]
    async fn test_payment_with_creditee_id(pool: PgPool) {
        // Test that creditee_id query parameter works
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            host_url: Some("http://localhost:3001".to_string()),
            amount: Decimal::new(100, 0),
        }));

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let state = AppState::builder()
            .db(crate::db::DbPools::new(pool.clone()))
            .config(config)
            .request_manager(request_manager)
            .build();

        let payer = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let recipient = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let auth_headers = crate::test::utils::add_auth_headers(&payer);

        let app = Router::new()
            .route("/payments", post(create_payment))
            .route("/payments/{id}", patch(process_payment))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        // Create checkout session with creditee_id query param
        let mut request = server.post(&format!("/payments?creditee_id={}", recipient.id));
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);
        let checkout_response: serde_json::Value = response.json();
        let checkout_url = checkout_response["url"].as_str().unwrap();

        // Verify URL contains session_id with recipient's ID (not payer's)
        assert!(checkout_url.contains("session_id="));
        assert!(checkout_url.contains(&format!("dummy_session_{}", recipient.id)));

        // Verify URL contains the user query parameter to return to filtered view
        assert!(
            checkout_url.contains(&format!("user={}", recipient.id)),
            "Redirect URL should preserve user filter: {}",
            checkout_url
        );
        assert!(
            checkout_url.contains("payment=success"),
            "Redirect URL should contain payment status: {}",
            checkout_url
        );
    }
}
