//! Payment provider abstraction layer
//!
//! This module defines the `PaymentProvider` trait which abstracts payment processing
//! functionality across different payment providers (Stripe, PayPal, etc.).

use async_trait::async_trait;
use axum::http::StatusCode;
use rust_decimal::Decimal;
use sqlx::PgPool;

use crate::{api::models::users::CurrentUser, config::PaymentConfig};

pub mod dummy;
pub mod stripe;

/// Create a payment provider from configuration
///
/// This is the single point where we convert config into provider instances.
/// Adding a new provider requires adding a match arm here.
pub fn create_provider(config: PaymentConfig) -> Box<dyn PaymentProvider> {
    match config {
        PaymentConfig::Stripe(stripe_config) => {
            Box::new(stripe::StripeProvider::new(
                stripe_config.api_key,
                stripe_config.price_id,
                stripe_config.webhook_secret,
            ))
        }
        PaymentConfig::Dummy(dummy_config) => {
            let amount = dummy_config.amount.unwrap_or(Decimal::new(50, 0));
            Box::new(dummy::DummyProvider::new(amount))
        }
        // Future providers:
        // PaymentConfig::PayPal(paypal_config) => {
        //     Box::new(paypal::PayPalProvider::new(...))
        // }
    }
}

/// Result type for payment provider operations
pub type Result<T> = std::result::Result<T, PaymentError>;

/// Errors that can occur during payment processing
#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("Payment provider API error: {0}")]
    ProviderApi(String),

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Payment not completed yet")]
    PaymentNotCompleted,

    #[error("Invalid payment data: {0}")]
    InvalidData(String),

    #[error("Payment already processed")]
    AlreadyProcessed,
}

impl From<PaymentError> for StatusCode {
    fn from(err: PaymentError) -> Self {
        match err {
            PaymentError::PaymentNotCompleted => StatusCode::PAYMENT_REQUIRED,
            PaymentError::InvalidData(_) => StatusCode::BAD_REQUEST,
            PaymentError::AlreadyProcessed => StatusCode::OK,
            PaymentError::ProviderApi(_) | PaymentError::Database(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

/// Represents a completed payment session
#[derive(Debug, Clone)]
pub struct PaymentSession {
    /// Unique identifier for this payment session
    pub id: String,
    /// User ID associated with this payment
    pub user_id: String,
    /// Amount paid (in dollars)
    pub amount: Decimal,
    /// Whether the payment has been completed
    pub is_paid: bool,
    /// Provider-specific customer ID (optional)
    pub customer_id: Option<String>,
}

/// Represents a webhook event from a payment provider
#[derive(Debug, Clone)]
pub struct WebhookEvent {
    /// Type of event (e.g., "checkout.session.completed")
    pub event_type: String,
    /// Session ID associated with this event, if applicable
    pub session_id: Option<String>,
    /// Raw event data for provider-specific processing
    pub raw_data: serde_json::Value,
}

/// Abstract payment provider interface
///
/// Implementors provide payment processing capabilities for different providers
/// (Stripe, PayPal, Square, etc.)
#[async_trait]
pub trait PaymentProvider: Send + Sync {
    /// Create a new checkout session
    ///
    /// Returns a URL that the user should be redirected to for payment.
    async fn create_checkout_session(
        &self,
        db_pool: &PgPool,
        user: &CurrentUser,
        cancel_url: &str,
        success_url: &str,
    ) -> Result<String>;

    /// Retrieve and validate a payment session
    ///
    /// Fetches the payment session from the provider and returns validated details.
    async fn get_payment_session(
        &self,
        session_id: &str,
    ) -> Result<PaymentSession>;

    /// Process a completed payment session
    ///
    /// This is idempotent - calling multiple times with the same session_id
    /// should not create duplicate transactions.
    async fn process_payment_session(
        &self,
        db_pool: &PgPool,
        session_id: &str,
    ) -> Result<()>;

    /// Validate and extract webhook event from raw request data
    ///
    /// Returns None if this provider doesn't support webhooks.
    /// Returns Err if validation fails (invalid signature, malformed data, etc.)
    async fn validate_webhook(
        &self,
        headers: &axum::http::HeaderMap,
        body: &str,
    ) -> Result<Option<WebhookEvent>>;

    /// Process a validated webhook event
    ///
    /// This is called after validate_webhook succeeds.
    /// Should be idempotent - processing the same event multiple times should be safe.
    async fn process_webhook_event(
        &self,
        db_pool: &PgPool,
        event: &WebhookEvent,
    ) -> Result<()>;

}
