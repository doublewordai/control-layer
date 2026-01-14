//! Payment provider abstraction layer
//!
//! This module defines the `PaymentProvider` trait which abstracts payment processing
//! functionality across different payment providers (Stripe, PayPal, etc.).

use async_trait::async_trait;
use axum::http::StatusCode;
use rust_decimal::Decimal;
use sqlx::PgPool;

use crate::{UserId, api::models::users::CurrentUser, config::PaymentConfig};

pub mod dummy;
pub mod stripe;

/// Create a payment provider from configuration
///
/// This is the single point where we convert config into provider instances.
/// Adding a new provider requires adding a match arm here.
pub fn create_provider(config: PaymentConfig) -> Box<dyn PaymentProvider> {
    match config {
        PaymentConfig::Stripe(stripe_config) => Box::new(stripe::StripeProvider::from(stripe_config)),
        PaymentConfig::Dummy(dummy_config) => Box::new(dummy::DummyProvider::from(dummy_config)),
        // Future providers:
        // PaymentConfig::PayPal(paypal_config) => {
        //     Box::new(paypal::PayPalProvider::from(paypal_config))
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

    #[error("User does not have a payment provider customer ID")]
    NoCustomerId,
}

impl From<PaymentError> for StatusCode {
    fn from(err: PaymentError) -> Self {
        match err {
            PaymentError::PaymentNotCompleted => StatusCode::PAYMENT_REQUIRED,
            PaymentError::InvalidData(_) | PaymentError::NoCustomerId => StatusCode::BAD_REQUEST,
            PaymentError::AlreadyProcessed => StatusCode::OK,
            PaymentError::ProviderApi(_) | PaymentError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<crate::db::errors::DbError> for PaymentError {
    fn from(err: crate::db::errors::DbError) -> Self {
        match err {
            // Handle the specific case of duplicate source_id as AlreadyProcessed for idempotency
            crate::db::errors::DbError::UniqueViolation { constraint, .. }
                if constraint.as_deref() == Some("credits_transactions_source_id_unique") =>
            {
                PaymentError::AlreadyProcessed
            }
            // Convert all other DbError cases through anyhow to sqlx::Error
            _ => {
                // DbError has an Other variant that contains anyhow::Error
                // We can wrap it as a generic database error
                PaymentError::InvalidData(format!("Database error: {}", err))
            }
        }
    }
}

/// Represents a completed payment session
#[derive(Debug, Clone)]
pub struct PaymentSession {
    /// Local User ID for the creditee
    pub creditee_id: UserId,
    /// Amount paid (in dollars)
    pub amount: Decimal,
    /// Whether the payment has been completed
    pub is_paid: bool,
    /// Local User ID for the creditor (person who paid)
    pub creditor_id: UserId,
    /// Optional: Payment provider ID for the creditor
    pub payment_provider_id: Option<String>,
}

/// Represents a webhook event from a payment provider
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEvent {
    /// Type of event (e.g., "checkout.session.completed")
    pub event_type: String,
    /// Session ID associated with this event, if applicable
    pub session_id: Option<String>,
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
    ///
    /// # Arguments
    /// * `db_pool` - Database connection pool
    /// * `user` - The authenticated user making the payment
    /// * `creditee_id` - Optional user ID to credit (for admin granting credits to another user)
    /// * `cancel_url` - URL to redirect to if payment is cancelled
    /// * `success_url` - URL to redirect to if payment succeeds
    async fn create_checkout_session(
        &self,
        user: &CurrentUser,
        creditee_id: Option<&str>,
        cancel_url: &str,
        success_url: &str,
    ) -> Result<String>;

    /// Retrieve and validate a payment session
    ///
    /// Fetches the payment session from the provider and returns validated details.
    async fn get_payment_session(&self, session_id: &str) -> Result<PaymentSession>;

    /// Process a completed payment session
    ///
    /// This is idempotent - calling multiple times with the same session_id
    /// should not create duplicate transactions.
    async fn process_payment_session(&self, db_pool: &PgPool, session_id: &str) -> Result<()>;

    /// Validate and extract webhook event from raw request data
    ///
    /// Returns None if this provider doesn't support webhooks.
    /// Returns Err if validation fails (invalid signature, malformed data, etc.)
    async fn validate_webhook(&self, headers: &axum::http::HeaderMap, body: &str) -> Result<Option<WebhookEvent>>;

    /// Process a validated webhook event
    ///
    /// This is called after validate_webhook succeeds.
    /// Should be idempotent - processing the same event multiple times should be safe.
    async fn process_webhook_event(&self, db_pool: &PgPool, event: &WebhookEvent) -> Result<()>;

    /// Create a billing portal session for customer self-service
    ///
    /// Returns a URL that the user should be redirected to for managing their billing.
    ///
    /// # Arguments
    /// * `user` - The authenticated user requesting portal access
    /// * `return_url` - The complete URL to redirect to after the customer is done (e.g., "https://example.com/cost-management")
    async fn create_billing_portal_session(&self, user: &CurrentUser, return_url: &str) -> Result<String>;
}
