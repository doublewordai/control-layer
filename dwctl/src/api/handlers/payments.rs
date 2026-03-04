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
//! - Uses `dashboard_url` from top-level config for building redirect URLs
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
use sqlx_pool_router::PoolProvider;

use crate::{
    AppState,
    api::models::users::CurrentUser,
    db::{handlers::repository::Repository, handlers::users::Users, models::users::UserUpdateDBRequest},
    payment_providers,
};

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
pub async fn create_payment<P: PoolProvider>(
    State(state): State<AppState<P>>,
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

    let origin = state.config.dashboard_url.clone();

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
        .create_checkout_session(&user, query.creditee_id.as_deref(), &cancel_url, &success_url)
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
pub async fn process_payment<P: PoolProvider>(
    State(state): State<AppState<P>>,
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
    match provider.process_payment_session(state.db.write(), &id).await {
        Ok(()) => Ok(Json(json!({
            "message": "Payment processed successfully"
        }))
        .into_response()),
        Err(e) => match e {
            payment_providers::PaymentError::PaymentNotCompleted => Ok((
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "message": "Payment is still processing. Please check back in a moment."
                })),
            )
                .into_response()),
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
        },
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
pub async fn webhook_handler<P: PoolProvider>(
    State(state): State<AppState<P>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> StatusCode {
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
    match provider.process_webhook_event(state.db.write(), &event).await {
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

/// Create a billing portal session for customer self-service
///
/// This endpoint creates a billing portal session URL for customers to manage
/// their billing details, view invoices, and update payment methods.
#[utoipa::path(
    post,
    path = "/billing-portal",
    tag = "payments",
    summary = "Create billing portal session",
    description = "Creates a billing portal session for the authenticated user. Requires the user to have a payment_provider_id (customer ID) set. The return URL is automatically constructed from the configured dashboard_url.",
    responses(
        (status = 200, description = "Billing portal session created successfully. Returns JSON with portal URL.", body = inline(Object)),
        (status = 400, description = "User does not have a payment provider customer ID"),
        (status = 503, description = "No payment provider configured"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_billing_portal_session<P: PoolProvider>(
    State(state): State<AppState<P>>,
    user: CurrentUser,
) -> Result<Response, StatusCode> {
    // Get payment provider from config
    let payment_config = match state.config.payment.clone() {
        Some(config) => config,
        None => {
            tracing::warn!("Billing portal requested but no payment provider is configured");
            let error_response = Json(json!({
                "message": "Billing management is currently unavailable. Please contact support."
            }));
            return Ok((StatusCode::SERVICE_UNAVAILABLE, error_response).into_response());
        }
    };

    let return_url = format!("{}/cost-management", state.config.dashboard_url);

    let provider = payment_providers::create_provider(payment_config);

    // Create billing portal session using the provider trait
    let portal_url = provider.create_billing_portal_session(&user, &return_url).await.map_err(|e| {
        tracing::error!("Failed to create billing portal session: {:?}", e);
        StatusCode::from(e)
    })?;

    // Return the portal URL as JSON for the frontend to navigate to
    Ok(Json(json!({
        "url": portal_url
    }))
    .into_response())
}

/// Create a checkout session for auto top-up setup
///
/// Creates a checkout session with the payment provider for setting up
/// auto top-up. Returns the checkout URL for the frontend to redirect to.
#[utoipa::path(
    post,
    path = "/auto-topup/checkout",
    tag = "payments",
    summary = "Create auto top-up checkout session",
    description = "Creates a checkout session for auto top-up setup. The user must have a payment provider customer ID.",
    responses(
        (status = 200, description = "Checkout session created. Returns JSON with checkout URL.", body = inline(Object)),
        (status = 400, description = "User does not have a payment provider customer ID"),
        (status = 503, description = "No payment provider configured"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_auto_topup_checkout<P: PoolProvider>(
    State(state): State<AppState<P>>,
    user: CurrentUser,
) -> Result<Response, StatusCode> {
    let payment_config = match state.config.payment.clone() {
        Some(config) => config,
        None => {
            tracing::warn!("Auto top-up checkout requested but no payment provider is configured");
            let error_response = Json(json!({
                "message": "Payment processing is currently unavailable. Please contact support."
            }));
            return Ok((StatusCode::SERVICE_UNAVAILABLE, error_response).into_response());
        }
    };

    let origin = state.config.dashboard_url.clone();
    let success_url = format!("{}/cost-management?autoTopupId={{CHECKOUT_SESSION_ID}}&autoTopup=true", origin);
    let cancel_url = format!("{}/cost-management?autoTopup=true&autoTopupId=fail", origin);

    let provider = payment_providers::create_provider(payment_config);

    let checkout_url = provider
        .create_auto_topup_checkout_session(&user, &cancel_url, &success_url)
        .await
        .map_err(|e| {
            tracing::error!("Failed to create auto top-up checkout session: {:?}", e);
            StatusCode::from(e)
        })?;

    Ok(Json(json!({
        "url": checkout_url
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
pub struct ProcessAutoTopupRequest {
    /// Balance threshold in dollars that triggers auto top-up.
    pub threshold: f32,
    /// Amount in dollars to top up when threshold is reached.
    pub amount: f32,
}

/// Enable auto top-up for the current user
///
/// Validates the checkout session with the payment provider, then enables
/// auto top-up at the specified threshold. Works like `PATCH /payments/{id}`
/// but instead of creating a credit transaction, it enables auto top-up.
#[utoipa::path(
    put,
    path = "/auto-topup/{id}",
    tag = "payments",
    summary = "Enable auto top-up",
    description = "Validates a checkout session with the payment provider and enables auto top-up for the current user at the specified threshold and amount.",
    params(
        ("id" = String, Path, description = "Checkout session ID from the payment provider")
    ),
    request_body(content = inline(Object), description = "Auto top-up configuration"),
    responses(
        (status = 200, description = "Auto top-up enabled successfully"),
        (status = 400, description = "Invalid session or threshold"),
        (status = 402, description = "Session not completed yet"),
        (status = 503, description = "No payment provider configured"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn process_auto_topup<P: PoolProvider>(
    State(state): State<AppState<P>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    user: CurrentUser,
    Json(body): Json<ProcessAutoTopupRequest>,
) -> Result<Response, StatusCode> {
    if body.threshold < 0.0 || body.amount <= 0.0 {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "message": "Threshold must be non-negative and amount must be positive"
            })),
        )
            .into_response());
    }

    // Validate the session with the payment provider
    let provider = match state.config.payment.clone() {
        Some(payment_config) => payment_providers::create_provider(payment_config),
        None => {
            tracing::warn!("Auto top-up requested but no payment provider is configured");
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "message": "Payment processing is currently unavailable. Please contact support."
                })),
            )
                .into_response());
        }
    };

    let payment_method_id = match provider.process_auto_topup_session(state.db.write(), &id).await {
        Ok(pm_id) => pm_id,
        Err(e) => match e {
            payment_providers::PaymentError::PaymentNotCompleted => {
                return Ok((
                    StatusCode::PAYMENT_REQUIRED,
                    Json(json!({
                        "message": "Session is still processing. Please check back in a moment."
                    })),
                )
                    .into_response());
            }
            _ => {
                tracing::error!("Failed to validate auto top-up session: {:?}", e);
                return Ok((
                    StatusCode::from(e),
                    Json(json!({
                        "message": "Failed to validate session with payment provider."
                    })),
                )
                    .into_response());
            }
        },
    };

    // Session validated - enable auto top-up
    let update = UserUpdateDBRequest {
        display_name: None,
        avatar_url: None,
        roles: None,
        password_hash: None,
        batch_notifications_enabled: None,
        low_balance_threshold: None,
        auto_topup_amount: Some(Some(body.amount)),
        auto_topup_threshold: Some(Some(body.threshold)),
        auto_topup_payment_id: Some(Some(payment_method_id)),
    };

    let mut conn = state.db.write().acquire().await.map_err(|e| {
        tracing::error!("Failed to acquire database connection: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut users = Users::new(&mut conn);
    users.update(user.id, &update).await.map_err(|e| {
        tracing::error!("Failed to enable auto top-up: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(json!({
        "message": "Auto top-up enabled successfully",
        "threshold": body.threshold,
        "amount": body.amount
    }))
    .into_response())
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
            amount: Decimal::new(100, 0), // $100
        }));

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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
            amount: Decimal::new(100, 0),
        }));

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

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

    #[sqlx::test]
    async fn test_billing_portal_success(pool: PgPool) {
        // Setup config with dummy provider
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        // Build AppState
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Create test user
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        // Set payment provider customer ID in database
        let customer_id = format!("cus_test_{}", user.id);
        sqlx::query("UPDATE users SET payment_provider_id = $1 WHERE id = $2")
            .bind(&customer_id)
            .bind(user.id)
            .execute(&pool)
            .await
            .unwrap();

        let auth_headers = crate::test::utils::add_auth_headers(&user);

        // Setup router with handler
        let app = Router::new()
            .route("/billing-portal", post(create_billing_portal_session))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        // Make request
        let mut request = server.post("/billing-portal");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        // Assert response
        response.assert_status(StatusCode::OK);
        let body: serde_json::Value = response.json();

        // Verify URL structure
        let url = body["url"].as_str().expect("Response should contain url field");
        assert!(url.starts_with("http://localhost:3001/cost-management"));
        assert!(url.contains("dummy_billing_portal=true"));
        assert!(url.contains(&format!("user_id={}", user.id)));
    }

    #[sqlx::test]
    async fn test_billing_portal_no_customer_id(pool: PgPool) {
        // Setup config with dummy provider
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        // Build AppState
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Create test user WITHOUT payment_provider_id (default is null)
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        let auth_headers = crate::test::utils::add_auth_headers(&user);

        // Setup router with handler
        let app = Router::new()
            .route("/billing-portal", post(create_billing_portal_session))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        // Make request
        let mut request = server.post("/billing-portal");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        // Assert 400 Bad Request because user doesn't have customer ID
        response.assert_status(StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_billing_portal_no_provider_configured(pool: PgPool) {
        // Setup config WITHOUT payment provider
        let config = create_test_config();
        // config.payment is None by default

        // Build AppState
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // Create test user
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        // Set payment provider customer ID in database
        let customer_id = format!("cus_test_{}", user.id);
        sqlx::query("UPDATE users SET payment_provider_id = $1 WHERE id = $2")
            .bind(&customer_id)
            .bind(user.id)
            .execute(&pool)
            .await
            .unwrap();

        let auth_headers = crate::test::utils::add_auth_headers(&user);

        // Setup router with handler
        let app = Router::new()
            .route("/billing-portal", post(create_billing_portal_session))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        // Make request
        let mut request = server.post("/billing-portal");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        // Assert 503 Service Unavailable because no payment provider configured
        response.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    }
}
