# Platform Event Webhooks

## Overview

The webhook system supports platform-wide event notifications in addition to the original per-user batch events. PlatformManagers can create platform-scoped webhooks to receive structured event payloads for activity across all users — user creation, batch creation, API key creation — and pipe them to downstream systems (analytics, CRM, alerting, etc.).

## Architecture

```
Database INSERT (user / api_key / batch)
        │
        ├─ users & api_keys: PG trigger fires NOTIFY on 'webhook_event' channel
        │                     payload: "table_name:record_id"
        │
        └─ batches: no trigger — detected by polling (fusillade schema is external)
                │
                ▼
Notification poller (notifications.rs)
        │
        ├─ PgListener receives NOTIFY → buffers in pending_webhook_events
        ├─ On each tick: process_platform_events() for user.created / api_key.created
        ├─ On each tick: process_new_batches() polls fusillade for recent batches
        ├─ Queries get_enabled_platform_webhooks() (scope='platform' + PM role check)
        └─ Inserts webhook_deliveries rows
                │
                ▼
Existing dispatcher (claim → sign → send → process results)
        │
        ▼
PM's webhook endpoint (external)
```

All event detection and delivery creation happens in the notification poller, not in HTTP handlers. This keeps handlers fast and decouples event processing from the request path. The existing dispatcher handles signing, retries (7 attempts with exponential backoff), circuit breaker (auto-disable after 10 consecutive failures), and crash recovery (`FOR UPDATE SKIP LOCKED`).

### Configuration gating

All webhook processing is gated on `config.webhooks.enabled` (under `background_services.notifications.webhooks`):

- If disabled: no `WebhookDispatcher` is spawned, no PG listener is created, no platform events are processed, no batch deliveries are created
- If enabled: dispatcher runs independently, PG listener subscribes to `webhook_event` channel, all event types are processed

The webhook management CRUD API is always available regardless of this setting — users can configure webhooks even when delivery is disabled.

### Why this approach

- **Reuses proven infrastructure** — the webhook dispatcher already handles signing, retries, circuit breaker, crash recovery, and multi-replica safety
- **No new dependencies or services** — no Redis, no OTel Collector, no message queue
- **Database-driven event detection** — PG triggers guarantee no events are missed, even if the application restarts
- **Polling fallback for external schemas** — `batch.created` uses polling because the fusillade schema is managed externally
- **Deduplication** — unique index on `(webhook_id, event_type, resource_id)` prevents duplicate deliveries during leader transitions
- **Decoupled from request path** — handlers are not slowed by webhook processing

## Event types

| Event type | Scope | Trigger mechanism | Source |
|------------|-------|-------------------|--------|
| `batch.completed` | Own | Notification poller polls fusillade for terminal batches | `create_batch_deliveries()` |
| `batch.failed` | Own | Notification poller polls fusillade for terminal batches | `create_batch_deliveries()` |
| `user.created` | Platform | PG trigger → NOTIFY → `process_platform_events()` | `users` table INSERT trigger |
| `api_key.created` | Platform | PG trigger → NOTIFY → `process_platform_events()` | `api_keys` table INSERT trigger |
| `batch.created` | Platform | Polling every tick → `process_new_batches()` | Polls `fusillade.batches` for recent rows |

## Event payloads

**File:** `dwctl/src/webhooks/events.rs`

All events share a common envelope:

```rust
pub struct WebhookEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub timestamp: DateTime<Utc>,
    pub data: serde_json::Value,  // event-specific payload
}
```

### Payload builders

```rust
impl WebhookEvent {
    pub fn batch_terminal(event_type: WebhookEventType, info: &BatchNotificationInfo) -> Self;
    pub fn user_created(user_id: UserId, email: &str, auth_source: &str) -> Self;
    pub fn batch_created(batch_id: Uuid, user_id: UserId, endpoint: &str) -> Self;
    pub fn api_key_created(key_id: Uuid, user_id: UserId, created_by: UserId, name: &str) -> Self;
}
```

### Example payloads

**batch.completed / batch.failed:**
```json
{
  "type": "batch.completed",
  "timestamp": "2025-01-15T10:30:00Z",
  "data": {
    "batch_id": "batch_abc123",
    "status": "completed",
    "request_counts": { "total": 100, "completed": 98, "failed": 2, "cancelled": 0 },
    "output_file_id": "file_def456",
    "error_file_id": null,
    "created_at": "2025-01-15T09:00:00Z",
    "finished_at": "2025-01-15T10:30:00Z"
  }
}
```

**user.created:**
```json
{
  "type": "user.created",
  "timestamp": "2025-01-15T10:30:00Z",
  "data": {
    "user_id": "550e8400-e29b-41d4-a716-446655440000",
    "email": "jane@example.com",
    "auth_source": "native"
  }
}
```

**batch.created:**
```json
{
  "type": "batch.created",
  "timestamp": "2025-01-15T10:30:00Z",
  "data": {
    "batch_id": "batch_abc123",
    "user_id": "550e8400-e29b-41d4-a716-446655440000",
    "endpoint": "/v1/chat/completions"
  }
}
```

**api_key.created:**
```json
{
  "type": "api_key.created",
  "timestamp": "2025-01-15T10:30:00Z",
  "data": {
    "api_key_id": "660e8400-e29b-41d4-a716-446655440000",
    "user_id": "550e8400-e29b-41d4-a716-446655440000",
    "created_by": "770e8400-e29b-41d4-a716-446655440000",
    "name": "My API Key"
  }
}
```

Note: `user_id` is the key owner (may be an organisation), `created_by` is the individual who created the key.

## Database schema

### Migration 085: Platform event webhooks

- Renames `webhook_deliveries.batch_id` → `resource_id` (nullable, generic reference)
- Adds `scope` column to `user_webhooks` (`'own'` or `'platform'`, default `'own'`)
- Partial index on `user_webhooks` for efficient platform webhook queries
- DB trigger `enforce_platform_webhook_scope`: prevents non-PlatformManagers from holding platform-scoped webhooks

### Migration 086: Webhook event notifications

- PG function `notify_webhook_event()`: sends NOTIFY with `"table_name:record_id"` payload
- Triggers on `users` and `api_keys` tables (AFTER INSERT)
- Unique index on `webhook_deliveries (webhook_id, event_type, resource_id)` for deduplication

## Scope enforcement

Three layers prevent unauthorised access to platform events:

1. **DB trigger** (`enforce_platform_webhook_scope`) — standard users can never set `scope = 'platform'` on a webhook, even if the application has a bug
2. **Create/update validation** (`webhooks.rs`) — event types must match the webhook's scope; platform event types are rejected on own-scoped webhooks and vice versa
3. **Runtime `accepts_event` check** (`Webhook::accepts_event`) — scope must match at delivery creation time; `event_types = null` only matches events within the webhook's own scope

### Demotion safety

If a PlatformManager is demoted:
- `get_enabled_platform_webhooks()` joins on `user_roles` and checks for the PM role at query time — their webhooks stop firing immediately
- The DB trigger prevents them from creating or updating webhooks to platform scope

## Key implementation files

| File | Role |
|------|------|
| `dwctl/src/webhooks/events.rs` | Event types, scopes, payload builders, unit tests |
| `dwctl/src/webhooks/dispatcher.rs` | Claim/send/drain loop, retry schedule, circuit breaker |
| `dwctl/src/webhooks/signing.rs` | HMAC-SHA256 per Standard Webhooks spec |
| `dwctl/src/notifications.rs` | Notification poller: PG listener, event processing, delivery creation |
| `dwctl/src/api/handlers/webhooks.rs` | CRUD API: create/read/update/delete/rotate-secret |
| `dwctl/src/db/handlers/webhooks.rs` | DB repository: queries, delivery management |
| `dwctl/src/db/models/webhooks.rs` | Webhook model, `accepts_event` scope filtering |
| `dwctl/migrations/085_platform_event_webhooks.sql` | Schema changes: scope column, resource_id rename, PM trigger |
| `dwctl/migrations/086_webhook_event_notifications.sql` | PG triggers for NOTIFY, deduplication index |

## Data protection

- **Deliberately constructed payloads** — each event type has a purpose-built payload builder. No accidental data leakage; you control exactly which fields are included.
- **Opt-in only** — no data flows externally until a PM explicitly creates a webhook and subscribes to event types.
- **Delivery record retention** — `webhook_deliveries` stores payloads as JSON. Consider adding a retention policy to prune delivered/exhausted records after a configurable period.
- **Right to erasure** — pending deliveries cascade-delete when the webhook or PM user is deleted.

## Adding new events

1. Add a variant to `WebhookEventType` (with scope assignment in `scope()`)
2. Add a payload builder to `WebhookEvent`
3. Update `FromStr` / `Display` / validation error messages
4. Choose detection mechanism:
   - **PG trigger** (preferred for dwctl-managed tables): add a trigger calling `notify_webhook_event()`, handle in `process_platform_events()`
   - **Polling** (for external schemas like fusillade): add a polling function similar to `process_new_batches()`
5. Add unit tests in `events.rs`

No infrastructure changes needed — the dispatcher, signing, retries, and CRUD API all work generically.
