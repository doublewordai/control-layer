# Webhooks

The control layer supports user-configurable webhooks that deliver HTTP POST
notifications when batches reach a terminal state. The implementation follows
the [Standard Webhooks](https://www.standardwebhooks.com/) specification for
payload signing.

## Overview

```
Fusillade batch reaches terminal state
        │
        ▼
Notification poller (runs every ~30s)
  1. Polls fusillade for completed/failed batches
  2. Creates webhook_delivery records for matching webhooks
  3. Dispatcher tick:
     ├─ Claims due deliveries (SELECT ... FOR UPDATE SKIP LOCKED)
     ├─ Signs payloads (HMAC-SHA256)
     ├─ Sends to sender task via mpsc channel
     └─ Drains results, updates delivery status
        │
        ▼
Sender task (background, always running)
  ├─ HTTP POST with signed headers
  ├─ Semaphore-bounded concurrency
  └─ Sends outcome back via result channel
        │
        ├─ Success (2xx): mark delivered, reset failure counter
        └─ Failure: mark failed, increment failure counter
                      │
                      ├─ Retries remaining → retry on next poll
                      └─ Exhausted after 7 attempts (terminal)
```

## Event types

| Event type        | Trigger                                                  |
|-------------------|----------------------------------------------------------|
| `batch.completed` | Batch finished (all or some requests succeeded)          |
| `batch.failed`    | Batch failed entirely (zero successful requests)         |

Users can subscribe to specific event types or receive all events (default).

## Payload format

```json
{
  "type": "batch.completed",
  "timestamp": "2025-01-15T10:30:45Z",
  "data": {
    "batch_id": "batch_abc123",
    "status": "completed",
    "request_counts": {
      "total": 100,
      "completed": 98,
      "failed": 2,
      "cancelled": 0
    },
    "output_file_id": "file_out_xyz",
    "error_file_id": "file_err_xyz",
    "created_at": "2025-01-15T10:00:00Z",
    "finished_at": "2025-01-15T10:30:45Z"
  }
}
```

## Signing (Standard Webhooks)

Every delivery is signed with the webhook's secret using HMAC-SHA256.

**Signed content:** `{msg_id}.{timestamp}.{payload}`

Where:
- `msg_id` is the delivery's `event_id` (stable UUID, reused across retries)
- `timestamp` is the Unix epoch in seconds at send time
- `payload` is the JSON body

**Headers:**

| Header                | Value                              |
|-----------------------|------------------------------------|
| `webhook-id`          | `{event_id}` (idempotency key)    |
| `webhook-timestamp`   | Unix epoch seconds                 |
| `webhook-signature`   | `v1,{base64-hmac-sha256}`         |
| `webhook-version`     | `1`                                |
| `Content-Type`        | `application/json`                 |

**Secrets** are generated as 32 random bytes, base64-encoded, prefixed with
`whsec_`. Consumers strip the prefix, base64-decode, and use the raw bytes as
the HMAC key.

## Delivery lifecycle

```
pending ──send──► delivered     (terminal, success)
   │
   └──fail──► failed ──retry──► pending  (back to top)
                 │
                 └──max attempts──► exhausted  (terminal)
```

A delivery is created in `pending` state. On each attempt:

- **2xx response**: Marked `delivered`. The webhook's consecutive failure
  counter is reset to 0.
- **Non-2xx or network error**: Marked `failed` with the next retry time
  calculated from the retry schedule. The webhook's consecutive failure counter
  is incremented.
- **Max attempts exceeded**: Marked `exhausted`. No further retries.

## Retry schedule

| Attempt | Delay after previous | Cumulative wait |
|---------|---------------------|-----------------|
| 1       | Immediate           | 0               |
| 2       | 5 seconds           | ~5s             |
| 3       | 5 minutes           | ~5m             |
| 4       | 30 minutes          | ~35m            |
| 5       | 2 hours             | ~2.5h           |
| 6       | 8 hours             | ~10.5h          |
| 7       | 24 hours            | ~34.5h          |

After attempt 7, the delivery is marked `exhausted`.

The schedule is configurable via `retry_schedule_secs`. Its length determines
the maximum number of attempts.

## Circuit breaker

If a webhook accumulates **10 consecutive failures** (configurable via
`circuit_breaker_threshold`) across any deliveries, it is automatically
disabled:

- `enabled` set to `false`
- `disabled_at` set to current timestamp
- Pending deliveries for this webhook are marked `exhausted` on next claim
- No new deliveries are created

The counter resets to 0 on any successful delivery. To re-enable an
auto-disabled webhook, the user must explicitly update it via the API.

## Dispatcher architecture

The dispatcher separates concerns into two tasks:

### Notification poller (owns DB, owns secrets)

Runs on the notification poll loop (~30s). On each tick:

1. **Claim deliveries**: `SELECT ... FOR UPDATE SKIP LOCKED` atomically claims
   a batch of due deliveries and bumps their `next_attempt_at` by 5 minutes as
   a crash-safety net.
2. **Sign and enqueue**: For each claimed delivery, looks up the webhook,
   verifies it's still enabled, computes the HMAC signature, and pushes a
   `WebhookSendRequest` into the send channel.
3. **Drain results**: Reads all available `WebhookSendResult` messages from the
   result channel and performs the appropriate DB updates (`mark_delivered`,
   `mark_failed`, `increment_failures`, `reset_failures`).

### Sender task (no DB, no secrets)

A long-lived background task that:

- Receives `WebhookSendRequest` from the send channel
- Caps concurrency with a semaphore (`max_concurrent_sends`, default 20)
- Performs the HTTP POST
- Sends the outcome back via the result channel

The sender has no pool access and never touches secrets. This keeps the blast
radius small and the code easy to reason about.

### Channel backpressure

Both channels are bounded (`channel_capacity`, default 200). If the send
channel is full, `try_send` fails — the delivery's `next_attempt_at` was
already bumped during claim, so it will be retried on a future poll cycle.

### Crash recovery

When the dispatcher claims a delivery, it bumps `next_attempt_at` 5 minutes
into the future. If the process crashes between claim and result processing,
the delivery becomes claimable again after 5 minutes. No manual intervention
needed.

### Multi-replica safety

`SELECT ... FOR UPDATE SKIP LOCKED` prevents multiple replicas from claiming
the same delivery.

## Cascade delete behavior

The `webhook_deliveries` table uses `ON DELETE CASCADE` from `user_webhooks`.
When a webhook is deleted, all its delivery rows are removed. The dispatcher
handles this gracefully:

- **During claim**: If the webhook is missing from the JOIN, the delivery is
  skipped.
- **During result drain**: If `increment_failures` returns no row (webhook was
  deleted between send and result), a debug log is emitted and processing
  continues.

## API endpoints

All endpoints are under `/admin/api/v1/users/{user_id}/webhooks`.

| Method   | Path                          | Description                          |
|----------|-------------------------------|--------------------------------------|
| `GET`    | `/`                           | List webhooks                        |
| `POST`   | `/`                           | Create webhook (returns secret once) |
| `GET`    | `/{webhook_id}`               | Get webhook (secret hidden)          |
| `PATCH`  | `/{webhook_id}`               | Update webhook                       |
| `DELETE` | `/{webhook_id}`               | Delete webhook                       |
| `POST`   | `/{webhook_id}/rotate-secret` | Rotate secret (returns new secret)   |

### URL validation

- Must be a valid URL with `https://` scheme
- Exception: `http://localhost` and `http://127.0.0.1` are allowed in the
  backend for development (the frontend enforces HTTPS-only)

### Secret rotation

`POST /webhooks/{webhook_id}/rotate-secret` generates a new secret and returns
it in the response. The old secret is immediately invalidated. In-flight
deliveries that were already signed with the old secret will fail verification
on the consumer side — they will be retried with the new secret.

## Configuration

All webhook settings live under `background_services.notifications.webhooks` in
`config.yaml`:

```yaml
background_services:
  notifications:
    enabled: true          # master switch for notification poller
    poll_interval: 30s     # how often the poller runs
    webhooks:
      enabled: true
      timeout_secs: 30                                  # HTTP timeout per attempt
      circuit_breaker_threshold: 10                     # failures before auto-disable
      retry_schedule_secs: [0, 5, 300, 1800, 7200, 28800, 86400]
      claim_batch_size: 50                              # max deliveries per claim
      max_concurrent_sends: 20                          # concurrent HTTP requests
      channel_capacity: 200                             # mpsc buffer size
```

Override via environment variables:

```bash
DWCTL_BACKGROUND_SERVICES__NOTIFICATIONS__WEBHOOKS__ENABLED=false
DWCTL_BACKGROUND_SERVICES__NOTIFICATIONS__WEBHOOKS__TIMEOUT_SECS=15
DWCTL_BACKGROUND_SERVICES__NOTIFICATIONS__WEBHOOKS__CIRCUIT_BREAKER_THRESHOLD=5
```

## Metrics

The dispatcher emits three Prometheus counters:

| Metric                                  | Labels            | Description                        |
|-----------------------------------------|-------------------|------------------------------------|
| `dwctl_webhook_deliveries_claimed_total`| —                 | Deliveries claimed per tick        |
| `dwctl_webhook_deliveries_total`        | `outcome=success` | Successful deliveries (2xx)        |
| `dwctl_webhook_deliveries_total`        | `outcome=failure` | Failed deliveries (non-2xx/error)  |

## Database schema

### `user_webhooks`

| Column                 | Type         | Notes                                  |
|------------------------|--------------|----------------------------------------|
| `id`                   | UUID PK      |                                        |
| `user_id`              | UUID FK      | CASCADE delete from users              |
| `url`                  | TEXT         | HTTPS endpoint                         |
| `secret`               | TEXT         | `whsec_` prefixed, base64-encoded      |
| `enabled`              | BOOLEAN      | Default true                           |
| `event_types`          | JSONB        | Null = all events                      |
| `description`          | TEXT         | Optional                               |
| `consecutive_failures` | INT          | Circuit breaker counter (internal)     |
| `disabled_at`          | TIMESTAMPTZ  | Set when circuit breaker trips         |
| `created_at`           | TIMESTAMPTZ  |                                        |
| `updated_at`           | TIMESTAMPTZ  |                                        |

### `webhook_deliveries`

| Column             | Type         | Notes                                      |
|--------------------|--------------|--------------------------------------------|
| `id`               | UUID PK      |                                            |
| `webhook_id`       | UUID FK      | CASCADE delete from user_webhooks          |
| `event_id`         | UUID         | Stable across retries (idempotency key)    |
| `event_type`       | TEXT         | `batch.completed` or `batch.failed`        |
| `payload`          | JSONB        | Full event, stored for retries             |
| `status`           | TEXT         | pending, delivered, failed, exhausted      |
| `attempt_count`    | INT          | Attempts so far                            |
| `next_attempt_at`  | TIMESTAMPTZ  | When eligible for next attempt             |
| `batch_id`         | UUID         | Reference to triggering batch              |
| `last_status_code` | INT          | HTTP status from most recent attempt       |
| `last_error`       | TEXT         | Error from most recent attempt             |
| `created_at`       | TIMESTAMPTZ  |                                            |
| `updated_at`       | TIMESTAMPTZ  |                                            |

**Key indexes:**
- `idx_webhook_deliveries_pending` on `(next_attempt_at) WHERE status IN ('pending', 'failed')` — optimized for the claim query
- `idx_webhook_deliveries_webhook_id`
- `idx_webhook_deliveries_batch_id`

## Key source files

| File                                    | Purpose                                     |
|-----------------------------------------|---------------------------------------------|
| `dwctl/src/webhooks/dispatcher.rs`      | Claim loop, sender task, result drain        |
| `dwctl/src/webhooks/signing.rs`         | HMAC-SHA256 signing, secret generation       |
| `dwctl/src/webhooks/events.rs`          | Event type enum, payload construction        |
| `dwctl/src/notifications.rs`            | Notification poller, delivery creation       |
| `dwctl/src/api/handlers/webhooks.rs`    | CRUD API handlers                            |
| `dwctl/src/api/models/webhooks.rs`      | API request/response models                  |
| `dwctl/src/db/handlers/webhooks.rs`     | Repository (deliveries, circuit breaker)     |
| `dwctl/src/db/models/webhooks.rs`       | Database record structs                      |
| `dwctl/migrations/064_add_webhook_configuration.sql` | Schema                        |
