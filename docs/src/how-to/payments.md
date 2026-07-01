# Set Up Payments

> Enable credit purchases via Stripe so users can buy credits through a self-service checkout flow.

This guide shows you how to enable credit purchases via Stripe so users can buy credits through a self-service checkout flow.

## Prerequisites

- A Stripe account with API access
- A Stripe Price configured for credit purchases
- Platform Manager role (for testing)

## Configure Stripe

Add your Stripe credentials to the configuration:

```yaml
payment:
  stripe:
    api_key: "sk_live_..."
    webhook_secret: "whsec_..."
    price_id: "price_..."
    host_url: "https://your-app.example.com"
```

Or via environment variables:

```bash
DWCTL_PAYMENT__STRIPE__API_KEY=sk_live_...
DWCTL_PAYMENT__STRIPE__WEBHOOK_SECRET=whsec_...
DWCTL_PAYMENT__STRIPE__PRICE_ID=price_...
DWCTL_PAYMENT__STRIPE__HOST_URL=https://your-app.example.com
```

### Configuration Options

| Option | Required | Description |
|--------|----------|-------------|
| `api_key` | Yes | Your Stripe secret key (`sk_live_...` or `sk_test_...`) |
| `webhook_secret` | Yes | Webhook signing secret (`whsec_...`) |
| `price_id` | Yes | Stripe Price ID for credit purchases |
| `host_url` | Yes | Your app's URL (for redirect after payment) |
| `enable_invoice_creation` | No | Generate invoices for payments (default: false) |

## Create a Stripe Price

In Stripe Dashboard:

1. Go to **Products** → **Add product**
2. Name it something like "API Credits"
3. Set the price (e.g., \$10.00 for 1000 credits)
4. Copy the **Price ID** (starts with `price_`)

The credit amount users receive is determined by the price amount. For example, if your price is \$10.00, users receive 10 credits (1 credit = \$1.00 by convention).

## Set Up the Webhook

Stripe uses webhooks to notify your application when payments complete. This is more reliable than waiting for users to return to your site.

### 1. Create the webhook in Stripe

1. Go to **Developers** → **Webhooks**
2. Click **Add endpoint**
3. Enter your webhook URL: `https://your-app.example.com/admin/api/v1/webhooks/payments`
4. Select events to listen for:
   - `checkout.session.completed`
5. Click **Add endpoint**
6. Copy the **Signing secret** (starts with `whsec_`)

### 2. Configure the webhook secret

Add the signing secret to your configuration:

```bash
DWCTL_PAYMENT__STRIPE__WEBHOOK_SECRET=whsec_...
```

The webhook secret is used to verify that incoming webhook requests actually came from Stripe.

## Test with Dummy Provider

For development and testing, use the dummy payment provider instead of Stripe:

```yaml
payment:
  dummy:
    amount: 50
    host_url: "http://localhost:3001"
```

Or via environment variables:

```bash
DWCTL_PAYMENT__DUMMY__AMOUNT=50
DWCTL_PAYMENT__DUMMY__HOST_URL=http://localhost:3001
```

The dummy provider:
- Skips actual payment processing
- Immediately grants the configured amount of credits
- Useful for testing the checkout flow without a Stripe account

> **Warning**
>
> Never use the dummy provider in production. It grants credits without collecting payment.

## How Users Purchase Credits

Once configured, users can purchase credits from the dashboard:

1. User navigates to **Cost Management**
2. User clicks **Buy Credits**
3. User is redirected to Stripe's hosted checkout page
4. User enters payment details and completes purchase
5. User is redirected back to your app
6. Credits are added to their balance

The checkout flow is handled entirely by Stripe's pre-built checkout page. You don't need to build any payment UI.

## Admin Credit Grants

Platform Managers can grant credits to users without requiring payment:

1. Go to **Cost Management**
2. Select a user
3. Click **Grant Credits** or **Remove Credits**
4. Enter the amount and confirm

This creates an `AdminGrant` or `AdminRemoval` transaction in the ledger.

Via API:

```bash
curl -X POST "https://your-instance/admin/api/v1/transactions" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user-uuid",
    "transaction_type": "admin_grant",
    "amount": 100,
    "description": "Welcome bonus"
  }'
```

## Initial Credits for New Users

To automatically grant credits to new users on signup, configure:

```yaml
credits:
  initial_credits_for_standard_users: 10
```

Or:

```bash
DWCTL_CREDITS__INITIAL_CREDITS_FOR_STANDARD_USERS=10
```

This creates an `AdminGrant` transaction when a new StandardUser account is created.

## Troubleshooting

### Payments complete but credits don't appear

Check that:
1. The webhook is configured correctly in Stripe
2. The webhook secret matches your configuration
3. The webhook endpoint is accessible from the internet
4. Check server logs for webhook processing errors

### Webhook signature verification fails

The webhook secret must match exactly. Copy it again from Stripe Dashboard → Developers → Webhooks → your endpoint → Signing secret.

### Users see "Payment not completed" error

The frontend may have tried to process the payment before Stripe sent the webhook. This is normal—the webhook will process the payment shortly. Users can refresh the page.

## Related Topics

- [How Billing Works](../conceptual-guides/how-billing-works.md) — Understand credits and transactions
- [Set Up Model Pricing](../how-to/tariffs.md) — Configure per-token pricing
- [Configuration Reference](../reference/configuration.md) — All payment configuration options
