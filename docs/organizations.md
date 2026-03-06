# Organizations Feature — Implementation Plan

## Context

The control layer is user-centric: credits, API keys, groups, and webhooks all reference `user_id`. There is no concept of shared billing or shared resource ownership. This feature introduces **organizations** so that:

- An org can hold its own credit balance and API keys
- Requests made with an org's API key are billed to the org
- Users can belong to orgs and manage them via the dashboard
- The inference hot path (onwards) requires **zero changes**

**Approach: Option C** — organizations are represented as rows in the `users` table with a distinguishing flag. A new `user_organizations` mapping table links individual users to their orgs. This leverages the entire existing credit, group, API key, and onwards sync system unchanged.

**Spelling:** US English throughout (`organization`, not `organisation`).

---

## Phase 1: Database Migration (`072_add_organizations.sql`)

**File:** `control-layer/dwctl/migrations/072_add_organizations.sql`

```sql
-- Add user_type column to distinguish individuals from organizations.
-- Default 'individual' preserves all existing behavior.
ALTER TABLE users
  ADD COLUMN user_type VARCHAR NOT NULL DEFAULT 'individual';

-- Mapping: which users belong to which organizations.
-- An organization is itself a row in the users table with user_type = 'organization'.
CREATE TABLE user_organizations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    organization_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR NOT NULL DEFAULT 'member',   -- 'owner' | 'admin' | 'member'
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, organization_id)
);

CREATE INDEX idx_user_organizations_user_id ON user_organizations(user_id);
CREATE INDEX idx_user_organizations_organization_id ON user_organizations(organization_id);

-- Enforce that organization_id always points to a user with user_type = 'organization'.
-- CHECK constraints can't query other tables, so we use a trigger.
CREATE OR REPLACE FUNCTION check_organization_type() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM users WHERE id = NEW.organization_id AND user_type = 'organization'
    ) THEN
        RAISE EXCEPTION 'organization_id must reference a user with user_type = ''organization'''
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER enforce_organization_type
    BEFORE INSERT OR UPDATE ON user_organizations
    FOR EACH ROW EXECUTE FUNCTION check_organization_type();

-- Trigger NOTIFY so onwards picks up any future config changes involving org users
CREATE TRIGGER user_organizations_notify
    AFTER INSERT OR UPDATE OR DELETE ON user_organizations
    FOR EACH STATEMENT EXECUTE FUNCTION notify_config_change();

-- Add api_key_id to http_analytics for per-key usage attribution within orgs.
-- Currently only user_id is tracked; this enables "which member's key caused this usage?"
-- NOTE: ALTER TABLE ADD COLUMN with no DEFAULT is metadata-only (instant, no table rewrite).
-- CREATE INDEX on 20M+ rows takes ~30-90s and holds a SHARE lock (blocks INSERTs).
-- This is acceptable: inference requests still flow, only analytics writes queue briefly.
-- The transactional migration ensures full rollback on failure (no orphaned state).
ALTER TABLE http_analytics ADD COLUMN api_key_id UUID;
CREATE INDEX idx_http_analytics_api_key_id ON http_analytics(api_key_id);

-- Add api_key_id to credits_transactions for per-key usage attribution within orgs.
-- Matches http_analytics pattern: filter by user_id, join through api_keys to attribute to org member.
-- NULL for legacy rows, system-generated transactions, and non-usage types.
ALTER TABLE credits_transactions ADD COLUMN api_key_id UUID;

-- Soft-delete for api_keys. Hard delete would orphan api_key_id references in
-- credits_transactions and http_analytics, losing attribution data.
-- Mirrors the existing soft-delete pattern on the users table.
ALTER TABLE api_keys ADD COLUMN is_deleted BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX idx_api_keys_is_deleted ON api_keys(user_id) WHERE is_deleted = false;

-- Track which individual user created each API key.
-- For individual users: created_by = user_id (self-created).
-- For org keys: created_by = the member who created it, user_id = the org.
ALTER TABLE api_keys ADD COLUMN created_by UUID REFERENCES users(id);

-- Backfill: all existing keys were created by the user who owns them.
UPDATE api_keys SET created_by = user_id;

-- Now make it NOT NULL for all future inserts.
ALTER TABLE api_keys ALTER COLUMN created_by SET NOT NULL;

-- Update hidden key unique constraint: one hidden key per (user_id, created_by, purpose).
-- Old: (user_id, purpose) WHERE hidden = true — one per user per purpose.
-- New: (user_id, created_by, purpose) WHERE hidden = true — one per org member per purpose.
-- For individuals, created_by = user_id, so behavior is unchanged.
DROP INDEX idx_api_keys_user_hidden_purpose;
CREATE UNIQUE INDEX idx_api_keys_user_hidden_purpose
  ON api_keys(user_id, created_by, purpose) WHERE hidden = true;
```

**Key decisions:**
- `user_type VARCHAR` rather than boolean — extensible without another migration
- Organization roles: `owner` (full management), `admin` (can manage members/keys), `member` (can use org context)
- CASCADE on both FKs: deleting a user removes their memberships; deleting an org removes all memberships
- `BEFORE INSERT` trigger enforces that `organization_id` actually points to an organization (DB-level integrity)
- NOTIFY trigger so onwards reloads if org membership affects routing (future-proofing)
- `api_key_id` on `http_analytics` enables per-member usage attribution within an org
- `api_key_id` on `credits_transactions` enables direct billing queries without joining through `http_analytics`
- `is_deleted` on `api_keys` — soft-delete preserves attribution data in `credits_transactions` and `http_analytics`
- `created_by` on `api_keys` — tracks which individual created each key; enables per-member hidden keys for orgs
- Hidden key unique constraint changes from `(user_id, purpose)` to `(user_id, created_by, purpose)` — each org member gets their own hidden keys

**Migration risk: `http_analytics` index (~30-90s)**

The `CREATE INDEX` on 20M+ rows holds a `SHARE` lock, blocking analytics INSERTs for the duration. This is a transactional migration, so failure triggers a full rollback — the lock releases and existing pods resume normal operation.

If the migration stalls and needs cancelling:
```bash
# Find the locking query
psql -c "SELECT pid, state, query, now() - query_start AS duration
         FROM pg_stat_activity
         WHERE query ILIKE '%http_analytics%' AND state != 'idle';"

# Cancel gracefully (triggers transaction rollback, releases lock)
psql -c "SELECT pg_cancel_backend(<pid>);"
```

The blue-green pod will crash-loop, but each attempt is safe: rollback cleans up, existing pods are unaffected.

### Organization auth: orgs cannot log in

No auth changes needed. Organizations naturally cannot authenticate because:
- `password_hash` = NULL (no native login)
- `auth_source` = `'organization'` — not handled by any auth extraction path (`current_user.rs` only handles JWT session, proxy headers, and API keys)
- `external_user_id` = NULL (no SSO match)
- `email` serves as org contact email (for Stripe, notifications), not a credential
- `username` serves as the org's slug/handle — globally unique via existing constraint

---

## Phase 2: Rust Backend — Models & DB Handlers

### 2a. DB Models (`db/models/organizations.rs`) — New file

```
OrganizationCreateDBRequest { name, email, display_name, avatar_url, created_by: UserId }
OrganizationUpdateDBRequest { display_name, avatar_url, email }
OrganizationMemberDBResponse { user_id, organization_id, role, created_at }
```

No separate `OrganizationDBResponse` needed — orgs are `UserDBResponse` with `user_type = "organization"`.

### 2b. Internal User struct changes (`db/handlers/users.rs`)

- Add `user_type: String` to the private `User` struct (line ~40)
- Add `user_type` to all SELECT queries that build `User` (get_by_id, get_bulk, list, create)
- Add `user_type` to GROUP BY clauses
- Pass through to `UserDBResponse`

### 2c. DB Response changes (`db/models/users.rs`)

- Add `pub user_type: String` to `UserDBResponse`

### 2d. User list filtering (`db/handlers/users.rs`)

- `list()` and `count()`: Add `AND user_type = 'individual'` to WHERE clauses
- This prevents org "users" from appearing in the regular user list
- The `get_by_id` and `get_bulk` methods remain unfiltered (they need to work for both)

### 2e. DB Handler (`db/handlers/organizations.rs`) — New file

Repository for organization CRUD + membership management:

```rust
pub struct Organizations<'c> { db: &'c mut PgConnection }
```

Methods:
- `create(request)` — Inserts into `users` with `user_type = 'organization'`, `auth_source = 'organization'`. Also inserts `created_by` as `owner` in `user_organizations`.
- `list(filter)` — `SELECT * FROM users WHERE user_type = 'organization' AND is_deleted = false`
- `count(filter)` — Count variant of above
- `get_by_id(id)` — Delegates to `Users::get_by_id` (orgs are users)
- `update(id, request)` — Updates display_name, avatar_url, email on the org user
- `delete(id)` — Soft-deletes the org user (same as user deletion)
- `add_member(org_id, user_id, role)` — Inserts into `user_organizations`
- `remove_member(org_id, user_id)` — Deletes from `user_organizations`
- `update_member_role(org_id, user_id, role)` — Updates role in `user_organizations`
- `list_members(org_id)` — Joins `user_organizations` with `users` to return members
- `list_user_organizations(user_id)` — Returns orgs a user belongs to
- `get_user_org_role(user_id, org_id)` — Returns the user's role in the org (used by permission checks)

Register in `db/handlers/mod.rs` and `db/models/mod.rs`.

---

## Phase 3: Rust Backend — API Layer

### 3a. API Models (`api/models/organizations.rs`) — New file

```rust
pub struct OrganizationCreate { name: String, email: String, display_name: Option<String> }
pub struct OrganizationUpdate { display_name: Option<String>, email: Option<String> }
pub struct OrganizationResponse { /* UserResponse fields + member_count */ }
pub struct OrganizationMemberResponse { user: UserResponse, role: String, created_at: DateTime }
pub struct AddMemberRequest { user_id: UserId, role: Option<String> }
pub struct UpdateMemberRoleRequest { role: String }
pub struct ListOrganizationsQuery { pagination: Pagination, search: Option<String>, include: Option<String> }
```

### 3b. API Handlers (`api/handlers/organizations.rs`) — New file

Endpoints following the existing pattern (generic over `PoolProvider`, utoipa docs, tracing):

| Method | Path | Permission | Description |
|--------|------|------------|-------------|
| POST | `/organizations` | `Organizations::CreateOwn` | Create org (any user, creator becomes owner) |
| GET | `/organizations` | `Organizations::ReadAll` | List all orgs (admin); standard users see own orgs |
| GET | `/organizations/{id}` | `Organizations::ReadOwn` | Get org details (must be member) |
| PATCH | `/organizations/{id}` | `Organizations::UpdateOwn` | Update org (owner/admin of org) |
| DELETE | `/organizations/{id}` | `Organizations::DeleteAll` | Delete org (platform admin only) |
| GET | `/organizations/{id}/members` | `Organizations::ReadOwn` | List org members |
| POST | `/organizations/{id}/members` | `Organizations::UpdateOwn` | Add member (owner/admin of org) |
| PATCH | `/organizations/{id}/members/{user_id}` | `Organizations::UpdateOwn` | Update member role |
| DELETE | `/organizations/{id}/members/{user_id}` | `Organizations::UpdateOwn` | Remove member |
| GET | `/users/{id}/organizations` | `Users::ReadOwn` | List user's orgs |

"Own" operations verify the current user is a member (or owner/admin) of the target org.

**Security guards on membership mutations:**

1. **Last-owner guard** (`remove_member`, `update_member_role`): Before removing an owner or demoting an owner to another role, check that at least one other owner exists. Prevents leaving an org in an ownerless state.

2. **Privilege escalation prevention** (`add_member`, `update_member_role`): Only owners (or platform managers) can assign the `owner` role. Org admins can assign `admin` or `member` but cannot promote themselves or others to `owner`. This is enforced by `check_role_assignment_privilege()`.

### 3c. Org-Aware Permission Checks for User Sub-Resources

**Problem:** Existing handlers for `/users/{user_id}/api-keys`, `/users/{user_id}/webhooks`, etc. check `can_*_own_resource(current_user, target_user_id)` which requires `current_user.id == target_user_id`. When Alice manages Acme Corp's API keys, `Alice.id != AcmeCorp.id`, so the check fails.

**Solution:** Add a shared helper and a third permission path to all user sub-resource handlers.

New helper in `auth/permissions.rs`:
```rust
/// Check if the current user can manage resources belonging to an organization.
/// Returns true if target_user_id is an org and current_user is an owner/admin of it.
pub async fn can_manage_org_resource(
    current_user: &CurrentUser,
    target_user_id: UserId,
    db: &mut PgConnection,
) -> Result<bool, DbError> {
    let mut repo = Organizations::new(db);
    match repo.get_user_org_role(current_user.id, target_user_id).await? {
        Some(role) if role == "owner" || role == "admin" => Ok(true),
        _ => Ok(false),
    }
}
```

Updated pattern in handlers (api_keys, webhooks, transactions):
```rust
// Before (current):
let can_all = can_read_all_resources(&current_user, Resource::ApiKeys);
let can_own = can_read_own_resource(&current_user, Resource::ApiKeys, target_user_id);
if !can_all && !can_own {
    return Err(Error::InsufficientPermissions { ... });
}

// After (with org support):
let can_all = can_read_all_resources(&current_user, Resource::ApiKeys);
let can_own = can_read_own_resource(&current_user, Resource::ApiKeys, target_user_id);
let can_org = can_manage_org_resource(&current_user, target_user_id, &mut conn).await?;
if !can_all && !can_own && !can_org {
    return Err(Error::InsufficientPermissions { ... });
}
```

**Handlers to update:**
- `api/handlers/api_keys.rs` — list, create, get, delete (4 handlers)
- `api/handlers/webhooks.rs` — list, create, get, update, delete, rotate-secret (6 handlers)
- `api/handlers/transactions.rs` — list, get (2 handlers, read-only for org members)

### 3d. Permissions (`auth/permissions.rs`, `types.rs`)

- Add `Organizations` to `Resource` enum in `types.rs`
- Add `resource::Organizations` struct + `From` impl in `permissions.rs`
- Permission rules:
  - `PlatformManager` → full access (consistent with existing pattern)
  - `StandardUser` → `CreateOwn`, `ReadOwn`, `UpdateOwn` (create orgs, manage orgs they belong to)
  - Other roles: no org access by default

### 3e. Routes (`lib.rs`)

Add organization routes in `build_router()`, after the groups section:

```rust
// Organization management
.route("/organizations", get(api::handlers::organizations::list_organizations))
.route("/organizations", post(api::handlers::organizations::create_organization))
.route("/organizations/{id}", get(api::handlers::organizations::get_organization))
.route("/organizations/{id}", patch(api::handlers::organizations::update_organization))
.route("/organizations/{id}", delete(api::handlers::organizations::delete_organization))
// Organization membership
.route("/organizations/{id}/members", get(api::handlers::organizations::list_members))
.route("/organizations/{id}/members", post(api::handlers::organizations::add_member))
.route("/organizations/{id}/members/{user_id}", patch(api::handlers::organizations::update_member_role))
.route("/organizations/{id}/members/{user_id}", delete(api::handlers::organizations::remove_member))
// User's organizations (sub-resource on users)
.route("/users/{user_id}/organizations", get(api::handlers::organizations::list_user_organizations))
// Organization session context (validates membership; client sends X-Organization-Id header)
.route("/session/organization", post(api::handlers::organizations::set_active_organization))
```

### 3f. UserResponse changes (`api/models/users.rs`)

- Add `pub user_type: String` to `UserResponse`
- Add `#[serde(skip_serializing_if = "Option::is_none")] pub organizations: Option<Vec<OrganizationSummary>>` for `include=organizations`
- Update `From<UserDBResponse> for UserResponse` to include `user_type`
- `ListUsersQuery.include` now also accepts `"organizations"`

### 3g. CurrentUser changes (`api/models/users.rs`)

- Add `pub organizations: Vec<UserOrganizationContext>` to `CurrentUser`
  - `UserOrganizationContext { id: UserId, name: String, role: String }`
- This tells the dashboard which orgs the user can switch to
- Populated during auth extraction by querying `user_organizations`

---

## Phase 4: Usage Attribution & Audit Trail

### Problem

When an org's API key is used for inference, the system currently tracks `user_id` (= the org) in `http_analytics` and `credits_transactions`. Two gaps:

1. **Which member's key?** — `http_analytics` has no `api_key_id` column, so we can't attribute usage to a specific key (and thus to the member who created it)

### Solution

**A. Inference path — `api_key_id` on both `http_analytics` and `credits_transactions`**

Migration adds `api_key_id UUID` to both tables (see Phase 1).

The analytics pipeline in `request_logging/analytics_handler.rs` already extracts the bearer token from requests. We extend it to also resolve and store the API key's UUID. The onwards proxy already knows which API key matched (it's in the routing config) — this ID just needs to flow through to the analytics writer.

`api_key_id` on `credits_transactions` enables direct billing queries without joining through the 20M+ row `http_analytics` table:

```sql
-- "Show Acme Corp's usage broken down by member's key"
SELECT ct.*, ak.name as key_name
FROM credits_transactions ct
JOIN api_keys ak ON ak.id = ct.api_key_id
WHERE ct.user_id = <org_id> AND ct.transaction_type = 'usage';
```

`api_key_id` on `http_analytics` is still useful for detailed request-level analysis (latency, tokens, model, etc.).

Within an org, each member creates their own API key. Usage per key = usage per member. The dashboard can show a breakdown: "Alice's key used 50k tokens, Bob's key used 120k tokens" for Acme Corp.

**B. Soft-delete for `api_keys`**

Migration adds `is_deleted BOOLEAN` to `api_keys` (see Phase 1).

Currently API keys are hard-deleted. With `api_key_id` references in `credits_transactions` and `http_analytics`, hard delete would orphan those references and lose attribution data (key name, which member created it).

Changes to `db/handlers/api_keys.rs`:
- `delete()` → `UPDATE api_keys SET is_deleted = true WHERE id = $1` (instead of `DELETE`)
- All list/get queries add `AND is_deleted = false`
- The onwards sync query (`load_targets_from_db`) must also filter `AND ak.is_deleted = false` so deleted keys stop routing immediately
- Partial index `idx_api_keys_is_deleted` keeps queries efficient

### D. `created_by` on `api_keys`

Migration adds `created_by UUID NOT NULL` to `api_keys` (see Phase 1), backfilled with `user_id` for existing rows.

This enables direct attribution: when Alice creates a key under Acme Corp, `api_keys.user_id = acme_id`, `api_keys.created_by = alice_id`. Combined with `api_key_id` on `credits_transactions` and `http_analytics`, this forms the complete attribution chain:

```
credits_transactions.api_key_id → api_keys.created_by → individual user
```

Changes to `db/handlers/api_keys.rs`:
- `create()` — accept and store `created_by` (from `current_user.id`)
- `get_or_create_hidden_key()` — accept `created_by` parameter (see Phase 4e below)

Changes to `db/models/api_keys.rs`:
- Add `pub created_by: UserId` to `ApiKeyCreateDBRequest`
- Add `pub created_by: UserId` to `ApiKeyDBResponse`

Changes to `api/models/api_keys.rs`:
- Add `pub created_by: UserId` to `ApiKeyResponse`
- `From<ApiKeyDBResponse>` passes through `created_by`

### E. Per-Member Hidden Keys for Organizations

**Problem:** Dashboard actions (playground, batch file upload) use hidden API keys — system-managed keys per user per purpose. The current unique constraint `(user_id, purpose) WHERE hidden = true` allows only one hidden key per user per purpose. When Alice and Bob both use the dashboard for org Acme Corp, they need separate hidden keys so their usage is individually attributable.

**Solution:** The unique constraint changes to `(user_id, created_by, purpose) WHERE hidden = true`. Each org member gets their own hidden key per purpose, where `user_id = org_id` and `created_by = member_id`. For individual users, `created_by = user_id`, so existing behavior is unchanged.

Updated `get_or_create_hidden_key` signature:

```rust
pub async fn get_or_create_hidden_key(
    &mut self,
    user_id: UserId,       // The key owner (individual user or org)
    purpose: ApiKeyPurpose,
    created_by: UserId,    // The individual creating/using the key
) -> Result<String>
```

Query changes from:
```sql
SELECT secret FROM api_keys
WHERE user_id = $1 AND purpose = $2 AND hidden = true
```
to:
```sql
SELECT secret FROM api_keys
WHERE user_id = $1 AND purpose = $2 AND created_by = $3 AND hidden = true
```

**Callers to update:**

| Caller | Current args | With org context |
|--------|-------------|-----------------|
| `db/handlers/users.rs:140-141` (user creation) | `(user_id, Batch)` | `(user_id, Batch, user_id)` — individual creates own keys |
| `api/handlers/files.rs:781` (batch file upload) | `(current_user.id, Batch)` | `(target_user_id, Batch, current_user.id)` — target is org or self |
| `auth/middleware.rs:61` (playground proxy) | `(current_user.id, Playground)` | `(target_user_id, Playground, current_user.id)` — target is org or self |
| `api/handlers/auth.rs:204` (fusillade auth) | `(user_id, Batch)` | `(user_id, Batch, user_id)` — fusillade auth is always for the key's owner |

**Organization context resolution via HTTP header:**

The org context is conveyed via an `X-Organization-Id` HTTP header, sent by the client on each request. The dashboard stores the active org ID in `localStorage` and includes the header via a fetch wrapper. CLI tools and curl pass it directly as a header. This approach works uniformly across browsers, CLIs, and programmatic clients.

**Note:** API-key-authenticated requests (OpenAI SDK, curl with Bearer token) do NOT need this header. The API key's `user_id` already determines the org/individual context. The `X-Organization-Id` header is only relevant for session-authenticated requests (JWT cookie, proxy headers).

**Validation endpoint — `POST /admin/api/v1/session/organization`:**

```rust
/// Validate organization membership and return confirmed org ID.
/// The client stores the returned ID and sends it as X-Organization-Id header.
/// Body: { "organization_id": "<uuid>" } or { "organization_id": null }
pub async fn set_active_organization(...) -> Result<Json<SetActiveOrganizationResponse>> {
    // If organization_id is provided, verify membership
    if let Some(org_id) = request.organization_id {
        let role = repo.get_user_org_role(current_user.id, org_id).await?;
        if role.is_none() { return Err(Error::InsufficientPermissions { ... }); }
    }

    Ok(Json(SetActiveOrganizationResponse {
        active_organization_id: request.organization_id,
    }))
}
```

Route: `.route("/session/organization", post(api::handlers::organizations::set_active_organization))`

**Reading the header — `CurrentUser` extraction:**

During auth extraction in `current_user.rs`, after resolving the user, read the `X-Organization-Id` header and populate `CurrentUser.active_organization`:

```rust
// In current_user.rs, after user is resolved:
if let Some(header_value) = parts.headers.get("x-organization-id")
    && let Ok(value_str) = header_value.to_str()
    && let Ok(org_id) = value_str.parse::<UserId>()
{
    // Verify the user is a member of this organization
    if user.organizations.iter().any(|o| o.id == org_id) {
        user.active_organization = Some(org_id);
    }
}
```

**Handlers read from `CurrentUser` (no header parsing):**

```rust
// In handlers that use hidden keys (files.rs, middleware.rs):
let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

let api_key = api_keys_repo
    .get_or_create_hidden_key(target_user_id, ApiKeyPurpose::Batch, current_user.id)
    .await?;
```

**Why an HTTP header (not a cookie):**
- Works with CLIs and curl — no cookie jar needed, just pass `-H "X-Organization-Id: <uuid>"`
- Works with any HTTP client (SDKs, scripts, CI pipelines)
- Stateless — no server-side session state, no cookie management
- Dashboard uses `localStorage` for persistence across page refreshes
- Dashboard's fetch wrapper adds the header automatically
- For API key users (programmatic): no header needed — their key already has `user_id = org_id`

**Hidden key pre-creation for org members:**

When a user is added to an org (`Organizations::add_member`), hidden keys are NOT pre-created. They are created lazily on first use (same as the `get_or_create` pattern). This avoids creating keys for members who never use the dashboard.

---

## Phase 5: Fusillade Schema Changes

**File:** `fusillade/migrations/YYYYMMDD000000_add_api_key_id.up.sql`

```sql
-- Add api_key_id to batches for per-member usage attribution within orgs.
-- When a batch is created via the dashboard, the hidden batch API key used
-- is recorded here. JOIN through api_keys.created_by to get the individual.
ALTER TABLE batches ADD COLUMN api_key_id UUID;
CREATE INDEX idx_batches_api_key_id ON batches(api_key_id) WHERE api_key_id IS NOT NULL;

-- Add api_key_id to files for the same reason.
ALTER TABLE files ADD COLUMN api_key_id UUID;
CREATE INDEX idx_files_api_key_id ON files(api_key_id) WHERE api_key_id IS NOT NULL;
```

**How it's populated:**

In `api/handlers/files.rs`, after resolving the hidden batch key, we also resolve the key's UUID (not just its secret) and pass it to fusillade when creating the file and batch:

```rust
let (api_key_secret, api_key_id) = api_keys_repo
    .get_or_create_hidden_key_with_id(target_user_id, ApiKeyPurpose::Batch, current_user.id)
    .await?;
// Pass api_key_id to fusillade file/batch creation
```

This requires a small variant of `get_or_create_hidden_key` that returns both `(secret, id)`, or a separate lookup.

**Dashboard query for "filter batches by member within org":**

```sql
-- "Show Acme Corp's batches, filterable by which member created them"
SELECT b.*, ak.created_by as member_id, u.display_name as member_name
FROM batches b
LEFT JOIN api_keys ak ON ak.id = b.api_key_id
LEFT JOIN users u ON u.id = ak.created_by
WHERE b.created_by = '<org_id>'   -- created_by TEXT = org's user ID
ORDER BY b.created_at DESC;
```

The same pattern applies to files via `files.api_key_id`.

---

## Phase 6: What Does NOT Change

These are the critical systems that work unchanged because an org is a user:

| System | Why it works |
|--------|-------------|
| **Credit balance** | `credits_transactions.user_id` + `user_balance_checkpoints.user_id` — org user gets own ledger |
| **Onwards routing** | `user_balances` CTE in `load_targets_from_db` queries all users — org users included automatically |
| **API key billing** | `api_keys.user_id` → org user's API keys bill to org's balance |
| **Group access** | `user_groups` + `deployment_groups` — org user added to groups, its keys get access |
| **LISTEN/NOTIFY** | Existing triggers on `api_keys`, `user_groups`, `deployment_groups` fire for org users too |
| **Webhooks** | `user_webhooks.user_id` — org can have its own webhooks |
| **Tariffs** | Per-model pricing, no user dimension — works for orgs |
| **Payments** | `users.payment_provider_id` — org gets own Stripe customer |

---

## Phase 7: Dashboard Changes

### 7a. TypeScript Types (`api/control-layer/types.ts`)

- Add `user_type` to `User` interface
- Add `created_by` to `ApiKey` interface
- Add `Organization`, `OrganizationMember`, `OrganizationCreateRequest`, etc.
- Add `organizations` to `CurrentUser` type (array of `{ id, name, role }`)

### 7b. API Client (`api/control-layer/client.ts`)

- Add `organizations` namespace with all CRUD + membership methods
- Add `setActiveOrganization(orgId)` / `clearActiveOrganization()` methods that call `POST /session/organization` and store the org ID in `localStorage`
- Add a fetch wrapper that reads `activeOrganizationId` from `localStorage` and includes `X-Organization-Id` header on all requests when set

### 7c. Query Keys & Hooks (`keys.ts`, `hooks.ts`)

- Add `queryKeys.organizations` key factory
- Add hooks: `useOrganizations`, `useOrganization`, `useCreateOrganization`, `useOrganizationMembers`, `useAddMember`, `useRemoveMember`, `useUserOrganizations`
- Existing hooks for API keys, batches, etc. need org ID in query keys for cache isolation

### 7d. Context Switcher (new component)

- Dropdown in the header/sidebar showing: "Personal" + list of orgs from `currentUser.organizations`
- Selecting an org calls `POST /session/organization` to validate membership, then stores the org ID in `localStorage`
- React context stores `activeOrganizationId` for UI state (which pages to show, which user ID to query with)
- The API client's fetch wrapper reads `activeOrganizationId` from `localStorage` and includes `X-Organization-Id` header on all requests
- When an org is active, pages that show user-scoped data (API keys, credits, webhooks, usage) query with the org's user ID instead of the current user's ID
- Hidden-key endpoints (file upload, playground) automatically use the correct org context via the header — the `CurrentUser` extractor populates `active_organization` from the header
- Existing sub-resource endpoints already work: `GET /users/{org_id}/api-keys` returns the org's keys, provided the permission check passes (see Phase 3c)

### 7e. Organization Management Page (new feature component)

- New page at `/organizations` (or tab in Users & Groups)
- List orgs with search/pagination
- Create/edit org modals
- Member management: add/remove users, change roles
- Org detail view: shows balance, API keys, group memberships, usage
- Usage breakdown by API key (leveraging `api_key_id` on `http_analytics`)

### 7f. Batch Status Sorting (COR-88)

**Issue:** [COR-88](https://linear.app/doubleword/issue/COR-88) — In-progress batches should appear above other batches.

Currently batches are sorted by `created_at` only. Platform managers frequently see in-progress batches buried below completed ones. The fix is a composite sort: **status priority first, then timestamp**.

**Status priority order** (highest first):
1. `in_progress`, `validating`, `finalizing`, `cancelling` (active)
2. `completed`, `failed`, `expired`, `cancelled` (terminal)

**Implementation in `Batches.tsx` / `BatchesTable/columns.tsx`:**

- The sorting is **client-side** (already uses `getSortedRowModel()` from TanStack Table)
- Add a custom `sortingFn` to the Status column that maps status to a numeric priority
- Set Status as the **default sort column** (descending) with `created_at` as secondary
- Users can still click column headers to override

```typescript
// In columns.tsx — make Status column sortable with custom sort
{
  accessorKey: "status",
  header: "Status",
  sortingFn: (rowA, rowB) => {
    const priority: Record<BatchStatus, number> = {
      in_progress: 0, validating: 0, finalizing: 0, cancelling: 0,
      completed: 1, failed: 1, expired: 1, cancelled: 1,
    };
    return (priority[rowA.original.status] ?? 2) - (priority[rowB.original.status] ?? 2);
  },
}

// In Batches.tsx — set default sort state
const [sorting, setSorting] = useState<SortingState>([
  { id: "status", desc: false },   // Active batches first
  { id: "created_at", desc: true }, // Then newest first
]);
```

### 7g. Org-Scoped Batch/File Filtering by User

When viewing batches or files in organization context, add a **user filter dropdown** so org admins can filter by which member created each batch/file.

**Batches page changes (`Batches.tsx`):**

- When `activeOrganizationId` is set, show a "Member" filter dropdown above the table
- Dropdown populated from org members list (`useOrganizationMembers(orgId)`)
- Selected member filters batches client-side by matching `api_key_id` → `api_keys.created_by`
- Alternatively, the backend can accept a `created_by` query parameter for server-side filtering

**Files page changes (same component):**

- Same member filter dropdown when in org context
- Filters files by `api_key_id` → member attribution

**Existing infrastructure to leverage:**
- `useBatches()` already accepts `BatchesListQuery` — extend with optional `created_by` param
- `useFiles()` already accepts `FilesListQuery` — extend similarly
- `showUserColumn` flag in `BatchesTable/columns.tsx` (line 34) already exists for platform managers — reuse for org context

### 7h. Organization/Personal Toggle in AppSidebar

**File:** `dashboard/src/components/layout/Sidebar/AppSidebar.tsx`

Add an organization/personal context toggle in the expandable profile dropdown at the bottom of the sidebar.

**Design:**

- Above the existing menu items (Profile, Billing, Support, Logout) in the `DropdownMenuContent`, add an org switcher section
- Shows "Personal" (always) plus each org from `currentUser.organizations`
- Active context indicated with a checkmark or highlight
- Selecting an org calls `POST /session/organization` (validates membership), stores org ID in `localStorage`, and updates React context
- Selecting "Personal" calls the same endpoint with `null` and clears `localStorage`

```tsx
// In AppSidebar.tsx DropdownMenuContent:
<DropdownMenuLabel className="text-xs text-muted-foreground">
  Switch Account
</DropdownMenuLabel>
<DropdownMenuItem onClick={() => setActiveOrg(null)}>
  <User className="w-4 h-4 mr-2" />
  Personal
  {!activeOrgId && <Check className="w-4 h-4 ml-auto" />}
</DropdownMenuItem>
{currentUser?.organizations?.map(org => (
  <DropdownMenuItem key={org.id} onClick={() => setActiveOrg(org.id)}>
    <Building className="w-4 h-4 mr-2" />
    {org.name}
    {activeOrgId === org.id && <Check className="w-4 h-4 ml-auto" />}
  </DropdownMenuItem>
))}
<DropdownMenuSeparator />
<DropdownMenuItem onClick={() => navigate("/profile")}>
  ...existing items...
```

**State management:**

- `OrganizationContext` React context provides `activeOrganizationId` and `setActiveOrganization()`
- `setActiveOrganization()` calls the backend validation endpoint, stores org ID in `localStorage`, updates React context, and invalidates relevant queries
- The API client's fetch wrapper reads from `localStorage` and adds `X-Organization-Id` header to all requests
- Balance display in the header (`AppLayout`) switches to org balance when org is active

---

## Files to Create

| File | Description |
|------|-------------|
| `dwctl/migrations/072_add_organizations.sql` | dwctl migration |
| `fusillade/migrations/YYYYMMDD000000_add_api_key_id.up.sql` | Fusillade migration (+ `.down.sql`) |
| `dwctl/src/db/models/organizations.rs` | DB request/response models |
| `dwctl/src/db/handlers/organizations.rs` | DB repository |
| `dwctl/src/api/models/organizations.rs` | API request/response models |
| `dwctl/src/api/handlers/organizations.rs` | API route handlers |
| `dashboard/src/components/features/organizations/` | Dashboard feature components |

## Files to Modify

| File | Change |
|------|--------|
| `dwctl/src/db/handlers/users.rs` | Add `user_type` to User struct + queries; filter list by `individual`; pass `user_id` as `created_by` in hidden key pre-creation |
| `dwctl/src/db/models/users.rs` | Add `user_type` to `UserDBResponse` |
| `dwctl/src/db/handlers/mod.rs` | Register `organizations` module |
| `dwctl/src/db/models/mod.rs` | Register `organizations` module |
| `dwctl/src/api/models/users.rs` | Add `user_type` to `UserResponse` and `CurrentUser`; add `organizations` to `CurrentUser` |
| `dwctl/src/db/handlers/api_keys.rs` | Soft-delete; add `created_by` to create/queries; update `get_or_create_hidden_key` to accept `created_by`; add `is_deleted = false` filter |
| `dwctl/src/db/models/api_keys.rs` | Add `created_by` to `ApiKeyCreateDBRequest` and `ApiKeyDBResponse` |
| `dwctl/src/api/models/api_keys.rs` | Add `created_by` to `ApiKeyResponse` |
| `dwctl/src/api/handlers/api_keys.rs` | Add `can_manage_org_resource` check; pass `current_user.id` as `created_by` on create |
| `dwctl/src/api/handlers/webhooks.rs` | Add `can_manage_org_resource` check to all CRUD handlers |
| `dwctl/src/api/handlers/transactions.rs` | Add `can_manage_org_resource` check |
| `dwctl/src/api/handlers/files.rs` | Read `current_user.active_organization` for target; pass `(target_user_id, purpose, current_user.id)` to hidden key; pass `api_key_id` to fusillade |
| `dwctl/src/auth/middleware.rs` | Read `current_user.active_organization` for playground proxy; update `get_or_create_hidden_key` call with `created_by` |
| `dwctl/src/api/handlers/auth.rs` | Update `get_or_create_hidden_key` call with `created_by` |
| `dwctl/src/api/handlers/mod.rs` | Register `organizations` module |
| `dwctl/src/api/models/mod.rs` | Register `organizations` module |
| `dwctl/src/auth/permissions.rs` | Add `Organizations` resource, permission rules, `can_manage_org_resource` helper |
| `dwctl/src/auth/current_user.rs` | Populate `organizations` on `CurrentUser` during extraction; read `X-Organization-Id` header into `active_organization` field |
| `dwctl/src/types.rs` | Add `Organizations` to `Resource` enum |
| `dwctl/src/lib.rs` | Add organization routes |
| `dwctl/src/request_logging/analytics_handler.rs` | Populate `api_key_id` in http_analytics |
| `dwctl/src/db/handlers/credits.rs` | Accept and store `api_key_id` on transaction creation |
| `dwctl/src/db/models/credits.rs` | Add `api_key_id` to `CreditTransactionCreateDBRequest` |
| `dwctl/src/sync/onwards_config/mod.rs` | Add `AND ak.is_deleted = false` to API key queries in `load_targets_from_db` |
| `dashboard/src/api/control-layer/types.ts` | Add org types, `user_type`, `created_by` on API keys |
| `dashboard/src/api/control-layer/client.ts` | Add org API methods; add `setActiveOrganization` / `clearActiveOrganization` (localStorage + `X-Organization-Id` header) |
| `dashboard/src/api/control-layer/keys.ts` | Add org query keys |
| `dashboard/src/api/control-layer/hooks.ts` | Add org hooks |

## PR Sequence

Each PR is non-breaking — existing functionality is preserved at every step.

| PR | Repo | Scope | Key changes |
|----|------|-------|-------------|
| **1** | control-layer | Migration + all Rust backend | `072_add_organizations.sql` + all Phases 2–4: organization models/handlers/API, permissions, routes, CurrentUser changes, API key soft-delete + created_by + hidden key updates, attribution (api_key_id), org permission checks on existing handlers, header-based org context + validation endpoint, onwards sync filter |
| **2** | control-layer | Dashboard | TS types, API client, hooks, query keys. AppSidebar org toggle (7h), org management page (7e), batch status sorting COR-88 (7f) |
| **3** | fusillade | Schema + release | Migration: api_key_id on batches + files. Pass-through in creation. Release new version to crates.io |
| **4** | control-layer | Org-scoped batch/file filtering + batch UX | Bump fusillade version, pass api_key_id from file upload handler to fusillade, org-scoped batch/file member filtering in dashboard (7g), batch status sorting COR-88 (7f) |

**Why this order:**
- PR 1 merges migration with all backend logic that depends on it — avoids intermediate states where columns exist but aren't used, and keeps review context together
- PR 2 (dashboard) uses the backend APIs from PR 1 but doesn't need fusillade changes — org toggle and management page are independent
- PR 3 (fusillade) is a separate repo and needs to be released + published to crates.io before control-layer can consume it
- PR 4 ties it together: bumps the fusillade dependency, wires api_key_id through file upload, adds the member filter dropdown in the dashboard, and includes batch status sorting (COR-88) alongside the batches view updates

## Verification

1. **Migration**: `just db-start && just db-setup` — verify dwctl migration applies cleanly
2. **Fusillade migration**: Verify fusillade migration applies cleanly in its own DB
3. **Compile**: `cargo build` — verify sqlx compile-time checks pass (both dwctl and fusillade)
4. **Unit tests**: `just test rust` — existing tests pass, add new tests for org handlers
5. **Trigger test**: Attempt to insert an individual user's ID as `organization_id` in `user_organizations` — should fail with check_violation
6. **Hidden key constraint test**: Verify that two different members of an org can each get their own hidden batch key (unique on `(user_id, created_by, purpose)`)
7. **Manual test flow**:
   - Create org via POST `/admin/api/v1/organizations`
   - Add member via POST `/admin/api/v1/organizations/{id}/members`
   - As member, create API key for org via POST `/admin/api/v1/users/{org_id}/api-keys`
   - Verify `api_keys.created_by` = the member's user ID
   - Grant org group access via POST `/admin/api/v1/groups/{group_id}/users/{org_id}`
   - Use org API key for inference via `/ai/v1/chat/completions`
   - Verify usage billed to org's credit balance
   - Verify `http_analytics` row has `api_key_id` populated
   - Verify `credits_transactions` usage row has `api_key_id` populated
   - Delete an org API key, verify it's soft-deleted (`is_deleted = true`, not removed)
   - Verify deleted key no longer routes in onwards (no inference access)
   - Verify `credits_transactions` referencing the deleted key still resolves (JOIN works)
   - Upload a batch file in org context (send `X-Organization-Id` header), verify `files.api_key_id` and `batches.api_key_id` populated
   - Verify batch file uses org's hidden batch key (not user's personal one)
   - Filter batches by member via `api_keys.created_by` JOIN — verify correct attribution
   - Verify removing the last owner is rejected
   - Verify an admin cannot assign the `owner` role (only owners can)
8. **Lint**: `just lint rust && just lint ts`
9. **Dashboard**: `cd dashboard && npm run dev`:
   - Verify org toggle in sidebar profile dropdown — switching stores org ID in localStorage and adds `X-Organization-Id` header
   - Verify balance in header switches to org balance when org is active
   - Verify batches page shows in-progress batches above completed (COR-88)
   - Verify batches page shows member filter when in org context
   - Verify org management page: create org, add/remove members, change roles
   - Verify API keys page shows org keys when in org context
