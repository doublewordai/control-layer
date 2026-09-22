# How Billing Works

> Understand the credits-based billing system: how charges are calculated, what happens at zero balance, and how payments work.

The Control Layer uses a **credits-based billing system**. Users have a credit balance that gets charged when they make API requests. When the balance runs out, API access stops until more credits are purchased or granted.

This page explains how the billing system works conceptually. For setup instructions, see [Set Up Model Pricing](../how-to/tariffs.md) and [Set Up Payments](../how-to/payments.md).

## Credits

Credits are the currency of the Control Layer. Every user has a credit balance, and API usage deducts from this balance.

**Key characteristics:**

- Credits are stored as decimal values (up to 8 decimal places)
- Balances can go negative (the last request that pushes you below zero still completes)
- New users can receive initial credits automatically via configuration
- Admins can grant or remove credits manually

Credits aren't tied to any real currency by default. You decide what a credit is worth when you configure your tariffs and payment amounts.

## How Charges Are Calculated

When a user makes an API request, the charge is calculated from two things:

1. **Token usage** — How many tokens were consumed (input and output separately)
2. **Tariff** — The price per token for that model

The formula is:

```
charge = (input_tokens × input_price) + (output_tokens × output_price)
```

For example, if a model's tariff is \$0.03 per 1K input tokens and \$0.06 per 1K output tokens, a request using 1,000 input tokens and 500 output tokens would cost:

```
(1000 × 0.00003) + (500 × 0.00006) = 0.03 + 0.03 = \$0.06
```

### What Are Tariffs?

Tariffs define per-token pricing for each model. A model can have different tariffs for different purposes:

- **Realtime** — Standard API requests
- **Batch** — Asynchronous batch processing (often cheaper)
- **Playground** — Interactive testing in the UI

If a model has no tariff, requests to that model are free.

Tariffs are time-versioned, so you can change pricing without affecting how historical transactions are displayed. The system records which tariff was active when each charge occurred.

### Organisation and serving-class price precedence

For the requested model, billing uses the API key's owning account and the
**resolved** serving class. A routing overlay or a price does not itself grant
access to a class. An existing private alias keeps its own prices; there is no
implicit pricing fallback between model aliases.

First discard tariffs outside their validity interval or for another account,
class, purpose or batch window. Then choose the first eligible entry below:

| Priority | Realtime | Playground | Batch/flex, completion window W |
|---|---|---|---|
| 1 | Account + class, realtime | Account + class, playground | Account + standard, batch W |
| 2 | Account all-class, realtime | Account + class, realtime | Account all-class, batch W |
| 3 | General model, realtime | Account all-class, playground | General model, batch W |
| 4 | — | Account all-class, realtime | — |
| 5 | — | General model, playground | — |
| 6 | — | General model, realtime | — |

Playground's realtime fallback is evaluated **within each scope** before moving
to the next scope. An organisation's realtime deal therefore beats a general
playground price. Realtime never substitutes for batch, and a 1h batch price
never substitutes for 24h. Realtime/playground use no completion window.
Continuation and platform purposes do not select customer prices.

A selected row supplies both input and output prices. Zero is an explicit price
and stops fallback; the resolver never chooses the cheapest row or combines
fields from different rows. If no eligible price exists, analytics cost remains
NULL and no usage debit is created. This avoids an invented charge, but missing
batch tiers can underbill: configure every supported completion window.

Validity is `valid_from <= time < valid_until` (an absent end is unbounded).
Batch pricing uses batch creation time; other pricing uses the captured request
time. Within the same scope/purpose/window, the latest valid start wins, then
ascending tariff ID breaks exact ties. SQL quotes and Rust billing use the same
rules. Quotes retain the matched tariff's actual purpose and class.

Cache multipliers are selected independently by account + resolved class →
account all-class → general model, at the same timestamp. They have no
purpose/window dimension. A general cache tariff is required for Control Layer
cache pricing to apply; an organisation override alone cannot enable it.
Applicable cache-read/write multipliers adjust the selected input token rate.

## The Transaction Ledger

All credit movements are recorded in an append-only transaction ledger. Transactions are never modified or deleted — this creates a complete audit trail.

**Transaction types:**

| Type | Effect | Created By |
|------|--------|------------|
| **Purchase** | Adds credits | Payment completion (Stripe) |
| **AdminGrant** | Adds credits | Admin action |
| **AdminRemoval** | Removes credits | Admin action |
| **Usage** | Removes credits | API request completion |

Each transaction has a unique `source_id` that prevents duplicates. For usage transactions, this is the analytics record ID. For purchases, it's the payment session ID. This means the same payment or request can never be processed twice.

### Balance Calculation

Rather than storing a running balance, the system calculates it from the transaction history. This append-only design means:

- No race conditions when multiple requests complete simultaneously
- Complete auditability — you can always recalculate the balance from transactions
- No data loss from failed updates

For performance, the system maintains checkpoints that cache the balance at certain points, so it doesn't need to sum every transaction from the beginning of time.

## What Happens at Zero

When a user's balance drops to zero or below:

1. The current request completes (balances can go slightly negative)
2. The system immediately notifies the API proxy
3. The proxy invalidates the user's API keys in its cache
4. Subsequent requests fail with an authentication error

There's no grace period — blocking is immediate. This prevents users from accumulating large negative balances.

When the user purchases more credits or an admin grants them credits, the process reverses: the proxy is notified, API keys become valid again, and requests start working.

## Payment Flow

Users can purchase credits through a self-service checkout flow powered by Stripe (or a dummy provider for testing).

**The flow works like this:**

1. User clicks "Buy Credits" in the dashboard
2. Backend creates a checkout session with Stripe
3. User is redirected to Stripe's hosted payment page
4. User completes payment
5. Stripe sends a webhook notification (or the frontend triggers manual processing)
6. Backend verifies the payment and creates a Purchase transaction
7. User's balance increases, API access is restored if it was blocked

The payment amount is configured on the Stripe side (via a Price ID). The Control Layer doesn't handle payment amounts directly — it just records the credit value associated with successful payments.

### Idempotent Processing

Payment processing is idempotent, meaning the same payment can't be applied twice. The payment session ID serves as a unique key. If a webhook fires multiple times or the user refreshes the success page, only the first processing attempt creates a transaction.

## Batch Requests and Billing

Batch requests are charged the same way as realtime requests — per token, based on the tariff. However:

- Batch tariffs can be configured separately (often at a discount)
- All requests in a batch are grouped in the transaction history for easier tracking
- Cost estimates are available before submitting a batch

When viewing transactions, batch charges can be grouped into a single line item showing the total cost and request count, rather than listing hundreds of individual charges.

## Free Usage Scenarios

API requests are free (no credits deducted) when:

- The model has no tariff configured
- The tariff prices are both zero
- The request fails (non-2xx response)
- The user is the system user (internal requests)

This means you can offer some models for free while charging for others, or run a deployment without any billing at all.

## Related Topics

- [Set Up Model Pricing](../how-to/tariffs.md) — Configure tariffs for your models
- [Set Up Payments](../how-to/payments.md) — Enable credit purchases via Stripe
- [Configuration Reference](../reference/configuration.md) — All billing-related configuration options
