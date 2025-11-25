//! HTTP handlers for payment processing endpoints.

use axum::{
    Json,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::{AppState, api::models::users::CurrentUser, payment_providers};

#[utoipa::path(
    post,
    path = "/payments",
    tag = "payments",
    summary = "Create payment",
    description = "Creates a payment checkout session with the payment provider. Returns a JSON object with the checkout URL for the client to handle navigation (better for SPAs).",
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
        Some(configured_host) => {
            configured_host.to_string()
        }
        None => {
            tracing::error!("No host_url configured in payment config - this is required for payment processing");
            let error_response = Json(json!({
                "message": "Payment processing is currently unavailable. Please contact support."
            }));
            return Ok((StatusCode::SERVICE_UNAVAILABLE, error_response).into_response());
        }
    };

    let success_url = format!("{}/cost-management?payment=success&session_id={{CHECKOUT_SESSION_ID}}", origin);
    let cancel_url = format!("{}/cost-management?payment=cancelled&session_id={{CHECKOUT_SESSION_ID}}", origin);

    let provider = payment_providers::create_provider(payment_config);

    // Create checkout session using the provider trait
    let checkout_url = provider
        .create_checkout_session(&state.db, &user, &cancel_url, &success_url)
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
        Err(e) => {
            tracing::error!("Failed to process payment session: {:?}", e);
            let status = StatusCode::from(&e);
            if status == StatusCode::PAYMENT_REQUIRED {
                Ok((
                    StatusCode::PAYMENT_REQUIRED,
                    Json(json!({
                        "message": "Payment is still processing. Please check back in a moment."
                    })),
                )
                    .into_response())
            } else {
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
