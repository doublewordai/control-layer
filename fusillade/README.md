# Fusillade

A batching system for HTTP requests with retry logic and per-model concurrency
control. The workspace is split into three independently versioned crates:
`fusillade-core` owns shared request, batch, daemon-record, and storage-trait
types; `fusillade-arsenal` owns PostgreSQL storage, migrations, and database
retry behavior; and `fusillade` owns the scheduling daemon runtime. Arsenal is
named for the place that holds queued rounds before the daemon fires them.

Lists of requests can be dispatched as 'files', from which 'batches' can be
spawned. The behaviour is inspired by the OpenAI [Batch API](https://platform.openai.com/docs/guides/batch).

## Usage

Create a file with a list of request 'templates'. Create a batch from that file
to execute all of its requests. Then track progress of each request in the
batch as they're executed by the daemon.

- **Files** group related request templates
- **Request templates** define HTTP requests (endpoint, method, body, API key)
- **Batches** snapshot all templates in a file and start executing them.
Multiple batches can be triggered from a single file.
- **Requests** are created from templates (one per batch) and progress through
states as the daemon processes them

### Basic Example

```rust
use fusillade::{BatchInput, DaemonConfig, PostgresDaemon, RequestTemplateInput};
use fusillade_arsenal::{PostgresRequestManager, Storage, TestDbPools};
use std::sync::Arc;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

// Setup
let pool = PgPool::connect("postgresql://localhost/fusillade").await?;
let pools = TestDbPools::new(pool).await?;
let config = DaemonConfig::default();
let store = Arc::new(PostgresRequestManager::new(pools, (&config).into()));

// Start the daemon
let shutdown_token = CancellationToken::new();
let daemon = Arc::new(PostgresDaemon::from_store(store.clone(), config));
let daemon_handle = daemon.run(shutdown_token.clone())?;

// Create a file with request templates
let file_id = store.create_file(
    "completions".to_string(),
    Some("GPT-4 completions batch".to_string()),
    vec![
        RequestTemplateInput {
            endpoint: "https://api.openai.com".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}"#.to_string(),
            model: "gpt-4".to_string(),
            api_key: env::var("OPENAI_API_KEY")?,
        },
    ],
).await?;

// Launch a batch from that file
let batch = store.create_batch(BatchInput {
    file_id,
    endpoint: "/v1/chat/completions".to_string(),
    completion_window: "24h".to_string(),
    metadata: None,
    created_by: None,
    api_key_id: None,
    api_key: None,
    total_requests: None,
}).await?;

// Check the status of the batch
let status = store.get_batch_status(batch.id).await?;
println!("Completed: {}/{}", status.completed_requests, status.total_requests);
```

### Concurrency Control

Fusillade allows setting per-model concurrency limits:

```rust
use std::sync::Arc;
use fusillade::DaemonConfig;
use fusillade_arsenal::{PostgresRequestManager, TestDbPools};

let mut config = DaemonConfig {
    max_retries: 3,
    backoff_ms: 1000,
    ..Default::default()
};
config.model_concurrency_limits.insert("gpt-4".to_string(), 5); // Max 5 concurrent GPT-4 requests
config.model_concurrency_limits.insert("gpt-3.5-turbo".to_string(), 20);

let pools = TestDbPools::new(pool).await?;
let store = Arc::new(PostgresRequestManager::new(pools, (&config).into()));
```

### Database Retry Cadence

`fusillade-arsenal` can retry transient SQLx pool-acquire failures such as
`pool timed out while waiting for an open connection`. Configure the cadence on
the Postgres client:

```rust
use fusillade_arsenal::{DbRetryConfig, PostgresRequestManager, TestDbPools};
use std::time::Duration;

let pools = TestDbPools::new(pool).await?;
let store = PostgresRequestManager::new(pools, Default::default())
    .with_db_retry_config(DbRetryConfig::new(vec![
        Duration::from_millis(25),
        Duration::from_millis(100),
        Duration::from_millis(250),
    ]));
```

### Tracking Requests

To get the status of all requests in a batch:

```rust
// Get all requests for a batch
let requests = store.get_batch_requests(batch_id).await?;

for req in requests {
    match req {
        AnyRequest::Completed(r) => {
            println!("Request {} completed: {}", r.data.id, r.state.response_body);
        }
        AnyRequest::Failed(r) => {
            println!("Request {} failed: {}", r.data.id, r.state.error);
        }
        _ => {}
    }
}
```

## Background work

Fusillade accepts no-SLA background work in both of its submission shapes. A
file-backed background batch retains normal batch status, cancellation, and
output/error files:

```rust
use fusillade::BackgroundBatchInput;

let batch = store.create_background_batch(BackgroundBatchInput {
    file_id,
    endpoint: "/v1/chat/completions".to_string(),
    metadata: None,
    created_by: Some("user-id".to_string()),
    api_key_id: None,
    api_key: None,
    total_requests: None,
}).await?;
```

An asynchronous request can enter the same queue without a batch:

```rust
use fusillade::CreateBackgroundInput;
use uuid::Uuid;

let request_id = Uuid::new_v4();
store.create_background(CreateBackgroundInput {
    request_id,
    body: r#"{"model":"gpt-4","input":"hello"}"#.to_string(),
    model: "gpt-4".to_string(),
    endpoint: "https://api.example.com".to_string(),
    method: "POST".to_string(),
    path: "/v1/responses".to_string(),
    api_key: "key".to_string(),
    created_by: "user-id".to_string(),
}).await?;
```

Background admission is evaluated independently per model. Configure an
ordinary model limit and a lower foreground threshold for background sends:

```rust
let mut config = DaemonConfig {
    background_concurrency_limit: 50,
    inject_deadline_priority: true,
    ..Default::default()
};
config.model_concurrency_limits.insert("gpt-4".to_string(), 100);
```

The background threshold is clamped to each daemon process's ordinary model
limit. Only foreground work consumes it: with limits 100/50, 70 or 50
foreground requests in flight block background sends, while 49 makes
background work eligible. Already-dispatched background requests do not consume
model or user foreground in-flight limits and do not reserve database headroom.
Pending, immediately schedulable foreground work for a model blocks background
claims. A later foreground arrival can still use the full ordinary capacity.

Background processing also requires:

- PostgreSQL storage support for background claims;
- an explicit latest `live` model-filter event (no event is not eligible); and
- `inject_deadline_priority = true`. Background always overwrites caller
  `nvext.agent_hints.priority` with `i32::MIN`; SLA priority is clamped above
  that reserved value.

Background workers mirror `DaemonMode`: `RequestOnly` starts the foreground and
background batchless workers, `BatchOnly` starts the foreground and background
batch workers, and `Both` starts all four when batch claiming is supported.
Background workers read foreground counters but never update them or acquire
the foreground claim mutex. Database-wide due-foreground gating and exact-live
gating apply in all modes, and node-level background priority remains strictly
lowest.

Background batches persist `service_tier = "background"` with
`completion_window = NULL` and `expires_at = NULL`. They do not expire, escalate,
or fail retries based on a completion deadline. A zero
`background_concurrency_limit` disables processing but not submission,
inspection, or cancellation.

## Automated content retention

Retained-response maintenance is an additive daemon capability configured
with `RetentionMaintenanceConfig`; it is deliberately outside the serialized
`DaemonConfig` so existing exhaustive configuration literals remain source
compatible. The default contains no retention duration and enables no data
movement or retirement.

Current scheduled lifecycle support is limited to terminal batchless response
graphs in the `priority`, `flex`, and `background` service tiers. Every
configured tier needs an explicit positive duration, and batchless policy also
requires an explicit positive late-writer fence. Scheduled file retention and
terminal file-backed batch retention are rejected until their complete payload
lifecycle is supported. Explicit deletion continues through the ordinary
orphan purge and is independent of scheduled retention.

The effective batch claim owner runs archive maintenance. It ensures the
retained-response partition runway before enabling a batchless mover, then
refreshes both archive partition families daily. Request-only daemons perform
no archive DDL or movement. Steady and backfill movement are independently
disabled by default and share the existing archive workers' pacing; both use
positive graph and byte budgets. Each tick resolves one immutable observation,
dwell, and cancellation-grace boundary for every graph considered in that
pass. The dwell and grace must each be shorter than every enabled retention
period.

The retirement control is independently disabled and does not run a scheduled
retirement task in this release. Movement and partition maintenance emit only
aggregate counts and fixed phase labels, never request identifiers or content.

Ordinary pending-count queries exclude background demand. To expose it, use an
explicit `ServiceTierFilter::Include(vec![Some("background".to_string())])`; results use a
separate `"background"` bucket per model and combine batched and batchless
backlog. The bucket is hidden while the model is not live, due SLA work exists,
or active foreground work meets the configured background threshold. Active
background rows do not hide additional eligible background demand.

Each background worker uses one atomic `FOR UPDATE SKIP LOCKED` claim query.
Background claims do not take per-model advisory locks or reserve the observed
foreground headroom. Concurrent background workers may therefore both dispatch
against the same headroom; the reserved lowest node priority keeps that queued
work behind later foreground sends.

## Claim daemons

The daemon can run two foreground claim loops and two independent background
claim loops:

- **Request daemon** — claims *batchless* pending rows (flex/async responses).
  Owns the leaky-bucket and deadline-ramp policy: rows for models that are not
  live trickle out at a bounded rate per `(user, window, model)`.
- **Batch daemon** — claims rows belonging to batches. It first selects the
  top-ranked batches per capacity-eligible model (fairness + deadline
  ordering), then claims rows only from those batches, so claim cost is
  bounded by batches selected rather than total pending backlog.
- **Background request daemon** — claims batchless background rows using
  `claim_interval_ms` and `claim_batch_size`, only for explicitly live models,
  below the foreground threshold, and after due foreground backlog is empty.
- **Background batch daemon** — claims file-backed background rows using
  `batch_claim_interval_ms`, `batch_claim_size`, and
  `batch_claim_batch_size`, with the same liveness, threshold, and
  due-foreground gates.

Batch claiming is gated on model liveness via the `model_filters` event log:
models whose latest event is `live` are always claimable; models with **no**
events (external / always-on providers not managed by a controller) are
claimable unless `batch_claim_require_live` is set; models explicitly
`coming`/`absent` are claimable only via the **deadline ramp** — within
`window_minutes ^ claim_ramp_exponent` minutes of the batch deadline, rows are
claimed at full capacity regardless of liveness so they can overflow to
fallback providers rather than miss their window.

Configuration (all optional):

| Knob | Default | Meaning |
|---|---|---|
| `batch_claim_size` | `0` (inherit `claim_batch_size`) | max rows per batch-claim iteration |
| `batch_claim_batch_size` | `4` | batches selected per model per iteration (spill-over pool) |
| `batch_claim_interval_ms` | `0` (inherit `claim_interval_ms`) | batch loop cadence |
| `batch_claim_require_live` | `false` | require an explicit `live` event to batch-claim |
| `background_concurrency_limit` | `0` | per-model foreground threshold below which background workers may send; zero disables them |
| `claim_ramp_exponent` | `0.56` | deadline-ramp curve (~59 min for 24h windows, ~10 min for 1h) |

**Breaking changes relative to v19:**

- `Storage::claim_requests` is now a **batchless-only** compatibility alias;
  batched rows are claimed exclusively via `Storage::claim_batch_requests`.
  Custom storage backends must implement `claim_batch_requests` (the default
  implementation errors) or opt out with `supports_batch_claims() -> false`.
- The **leaky-bucket trickle no longer applies to batched rows** (flex is
  unchanged); not-live batches wait for liveness or the deadline ramp.
- Claim metrics (`fusillade_claim_capacity`, `fusillade_claim_duration_seconds`,
  `fusillade_claim_size`) gained a `daemon` label (`request_daemon` /
  `batch_daemon` / `background_request_daemon` /
  `background_batch_daemon`). Unlabeled legacy series are dual-emitted only by
  foreground workers during the deprecation window; background metrics are
  labeled-only so they cannot overwrite foreground dashboard values.

## Database Setup

Run migrations before first use, by importing the migrator and executing it against your database pool:

```rust
fusillade_arsenal::migrator().run(&pool).await?;
```
