# Platform Event Webhooks

## Overview

Extend the existing webhook system to support platform-wide event notifications. Currently, webhooks only fire for batch terminal states (`batch.completed`, `batch.failed`) and are scoped to the batch creator's own webhooks. This proposal adds new event types for platform activity — user creation, batch creation, API key creation — and allows PlatformManagers to receive webhooks for events across all users.

This keeps the control layer fully generic. PlatformManagers configure webhooks to receive structured event payloads, then pipe them to whatever downstream systems they choose (analytics, CRM, alerting, etc.) — all outside of this codebase.

## Architecture

```
Handler (user/batch/key created)
        │
        ▼
emit_platform_event()             ← new function, fire-and-forget
        │
        ├─ Query webhooks WHERE scope = 'platform'
        ├─ DB trigger enforces only PMs can have platform scope
        ├─ Build event payload
        └─ Insert webhook_deliveries rows
                │
                ▼
Existing dispatcher (claim → sign → send → process results)
        │
        ▼
PM's webhook endpoint (external)
```

The existing dispatcher, signing, retry logic, circuit breaker, and CRUD API all work unchanged.

### Why this approach

- **Reuses proven infrastructure** — the webhook dispatcher already handles signing, retries (7 attempts with exponential backoff), circuit breaker (auto-disable after 10 consecutive failures), crash recovery (`FOR UPDATE SKIP LOCKED`), and multi-replica safety
- **No new dependencies or services** — no Redis, no OTel Collector, no message queue
- **User-facing feature** — PlatformManagers self-serve webhook configuration via the existing API
- **Generic** — the control layer has no knowledge of what providers consume the webhooks; that's the PM's concern
- **Extensible** — adding a new event type is an enum variant + a payload builder + an emit call in the handler
- **Database-enforced access control** — a trigger guarantees only PlatformManagers can hold platform-scoped webhooks, preventing PII leakage to standard users even in the case of a bad application query

### Alternatives considered

**OTel Collector**: Insert a custom OTel Collector between dwctl and the trace backend to filter spans into events. Rejected in favour of webhooks because: requires a new Go service, a new repo, and infrastructure changes; doesn't give PlatformManagers self-service control; and the webhook system already solves delivery, retries, and signing.

**Redis Streams**: Publish events to a Redis Stream with a separate consumer service. Rejected because it introduces a new dependency and infrastructure for the same outcome.

**PostgreSQL LISTEN/NOTIFY**: Emit events via PG notifications. Rejected because it would bake downstream dispatch logic into the open-source backend.

---

## Changes required

All changes are within the `control-layer` repo.

### 1. Database migration

Rename `batch_id` to `resource_id` (generic reference to the resource that triggered the event), make it nullable, add `scope` column to `user_webhooks` with a trigger enforcing that only PlatformManagers can create platform-scoped webhooks.

```sql
-- Rename batch_id to resource_id and make nullable
ALTER TABLE webhook_deliveries RENAME COLUMN batch_id TO resource_id;
ALTER TABLE webhook_deliveries ALTER COLUMN resource_id DROP NOT NULL;

-- Update index to match new column name
ALTER INDEX idx_webhook_deliveries_batch_id RENAME TO idx_webhook_deliveries_resource_id;

-- Add scope column to user_webhooks
ALTER TABLE user_webhooks
  ADD COLUMN scope TEXT NOT NULL DEFAULT 'own'
  CHECK (scope IN ('own', 'platform'));

-- Index for efficient platform webhook queries
CREATE INDEX idx_user_webhooks_platform
  ON user_webhooks (scope)
  WHERE scope = 'platform' AND enabled = true;

-- Trigger: only PlatformManagers can have platform-scoped webhooks
CREATE OR REPLACE FUNCTION enforce_platform_webhook_scope()
RETURNS TRIGGER AS $$
BEGIN
  IF NEW.scope = 'platform' THEN
    IF NOT EXISTS (
      SELECT 1 FROM user_roles
      WHERE user_id = NEW.user_id AND role = 'PlatformManager'
    ) THEN
      RAISE EXCEPTION 'Only PlatformManagers can create platform-scoped webhooks';
    END IF;
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_enforce_platform_webhook_scope
  BEFORE INSERT OR UPDATE ON user_webhooks
  FOR EACH ROW
  EXECUTE FUNCTION enforce_platform_webhook_scope();
```

This provides a hard database-level guarantee: even if the application has a bug in its query logic, a standard user's webhook can never have `scope = 'platform'`, so they can never receive platform event deliveries.

If a PlatformManager is demoted, their existing platform webhooks remain in the DB but the `emit_platform_event` query also checks the role at query time (belt and suspenders). The trigger prevents them from creating or updating webhooks to platform scope after demotion.

### 2. Scoped event types

**File:** `dwctl/src/webhooks/events.rs`

Add a `WebhookScope` enum and new event type variants. Each event type declares its scope, which determines whether it belongs to own-scoped or platform-scoped webhooks:

```rust
/// Webhook scope — determines visibility of events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookScope {
    /// Events about the webhook owner's own resources
    Own,
    /// Platform-wide events visible to PlatformManagers
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebhookEventType {
    // Own-scope events (existing)
    #[serde(rename = "batch.completed")]
    BatchCompleted,
    #[serde(rename = "batch.failed")]
    BatchFailed,

    // Platform-scope events (new)
    #[serde(rename = "user.created")]
    UserCreated,
    #[serde(rename = "batch.created")]
    BatchCreated,
    #[serde(rename = "api_key.created")]
    ApiKeyCreated,
}

impl WebhookEventType {
    /// Which scope this event type belongs to.
    pub fn scope(&self) -> WebhookScope {
        match self {
            Self::BatchCompleted | Self::BatchFailed => WebhookScope::Own,
            Self::UserCreated | Self::BatchCreated | Self::ApiKeyCreated => WebhookScope::Platform,
        }
    }
}
```

Update `FromStr`, `Display`, and the validation in the webhook create/update API handlers to accept the new types.

### 3. Scope-aware event filtering

**File:** `dwctl/src/db/models/webhooks.rs`

Update `accepts_event` to enforce scope matching. This means `event_types = null` (accept all) is scoped — an own-scope webhook with no event type filter only receives own-scope events, never platform events:

```rust
impl Webhook {
    pub fn accepts_event(&self, event_type: WebhookEventType) -> bool {
        if !self.enabled {
            return false;
        }
        // Scope must match
        let webhook_scope = self.scope.parse::<WebhookScope>().unwrap_or(WebhookScope::Own);
        if event_type.scope() != webhook_scope {
            return false;
        }
        // If event_types is null, accept all events within this scope
        match &self.event_types {
            None => true,
            Some(types) => types.as_array().map_or(false, |arr| {
                arr.iter().any(|v| v.as_str() == Some(&event_type.to_string()))
            }),
        }
    }
}
```

### 4. Scope validation on webhook create/update

**File:** `dwctl/src/api/handlers/webhooks.rs`

When creating or updating a webhook, validate that requested event types match the webhook's scope:

```rust
if let Some(ref event_types) = data.event_types {
    let webhook_scope = WebhookScope::from_str(&data.scope)?;
    for et in event_types {
        let parsed: WebhookEventType = et.parse()?;
        if parsed.scope() != webhook_scope {
            return Err(Error::BadRequest {
                message: format!(
                    "Event type '{}' requires scope '{:?}', but webhook scope is '{:?}'",
                    et, parsed.scope(), webhook_scope
                ),
            });
        }
    }
}
```

This gives three layers of enforcement:
1. **DB trigger** — standard users can never set `scope = 'platform'`
2. **Create/update validation** — event types must match the webhook's scope
3. **`accepts_event` runtime check** — filters mismatches even if data is inconsistent

### 5. Event payloads

**File:** `dwctl/src/webhooks/events.rs`

The existing `WebhookEvent` struct has a hardcoded `BatchEventData` field. Generalise it to support different payload shapes per event type:

```rust
pub struct WebhookEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub timestamp: DateTime<Utc>,
    pub data: serde_json::Value,  // event-specific payload
}
```

Add payload builders for each new event:

```rust
impl WebhookEvent {
    pub fn user_created(user_id: UserId, email: &str, auth_source: &str) -> Self { ... }
    pub fn batch_created(batch_id: Uuid, user_id: UserId, endpoint: &str) -> Self { ... }
    pub fn api_key_created(key_id: ApiKeyId, user_id: UserId, name: &str) -> Self { ... }
}
```

### 6. Query platform-scoped webhooks

**File:** `dwctl/src/db/handlers/webhooks.rs`

Add a new query to fetch enabled platform-scoped webhooks owned by PlatformManagers:

```rust
pub async fn get_enabled_platform_webhooks(&mut self) -> Result<Vec<Webhook>> {
    sqlx::query_as!(
        Webhook,
        r#"
        SELECT w.*
        FROM user_webhooks w
        INNER JOIN user_roles ur ON ur.user_id = w.user_id
        WHERE w.enabled = true
          AND w.scope = 'platform'
          AND ur.role = 'PlatformManager'
        "#,
    )
    .fetch_all(&mut *self.tx)
    .await
    .map_err(DbError::from)
}
```

The `scope = 'platform'` filter is enforced by the DB trigger at write time, but the `role = 'PlatformManager'` check at read time handles the demotion case — if a PM loses their role, their existing platform webhooks stop firing immediately.

### 7. Event delivery helper

**File:** `dwctl/src/webhooks/emit.rs` (new)

A generic helper to create delivery records for platform events, callable from any handler:

```rust
pub fn emit_platform_event(
    pool: &PgPool,
    event: WebhookEvent,
    event_type: WebhookEventType,
    resource_id: Option<Uuid>,
) {
    let pool = pool.clone();
    tokio::spawn(async move {
        let mut conn = match pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to acquire connection for webhook delivery");
                return;
            }
        };
        let mut repo = Webhooks::new(&mut conn);

        let pm_webhooks = match repo.get_enabled_platform_webhooks().await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to fetch platform webhooks");
                return;
            }
        };

        let payload = match serde_json::to_value(&event) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize webhook event");
                return;
            }
        };

        for webhook in pm_webhooks.iter().filter(|w| w.accepts_event(event_type)) {
            let delivery = WebhookDeliveryCreateDBRequest {
                webhook_id: webhook.id,
                event_id: Uuid::new_v4(),
                event_type: event_type.to_string(),
                payload: payload.clone(),
                resource_id,
                next_attempt_at: None,
            };
            if let Err(e) = repo.create_delivery(&delivery).await {
                tracing::warn!(error = %e, webhook_id = %webhook.id, "Failed to create webhook delivery");
            }
        }
    });
}
```

This follows the same fire-and-forget pattern as `maybe_update_last_login` — the handler doesn't wait for delivery creation, and failures are logged but never propagate to the API response.

The delivery records are picked up by the existing dispatcher on its next tick (~30s polling interval).

### 8. Emit events from handlers

Add `emit_platform_event` calls after successful operations in each handler:

**User creation** — `dwctl/src/api/handlers/users.rs` (`create_user`, ~line 344):

```rust
webhooks::emit_platform_event(
    state.db.write(),
    WebhookEvent::user_created(user.id, &user.email, &user.auth_source),
    WebhookEventType::UserCreated,
    Some(user.id),
);
```

Also in `dwctl/src/auth/current_user.rs` for auto-created users via proxy header auth.

**Batch creation** — `dwctl/src/api/handlers/batches.rs` (`create_batch`, ~line 445):

```rust
webhooks::emit_platform_event(
    state.db.write(),
    WebhookEvent::batch_created(batch.id, current_user.id, &req.endpoint),
    WebhookEventType::BatchCreated,
    Some(batch.id),
);
```

**API key creation** — `dwctl/src/api/handlers/api_keys.rs` (`create_user_api_key`, ~line 121):

```rust
webhooks::emit_platform_event(
    state.db.write(),
    WebhookEvent::api_key_created(api_key.id, current_user.id, &api_key.name),
    WebhookEventType::ApiKeyCreated,
    Some(api_key.id),
);
```

### 9. Update existing code for column rename

Update all references to `batch_id` in webhook-related code:

- `WebhookDeliveryCreateDBRequest.batch_id` → `resource_id`
- `WebhookDelivery.batch_id` → `resource_id`
- `create_batch_deliveries` in `notifications.rs` — pass batch UUID as `resource_id`
- SQL queries in `db/handlers/webhooks.rs` referencing the column
- SQLx offline query metadata (`.sqlx/` files)

### 10. Update webhook CRUD API

**File:** `dwctl/src/api/handlers/webhooks.rs`

- Add `scope` to `WebhookCreate` request model (default: `"own"`)
- Add `scope` to `WebhookResponse`
- Validate that `scope` is either `"own"` or `"platform"`
- Application-level check: if `scope = "platform"`, verify the user has the PlatformManager role (fail fast before hitting the DB trigger)
- Validate that requested `event_types` match the webhook's scope (see step 4)

---

## Data protection

- **Three-layer scope enforcement** — (1) DB trigger prevents standard users from holding platform-scoped webhooks, (2) create/update validation rejects event types that don't match the webhook's scope, (3) `accepts_event` runtime check filters mismatches even with inconsistent data. A standard user can never receive platform event deliveries.
- **Scoped event_types = null** — a webhook with no event type filter (`null`) receives all events **within its scope only**. An own-scoped webhook never receives platform events, even with `event_types = null`.
- **Demotion safety** — if a PM is demoted, the query-time role check in `get_enabled_platform_webhooks` stops their platform webhooks from firing immediately. The DB trigger prevents them from creating new ones.
- **Deliberately constructed payloads** — each event type has a purpose-built payload builder. No accidental data leakage — you control exactly which fields are included.
- **Opt-in only** — no data flows externally until a PM explicitly creates a webhook and subscribes to event types.
- **Delivery record retention** — `webhook_deliveries` stores payloads as JSON. Consider adding a retention policy to prune delivered/exhausted records after a configurable period (e.g., 30 days).
- **Right to erasure** — pending deliveries cascade-delete when the webhook or PM user is deleted. Already-delivered payloads at external endpoints are outside the platform's control, but this is standard for webhook systems.

---

## Existing infrastructure reused (no changes needed)

| Component | File | What it does |
|-----------|------|-------------|
| Dispatcher | `dwctl/src/webhooks/dispatcher.rs` | Claim/send/drain loop, retry schedule, circuit breaker |
| Signing | `dwctl/src/webhooks/signing.rs` | HMAC-SHA256 per Standard Webhooks spec |
| CRUD API | `dwctl/src/api/handlers/webhooks.rs` | Create/read/update/delete/rotate-secret endpoints |
| DB repository | `dwctl/src/db/handlers/webhooks.rs` | Delivery management, failure tracking |
| Notification poller | `dwctl/src/notifications.rs` | Ticks the dispatcher every ~30s |

## Implementation order

1. Migration: rename `batch_id` → `resource_id`, make nullable, add `scope` column + trigger
2. Update existing code for column rename (`batch_id` → `resource_id`)
3. Add `WebhookScope` enum and new event types with `scope()` method
4. Update `accepts_event` for scope-aware filtering
5. Generalise `WebhookEvent` payload to support different data shapes
6. Add `scope` to webhook CRUD API (create, response, validation) with event type/scope mismatch rejection
7. Add `get_enabled_platform_webhooks` query
8. Add `emit_platform_event` helper
9. Wire emit calls into the three handlers
10. Tests for each new event type + scope enforcement

## Verification

1. `just test rust` — all existing webhook tests pass (batch events unchanged, `resource_id` replaces `batch_id`)
2. Scope enforcement tests:
   - Standard user cannot create a `scope = 'platform'` webhook (application-level rejection + DB trigger rejection)
   - Platform event types rejected on own-scoped webhooks, and vice versa
   - `event_types = null` on own-scoped webhook does not match platform events
3. Demotion test: PM creates platform webhook, PM is demoted, verify webhook stops firing (query-time role check)
4. New event tests: create a PM user with a platform webhook, trigger each event, verify delivery records are created with correct payloads and `resource_id`
5. Integration: run dwctl locally, configure a platform webhook pointing at a request bin, create a user/batch/key, verify signed payloads arrive with retry on failure

## Adding new events in future

1. Add a variant to `WebhookEventType`
2. Add a payload builder to `WebhookEvent`
3. Call `emit_platform_event` from the relevant handler
4. Add the event type to `FromStr`/validation

No infrastructure changes needed — the dispatcher, signing, retries, and API all work generically.
