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

The effective batch claim owner runs archive maintenance. Weekly archive DDL
starts asynchronously and never delays claim-loop startup. A separate retained
gate maintains the continuous daily range from tomorrow through the furthest
configured retention horizon plus its runway. Request-only daemons perform no
archive DDL or movement. Steady and backfill movement are independently
disabled by default and share the existing archive workers' pacing; both use
positive graph and byte budgets. Each tick resolves one immutable observation,
dwell, and cancellation-grace boundary for every graph considered in that
pass. The dwell and grace must each be shorter than every enabled retention
period.

Before enabling retained-response movement, an operator must build the exact
payload-free candidate index outside a transaction:

```sql
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_batchless_retention_due
  ON requests (
    service_tier,
    (CASE state WHEN 'completed' THEN completed_at
                WHEN 'failed' THEN failed_at
                WHEN 'canceled' THEN canceled_at END),
    id
  )
  WHERE batch_id IS NULL
    AND state IN ('completed', 'failed', 'canceled');
```

Verify its keys, predicate, validity, and readiness through the migration-owned
guard rather than by name alone:

```sql
SELECT retained_response_archive_index_ready(current_schema());
```

The safe enable order is: deploy the archive-aware schema and readers; create
the index concurrently; verify the readiness query returns `true`; configure
the retention durations, late-writer fence, and partition runway while movers
remain disabled; then enable the steady mover and, if needed, backfill. Both
the exact index and continuous runway are checked fail closed. Missing,
invalid, or transiently unavailable prerequisites prevent batchless movement,
and bounded background checks allow movement to recover without a restart
once both become ready.

Daily partition retirement is independently disabled by default. When
enabled, the archive-maintenance owner retires at most one expired
retained-response partition per pass: it journals the exact partition
identity (schema, schema OID, parent OID, child name and OID, and daily
bounds), fences the bucket `retiring` in the same transaction so every read
fails closed, detaches the partition concurrently, drops only the exact
journaled relation, and records completion durably. PostgreSQL's own UTC
date decides eligibility; a partition for the current day is retired only
after the database clock passes its `delete_on`, never early and never from
the pod clock. Recovery resumes an unfinished journal before selecting new
work, refuses any renamed, replaced, rebounded, or reparented relation, and
treats lock or statement timeouts as retryable no-ops.

Retirement DDL requires an explicitly installed single-session maintenance
pool (`with_partition_maintenance_pool`, max one connection, min zero) whose
target is attested against the primary at startup; server-side lock and
statement timeouts are set on that session. Enabling the flag without the
pool fails startup validation, and an unfinished journal must keep an
enabled owner until it completes — disabling retirement mid-journal is not a
rollback. After physical retirement, every retained identifier is moved to a
durable content-free resurrection fence before its routes are deleted in
bounded chunks by an independent cleanup phase.

Movement and partition maintenance emit only
aggregate counts and fixed phase labels, never request identifiers or content.

### Rollout ordering and rollback boundaries

The complete deployment sequence is expand-only and each destructive control
is enabled separately, in order, only after the previous stage has been
observed healthy:

1. Build the candidate index with `CREATE INDEX CONCURRENTLY` as a monitored
   standalone operation; application migrations never build indexes on
   existing hot tables.
2. Apply the expand-only migration. It adds new relations and helpers and
   moves no data. It never scans or rewrites the existing request or
   template heaps, but two statements take a brief `ACCESS EXCLUSIVE` lock
   on `requests` (dropping the template foreign key here, and dropping the
   dead `response_steps` table in the follow-up migration); both run under
   a short `lock_timeout` so a busy claim path is never queued behind them.
   Apply in a quiet window and expect one retry if the daemon contends.
3. Roll out an archive-aware reader fleet everywhere before any movement, so
   every running process can resolve retained routes.
4. Verify prerequisites with the read-only preflight
   (`.github/scripts/check-retained-response-indexes.sql`): exact index
   readiness, exactly attached daily partitions, no unexplained
   detach-pending child, and the installed schema generation. Confirm the
   partition-runway gauge reports the full configured horizon.
5. Enable steady-state movement with minimal graph and byte budgets, observe
   write amplification, replication, pool, and latency signals, then ramp.
6. Enable backfill for non-expired history under the same bounds.
7. Run the separately gated one-time drain for content that is already past
   its retention period. This is where `batchless_archive_backfill_concurrency`
   matters: the backfill worker discovers one wave of distinct candidate
   graphs per tick and then moves up to that many of them at the same time,
   each in its own transaction under the per-graph advisory lock, so wall
   clock per tick shrinks roughly with the concurrency until the database
   becomes the bottleneck. The steady sweep is always sequential. Concurrent
   passes (several pods, or a pod plus a drain) remain safe because every
   move takes `FOR UPDATE SKIP LOCKED` and verifies by read-back; they simply
   do not add throughput, because every pass discovers the same oldest
   graphs. See the drain-pod recipe below.
8. Enable daily response-partition retirement after installing the dedicated
   single-session maintenance connection.
9. Enable weekly batch-archive retirement with an explicit
   finalization-anchored retention period. Every batch in a week must be
   fully archived, frozen, and individually past its period before the week
   drops; completion stamps batch metadata rows (which are never deleted).
10. Enable the generation-2 template write cutover, then file-content expiry
    and weekly template retirement with an explicit creation-anchored period.
    A template week drops only when no live file still owns rows in it; file
    rows are tombstoned, never deleted. The frozen legacy template heap is
    dropped later as one relation, in a separately approved forward
    migration, once its whole horizon has passed.

Rollback boundaries: before step 5 every change is reversible by disabling
flags and (only on an empty lifecycle) reverting the expand migration — the
down migration fails closed while any retained object, route, or unfinished
retirement exists. After movement has run, roll back only to archive-aware
reader versions; content already in retained partitions is served through
routes and must not be abandoned by downgrading readers below step 3. After
the first partition drop, retirement is irreversible by design; an unfinished
retirement journal must keep an enabled maintenance owner until it completes.

Abort and hold conditions: stop the ramp if the preflight fails, the runway
gauge falls behind the horizon, retirement retries persist, mover integrity
errors appear, or read-parity/latency regressions are observed. All of these
are content-free signals exported by the daemon; none require inspecting
customer data.

### One-off drain pod

A historical backlog of terminal batchless graphs is far larger than the
steady state, so drain it from a dedicated, disposable daemon pod rather than
by raising the fleet's backfill settings. The pod runs the same image and
configuration as the batch daemon, with these differences:

- daemon `enabled: always` and leader election off, so the pod moves data
  regardless of which fleet pod currently owns archive maintenance (the
  movers coordinate through the database, not through leadership);
- `batchless_archive_backfill_enabled: true`, sweep left as it is in the
  fleet;
- `batchless_archive_backfill_concurrency` high (8 to 16 is a sensible
  starting range), with `batchless_archive_groups_per_tick` several times
  the concurrency and `batchless_archive_bytes_per_tick` generous enough that
  the byte budget is not what ends a tick;
- its own database pool sizing: the fusillade write pool needs at least the
  concurrency in spare connections, plus the pod's ordinary daemon use, and
  the pool must fit under the database's connection ceiling alongside the
  fleet;
- `batch_archive_backfill_interval_ms` low, so ticks run back to back.

Watch `fusillade_retained_response_movers_active{worker="backfill"}` for the
configured fan-out, `fusillade_retained_response_groups_archived_total` for
progress, and `fusillade_retained_response_graphs_incomplete_skipped_total`
for graphs an operator must look at. Stop the pod once
`fusillade_retained_response_archive_may_have_more{worker="backfill"}` stays
at zero; from then on the fleet's steady sweep keeps up on its own. Ramp the
concurrency gradually while watching replication lag, write pool saturation,
and request latency on the primary, and lower it or stop the pod when any
abort condition above appears. Throughput scales with concurrency only until
the primary's write path or the pool is saturated; measure on preview or
staging before choosing the production value.

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
