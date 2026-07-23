# Configuration Reference

> Reference guide for configuring the Control Layer using YAML and environment variables.

The Control Layer uses a YAML configuration file with environment variable overrides for all settings.

## Configuration File Location

The configuration file is named `config.yaml` by default. The system checks:

1. Path specified via `--config` CLI flag
2. Path in `DWCTL_CONFIG` environment variable
3. `./config.yaml` in the current directory

For Docker deployments, mount your config at `/app/config.yaml`.

## Environment Variable Overrides

Any setting can be overridden with environment variables prefixed with `DWCTL_`. Nested keys use double underscores:

```bash
DWCTL_PORT=8080
DWCTL_AUTH__NATIVE__ENABLED=false
DWCTL_DATABASE__POOL__MAX_CONNECTIONS=20
```

The special `DATABASE_URL` variable (no prefix) sets the database connection string.

## Core Settings

### Server

```yaml
host: "0.0.0.0"
port: 3001
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | string | `"0.0.0.0"` | Network interface to bind to. Use `127.0.0.1` for local-only access. |
| `port` | integer | `3001` | TCP port for the HTTP server. |

### Secret Key

```yaml
secret_key: "your-secret-key-here"
```

Required when native authentication is enabled. Used for JWT signing. Generate with:

```bash
openssl rand -base64 32
```

### Admin User

```yaml
admin_email: "admin@example.com"
admin_password: "change-me-in-production"
```

Created on first startup if it doesn't exist. The admin user has the `PlatformManager` role.

> **Danger**
>
> Change the default admin password immediately in production!

## Database Configuration

The Control Layer requires PostgreSQL. Two modes are available:

### External Database (Recommended)

```yaml
database:
  type: external
  url: "postgres://user:pass@localhost:5432/control_layer"
  replica_url: "postgres://user:pass@replica:5432/control_layer"  # Optional
  pool:
    max_connections: 10
    min_connections: 0
    acquire_timeout_secs: 30
    idle_timeout_secs: 600
    max_lifetime_secs: 1800
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | string | - | PostgreSQL connection URL. |
| `replica_url` | string | - | Optional read replica URL. |
| `pool.max_connections` | integer | `10` | Maximum connections in pool. |
| `pool.min_connections` | integer | `0` | Minimum idle connections. |
| `pool.acquire_timeout_secs` | integer | `30` | Max wait for a connection. |
| `pool.idle_timeout_secs` | integer | `600` | Close idle connections after N seconds. |
| `pool.max_lifetime_secs` | integer | `1800` | Maximum connection lifetime. |

### Embedded Database

For development or single-node deployments, use the embedded PostgreSQL database:

```yaml
database:
  type: embedded
  data_dir: ".dwctl_data/postgres"
  persistent: false
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `data_dir` | string | - | Directory for database files. |
| `persistent` | boolean | `false` | Persist data between restarts. |

### Component Databases

The batch processing system (Fusillade) and request logging (Outlet) can use separate databases or schemas:

```yaml
database:
  # ... main database config ...
  fusillade:
    mode: schema      # Use schema in main database
    name: "fusillade"
    pool:
      max_connections: 20
      min_connections: 2
  outlet:
    mode: dedicated   # Use separate database
    url: "postgres://user:pass@localhost:5432/outlet"
    pool:
      max_connections: 5
```

## Authentication Configuration

At least one authentication method must be enabled.

### Native Authentication

Username/password authentication with session cookies:

```yaml
auth:
  native:
    enabled: true
    allow_registration: false
    password:
      min_length: 8
      max_length: 64
      argon2_memory_kib: 19456
      argon2_iterations: 2
      argon2_parallelism: 1
    session:
      timeout: "24h"
      cookie_name: "dwctl_session"
      cookie_secure: true
      cookie_same_site: "strict"
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `true` | Enable native login. |
| `allow_registration` | boolean | `false` | Allow self-registration. |
| `password.min_length` | integer | `8` | Minimum password length. |
| `password.max_length` | integer | `64` | Maximum password length. |
| `password.argon2_*` | integer | - | Argon2 hashing parameters. Lower values speed up tests. |
| `session.timeout` | duration | `"24h"` | Session expiration. |
| `session.cookie_secure` | boolean | `true` | Require HTTPS for cookies. |
| `session.cookie_same_site` | string | `"strict"` | SameSite attribute: `strict`, `lax`, or `none`. |

### Email for Password Resets

Configure email transport for password reset functionality:

**File transport** (development):
```yaml
auth:
  native:
    email:
      type: file
      path: "./emails"
      from_email: "noreply@example.com"
      from_name: "Control Layer"
      password_reset:
        token_expiry: "30m"
        base_url: "http://localhost:3001"
```

**SMTP transport** (production):
```yaml
auth:
  native:
    email:
      type: smtp
      host: "smtp.example.com"
      port: 587
      username: "noreply@example.com"
      password: "smtp-password"
      use_tls: true
      from_email: "noreply@example.com"
      from_name: "Control Layer"
      password_reset:
        token_expiry: "30m"
        base_url: "https://app.example.com"
```

### Proxy Header Authentication

For use with identity-aware proxies (SSO):

```yaml
auth:
  proxy_header:
    enabled: false
    header_name: "x-doubleword-user"
    email_header_name: "x-doubleword-email"
    groups_field_name: "x-doubleword-user-groups"
    provider_field_name: "x-doubleword-sso-provider"
    auto_create_users: true
    import_idp_groups: false
    blacklisted_sso_groups:
      - "external-contractors"
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable proxy header auth. |
| `header_name` | string | `"x-doubleword-user"` | Header with unique user ID. |
| `email_header_name` | string | `"x-doubleword-email"` | Header with user email. |
| `groups_field_name` | string | `"x-doubleword-user-groups"` | Header with comma-separated groups. |
| `auto_create_users` | boolean | `true` | Create users automatically. |
| `import_idp_groups` | boolean | `false` | Sync groups from IdP. |
| `blacklisted_sso_groups` | list | `[]` | Groups to exclude from import. |

### Default User Roles

Roles assigned to new users (admin excluded):

```yaml
auth:
  default_user_roles:
    - StandardUser
    - BatchAPIUser
```

Available roles:
- `StandardUser` - Base access (always included, cannot be removed)
- `RequestViewer` - Read-only access to request logs
- `BillingManager` - Credit and billing management
- `BatchAPIUser` - Batch file and job management

### Security Settings

```yaml
auth:
  security:
    jwt_expiry: "24h"
    cors:
      allowed_origins:
        - "https://app.example.com"
      allow_credentials: true
      max_age: 3600
      exposed_headers:
        - "location"
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `jwt_expiry` | duration | `"24h"` | JWT token lifetime. Must be 5min-30days. |
| `cors.allowed_origins` | list | `["http://localhost:3001"]` | Allowed CORS origins. Use `"*"` for any. |
| `cors.allow_credentials` | boolean | `true` | Allow credentials. Cannot use with wildcard origin. |
| `cors.max_age` | integer | `3600` | Preflight cache duration (seconds). |

> **Warning**
>
> In production, ensure your frontend URL is listed in `allowed_origins`.

## Credits & Payments

### Initial Credits

```yaml
credits:
  initial_credits_for_standard_users: 10.00
```

Credits given to new users on creation. Set to `0` to disable.

### Payment Provider

**Stripe** (production):
```yaml
payment:
  stripe:
    api_key: "sk_live_..."
    webhook_secret: "whsec_..."
    price_id: "price_..."
    host_url: "https://app.example.com"
    enable_invoice_creation: false
```

| Field | Type | Description |
|-------|------|-------------|
| `api_key` | string | Stripe secret key (starts with `sk_`). |
| `webhook_secret` | string | Webhook signing secret (starts with `whsec_`). |
| `price_id` | string | Stripe price ID for credit purchases (starts with `price_`). |
| `host_url` | string | Base URL for success/cancel redirects. |
| `enable_invoice_creation` | boolean | Create invoices for sessions. |

**Dummy provider** (testing):
```yaml
payment:
  dummy:
    amount: 50.00
    host_url: "http://localhost:3001"
```

Adds a fixed amount without real payment processing.

## Batches Configuration

Configure the batch inference API:

```yaml
batches:
  enabled: true
  allowed_completion_windows:
    - "24h"
    - "1h"
    - "48h"
  files:
    max_file_size: 104857600        # 100 MB
    default_expiry_seconds: 86400   # 24 hours
    min_expiry_seconds: 3600        # 1 hour
    max_expiry_seconds: 2592000     # 30 days
    upload_buffer_size: 100
    download_buffer_size: 100
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `true` | Enable `/ai/v1/files` and `/ai/v1/batches` endpoints. |
| `allowed_completion_windows` | list | `["24h"]` | SLA options users can select. |
| `files.max_file_size` | integer | `104857600` | Maximum upload size in bytes. |
| `files.default_expiry_seconds` | integer | `86400` | Default file retention. |

## Background Services

### Onwards Sync

Synchronizes configuration to the AI proxy routing layer:

```yaml
background_services:
  onwards_sync:
    enabled: true
```

> **Note**
>
> Disable only if you're not using the AI proxy functionality.

### Probe Scheduler

Runs health checks against model endpoints:

```yaml
background_services:
  probe_scheduler:
    enabled: true
```

Only runs on the leader instance when leader election is enabled.

### Batch Daemon

Processes batch inference jobs:

```yaml
background_services:
  batch_daemon:
    enabled: "leader"              # "always", "leader", or "never"
    claim_batch_size: 100
    default_model_concurrency: 10
    claim_interval_ms: 1000
    max_concurrent_state_writes: 64
    max_concurrent_response_reads: 8
    max_retries: 1000
    first_chunk_timeout_ms: 86400000
    chunk_timeout_ms: 86400000
    body_timeout_ms: 86400000
    claim_timeout_ms: 60000
    processing_timeout_ms: 600000  # Compatibility/dispatch-URL TTL input
    backoff_ms: 1000
    backoff_factor: 2
    max_backoff_ms: 10000
    stop_before_deadline_ms: 900000  # 15 min safety buffer
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | string | `"leader"` | When to run: `always`, `leader`, or `never`. |
| `claim_batch_size` | integer | `100` | Requests claimed per iteration. |
| `default_model_concurrency` | integer | `10` | Concurrent requests per model. |
| `max_concurrent_state_writes` | integer | `64` | Response-related state-write class limit, capped by shared pool headroom. `0` disables only this class limit. |
| `max_concurrent_response_reads` | integer | `8` | Consolidated response-detail read class limit. `0` disables only this class limit. |
| `max_retries` | integer | `1000` | Max retry attempts. `null` = unlimited until deadline. |
| `first_chunk_timeout_ms` | integer | `86400000` | Maximum wait for response headers. |
| `chunk_timeout_ms` | integer | `86400000` | Maximum idle gap between streamed response chunks. |
| `body_timeout_ms` | integer | `86400000` | Maximum total response-body collection time. |
| `claim_timeout_ms` | integer | `60000` | Maximum age of a tokenized pre-dispatch claim before that exact attempt is revoked and the row returns to pending. |
| `processing_timeout_ms` | integer | `600000` | Compatibility value and image-normalizer dispatch-URL TTL input. It is not a processing reclaim deadline. |
| `stop_before_deadline_ms` | integer | `900000` | Stop retrying before deadline (15 min buffer). |

Response reads and writes also share an aggregate budget of
`max_connections - 2`, with a minimum of one permit. This preserves two
primary-pool connections when the pool has at least three; a one-connection
pool keeps one admitted operation for liveness and cannot reserve headroom.

#### Attempt ownership and recovery

Each new daemon claim carries a unique attempt token. A tokenized row that
remains `claimed` beyond `claim_timeout_ms` may be returned to `pending`
because any later write from that exact attempt is fenced out. Once the row is
`processing`, age alone does not make it reclaimable. Recovery requires
positive evidence that its daemon owner is missing, marked dead, or
heartbeat-stale.

The heartbeat interval (5 seconds), stale-owner threshold (30 seconds), and
reclaim batch size (100 rows) are internal compatibility defaults rather than
additional configuration keys. Legacy NULL-token claims also require an
unavailable owner before reclamation. The maintenance loop uses the same
cadence and batch bound to repair `pending` rows carrying a residual non-NULL
`attempt_id`. Repair defensively clears the token and all daemon ownership
timestamps before the row is claimed.

The `attempt_id` database columns are additive and nullable for rolling
deployment:

- During a mixed-version rollout, legacy pods continue to create NULL-token
  claims. Exact-attempt fencing is complete only after every daemon pod runs the
  new version.
- Roll back application code without rolling back the schema. Legacy code does
  not maintain the token lifecycle and may leave ownership residue on a
  `pending` row. A later roll-forward needs no manual cleanup: upgraded
  maintenance repairs those rows in bounded batches. Legacy pods can recreate
  residue during a mixed-version rollout, so convergence is complete only
  after they are gone.
- The database down migration refuses to remove the columns while any token is
  non-NULL.

Canceled rows that were already in flight remain live for
`batch_archive_cancel_grace_secs` (default 600 seconds), allowing a late billed
result from the owning attempt to supersede best-effort cancellation. Once the
batch is archived, the live row is gone and that late-write opportunity is
revoked: persistence observes a missing live row and discards the late result.
The grace is a fixed boundary independent of `processing_timeout_ms`.

#### Model Escalation

Route requests to fallback models when approaching SLA deadlines:

```yaml
background_services:
  batch_daemon:
    model_escalations:
      "llama-3.1-70b":
        escalation_model: "gpt-4o-mini"
        escalation_api_key: "OPENAI_API_KEY"  # Environment variable name
    sla_check_interval_seconds: 60
    sla_thresholds:
      - name: "warning"
        threshold_seconds: 3600
        action: "log"
        allowed_states: ["pending"]
      - name: "critical"
        threshold_seconds: 900
        action: "escalate"
        allowed_states: ["pending", "claimed"]
```

### Leader Election

For multi-instance deployments:

```yaml
background_services:
  leader_election:
    enabled: true
```

Uses PostgreSQL advisory locks. Only the leader runs probe scheduler and batch daemon (when set to `"leader"` mode).

## Model Sources

Seed model endpoints on first startup:

```yaml
model_sources:
  - name: "openai"
    url: "https://api.openai.com"
    api_key: "sk-..."
    sync_interval: "30s"
    default_models:
      - name: "gpt-4o"
        add_to_everyone_group: true
      - name: "gpt-4o-mini"
        add_to_everyone_group: true
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | - | Identifier for the model source. |
| `url` | string | - | Base URL of the OpenAI-compatible API. |
| `api_key` | string | - | API key for authentication. |
| `sync_interval` | duration | `"10s"` | How often to refresh model list. |
| `default_models` | list | - | Models to auto-import on first run. |

> **Note**
>
> Model sources are only seeded on first startup. After that, manage endpoints through the UI or API.

## Metadata

UI display settings:

```yaml
metadata:
  region: "UK South"
  organization: "ACME Corp"
  title: "ACME AI Gateway"
  docs_url: "https://docs.example.com"
  docs_jsonl_url: "https://docs.example.com/jsonl"
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `region` | string | - | Region displayed in UI header. |
| `organization` | string | - | Organization name in UI header. |
| `title` | string | - | Custom browser tab title. |
| `docs_url` | string | `"https://doublewordai.github.io/control-layer/"` | Documentation link in header. |
| `docs_jsonl_url` | string | - | JSONL docs link in batch upload modal. |

## Observability

### Metrics

```yaml
enable_metrics: true
```

Exposes Prometheus metrics at `/internal/metrics`.

### Request Logging

```yaml
enable_request_logging: true
```

Logs all AI proxy requests and responses to PostgreSQL. Disable if you have sensitive data.

### OpenTelemetry

```yaml
enable_otel_export: false
```

Exports traces via OTLP. Configure the exporter endpoint with standard OpenTelemetry environment variables (`OTEL_EXPORTER_OTLP_ENDPOINT`, etc.).

## Sample Files

Generate sample JSONL files for new users:

```yaml
sample_files:
  enabled: true
  requests_per_file: 2000
```

## Validation

The system validates configuration on startup and fails if:

- Native auth is enabled but `secret_key` is missing
- No authentication method is enabled
- `jwt_expiry` is outside 5min-30day range
- CORS uses wildcard origin with credentials enabled
- Database URL is invalid or unreachable

Run validation without starting the server:

```bash
dwctl --config config.yaml --validate
```
