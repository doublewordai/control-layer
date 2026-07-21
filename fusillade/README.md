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

## Claim daemons

As of v20, the daemon runs **two independent claim loops** instead of one:

- **Request daemon** — claims *batchless* pending rows (flex/async responses).
  Owns the leaky-bucket and deadline-ramp policy: rows for models that are not
  live trickle out at a bounded rate per `(user, window, model)`.
- **Batch daemon** — claims rows belonging to batches. It first selects the
  top-ranked batches per capacity-eligible model (fairness + deadline
  ordering), then claims rows only from those batches, so claim cost is
  bounded by batches selected rather than total pending backlog.

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
  `batch_daemon`); unlabeled legacy series are dual-emitted during a
  deprecation window.

## Database Setup

Run migrations before first use, by importing the migrator and executing it against your database pool:

```rust
fusillade_arsenal::migrator().run(&pool).await?;
```
