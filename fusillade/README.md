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

### Adaptive concurrency

With `adaptive_concurrency` on, each daemon discovers a model's sustainable
in-flight count from downstream backpressure instead of trusting the configured
number, which is too high at one model replica and far too low at a hundred.

`model_concurrency_limits` are where each model **starts**; from there the
controller owns the number, and `max_total_in_flight` bounds the process. There is
no per-model ceiling: memory is total in-flight times request size across all
models, so a per-model cap would not correspond to the risk, and capping the
controller would leave the "far too low at a hundred replicas" half unfixed.

Turning it off returns every model to its configured value exactly as before, so
the flag is safe to flip in either direction.

The limit moves multiplicatively in both directions:

- **Down** on an HTTP 529, by `adaptive_cut_factor`.
- **Up** by `adaptive_growth_factor`, once per claim cycle, for any model that
  used every slot it was offered on the last claim. A model that used fewer had
  run out of work, so a bigger limit would sit unused until a burst dispatched
  the lot at once.

Nothing here is sized to the fleet: a model at 500 and one at 50,000 take the
same number of steps to move by the same proportion. That matters because Dynamo
rejects by priority, so batch work is pushed down to almost no concurrency
whenever realtime traffic is busy and has to climb straight back afterwards -
and the controller cannot tell "the model is full" from "I am being outranked".

Each request is stamped with a counter that is bumped on every adjustment, and a
529 carrying an old stamp is discarded. In-flight work is never cancelled, so
after a cut the requests sent under the old limit keep failing for up to a
request lifetime; reacting to those would cut repeatedly for a single overload
event, and a scale-down evicting thousands at once would drive the limit to 1.
Genuinely sustained overload keeps producing fresh reports and keeps cutting.

The cost is that finding the wall means overshooting it, so in steady state a
fraction of requests are rejected. They are retried rather than lost, but each
is wasted work and a database write, and `adaptive_growth_factor` sets how much
of it there is.

If a cut puts the limit below what is already running, the daemon stops claiming
for that model until enough requests drain.

Only exact HTTP 529 counts as overload. Timeouts, network errors and 5xx
generally are failures of a request that was admitted, so they say nothing about
capacity. Note that onwards' own concurrency limiter returns 429, not 529.

Only foreground work drives the controller. Background work is opportunistic,
runs on top of the foreground limit rather than inside it, and is admitted only
while foreground is quiet - so its rejections mean background overflowed, not
that the foreground ceiling is too high.

State is local to each daemon process and shared by all of its claim loops.
Nothing is coordinated between replicas or between the separately-deployed batch
and non-batch daemons; how a model's capacity divides between them is settled
downstream by Dynamo's priority-based rejection, not here.

Off by default. Set `max_total_in_flight` before turning it on. Watch
`fusillade_adaptive_concurrency_limit` against `dwctl_model_batch_capacity`: a
limit that climbs without ever being cut means something that does not speak 529
is in the way, most likely onwards' own concurrency limit, which returns 429.

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

The daemon can apply operator-defined retention rules without embedding a
retention schedule in the library. `DaemonConfig::retention` accepts rules for
existing file deadlines, terminal file-backed batches, and terminal
batchless tiers. `retention_sweep_interval_ms` controls the worker
cadence; both are disabled by default and must be enabled together.

Each sweep resolves relative periods to one set of absolute cutoffs, then:

- retires due files only when they are not referenced by active batches;
- soft-deletes batches only after their terminal counters are frozen and the
  existing `batch_archive_cancel_grace_secs` window no longer protects a
  canceled, previously claimed request;
- hard-deletes eligible terminal batchless requests; and
- for a canceled request that was already dispatched, erases request,
  response, template, and response-step content while retaining the minimal
  lifecycle row needed to accept a late billed result. A later sweep
  hard-deletes that row if the result supersedes the cancellation.

Batchless rules accept the persisted `priority`, `flex`, and `background`
service tiers. Any unconfigured tier remains subject to explicit deletion.

New request-template payloads are written to weekly partitions through a
compatibility layer. The pre-cutover table is renamed in place and is not
copied; it temporarily holds its existing payload rows plus content-free UUID
routing stubs so the existing request foreign key remains valid. Bulk file
ingest uses a set-based dual write to avoid per-template trigger overhead. A
narrow file-to-week route keeps file reads set-oriented: each owning partition
is range-scanned once instead of being probed once per template.
The daily archive maintenance tick pre-creates both archive and template
partitions.

Scheduled batch content is retired by concurrently detaching and dropping a
whole eligible archive partition. Scheduled template content is retired the
same way once no active file, live request, or undeleted batch needs any
payload in that weekly partition. A small journal makes an interrupted
detach/finalize/drop sequence
resumable. Explicit deletion remains row-targeted and does not wait for a
partition boundary. Pre-cutover template payloads remain in their original
generation; `request_template_legacy_retirement_ready(minimum_age)` is the
gate for a one-time follow-up migration that drops that generation after the
deployment retention age has elapsed and no live reference still needs it.

Root selection is ordered, limited, and uses `FOR UPDATE SKIP LOCKED`. A
nonblocking database lease prevents multiple daemon replicas from running the
same phase concurrently. Each tick processes at most four bounded chunks so a
backlog cannot monopolize the worker. A daemon also rejects enabled retention at startup
unless its storage backend explicitly advertises sweep support. The
ordinary orphan purge subsequently removes explicitly deleted request/template
content. Scheduled archived content stays physically intact only until its
whole weekly partition becomes eligible, avoiding row-delete churn. Aggregate
metrics report sweep duration, affected rows by category, errors, and whether
the current work budget was exhausted; no request identifiers or content are
emitted.

Deployments should supply their own periods and worker cadence through runtime
configuration. Pre-create all four candidate indexes with `CONCURRENTLY` in
each applicable environment before deploying the migration/release, following
the statements in the retention-sweep migration. Use the application's
component-schema `search_path` (or schema-qualify both indexes and tables), and
verify `pg_index.indisready` and `pg_index.indisvalid` are both true for all
four indexes. The migration uses a short lock timeout for its metadata-only
rename and column additions. Enable the rules and worker cadence only after
the new application generation is healthy. Keep the pre-cutover generation
until the readiness function returns true for the configured retention age;
the final drop is intentionally a separate deployment so it is never coupled
to the initial cutover.

The schema cutover is roll-forward-only after the first retained template is
written: the down migration deliberately refuses to discard that data, and an
older orphan-purge implementation cannot row-lock the compatibility union.
Drain older daemon pods before allowing retained writes, and do not roll back
to an older binary after that boundary. Recovery should deploy a corrected
forward-compatible release; the initial migration can be reverted only while
its retained generation is still empty.

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
| `adaptive_concurrency` | `false` | discover each model concurrency from downstream 529s, starting from its configured value |
| `adaptive_growth_factor` | `1.5` | multiplier applied each time a limit goes up |
| `adaptive_cut_factor` | `0.8` | multiplier applied to the limit on downstream 529 |
| `max_total_in_flight` | `0` | hard cap on the process total in-flight across all models; zero disables |

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
