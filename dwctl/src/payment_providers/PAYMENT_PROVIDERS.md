# Payment Provider Integration Guide

This document explains how the payment provider system works in dwctl, including configuration, architecture, API endpoints, and how to implement new payment providers.

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Configuration](#configuration)
- [Payment Flow](#payment-flow)
- [API Endpoints](#api-endpoints)
- [Implementing a New Provider](#implementing-a-new-provider)
- [Frontend Integration](#frontend-integration)

## Overview

The dwctl payment system provides a flexible abstraction layer for integrating various payment providers (Stripe, PayPal, etc.) to enable users to purchase credits. The system uses a redirect-based checkout flow where users are sent to the payment provider's hosted checkout page, complete payment, and are redirected back to the application.

### Key Features

- **Provider abstraction**: Single trait-based interface for all payment providers
- **Webhook support**: Automatic balance updates via provider webhooks
- **Idempotency**: Prevents duplicate credit transactions
- **Hosted checkout**: Users complete payment on provider's secure page
- **Flexible configuration**: Environment variable-based provider setup

## Architecture

### Component Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                         Frontend (React)                         │
│  - Cost Management page                                          │
│  - Triggers payment flow                                         │
│  - Handles success/cancel redirects                              │
└──────────────────────┬──────────────────────────────────────────┘
                       │
                       │ POST /admin/api/v1/payments
                       │
┌──────────────────────▼──────────────────────────────────────────┐
│                    Payment Handler (Rust)                        │
│  - Creates checkout session                                      │
│  - Returns checkout URL                                          │
└──────────────────────┬──────────────────────────────────────────┘
                       │
                       │ Uses
                       ▼
┌─────────────────────────────────────────────────────────────────┐
│               PaymentProvider Trait (Abstraction)                │
│  - create_checkout_session()                                     │
│  - process_payment_session()                                     │
│  - validate_webhook()                                            │
│  - process_webhook_event()                                       │
└──────────┬──────────────────────────────────────────────────────┘
           │
           ├─── StripeProvider (impl PaymentProvider)
           ├─── DummyProvider (impl PaymentProvider)
           └─── [Your Provider] (impl PaymentProvider)
                       │
                       │ API calls
                       ▼
┌─────────────────────────────────────────────────────────────────┐
│                  External Payment Provider                       │
│  - Stripe / PayPal / etc.                                        │
│  - Hosted checkout page                                          │
│  - Webhook delivery                                              │
└─────────────────────────────────────────────────────────────────┘
```

### File Structure

```
dwctl/
├── src/
│   ├── config.rs                    # Payment provider configuration
│   ├── api/handlers/payments.rs     # HTTP handlers for payment endpoints
│   ├── payment_providers/
│   │   ├── mod.rs                   # PaymentProvider trait definition
│   │   ├── stripe.rs                # Stripe implementation
│   │   └── dummy.rs                 # Dummy/test implementation
│   └── db/models/credits.rs         # Credit transaction models
└── docs/
    └── PAYMENT_PROVIDERS.md         # This document
```

## Configuration

### Backend Configuration

Payment providers are configured via `config.yaml` or environment variables. The configuration is defined in `src/config.rs`.

#### Stripe Configuration

**In `config.yaml`:**

```yaml
payment:
  stripe:
    api_key: "sk_test_..."
    webhook_secret: "whsec_..."
    price_id: "price_..."
    host_url: "https://app.example.com"  # Where users are redirected after payment
```

**Via Environment Variables:**

```bash
DWCTL_PAYMENT__STRIPE__API_KEY="sk_test_..."
DWCTL_PAYMENT__STRIPE__WEBHOOK_SECRET="whsec_..."
DWCTL_PAYMENT__STRIPE__PRICE_ID="price_..."
DWCTL_PAYMENT__STRIPE__HOST_URL="https://app.example.com"
```

#### Dummy Provider (for testing)

```yaml
payment:
  dummy:
    amount: 50.0              # Default amount in dollars
    host_url: "http://localhost:3001"
```

### Configuration Fields

| Field | Required | Description |
|-------|----------|-------------|
| `api_key` | Yes (Stripe) | Payment provider API secret key |
| `webhook_secret` | Yes (Stripe) | Webhook signature verification secret |
| `price_id` | Yes (Stripe) | Product/price ID from payment provider |
| `host_url` | Yes | Base URL for redirect URLs (e.g., `https://app.example.com`) |
| `amount` | No (Dummy) | Default credit amount for dummy provider |

### Why `host_url`?

Previously, the system attempted to read the redirect URL from request headers (`Origin`, `Referer`, `Host`, `X-Forwarded-Proto`). This was unreliable because:

- Headers can be missing or incorrect
- Proxy setups can complicate header values
- Security-conscious browsers may omit certain headers
- Header spoofing attacks

The `host_url` configuration provides a reliable, explicit setting for where users should be redirected after payment.

## Payment Flow

### Complete User Journey

```
┌──────────────────────────────────────────────────────────────────┐
│ 1. User clicks "Add Funds" on Cost Management page               │
└───────────────────────────┬──────────────────────────────────────┘
                            │
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│ 2. Frontend: POST /admin/api/v1/payments                         │
│    - No request body needed (user from auth)                     │
└───────────────────────────┬──────────────────────────────────────┘
                            │
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│ 3. Backend: create_payment() handler                             │
│    - Gets payment config (stripe/dummy)                          │
│    - Determines redirect URLs from config.host_url               │
│    - Calls provider.create_checkout_session()                    │
│    - Returns JSON: { "url": "https://checkout.stripe.com/..." } │
└───────────────────────────┬──────────────────────────────────────┘
                            │
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│ 4. Frontend: Redirects browser to checkout URL                   │
│    window.location.href = response.url                           │
└───────────────────────────┬──────────────────────────────────────┘
                            │
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│ 5. User completes payment on provider's hosted page              │
│    (Stripe/PayPal/etc.)                                          │
└───────────────────────────┬──────────────────────────────────────┘
                            │
                            ├─────────────────────┐
                            │                     │
                            ▼                     ▼
┌───────────────────────────────────┐  ┌────────────────────────────┐
│ 6a. Webhook (Async)               │  │ 6b. User Redirect (Sync)   │
│ POST /admin/api/v1/webhooks/...   │  │ Browser → success_url      │
│ - Provider sends event            │  │ with ?session_id=...       │
│ - validate_webhook()              │  │                            │
│ - process_webhook_event()         │  │                            │
│ - Credits added to account        │  │                            │
└───────────────────────────────────┘  └────────┬───────────────────┘
                                                 │
                                                 ▼
                                    ┌────────────────────────────────┐
                                    │ 7. Frontend: Payment Success   │
                                    │ - Detects ?payment=success     │
                                    │ - Calls PATCH /payments/:id    │
                                    │ - Shows success modal          │
                                    │ - Refreshes balance            │
                                    └────────────────────────────────┘
```

### Redirect URLs

The system constructs two redirect URLs:

1. **Success URL**: `{host_url}/cost-management?payment=success&session_id={CHECKOUT_SESSION_ID}`
2. **Cancel URL**: `{host_url}/cost-management?payment=cancelled&session_id={CHECKOUT_SESSION_ID}`

The `{CHECKOUT_SESSION_ID}` placeholder is replaced by the payment provider with the actual session ID.

### Idempotency

The system ensures idempotent credit transactions through:

1. **Fast path check**: Before making expensive API calls to the provider, check if a transaction with the given `source_id` already exists
2. **Unique constraint**: Database has a unique constraint on `credits_transactions.source_id`
3. **Race condition handling**: If two replicas process the same payment simultaneously, the second one catches the unique constraint violation and returns success

This prevents:
- Duplicate credits from webhook retries
- Double-processing from user refreshing the success page
- Race conditions in multi-instance deployments

## API Endpoints

### 1. Create Payment

Creates a payment checkout session and returns the checkout URL.

**Endpoint**: `POST /admin/api/v1/payments`

**Authentication**: Required (Bearer token, session cookie, or proxy headers)

**Request**: No body required (user extracted from authentication)

**Response**:
```json
{
  "url": "https://checkout.stripe.com/c/pay/cs_test_..."
}
```

**Implementation** (`src/api/handlers/payments.rs`):

```rust
pub async fn create_payment(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    user: CurrentUser,
) -> Result<Response, StatusCode>
```

**Flow**:
1. Get payment config from `state.config.payment`
2. Determine `origin` from `config.host_url()` (or fallback to headers if not configured)
3. Build success/cancel URLs: `{origin}/cost-management?payment=...&session_id={CHECKOUT_SESSION_ID}`
4. Call `provider.create_checkout_session(&db, &user, &cancel_url, &success_url)`
5. Return checkout URL as JSON

### 2. Process Payment

Manually processes a payment session (useful as webhook fallback).

**Endpoint**: `PATCH /admin/api/v1/payments/:id`

**Authentication**: Required

**Parameters**: `:id` - Payment session ID from provider

**Response**:
```json
{
  "success": true,
  "message": "Payment processed successfully"
}
```

**Implementation**:

```rust
pub async fn process_payment(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    _user: CurrentUser,
) -> Result<Response, StatusCode>
```

**Flow**:
1. Get payment provider from config
2. Call `provider.process_payment_session(&db, &session_id)`
3. Provider fetches session details, verifies payment, creates credit transaction
4. Returns success or appropriate error status

### 3. Webhook Handler

Receives and processes webhook events from payment providers.

**Endpoint**: `POST /admin/api/v1/webhooks/payments`

**Authentication**: None (validated via webhook signature)

**Request**: Raw body from payment provider

**Headers**: Provider-specific signature header (e.g., `stripe-signature`)

**Response**: `200 OK` or `400 Bad Request`

**Implementation**:

```rust
pub async fn webhook_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String
) -> StatusCode
```

**Flow**:
1. Get payment provider from config
2. Call `provider.validate_webhook(&headers, &body)`
   - Provider verifies webhook signature
   - Parses event data
   - Returns `WebhookEvent` struct
3. Call `provider.process_webhook_event(&db, &event)`
   - Only processes `checkout.session.completed` type events
   - Extracts session ID
   - Calls `process_payment_session()` to credit user
4. Always returns `200 OK` (even on errors) to prevent webhook retries for already-processed events

## Implementing a New Provider

To add a new payment provider (e.g., PayPal, Square), follow these steps:

### Step 1: Add Configuration

**In `src/config.rs`**, add your provider to the `PaymentConfig` enum:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PaymentConfig {
    Stripe(StripeConfig),
    Dummy(DummyConfig),
    // Add your provider
    Paypal(PaypalConfig),
}

// Define your config struct
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PaypalConfig {
    pub client_id: String,
    pub client_secret: String,
    pub host_url: String,
}

```

### Step 2: Create Provider Implementation

**Create `src/payment_providers/paypal.rs`:**

```rust
use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::PgPool;

use crate::{
    api::models::users::CurrentUser,
    db::{
        handlers::credits::Credits,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    },
    payment_providers::{PaymentError, PaymentProvider, PaymentSession, Result, WebhookEvent},
    types::UserId,
};

pub struct PaypalProvider {
    client_id: String,
    client_secret: String,
}

impl PaypalProvider {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client_id,
            client_secret,
        }
    }

    fn client(&self) -> PaypalClient {
        // Initialize PayPal SDK client
        PaypalClient::new(&self.client_id, &self.client_secret)
    }
}

#[async_trait]
impl PaymentProvider for PaypalProvider {
    async fn create_checkout_session(
        &self,
        db_pool: &PgPool,
        user: &CurrentUser,
        cancel_url: &str,
        success_url: &str,
    ) -> Result<String> {
        let client = self.client();

        // Create PayPal order
        let order = client.create_order(
            amount: "10.00",
            currency: "USD",
            return_url: success_url,
            cancel_url: cancel_url,
            // ... other PayPal parameters
        ).await.map_err(|e| {
            tracing::error!("Failed to create PayPal order: {:?}", e);
            PaymentError::ProviderApi(e.to_string())
        })?;

        // Extract approval URL for user redirect
        let approval_url = order.links
            .iter()
            .find(|link| link.rel == "approve")
            .map(|link| link.href.clone())
            .ok_or_else(|| PaymentError::ProviderApi("No approval URL".to_string()))?;

        Ok(approval_url)
    }

    async fn get_payment_session(&self, session_id: &str) -> Result<PaymentSession> {
        let client = self.client();

        // Retrieve PayPal order details
        let order = client.get_order(session_id).await.map_err(|e| {
            tracing::error!("Failed to retrieve PayPal order: {:?}", e);
            PaymentError::ProviderApi(e.to_string())
        })?;

        // Extract relevant information
        Ok(PaymentSession {
            id: order.id.clone(),
            user_id: order.custom_id.ok_or_else(|| {
                PaymentError::InvalidData("Missing custom_id".to_string())
            })?,
            amount: Decimal::from_str(&order.amount.value)
                .map_err(|e| PaymentError::InvalidData(e.to_string()))?,
            is_paid: order.status == "COMPLETED",
            customer_id: order.payer.payer_id.clone(),
        })
    }

    async fn process_payment_session(&self, db_pool: &PgPool, session_id: &str) -> Result<()> {
        // Fast path: Check if already processed
        let existing = sqlx::query!(
            "SELECT id FROM credits_transactions WHERE source_id = $1",
            session_id
        )
        .fetch_optional(db_pool)
        .await?;

        if existing.is_some() {
            tracing::info!("Transaction {} already processed", session_id);
            return Ok(());
        }

        // Get payment session and verify it's paid
        let payment_session = self.get_payment_session(session_id).await?;
        if !payment_session.is_paid {
            return Err(PaymentError::PaymentNotCompleted);
        }

        // Create credit transaction
        let mut conn = db_pool.acquire().await?;
        let mut credits = Credits::new(&mut conn);

        let user_id: UserId = payment_session.user_id.parse()
            .map_err(|e| PaymentError::InvalidData(format!("Invalid user ID: {}", e)))?;

        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Purchase,
            amount: payment_session.amount,
            source_id: session_id.to_string(),
            description: Some("PayPal payment".to_string()),
        };

        match credits.create_transaction(&request).await {
            Ok(_) => Ok(()),
            Err(crate::db::errors::DbError::UniqueViolation { constraint, .. })
                if constraint.as_deref() == Some("credits_transactions_source_id_unique") =>
            {
                tracing::info!("Transaction {} already processed (unique constraint)", session_id);
                Ok(())
            }
            Err(e) => {
                tracing::error!("Failed to create transaction: {:?}", e);
                Err(PaymentError::Database(sqlx::Error::RowNotFound))
            }
        }
    }

    async fn validate_webhook(
        &self,
        headers: &axum::http::HeaderMap,
        body: &str,
    ) -> Result<Option<WebhookEvent>> {
        // Get PayPal webhook signature headers
        let signature = headers
            .get("paypal-transmission-sig")
            .ok_or_else(|| PaymentError::InvalidData("Missing signature header".to_string()))?
            .to_str()
            .map_err(|_| PaymentError::InvalidData("Invalid signature header".to_string()))?;

        // Verify webhook signature using PayPal SDK
        let client = self.client();
        let verified = client.verify_webhook(body, signature, &self.webhook_id).await
            .map_err(|e| PaymentError::InvalidData(format!("Webhook verification failed: {}", e)))?;

        if !verified {
            return Err(PaymentError::InvalidData("Invalid webhook signature".to_string()));
        }

        // Parse webhook event
        let event: PaypalWebhookEvent = serde_json::from_str(body)
            .map_err(|e| PaymentError::InvalidData(format!("Failed to parse webhook: {}", e)))?;

        Ok(Some(WebhookEvent {
            event_type: event.event_type,
            session_id: Some(event.resource.id),
            raw_data: serde_json::to_value(&event).unwrap_or(serde_json::Value::Null),
        }))
    }

    async fn process_webhook_event(&self, db_pool: &PgPool, event: &WebhookEvent) -> Result<()> {
        // Only process order completion events
        if event.event_type != "CHECKOUT.ORDER.COMPLETED" {
            tracing::debug!("Ignoring webhook event: {}", event.event_type);
            return Ok(());
        }

        let session_id = event.session_id.as_ref()
            .ok_or_else(|| PaymentError::InvalidData("Missing session_id".to_string()))?;

        tracing::info!("Processing PayPal webhook for order: {}", session_id);
        self.process_payment_session(db_pool, session_id).await
    }
}
```

### Step 3: Register Provider

**In `src/payment_providers/mod.rs`:**

```rust
pub mod dummy;
pub mod stripe;
pub mod paypal;  // Add this

pub fn create_provider(config: PaymentConfig) -> Box<dyn PaymentProvider> {
    match config {
        PaymentConfig::Stripe(stripe_config) => Box::new(stripe::StripeProvider::new(
            stripe_config.api_key,
            stripe_config.price_id,
            stripe_config.webhook_secret,
        )),
        PaymentConfig::Dummy(dummy_config) => {
            let amount = dummy_config.amount.unwrap_or(Decimal::new(50, 0));
            Box::new(dummy::DummyProvider::new(amount))
        },
        // Add your provider
        PaymentConfig::Paypal(paypal_config) => Box::new(paypal::PaypalProvider::new(
            paypal_config.client_id,
            paypal_config.client_secret,
        )),
    }
}
```

### Step 4: Configure Provider

**In `config.yaml`:**

```yaml
payment:
  paypal:
    client_id: "your-client-id"
    client_secret: "your-client-secret"
    host_url: "https://app.example.com"
```

### Key Implementation Notes

1. **create_checkout_session**: Must return a URL where users can complete payment
2. **get_payment_session**: Must return session details including payment status
3. **process_payment_session**: Must be idempotent (check for existing transaction first)
4. **validate_webhook**: Must verify webhook signature to prevent spoofing
5. **process_webhook_event**: Should only process completion events
6. **Error handling**: Use `PaymentError` enum variants appropriately
7. **Logging**: Add tracing for debugging and monitoring

## Frontend Integration

### Cost Management Component

The frontend initiates payments through the Cost Management page (`src/components/features/cost-management/CostManagement/CostManagement.tsx`).

**Payment Flow**:

```typescript
const handleAddFunds = async () => {
  if (config?.payment_enabled) {
    try {
      // 1. Call backend to create checkout session
      const data = await createPaymentMutation.mutateAsync();

      // 2. Redirect to payment provider
      if (data.url) {
        window.location.href = data.url;
      }
    } catch (error) {
      toast.error("Failed to initiate payment");
    }
  }
};
```

**Success Handling**:

```typescript
useEffect(() => {
  const urlParams = new URLSearchParams(window.location.search);
  const paymentStatus = urlParams.get("payment");
  const sessionId = urlParams.get("session_id");

  if (paymentStatus === "success" && sessionId) {
    // Show success modal
    setShowSuccessModal(true);

    // Process payment (fallback if webhook hasn't fired yet)
    processPaymentMutation.mutate(sessionId);

    // Clean up URL
    window.history.replaceState({}, "", window.location.pathname);
  }
}, []);
```

### Configuration Check

The frontend checks if payment processing is enabled:

```typescript
const { data: config } = useConfig();
const canAddFunds = config?.payment_enabled;
```

This is set by the backend based on whether `payment` config exists.

## Testing

### Using the Dummy Provider

The dummy provider is useful for testing without real payment integration:

```yaml
payment:
  dummy:
    amount: 10.0
    host_url: "http://localhost:3001"
```

The dummy provider:
- Always succeeds immediately
- Doesn't actually charge money
- Credits the configured amount
- Useful for frontend development and testing

### Testing Real Providers

1. **Set up test environment**: Use provider's test/sandbox mode (e.g., Stripe test keys)
2. **Configure webhooks**: Point provider webhooks to your development server (use ngrok if needed)
3. **Test scenarios**:
   - Successful payment
   - Cancelled payment
   - Webhook delivery
   - Webhook retries
   - Race conditions (multiple webhooks)
   - Session expiration

### Webhook Testing

Use provider CLI tools or services:

**Stripe**:
```bash
stripe listen --forward-to localhost:3001/admin/api/v1/webhooks/payments
stripe trigger checkout.session.completed
```

## Security Considerations

### Webhook Verification

Always verify webhook signatures to prevent:
- Spoofed webhooks granting free credits
- Replay attacks
- Man-in-the-middle attacks

Each provider has its own signature verification method (HMAC, JWT, etc.).

### Source ID Uniqueness

The `source_id` field prevents duplicate credits:
- Must be unique per transaction
- Use provider's transaction/session ID
- Database enforces unique constraint
- Handle unique violations gracefully

### Host URL Configuration

Always set `host_url` in config rather than trusting request headers:
- Prevents header spoofing
- Ensures correct redirect destination
- Works reliably with proxies
- No dependency on browser headers

### Authentication

Payment endpoints require authentication except webhooks:
- Webhooks authenticated via signature verification
- User endpoints require valid session/token
- User can only see their own transactions

## Troubleshooting

### Payment not credited after success

**Symptoms**: User sees success but balance doesn't update

**Causes**:
1. Webhook not configured or failing
2. Network issues preventing webhook delivery
3. Webhook signature mismatch

**Solutions**:
1. Check webhook logs in provider dashboard
2. Verify webhook secret in config
3. User can trigger manual processing via frontend (calls PATCH endpoint)
4. Admin can manually create transaction via API

### Duplicate credits

**Symptoms**: User credited multiple times for same payment

**Causes**:
1. Unique constraint not enforced
2. Using non-unique source_id
3. Database migration issue

**Solutions**:
1. Verify database has unique constraint on `source_id`
2. Check `source_id` is provider's transaction ID, not generated locally
3. Review transaction logs for duplicates

### Checkout URL missing

**Symptoms**: User clicks "Add Funds" but nothing happens

**Causes**:
1. Payment config not set
2. Provider API error
3. Invalid credentials

**Solutions**:
1. Check `config.payment` is configured
2. Verify API keys are correct
3. Check provider dashboard for API errors
4. Review backend logs for error details

## Future Enhancements

Potential improvements to the payment system:

1. **Multiple providers**: Support multiple active providers with user selection
2. **Currency support**: Multi-currency support with automatic conversion
3. **Subscription billing**: Recurring payment support
4. **Usage-based billing**: Automatic top-up when balance is low
5. **Invoice generation**: PDF invoices for purchases
6. **Payment history**: Detailed payment transaction history separate from credits
7. **Refund support**: Automated refund processing
8. **Payment methods**: Support for alternative payment methods (ACH, wire transfer, etc.)

## Additional Resources

- [Stripe Checkout Documentation](https://stripe.com/docs/payments/checkout)
- [Stripe Webhook Testing](https://stripe.com/docs/webhooks/test)
- [PayPal Checkout Integration](https://developer.paypal.com/docs/checkout/)
- [Database Transactions in SQLx](https://github.com/launchbadge/sqlx)
