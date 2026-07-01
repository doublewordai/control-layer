# Set Up Model Pricing

> Configure per-token pricing for your models using tariffs. Set different rates for realtime, batch, and playground usage.

This guide shows you how to configure per-token pricing for your models using tariffs.

## Prerequisites

- A deployed model (see [Add Endpoints](endpoints.md))
- Platform Manager role

## Understanding Tariffs

A tariff defines per-token pricing for a model. Each model can have multiple tariffs for different purposes:

| Purpose | Description | Limit |
|---------|-------------|-------|
| **Realtime** | Standard API requests | One per model |
| **Batch** | Asynchronous batch processing | One per SLA (e.g., 24h) |
| **Playground** | Dashboard testing | One per model |

Models without tariffs are free to use.

## Set Pricing via the Dashboard

1. Go to **Models** in the sidebar
2. Click on the model you want to price
3. Click **Manage Pricing Tariffs**
4. Click **+ Add Tariff**
5. Fill in the pricing details:
   - **Name**: Descriptive label (e.g., "Standard Pricing")
   - **Purpose**: Select realtime, batch, or playground
   - **Input price**: Cost per 1M input tokens
   - **Output price**: Cost per 1M output tokens
6. For batch tariffs, select the **SLA** (completion window like "24h")
7. Click **Save Changes**

Pricing takes effect immediately.

## Set Pricing via the API

Include a `tariffs` array when creating or updating a model:

```bash
curl -X PATCH "https://your-instance/admin/api/v1/models/{model-id}" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "tariffs": [
      {
        "name": "Realtime Pricing",
        "input_price_per_token": "0.003",
        "output_price_per_token": "0.015",
        "api_key_purpose": "realtime"
      }
    ]
  }'
```

### Tariff Fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Descriptive name for the tariff |
| `input_price_per_token` | Yes | Price per input token (decimal) |
| `output_price_per_token` | Yes | Price per output token (decimal) |
| `api_key_purpose` | No | `"realtime"`, `"batch"`, or `"playground"` |
| `completion_window` | Batch only | SLA like `"24h"` |

> **Note**
>
> Prices are stored per-token with 8 decimal places. The dashboard displays prices per 1M tokens for readability. To convert: `\$3.00 per 1M tokens = 0.000003 per token`.

## Example: Pricing with Multiple Tiers

Configure different prices for realtime, batch, and playground:

```json
{
  "tariffs": [
    {
      "name": "Realtime",
      "input_price_per_token": "0.00003",
      "output_price_per_token": "0.00006",
      "api_key_purpose": "realtime"
    },
    {
      "name": "Batch 24h",
      "input_price_per_token": "0.000015",
      "output_price_per_token": "0.00003",
      "api_key_purpose": "batch",
      "completion_window": "24h"
    },
    {
      "name": "Playground (Free)",
      "input_price_per_token": "0",
      "output_price_per_token": "0",
      "api_key_purpose": "playground"
    }
  ]
}
```

This configuration:
- Charges full price for realtime API usage
- Offers 50% discount for 24-hour batch jobs
- Makes playground testing free

## Updating Prices

When you update tariffs, the system:

1. Closes old tariffs by setting their end date to now
2. Creates new tariffs effective immediately
3. Preserves historical pricing for accurate transaction records

Old transactions are charged at the rate that was active when they occurred. New transactions use the updated rates.

## Viewing Current Tariffs

To see a model's current pricing via API, include `pricing` in the query:

```bash
curl "https://your-instance/admin/api/v1/models/{model-id}?include=pricing" \
  -H "Authorization: Bearer $API_KEY"
```

The response includes a `tariffs` array with all active tariffs.

## Removing Pricing

To make a model free, update it with an empty tariffs array:

```bash
curl -X PATCH "https://your-instance/admin/api/v1/models/{model-id}" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"tariffs": []}'
```

This closes all active tariffs. Requests to the model will no longer incur charges.

## Related Topics

- [How Billing Works](../conceptual-guides/how-billing-works.md) — Understand the credits system
- [Configuration Reference](../reference/configuration.md) — Batch SLA configuration
