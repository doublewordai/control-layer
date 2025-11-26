//! Stripe payment provider implementation

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::PgPool;
use stripe::{
    CheckoutSession, CheckoutSessionCustomerCreation, CheckoutSessionMode, CheckoutSessionPaymentStatus, CheckoutSessionUiMode, Client,
    CreateCheckoutSession, CreateCheckoutSessionAutomaticTax, CreateCheckoutSessionLineItems,
};

use crate::{
    api::models::users::CurrentUser,
    db::{
        handlers::credits::Credits,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    },
    payment_providers::{PaymentError, PaymentProvider, PaymentSession, Result, WebhookEvent},
    types::UserId,
};

/// Stripe payment provider
pub struct StripeProvider {
    api_key: String,
    price_id: String,
    webhook_secret: String,
}

impl StripeProvider {
    /// Create a new Stripe provider
    pub fn new(api_key: String, price_id: String, webhook_secret: String) -> Self {
        Self {
            api_key,
            price_id,
            webhook_secret,
        }
    }

    /// Get a Stripe client
    fn client(&self) -> Client {
        Client::new(&self.api_key)
    }
}

#[async_trait]
impl PaymentProvider for StripeProvider {
    async fn create_checkout_session(&self, db_pool: &PgPool, user: &CurrentUser, cancel_url: &str, success_url: &str) -> Result<String> {
        let client = self.client();

        // Build checkout session parameters
        let mut checkout_params = CreateCheckoutSession {
            cancel_url: Some(cancel_url),
            success_url: Some(success_url),
            client_reference_id: Some(&user.id.to_string()),
            currency: Some(stripe::Currency::USD),
            line_items: Some(vec![CreateCheckoutSessionLineItems {
                price: Some(self.price_id.clone()),
                quantity: Some(1),
                ..Default::default()
            }]),
            automatic_tax: Some(CreateCheckoutSessionAutomaticTax {
                enabled: true,
                ..Default::default()
            }),
            mode: Some(CheckoutSessionMode::Payment),
            ui_mode: Some(CheckoutSessionUiMode::Hosted),
            customer_creation: Some(CheckoutSessionCustomerCreation::Always),
            expand: &["line_items"],
            ..Default::default()
        };

        // Include existing customer ID if we have one
        if let Some(existing_id) = &user.payment_provider_id {
            tracing::info!("Using existing Stripe customer ID {} for user {}", existing_id, user.id);
            checkout_params.customer = Some(existing_id.as_str().parse().unwrap());
        } else {
            tracing::info!("No customer ID found for user {}, Stripe will create one", user.id);
            // Provide customer email for the new customer
            checkout_params.customer_email = Some(&user.email);
        }

        // Create checkout session
        let checkout_session = CheckoutSession::create(&client, checkout_params).await.map_err(|e| {
            tracing::error!("Failed to create Stripe checkout session: {:?}", e);
            PaymentError::ProviderApi(e.to_string())
        })?;

        tracing::info!("Created checkout session {} for user {}", checkout_session.id, user.id);

        // If we didn't have a customer ID before, save the newly created one
        if user.payment_provider_id.is_none() && checkout_session.customer.is_some() {
            let customer_id = checkout_session.customer.as_ref().unwrap().id().to_string();
            tracing::trace!("Saving newly created customer ID {} for user {}", customer_id, user.id);

            sqlx::query!("UPDATE users SET payment_provider_id = $1 WHERE id = $2", customer_id, user.id)
                .execute(db_pool)
                .await?;
        }

        // Return checkout URL for hosted checkout
        checkout_session.url.ok_or_else(|| {
            tracing::error!("Checkout session missing URL");
            PaymentError::ProviderApi("Checkout session missing URL".to_string())
        })
    }

    async fn get_payment_session(&self, session_id: &str) -> Result<PaymentSession> {
        let client = self.client();

        let session_id: stripe::CheckoutSessionId = session_id
            .parse()
            .map_err(|_| PaymentError::InvalidData("Invalid Stripe session ID".to_string()))?;

        // Retrieve full checkout session with line items
        let checkout_session = CheckoutSession::retrieve(&client, &session_id, &["line_items"])
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe checkout session: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })?;

        // Extract user ID from client_reference_id
        let user_id = checkout_session.client_reference_id.ok_or_else(|| {
            tracing::error!("Checkout session missing client_reference_id");
            PaymentError::InvalidData("Missing client_reference_id".to_string())
        })?;

        // Get price from line_items or amount_total
        let price = checkout_session
            .line_items
            .and_then(|items| items.data.first().map(|item| item.amount_total))
            .or(checkout_session.amount_total)
            .ok_or_else(|| {
                tracing::error!("Checkout session missing both line_items and amount_total");
                PaymentError::InvalidData("Missing payment amount".to_string())
            })?
            / 100; // Convert cents to dollars

        Ok(PaymentSession {
            user_id,
            amount: Decimal::from(price),
            is_paid: checkout_session.payment_status == CheckoutSessionPaymentStatus::Paid,
        })
    }

    async fn process_payment_session(&self, db_pool: &PgPool, session_id: &str) -> Result<()> {
        // Fast path: Check if we've already processed this payment
        // This avoids expensive Stripe API calls for duplicate webhook deliveries,
        // user retries, etc. The unique constraint below handles race conditions.
        let existing = sqlx::query!(
            r#"
            SELECT id FROM credits_transactions
            WHERE source_id = $1
            LIMIT 1
            "#,
            session_id
        )
        .fetch_optional(db_pool)
        .await?;

        if existing.is_some() {
            tracing::trace!("Transaction for session_id {} already exists, skipping (fast path)", session_id);
            return Ok(());
        }

        // Get payment session details
        let payment_session = self.get_payment_session(session_id).await?;

        // Verify payment status
        if !payment_session.is_paid {
            tracing::trace!("Transaction for session_id {} has not been paid, skipping.", session_id);
            return Err(PaymentError::PaymentNotCompleted);
        }

        // Create the credit transaction
        let mut conn = db_pool.acquire().await?;
        let mut credits = Credits::new(&mut conn);

        let user_id: UserId = payment_session.user_id.parse().map_err(|e| {
            tracing::error!("Failed to parse user ID: {:?}", e);
            PaymentError::InvalidData(format!("Invalid user ID: {}", e))
        })?;

        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Purchase,
            amount: payment_session.amount,
            source_id: session_id.to_string(),
            description: Some("Stripe payment".to_string()),
        };

        match credits.create_transaction(&request).await {
            Ok(_) => {
                tracing::info!("Successfully fulfilled checkout session {} for user {}", session_id, user_id);
                Ok(())
            }
            Err(crate::db::errors::DbError::UniqueViolation { constraint, .. }) => {
                // Check if this is a unique constraint violation on source_id
                // This can happen if two replicas try to process the same payment simultaneously
                if constraint.as_deref() == Some("credits_transactions_source_id_unique") {
                    tracing::trace!(
                        "Transaction for session_id {} already processed (caught unique constraint violation), returning success (idempotent)",
                        session_id
                    );
                    Ok(())
                } else {
                    tracing::error!("Unexpected unique constraint violation: {:?}", constraint);
                    Err(PaymentError::Database(sqlx::Error::RowNotFound))
                }
            }
            Err(e) => {
                tracing::error!("Failed to create credit transaction: {:?}", e);
                Err(PaymentError::Database(sqlx::Error::RowNotFound))
            }
        }
    }

    async fn validate_webhook(&self, headers: &axum::http::HeaderMap, body: &str) -> Result<Option<WebhookEvent>> {
        // Get the Stripe signature from headers
        let signature = headers
            .get("stripe-signature")
            .ok_or_else(|| {
                tracing::error!("Missing stripe-signature header");
                PaymentError::InvalidData("Missing stripe-signature header".to_string())
            })?
            .to_str()
            .map_err(|e| {
                tracing::error!("Invalid stripe-signature header: {:?}", e);
                PaymentError::InvalidData("Invalid stripe-signature header".to_string())
            })?;

        // Validate the webhook signature and construct the event
        let event = stripe::Webhook::construct_event(body, signature, &self.webhook_secret).map_err(|e| {
            tracing::error!("Failed to construct webhook event: {:?}", e);
            PaymentError::InvalidData(format!("Webhook validation failed: {}", e))
        })?;

        tracing::trace!("Validated Stripe webhook event: {:?}", event.type_);

        // Convert Stripe event to our generic WebhookEvent
        let session_id = match &event.data.object {
            stripe::EventObject::CheckoutSession(session) => Some(session.id.to_string()),
            _ => None,
        };

        let webhook_event = WebhookEvent {
            event_type: format!("{:?}", event.type_),
            session_id,
        };

        Ok(Some(webhook_event))
    }

    async fn process_webhook_event(&self, db_pool: &PgPool, event: &WebhookEvent) -> Result<()> {
        // Only process checkout session completion events
        if event.event_type != "CheckoutSessionCompleted" && event.event_type != "CheckoutSessionAsyncPaymentSucceeded" {
            tracing::debug!("Ignoring webhook event type: {}", event.event_type);
            return Ok(());
        }

        // Extract session ID
        let session_id = event.session_id.as_ref().ok_or_else(|| {
            tracing::error!("Webhook event missing session_id");
            PaymentError::InvalidData("Missing session_id in webhook event".to_string())
        })?;

        tracing::trace!("Processing webhook event {} for session: {}", event.event_type, session_id);

        // Use the existing process_payment_session method
        self.process_payment_session(db_pool, session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use sqlx::PgPool;
    use uuid::Uuid;

    /// Helper to create a test user in the database
    async fn create_test_user(pool: &PgPool) -> Uuid {
        let user = crate::test_utils::create_test_user(pool, crate::api::models::users::Role::StandardUser).await;
        user.id
    }

    #[test]
    fn test_stripe_provider_creation() {
        let provider = StripeProvider::new("sk_test_fake".to_string(), "price_fake".to_string(), "whsec_fake".to_string());

        assert_eq!(provider.api_key, "sk_test_fake");
        assert_eq!(provider.price_id, "price_fake");
        assert_eq!(provider.webhook_secret, "whsec_fake");
    }

    #[sqlx::test]
    async fn test_stripe_idempotency_fast_path(pool: PgPool) {
        // Test the fast path: transaction already exists in DB
        let user_id = create_test_user(&pool).await;
        let session_id = "cs_test_fake_session_123";

        // Create a transaction using the Credits repository (handles balance_after properly)
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = crate::db::handlers::Credits::new(&mut conn);

        let request = crate::db::models::credits::CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: crate::db::models::credits::CreditTransactionType::Purchase,
            amount: Decimal::new(5000, 2),
            source_id: session_id.to_string(),
            description: Some("Test Stripe payment".to_string()),
        };

        credits.create_transaction(&request).await.unwrap();

        let provider = StripeProvider::new("sk_test_fake".to_string(), "price_fake".to_string(), "whsec_fake".to_string());

        // Process the same session - should hit fast path and succeed
        let result = provider.process_payment_session(&pool, session_id).await;
        assert!(result.is_ok(), "Should succeed via fast path (transaction already exists)");

        // Verify only one transaction exists
        let count = sqlx::query!(
            r#"
            SELECT COUNT(*) as count
            FROM credits_transactions
            WHERE source_id = $1
            "#,
            session_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(count.count.unwrap(), 1, "Should still have exactly one transaction");
    }

    #[test]
    fn test_payment_session_parsing() {
        // Test that PaymentSession structure is correct
        let session = PaymentSession {
            user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            amount: Decimal::new(5000, 2),
            is_paid: true,
        };

        assert_eq!(session.user_id, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(session.amount, Decimal::new(5000, 2));
        assert!(session.is_paid);
    }

    #[test]
    fn test_webhook_event_parsing() {
        // Test WebhookEvent structure
        let event = WebhookEvent {
            event_type: "CheckoutSessionCompleted".to_string(),
            session_id: Some("cs_test_123".to_string()),
        };

        assert_eq!(event.event_type, "CheckoutSessionCompleted");
        assert_eq!(event.session_id, Some("cs_test_123".to_string()));
    }
}
