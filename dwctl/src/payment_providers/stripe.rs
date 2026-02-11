//! Stripe payment provider implementation

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashMap;
use stripe::Client;
use stripe_billing::billing_portal_session::CreateBillingPortalSession;
use stripe_checkout::checkout_session::{
    CreateCheckoutSessionCustomerUpdate, CreateCheckoutSessionCustomerUpdateAddress, CreateCheckoutSessionCustomerUpdateName,
    CreateCheckoutSessionInvoiceCreation, CreateCheckoutSessionNameCollection, CreateCheckoutSessionNameCollectionBusiness,
    CreateCheckoutSessionSavedPaymentMethodOptions, CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodRemove,
    CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodSave,
};
use stripe_checkout::{
    CheckoutSessionId, CheckoutSessionMode, CheckoutSessionPaymentStatus, CheckoutSessionUiMode,
    checkout_session::{
        CreateCheckoutSession, CreateCheckoutSessionAutomaticTax, CreateCheckoutSessionCustomerCreation, CreateCheckoutSessionLineItems,
        CreateCheckoutSessionTaxIdCollection, RetrieveCheckoutSession,
    },
};
use stripe_types::Currency;
use stripe_webhook::{EventObject, Webhook};

use crate::{
    api::models::users::CurrentUser,
    db::{
        handlers::{credits::Credits, repository::Repository},
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    },
    payment_providers::{PaymentError, PaymentProvider, PaymentSession, Result, WebhookEvent},
    types::UserId,
};

/// Stripe payment provider
pub struct StripeProvider {
    config: crate::config::StripeConfig,
    client: Client,
}

impl From<crate::config::StripeConfig> for StripeProvider {
    fn from(config: crate::config::StripeConfig) -> Self {
        crate::crypto::ensure_crypto_provider();
        let client = Client::new(&config.api_key);
        Self { config, client }
    }
}

#[async_trait]
impl PaymentProvider for StripeProvider {
    async fn create_checkout_session(
        &self,
        user: &CurrentUser,
        creditee_id: Option<&str>,
        cancel_url: &str,
        success_url: &str,
    ) -> Result<String> {
        let mut checkout_params = CreateCheckoutSession::new()
            .cancel_url(cancel_url)
            .success_url(success_url)
            .client_reference_id(user.id.to_string()) // This is who will purchase the credits
            .currency(Currency::USD)
            .line_items(vec![CreateCheckoutSessionLineItems {
                price: Some(self.config.price_id.clone()),
                quantity: Some(1),
                ..Default::default()
            }])
            .automatic_tax(CreateCheckoutSessionAutomaticTax::new(true))
            .mode(CheckoutSessionMode::Payment)
            .ui_mode(CheckoutSessionUiMode::Hosted)
            .expand(vec!["line_items".to_string()])
            .tax_id_collection(CreateCheckoutSessionTaxIdCollection::new(true))
            .name_collection(CreateCheckoutSessionNameCollection {
                business: Some(CreateCheckoutSessionNameCollectionBusiness::new(true)),
                individual: None,
            })
            .saved_payment_method_options(CreateCheckoutSessionSavedPaymentMethodOptions {
                allow_redisplay_filters: None,
                payment_method_save: Some(CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodSave::Enabled),
                payment_method_remove: Some(CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodRemove::Enabled),
            });

        if let Some(user_receiving_credits) = creditee_id {
            let mut metadata = HashMap::new();
            metadata.insert("creditee_id".to_string(), user_receiving_credits.to_string());
            checkout_params = checkout_params.metadata(metadata);
        }

        // Enable invoice creation if configured
        if self.config.enable_invoice_creation {
            checkout_params = checkout_params.invoice_creation(CreateCheckoutSessionInvoiceCreation::new(true));
        }

        // Include existing customer ID if we have one
        if let Some(existing_id) = &user.payment_provider_id {
            // This is who is giving the credits
            tracing::debug!("Using existing Stripe customer ID {} for user {}", existing_id, user.id);
            checkout_params = checkout_params
                .customer(existing_id.as_str())
                .customer_update(CreateCheckoutSessionCustomerUpdate {
                    address: Some(CreateCheckoutSessionCustomerUpdateAddress::Auto),
                    name: Some(CreateCheckoutSessionCustomerUpdateName::Auto),
                    shipping: None,
                })
        } else {
            tracing::debug!("No customer ID found for user {}, Stripe will create one", user.id);
            // Provide customer email for the new customer
            checkout_params = checkout_params
                .customer_email(&user.email)
                .customer_creation(CreateCheckoutSessionCustomerCreation::Always);
        }

        // Create checkout session
        let checkout_session = checkout_params.send(&self.client).await.map_err(|e| {
            tracing::error!("Failed to create Stripe checkout session: {:?}", e);
            PaymentError::ProviderApi(e.to_string())
        })?;

        tracing::info!(
            "Created checkout session {} for user {} (payer: {})",
            checkout_session.id,
            creditee_id.unwrap_or(&user.id.to_string()),
            user.id
        );

        // Return checkout URL for hosted checkout
        checkout_session.url.ok_or_else(|| {
            tracing::error!("Checkout session missing URL");
            PaymentError::ProviderApi("Checkout session missing URL".to_string())
        })
    }

    async fn get_payment_session(&self, session_id: &str) -> Result<PaymentSession> {
        let session_id: CheckoutSessionId = session_id
            .parse()
            .map_err(|_| PaymentError::InvalidData("Invalid Stripe session ID".to_string()))?;

        // Retrieve full checkout session with line items
        let checkout_session = RetrieveCheckoutSession::new(session_id)
            .expand(vec!["line_items".to_string()])
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe checkout session: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })?;

        // Parse creditor ID from client_reference_id
        let creditor_id: UserId = checkout_session
            .client_reference_id
            .ok_or_else(|| {
                tracing::error!("Checkout session missing client_reference_id");
                PaymentError::InvalidData("Missing client_reference_id".to_string())
            })?
            .parse()
            .map_err(|e| {
                tracing::error!("Failed to parse creditor ID: {:?}", e);
                PaymentError::InvalidData(format!("Invalid creditor user ID: {}", e))
            })?;

        // Parse creditee ID from metadata, or use creditor_id if not present (self-payment)
        let creditee_id: UserId = checkout_session
            .metadata
            .as_ref()
            .and_then(|m| m.get("creditee_id"))
            .map(|s| s.parse())
            .transpose()
            .map_err(|e| {
                tracing::error!("Failed to parse creditee ID: {:?}", e);
                PaymentError::InvalidData(format!("Invalid creditee user ID: {}", e))
            })?
            .unwrap_or(creditor_id);

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
            creditee_id,
            amount: Decimal::from(price),
            is_paid: checkout_session.payment_status == CheckoutSessionPaymentStatus::Paid,
            creditor_id,
            payment_provider_id: checkout_session.customer.as_ref().map(|c| c.id().to_string()),
        })
    }

    async fn process_payment_session(&self, db_pool: &PgPool, session_id: &str) -> Result<()> {
        // Acquire connection early for idempotency check
        let mut conn = db_pool.acquire().await?;

        // Fast path: Check if we've already processed this payment
        // This avoids expensive Stripe API calls for duplicate webhook deliveries,
        // user retries, etc. The unique constraint below handles race conditions.
        {
            let mut credits = Credits::new(&mut conn);
            if credits.transaction_exists_by_source_id(session_id).await? {
                tracing::trace!("Transaction for session_id {} already exists, skipping (fast path)", session_id);
                return Ok(());
            }
        }

        // Get payment session details
        let payment_session = self.get_payment_session(session_id).await?;

        // Verify payment status
        if !payment_session.is_paid {
            tracing::trace!("Transaction for session_id {} has not been paid, skipping.", session_id);
            return Err(PaymentError::PaymentNotCompleted);
        }

        // Look up creditor user and build description + set creditor stripe ID in db.
        // This is one block to scope user repo lifetime properly
        let description = {
            let mut users = crate::db::handlers::users::Users::new(&mut conn);

            // Verify creditor user exists before proceeding
            let creditor_user = users.get_by_id(payment_session.creditor_id).await?;
            if creditor_user.is_none() {
                tracing::error!(
                    "Creditor user {} not found for payment session {}. This indicates a data integrity issue.",
                    payment_session.creditor_id,
                    session_id
                );
            }

            // Build description with payer information
            let description = if payment_session.creditor_id == payment_session.creditee_id {
                // Self-payment
                "Stripe payment".to_string()
            } else if let Some(creditor) = creditor_user.as_ref() {
                let creditor_name = creditor.display_name.as_ref().unwrap_or(&creditor.email);
                format!("Stripe payment from {}", creditor_name)
            } else {
                "Stripe payment".to_string()
            };

            // Save the customer ID if we don't have one yet, so we can offer the billing portal
            if let Some(ref provider_id) = payment_session.payment_provider_id
                && users
                    .set_payment_provider_id_if_empty(payment_session.creditor_id, provider_id)
                    .await?
            {
                tracing::info!(
                    "Saved newly created stripe ID {} for user ID {}",
                    provider_id,
                    payment_session.creditor_id
                );
            }

            description
        };

        // Create the credit transaction
        let request = CreditTransactionCreateDBRequest {
            user_id: payment_session.creditee_id,
            transaction_type: CreditTransactionType::Purchase,
            amount: payment_session.amount,
            source_id: session_id.to_string(),
            description: Some(description),
            fusillade_batch_id: None,
        };

        let mut credits = Credits::new(&mut conn);
        credits.create_transaction(&request).await?;

        tracing::info!(
            "Successfully fulfilled checkout session {} for user {}",
            session_id,
            payment_session.creditee_id
        );
        Ok(())
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
        let event = Webhook::construct_event(body, signature, &self.config.webhook_secret).map_err(|e| {
            tracing::error!("Failed to construct webhook event: {:?}", e);
            PaymentError::InvalidData(format!("Webhook validation failed: {}", e))
        })?;

        tracing::trace!("Validated Stripe webhook event: {:?}", event.type_);

        // Convert Stripe event to our generic WebhookEvent
        let session_id = match &event.data.object {
            EventObject::CheckoutSessionCompleted(session) | EventObject::CheckoutSessionAsyncPaymentSucceeded(session) => {
                Some(session.id.to_string())
            }
            _ => None,
        };

        let webhook_event = WebhookEvent {
            event_type: event.type_.to_string(),
            session_id,
        };

        Ok(Some(webhook_event))
    }

    async fn process_webhook_event(&self, db_pool: &PgPool, event: &WebhookEvent) -> Result<()> {
        // Only process checkout session completion events
        if event.event_type != "checkout.session.completed" && event.event_type != "checkout.session.async_payment_succeeded" {
            tracing::error!("Unexpected webhook received of type: {}", event.event_type); // Stripe should be configured to only send the two types above
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

    async fn create_billing_portal_session(&self, user: &CurrentUser, return_url: &str) -> Result<String> {
        // Fetch user's payment provider customer ID from user struct
        let customer_id_str = user.payment_provider_id.as_ref().ok_or(PaymentError::NoCustomerId)?;

        // Create billing portal session using builder pattern
        let session = CreateBillingPortalSession::new()
            .customer(customer_id_str.as_str())
            .return_url(return_url)
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to create Stripe billing portal session: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })?;

        tracing::debug!(
            "Created billing portal session {} for user {} (customer: {})",
            session.id,
            user.id,
            customer_id_str
        );

        // Return the portal session URL
        Ok(session.url)
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
        let user = crate::test::utils::create_test_user(pool, crate::api::models::users::Role::StandardUser).await;
        user.id
    }

    #[test]
    fn test_stripe_provider_from_config() {
        let config = crate::config::StripeConfig {
            api_key: "sk_test_fake".to_string(),
            price_id: "price_fake".to_string(),
            webhook_secret: "whsec_fake".to_string(),
            enable_invoice_creation: false,
        };
        let provider = StripeProvider::from(config);

        assert_eq!(provider.config.api_key, "sk_test_fake");
        assert_eq!(provider.config.price_id, "price_fake");
        assert_eq!(provider.config.webhook_secret, "whsec_fake");
        assert!(!provider.config.enable_invoice_creation);
    }

    #[test]
    fn test_stripe_provider_with_invoice_creation() {
        let config = crate::config::StripeConfig {
            api_key: "sk_test_fake".to_string(),
            price_id: "price_fake".to_string(),
            webhook_secret: "whsec_fake".to_string(),
            enable_invoice_creation: true,
        };
        let provider = StripeProvider::from(config);

        assert!(provider.config.enable_invoice_creation);
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
            fusillade_batch_id: None,
        };

        credits.create_transaction(&request).await.unwrap();

        let config = crate::config::StripeConfig {
            api_key: "sk_test_fake".to_string(),
            price_id: "price_fake".to_string(),
            webhook_secret: "whsec_fake".to_string(),
            enable_invoice_creation: false,
        };
        let provider = StripeProvider::from(config);

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
        let creditee_id = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        let creditor_id = "550e8400-e29b-41d4-a716-446655440001".parse().unwrap();

        let session = PaymentSession {
            creditee_id,
            creditor_id,
            amount: Decimal::new(5000, 2),
            is_paid: true,
            payment_provider_id: Some("cus_test123".to_string()), // Stripe customer ID
        };

        assert_eq!(session.creditee_id, creditee_id);
        assert_eq!(session.creditor_id, creditor_id);
        assert_eq!(session.amount, Decimal::new(5000, 2));
        assert!(session.is_paid);
        assert_eq!(session.payment_provider_id, Some("cus_test123".to_string()));
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

    #[sqlx::test]
    async fn test_payment_description_self(pool: PgPool) {
        // Test that when a user pays for themselves, description is just "Stripe payment"
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        // Set a Stripe customer ID for the user
        let customer_id = "cus_test_self_payment";
        sqlx::query!("UPDATE users SET payment_provider_id = $1 WHERE id = $2", customer_id, user.id)
            .execute(&pool)
            .await
            .unwrap();

        // Create a payment session where payer = recipient (self-payment)
        let payment_session = PaymentSession {
            creditee_id: user.id,
            creditor_id: user.id,
            amount: Decimal::new(5000, 2),
            is_paid: true,
            payment_provider_id: Some(customer_id.to_string()),
        };

        // Build description using the new logic (creditor_id comparison)
        let description = if payment_session.creditor_id == payment_session.creditee_id {
            "Stripe payment".to_string()
        } else {
            let mut conn = pool.acquire().await.unwrap();
            let mut users = crate::db::handlers::users::Users::new(&mut conn);

            if let Some(creditor) = users.get_by_id(payment_session.creditor_id).await.unwrap() {
                let creditor_name = creditor.display_name.unwrap_or(creditor.email);
                format!("Stripe payment from {}", creditor_name)
            } else {
                "Stripe payment".to_string()
            }
        };

        assert_eq!(description, "Stripe payment", "Self-payment should not include 'from' attribution");
    }

    #[sqlx::test]
    async fn test_payment_description_other(pool: PgPool) {
        // Test that when a user pays for someone else, description includes "from {name}"
        let payer = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let recipient = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        // Set a Stripe customer ID for the payer
        let customer_id = "cus_test_other_payment";
        sqlx::query!(
            "UPDATE users SET payment_provider_id = $1, display_name = $2 WHERE id = $3",
            customer_id,
            "John Admin",
            payer.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create a payment session where payer != recipient
        let payment_session = PaymentSession {
            creditee_id: recipient.id,
            creditor_id: payer.id,
            amount: Decimal::new(5000, 2),
            is_paid: true,
            payment_provider_id: Some(customer_id.to_string()),
        };

        // Build description using the new logic (creditor_id comparison)
        let description = if payment_session.creditor_id == payment_session.creditee_id {
            "Stripe payment".to_string()
        } else {
            let mut conn = pool.acquire().await.unwrap();
            let mut users = crate::db::handlers::users::Users::new(&mut conn);

            if let Some(creditor) = users.get_by_id(payment_session.creditor_id).await.unwrap() {
                let creditor_name = creditor.display_name.unwrap_or(creditor.email);
                format!("Stripe payment from {}", creditor_name)
            } else {
                "Stripe payment".to_string()
            }
        };

        assert_eq!(
            description, "Stripe payment from John Admin",
            "Payment for others should include 'from' attribution"
        );
    }
}
