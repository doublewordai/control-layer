//! HTTP handlers for payment processing endpoints.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde_json::json;

use crate::{api::models::users::CurrentUser, AppState};

/// Payment processor types
#[derive(Debug, Clone, PartialEq)]
pub enum PaymentProcessor {
    Stripe,
    // Future: PayPal, Square, etc.
}

impl PaymentProcessor {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "stripe" => Some(PaymentProcessor::Stripe),
            _ => None,
        }
    }
}

/// Create a Stripe checkout session
async fn create_stripe_checkout(
    _state: AppState,
    _user: CurrentUser,
) -> Result<Response, StatusCode> {
    // TODO: Implement actual Stripe checkout session creation
    // For now, this is a shell that would redirect to Stripe
    tracing::info!("Creating Stripe checkout session");

    // In a real implementation, you would:
    // 1. Create a Stripe checkout session via their API
    // 2. Get the session URL
    // 3. Redirect to that URL

    // Placeholder redirect to Stripe's example checkout
    Ok(Redirect::to("https://checkout.stripe.com/example").into_response())
}

/// Fallback when no payment provider is configured
async fn no_payment_provider_configured() -> Result<Response, StatusCode> {
    tracing::warn!("Checkout requested but no payment provider is configured");

    let error_response = Json(json!({
        "error": "No payment provider configured",
        "message": "Sorry, there's no payment provider setup. Please contact support."
    }));

    Ok((StatusCode::NOT_IMPLEMENTED, error_response).into_response())
}

#[utoipa::path(
    post,
    path = "/create_checkout",
    tag = "payments",
    summary = "Create checkout session",
    description = "Creates a checkout session and redirects to the payment provider",
    responses(
        (status = 303, description = "Redirect to payment provider checkout page"),
        (status = 501, description = "No payment provider configured"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_checkout(
    State(state): State<AppState>,
    user: CurrentUser,
) -> Result<Response, StatusCode> {
    // Determine which payment processor to use from config
    let payment_processor = state
        .config
        .metadata
        .payment_processor
        .as_ref()
        .and_then(|s| PaymentProcessor::from_str(s));

    // Dispatch to the appropriate payment provider handler
    match payment_processor {
        Some(PaymentProcessor::Stripe) => create_stripe_checkout(state, user).await,
        None => no_payment_provider_configured().await,
    }
}
