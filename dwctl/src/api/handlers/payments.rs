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
    auth::permissions,
    db::{handlers::repository::Repository, handlers::users::Users, models::users::UserUpdateDBRequest},
    payment_providers,
};

/// Resolved billing target for payment operations.
struct BillingTarget {
    id: crate::types::UserId,
    payment_provider_id: Option<String>,
    email: String,
    display_name: Option<String>,
}

/// Resolve the billing target for payment operations.
///
/// In org context: verifies the caller is an org admin/owner, loads the org's
/// payment details, and returns the org as the target.
/// Otherwise: returns the caller's own details.
async fn resolve_billing_target(
    user: &CurrentUser,
    conn: &mut sqlx::PgConnection,
) -> Result<BillingTarget, StatusCode> {
    if let Some(org_id) = user.active_organization {
        let can_manage = permissions::can_manage_org_resource(user, org_id, conn)
            .await
            .map_err(|e| {
                tracing::error!("Failed to check org permissions: {:?}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        if !can_manage {
            return Err(StatusCode::FORBIDDEN);
        }
        let org = Users::new(conn)
            .get_by_id(org_id)
            .await
            .map_err(|e| {
                tracing::error!("Failed to load org user: {:?}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::NOT_FOUND)?;
        Ok(BillingTarget {
            id: org.id,
            payment_provider_id: org.payment_provider_id,
            email: org.email,
            display_name: org.display_name,
        })
    } else {
        Ok(BillingTarget {
            id: user.id,
            payment_provider_id: user.payment_provider_id.clone(),
            email: user.email.clone(),
            display_name: user.display_name.clone(),
        })
    }
}

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
    let config = state.current_config();
    // Get payment provider from config (generic - works for any provider)
    let payment_config = match config.payment.clone() {
        Some(config) => config,
        None => {
            tracing::warn!("Checkout requested but no payment provider is configured");
            let error_response = Json(json!({
                "message": "Payment processing is currently unavailable. Please contact support."
            }));
            return Ok((StatusCode::SERVICE_UNAVAILABLE, error_response).into_response());
        }
    };

    let origin = config.dashboard_url.clone();

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

    // Resolve billing target (org or individual)
    let mut conn = state.db.write().acquire().await.map_err(|e| {
        tracing::error!("Failed to acquire database connection: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let target = resolve_billing_target(&user, &mut conn).await?;
    drop(conn);

    let payer = payment_providers::CheckoutPayer {
        id: target.id,
        email: target.email,
        payment_provider_id: target.payment_provider_id,
    };

    let provider = payment_providers::create_provider(payment_config);

    // Create checkout session using the provider trait
    let checkout_url = provider
        .create_checkout_session(&payer, query.creditee_id.as_deref(), &cancel_url, &success_url)
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
    let config = state.current_config();
    // Get payment provider from config (generic - works for any provider)
    let provider = match config.payment.clone() {
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
    let config = state.current_config();
    // Get payment provider from config
    let provider = match config.payment.clone() {
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
        Err(payment_providers::PaymentError::AlreadyProcessed) => {
            tracing::trace!("Webhook event already processed (idempotent): {}", event.event_type);
            StatusCode::OK
        }
        Err(payment_providers::PaymentError::Database(_)) => {
            // Transient DB errors: return 500 so the payment provider retries
            tracing::error!("Transient database error processing webhook event: {}", event.event_type);
            StatusCode::INTERNAL_SERVER_ERROR
        }
        Err(e) => {
            // Permanent errors (invalid data, etc.): return 200 to prevent infinite retries
            tracing::error!("Failed to process webhook event (non-retryable): {:?}", e);
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
    let config = state.current_config();
    // Get payment provider from config
    let payment_config = match config.payment.clone() {
        Some(config) => config,
        None => {
            tracing::warn!("Billing portal requested but no payment provider is configured");
            let error_response = Json(json!({
                "message": "Billing management is currently unavailable. Please contact support."
            }));
            return Ok((StatusCode::SERVICE_UNAVAILABLE, error_response).into_response());
        }
    };

    // Resolve billing target (org or individual) with permission check
    let mut conn = state.db.write().acquire().await.map_err(|e| {
        tracing::error!("Failed to acquire database connection: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let target = resolve_billing_target(&user, &mut conn).await?;

    let customer_id = target.payment_provider_id.filter(|s| !s.is_empty()).ok_or_else(|| {
        tracing::warn!("Target {} has no payment provider customer ID", target.id);
        StatusCode::BAD_REQUEST
    })?;

    let return_url = format!("{}/cost-management", config.dashboard_url);

    let provider = payment_providers::create_provider(payment_config);

    // Create billing portal session using the provider trait
    let portal_url = provider.create_billing_portal_session(&customer_id, &return_url).await.map_err(|e| {
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
    let config = state.current_config();
    let payment_config = match config.payment.clone() {
        Some(config) => config,
        None => {
            tracing::warn!("Auto top-up checkout requested but no payment provider is configured");
            let error_response = Json(json!({
                "message": "Payment processing is currently unavailable. Please contact support."
            }));
            return Ok((StatusCode::SERVICE_UNAVAILABLE, error_response).into_response());
        }
    };

    let origin = config.dashboard_url.clone();
    let success_url = format!("{}/cost-management?autoTopupId={{CHECKOUT_SESSION_ID}}&autoTopup=true", origin);
    let cancel_url = format!("{}/cost-management?autoTopup=true&autoTopupId=fail", origin);

    let payer = payment_providers::CheckoutPayer {
        id: user.id,
        email: user.email.clone(),
        payment_provider_id: user.payment_provider_id.clone(),
    };

    let provider = payment_providers::create_provider(payment_config);

    let checkout_url = provider
        .create_auto_topup_checkout_session(&payer, &cancel_url, &success_url)
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

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct ProcessAutoTopupRequest {
    /// Balance threshold in dollars that triggers auto top-up.
    pub threshold: f32,
    /// Amount in dollars to top up when threshold is reached.
    pub amount: f32,
    /// Optional monthly spending limit in dollars. Null or omitted means no limit.
    pub monthly_limit: Option<f32>,
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

    if let Some(limit) = body.monthly_limit
        && limit <= 0.0
    {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "message": "Monthly limit must be positive"
            })),
        )
            .into_response());
    }

    // Validate the session with the payment provider
    let config = state.current_config();
    let provider = match config.payment.clone() {
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

    let setup_result = match provider.process_auto_topup_session(state.db.write(), &id).await {
        Ok(result) => result,
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

    // Verify session ownership: the session must belong to the authenticated user
    if let Some(ref session_user_id) = setup_result.user_id
        && session_user_id != &user.id.to_string()
    {
        tracing::warn!(
            authenticated_user = %user.id,
            session_user = %session_user_id,
            "Auto top-up session ownership mismatch"
        );
        return Ok((
            StatusCode::FORBIDDEN,
            Json(json!({
                "message": "This session does not belong to your account."
            })),
        )
            .into_response());
    }

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
        auto_topup_monthly_limit: Some(body.monthly_limit),
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

    // Save the customer ID if the user didn't have one (first-time Stripe customer)
    if let Some(customer_id) = &setup_result.customer_id
        && user.payment_provider_id.is_none()
    {
        let mut users = Users::new(&mut conn);
        if let Err(e) = users.set_payment_provider_id_if_empty(user.id, customer_id).await {
            tracing::warn!(user_id = %user.id, error = %e, "Failed to save customer ID from auto top-up setup");
        }
    }

    Ok(Json(json!({
        "message": "Auto top-up enabled successfully",
        "threshold": body.threshold,
        "amount": body.amount,
        "monthly_limit": body.monthly_limit
    }))
    .into_response())
}

/// Enable auto top-up by checking if a payment method exists
///
/// Smart toggle: checks the payment provider for a default payment method.
/// Creates a customer if one doesn't exist. Returns one of two outcomes:
/// - `has_payment_method: true` — auto top-up enabled directly
/// - `needs_billing_portal: true` — no card on file, redirect to billing portal
#[utoipa::path(
    post,
    path = "/auto-topup/enable",
    tag = "payments",
    summary = "Enable auto top-up",
    description = "Validates threshold/amount, checks for a default payment method with the payment provider, and enables auto top-up if possible. Returns instructions for the frontend on what to do next.",
    request_body(content = ProcessAutoTopupRequest, description = "Auto top-up configuration"),
    responses(
        (status = 200, description = "Result of the enable attempt", body = inline(Object)),
        (status = 400, description = "Invalid threshold or amount"),
        (status = 404, description = "Target organization not found"),
        (status = 503, description = "No payment provider configured"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn enable_auto_topup<P: PoolProvider>(
    State(state): State<AppState<P>>,
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

    if let Some(limit) = body.monthly_limit
        && limit <= 0.0
    {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "message": "Monthly limit must be positive"
            })),
        )
            .into_response());
    }

    let config = state.current_config();
    let provider = match config.payment.clone() {
        Some(payment_config) => payment_providers::create_provider(payment_config),
        None => {
            tracing::warn!("Auto top-up enable requested but no payment provider is configured");
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "message": "Payment processing is currently unavailable. Please contact support."
                })),
            )
                .into_response());
        }
    };

    // Resolve billing target (org or individual) with permission check
    let mut conn = state.db.write().acquire().await.map_err(|e| {
        tracing::error!("Failed to acquire database connection: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let target = resolve_billing_target(&user, &mut conn).await?;

    // Check if target user has a customer ID with the payment provider, create one if not
    let customer_id = match &target.payment_provider_id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => {
            let new_id = provider
                .create_customer(&target.email, target.display_name.as_deref())
                .await
                .map_err(|e| {
                    tracing::error!("Failed to create payment provider customer: {:?}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;

            Users::new(&mut conn)
                .set_payment_provider_id_if_empty(target.id, &new_id)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to save customer ID: {:?}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;

            new_id
        }
    };

    // Check if customer has an address (required for tax calculation)
    match provider.customer_has_address(&customer_id).await {
        Ok(true) => {}
        Ok(false) => {
            return Ok(Json(json!({
                "needs_billing_portal": true,
                "reason": "Customer must have an address on file for tax calculation. Please update your billing details."
            }))
            .into_response());
        }
        Err(e) => {
            tracing::error!("Failed to check customer address: {:?}", e);
            return Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "message": "Failed to verify billing address with payment provider."
                })),
            )
                .into_response());
        }
    }

    // Check if the customer has a default payment method
    match provider.get_default_payment_method(&customer_id).await {
        Ok(Some(_pm_id)) => {
            // Has a payment method — enable auto top-up directly
            let update = UserUpdateDBRequest {
                display_name: None,
                avatar_url: None,
                roles: None,
                password_hash: None,
                batch_notifications_enabled: None,
                low_balance_threshold: None,
                auto_topup_amount: Some(Some(body.amount)),
                auto_topup_threshold: Some(Some(body.threshold)),
                auto_topup_monthly_limit: Some(body.monthly_limit),
            };

            Users::new(&mut conn).update(target.id, &update).await.map_err(|e| {
                tracing::error!("Failed to enable auto top-up: {:?}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

            Ok(Json(json!({
                "has_payment_method": true,
                "threshold": body.threshold,
                "amount": body.amount,
                "monthly_limit": body.monthly_limit
            }))
            .into_response())
        }
        Ok(None) => {
            // Has customer but no default payment method — redirect to billing portal
            Ok(Json(json!({
                "needs_billing_portal": true
            }))
            .into_response())
        }
        Err(e) => {
            tracing::error!("Failed to check payment method: {:?}", e);
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "message": "Failed to check payment method with payment provider."
                })),
            )
                .into_response())
        }
    }
}

/// Disable auto top-up by clearing the threshold, amount, and monthly limit.
///
/// Respects org context: when an active organization is set, disables auto top-up
/// for the org rather than the individual user.
#[utoipa::path(
    post,
    path = "/auto-topup/disable",
    tag = "payments",
    summary = "Disable auto top-up",
    description = "Clears auto top-up configuration for the current user or active organization.",
    responses(
        (status = 200, description = "Auto top-up disabled"),
        (status = 404, description = "Target user not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn disable_auto_topup<P: PoolProvider>(State(state): State<AppState<P>>, user: CurrentUser) -> Result<Response, StatusCode> {
    let mut conn = state.db.write().acquire().await.map_err(|e| {
        tracing::error!("Failed to acquire database connection: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let target = resolve_billing_target(&user, &mut conn).await?;

    let update = UserUpdateDBRequest {
        display_name: None,
        avatar_url: None,
        roles: None,
        password_hash: None,
        batch_notifications_enabled: None,
        low_balance_threshold: None,
        auto_topup_amount: Some(None),
        auto_topup_threshold: Some(None),
        auto_topup_monthly_limit: Some(None),
    };

    Users::new(&mut conn).update(target.id, &update).await.map_err(|e| match e {
        crate::db::errors::DbError::NotFound => StatusCode::NOT_FOUND,
        other => {
            tracing::error!("Failed to disable auto top-up: {:?}", other);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })?;

    Ok(Json(json!({ "message": "Auto top-up disabled" })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DummyConfig;
    use crate::{config::PaymentConfig, test::utils::create_test_config};
    use axum::Router;
    use axum::routing::{patch, post, put};
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
        assert!(url.contains(&format!("customer_id=cus_test_{}", user.id)));
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

    #[sqlx::test]
    async fn test_auto_topup_checkout_success(pool: PgPool) {
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        // User needs a payment_provider_id for auto top-up checkout
        sqlx::query!("UPDATE users SET payment_provider_id = $1 WHERE id = $2", "cus_test_123", user.id)
            .execute(&pool)
            .await
            .unwrap();

        let auth_headers = crate::test::utils::add_auth_headers(&user);

        let app = Router::new()
            .route("/auto-topup/checkout", post(create_auto_topup_checkout))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let mut request = server.post("/auto-topup/checkout");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);
        let body: serde_json::Value = response.json();
        let url = body["url"].as_str().expect("Should contain checkout URL");
        assert!(url.contains("autoTopupId="), "URL should contain autoTopupId param");
        assert!(url.contains("autoTopup=true"), "URL should contain autoTopup param");
    }

    #[sqlx::test]
    async fn test_auto_topup_checkout_no_provider(pool: PgPool) {
        let config = create_test_config();
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let auth_headers = crate::test::utils::add_auth_headers(&user);

        let app = Router::new()
            .route("/auto-topup/checkout", post(create_auto_topup_checkout))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let mut request = server.post("/auto-topup/checkout");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    }

    #[sqlx::test]
    async fn test_process_auto_topup_success(pool: PgPool) {
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let auth_headers = crate::test::utils::add_auth_headers(&user);

        // First create a checkout session to get a valid session ID
        let app = Router::new()
            .route("/auto-topup/checkout", post(create_auto_topup_checkout))
            .route("/auto-topup/{id}", put(process_auto_topup))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        // Get checkout URL
        let mut request = server.post("/auto-topup/checkout");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;
        response.assert_status(StatusCode::OK);

        let body: serde_json::Value = response.json();
        let checkout_url = body["url"].as_str().unwrap();

        // Extract session ID from URL
        let url = url::Url::parse(checkout_url).unwrap();
        let query_pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        let session_id = query_pairs.get("autoTopupId").unwrap();

        // Process auto top-up with threshold and amount
        let mut request = server.put(&format!("/auto-topup/{}", session_id)).json(&serde_json::json!({
            "threshold": 5.0,
            "amount": 25.0
        }));
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);
        let body: serde_json::Value = response.json();
        assert_eq!(body["threshold"], 5.0);
        assert_eq!(body["amount"], 25.0);

        // Verify auto top-up settings saved in DB
        let row = sqlx::query!(
            "SELECT auto_topup_amount, auto_topup_threshold, payment_provider_id FROM users WHERE id = $1",
            user.id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.auto_topup_amount, Some(25.0));
        assert_eq!(row.auto_topup_threshold, Some(5.0));
        assert!(
            row.payment_provider_id.is_some(),
            "Customer ID should be saved for first-time users"
        );
        assert!(
            row.payment_provider_id.unwrap().starts_with("dummy_cus_"),
            "Should be a dummy customer ID"
        );
    }

    #[sqlx::test]
    async fn test_process_auto_topup_invalid_params(pool: PgPool) {
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let auth_headers = crate::test::utils::add_auth_headers(&user);

        let app = Router::new().route("/auto-topup/{id}", put(process_auto_topup)).with_state(state);

        let server = TestServer::new(app).unwrap();

        // Test negative threshold
        let mut request = server.put("/auto-topup/dummy_session_fake").json(&serde_json::json!({
            "threshold": -1.0,
            "amount": 25.0
        }));
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;
        response.assert_status(StatusCode::BAD_REQUEST);

        // Test zero amount
        let mut request = server.put("/auto-topup/dummy_session_fake").json(&serde_json::json!({
            "threshold": 5.0,
            "amount": 0.0
        }));
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;
        response.assert_status(StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_enable_auto_topup_with_payment_method(pool: PgPool) {
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        // Set up a payment provider ID (dummy provider always returns a payment method)
        sqlx::query!("UPDATE users SET payment_provider_id = $1 WHERE id = $2", "cus_test_123", user.id)
            .execute(&pool)
            .await
            .unwrap();

        let auth_headers = crate::test::utils::add_auth_headers(&user);

        let app = Router::new().route("/auto-topup/enable", post(enable_auto_topup)).with_state(state);

        let server = TestServer::new(app).unwrap();

        let mut request = server.post("/auto-topup/enable").json(&serde_json::json!({
            "threshold": 5.0,
            "amount": 25.0
        }));
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);
        let body: serde_json::Value = response.json();
        assert_eq!(body["has_payment_method"], true);
        assert_eq!(body["threshold"], 5.0);
        assert_eq!(body["amount"], 25.0);

        // Verify settings saved in DB
        let row = sqlx::query!("SELECT auto_topup_amount, auto_topup_threshold FROM users WHERE id = $1", user.id)
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(row.auto_topup_amount, Some(25.0));
        assert_eq!(row.auto_topup_threshold, Some(5.0));
    }

    #[sqlx::test]
    async fn test_enable_auto_topup_no_customer(pool: PgPool) {
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        // User has no payment_provider_id — should create customer and return needs_billing_portal
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let auth_headers = crate::test::utils::add_auth_headers(&user);

        let app = Router::new().route("/auto-topup/enable", post(enable_auto_topup)).with_state(state);

        let server = TestServer::new(app).unwrap();

        let mut request = server.post("/auto-topup/enable").json(&serde_json::json!({
            "threshold": 5.0,
            "amount": 25.0
        }));
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);
        let body: serde_json::Value = response.json();
        // Dummy provider creates a customer and always returns a payment method,
        // so auto top-up is enabled directly
        assert_eq!(body["has_payment_method"], true);

        // Verify customer was created and saved
        let row = sqlx::query!("SELECT payment_provider_id FROM users WHERE id = $1", user.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(row.payment_provider_id.is_some(), "Customer ID should be saved");
    }

    #[sqlx::test]
    async fn test_enable_auto_topup_in_org_context(pool: PgPool) {
        let mut config = create_test_config();
        config.payment = Some(PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, user.id).await;

        // Give the org a payment provider ID
        sqlx::query!("UPDATE users SET payment_provider_id = $1 WHERE id = $2", "cus_org_123", org.id)
            .execute(&pool)
            .await
            .unwrap();

        let mut auth_headers = crate::test::utils::add_auth_headers(&user);
        auth_headers.push(("x-organization-id".to_string(), org.id.to_string()));

        let app = Router::new().route("/auto-topup/enable", post(enable_auto_topup)).with_state(state);

        let server = TestServer::new(app).unwrap();

        let mut request = server.post("/auto-topup/enable").json(&serde_json::json!({
            "threshold": 10.0,
            "amount": 50.0,
            "monthly_limit": 200.0
        }));
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);
        let body: serde_json::Value = response.json();
        assert_eq!(body["has_payment_method"], true);

        // Verify settings saved on the ORG, not the individual
        let org_row = sqlx::query!(
            "SELECT auto_topup_amount, auto_topup_threshold, auto_topup_monthly_limit FROM users WHERE id = $1",
            org.id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(org_row.auto_topup_amount, Some(50.0));
        assert_eq!(org_row.auto_topup_threshold, Some(10.0));
        assert_eq!(org_row.auto_topup_monthly_limit, Some(200.0));

        // Verify individual user was NOT modified
        let user_row = sqlx::query!("SELECT auto_topup_amount, auto_topup_threshold FROM users WHERE id = $1", user.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(user_row.auto_topup_amount, None);
        assert_eq!(user_row.auto_topup_threshold, None);
    }

    #[sqlx::test]
    async fn test_disable_auto_topup(pool: PgPool) {
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), create_test_config()).await;

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        // Set up auto-topup fields directly in DB
        sqlx::query!(
            "UPDATE users SET auto_topup_amount = 25.0, auto_topup_threshold = 5.0, auto_topup_monthly_limit = 100.0 WHERE id = $1",
            user.id
        )
        .execute(&pool)
        .await
        .unwrap();

        let auth_headers = crate::test::utils::add_auth_headers(&user);

        let app = Router::new()
            .route("/auto-topup/disable", post(disable_auto_topup))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let mut request = server.post("/auto-topup/disable");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);

        // Verify fields cleared
        let row = sqlx::query!(
            "SELECT auto_topup_amount, auto_topup_threshold, auto_topup_monthly_limit FROM users WHERE id = $1",
            user.id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.auto_topup_amount, None);
        assert_eq!(row.auto_topup_threshold, None);
        assert_eq!(row.auto_topup_monthly_limit, None);
    }

    #[sqlx::test]
    async fn test_disable_auto_topup_in_org_context(pool: PgPool) {
        let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), create_test_config()).await;

        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, user.id).await;

        // Set up auto-topup on the org
        sqlx::query!(
            "UPDATE users SET auto_topup_amount = 50.0, auto_topup_threshold = 10.0 WHERE id = $1",
            org.id
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut auth_headers = crate::test::utils::add_auth_headers(&user);
        auth_headers.push(("x-organization-id".to_string(), org.id.to_string()));

        let app = Router::new()
            .route("/auto-topup/disable", post(disable_auto_topup))
            .with_state(state);

        let server = TestServer::new(app).unwrap();

        let mut request = server.post("/auto-topup/disable");
        for (key, value) in &auth_headers {
            request = request.add_header(key.as_str(), value.as_str());
        }
        let response = request.await;

        response.assert_status(StatusCode::OK);

        // Verify org's auto-topup cleared
        let org_row = sqlx::query!("SELECT auto_topup_amount, auto_topup_threshold FROM users WHERE id = $1", org.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(org_row.auto_topup_amount, None);
        assert_eq!(org_row.auto_topup_threshold, None);

        // Verify individual user was NOT touched
        let user_row = sqlx::query!("SELECT auto_topup_amount, auto_topup_threshold FROM users WHERE id = $1", user.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(user_row.auto_topup_amount, None);
        assert_eq!(user_row.auto_topup_threshold, None);
    }
}
