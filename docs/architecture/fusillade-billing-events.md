# Durable billing for accepted inference requests

Durable billing stores usage in a logged Postgres queue before Fusillade accepts
an inference response. A separate worker charges the accepted logical request once.
The queue contains scalar attribution and usage fields, not prompts, responses,
credentials, or another serialized event envelope.

## Capture and acceptance

Before the first dispatch, Fusillade persists an immutable `legacy` or `durable`
billing mode. Existing requests remain legacy when the migration runs. Changing
the dispatch flag affects only requests that have not selected a mode yet.

Each physical dispatch gets an event UUID; retries get different UUIDs. The SSE
reassembler's `finish_with` callback observes its existing parsed response. Cache
accounting updates that typed snapshot during its existing response rewrite. This
adds no request/response JSON parse or serialization pass. Billing attribution is
snapshotted before inference and usage columns are bound directly to Postgres.

The producer awaits the queue insert, retrying the same event UUID on transient
failure. Only after commit does it return `x-fusillade-billing-event-id` to the
internal HTTP client. The client requires a matching acknowledgement for a durable
successful response. Missing usage stays explicit; an ordinary JSON response from
a provider that ignores forced streaming fails capture rather than adding another
parse. Failed HTTP responses do not produce billable events.

Fusillade atomically persists the accepted event UUID with its terminal result
and in an independent scalar `billing_acceptances` record. That evidence has no
cascading dependency on request rows or response payloads, so retention and erasure
cannot remove an unprocessed billing obligation. It is retained independently,
like the main-database billing receipt.
Acceptance is first-wins, survives archival, and prevents manual reopening of an
accepted durable completion. Internal metadata relies on the existing ingress
contract that strips all `x-fusillade-*` headers from external requests; deployments
must preserve that boundary before enabling durable dispatch. Legacy analytics billing skips only trusted internal
stream requests marked durable. Raw request logging remains independent.

## Processing and recovery

Workers claim bounded pages using `FOR UPDATE SKIP LOCKED` and expiring leases.
An expired lease can be reclaimed after a worker dies. Every mutation is fenced
by lease ownership. The worker reads acceptance from the primary Fusillade database,
using independent acceptance evidence before live/archive state; the two databases
can be separate.

Only the accepted attempt authorizes charging. Unaccepted attempts are marked
processed without a charge. Pending acceptance is retried, while missing usage,
identity, or historical pricing is retained as unresolved for investigation.
Pricing uses the captured model UUID, request time, key purpose, and cache usage.

One main-database transaction inserts a permanent receipt unique on logical
request UUID, writes the ledger debit and analytics, folds balance/cap/batch usage,
and acknowledges the queue event. A crash rolls all of these back together.
Replay cannot debit a receipted request again. Zero-cost requests still receive a
receipt and usage analytics without a debit. Daily usage views use the existing
asynchronous analytics cursor, so those views remain eventually consistent.

The worker continuously audits accepted completions for missing capture or billing
records. An independent receipt scan checks canonical ledger source, owner, debit
cardinality and amount, including the absence of a debit for zero-cost requests.
While supporting records remain, it also checks accepted event identity and analytics
usage/prices. Shared batch aggregates are checked for owner, presence, and minimum
contribution; this is not an exact recomputation of shared balances or aggregates.
Audit issues and unresolved events require investigation; the worker does
not infer missing token counts or automatically charge historical legacy requests.

## Configuration and rollout

All new billing flags default off:

```yaml
analytics:
  capture_fusillade_billing_events: false
  durable_billing:
    dispatch_enabled: false
    worker_enabled: false
    worker_only: false
    poll_interval_ms: 1000
    batch_size: 100
    retention_hours: 24
    analytics_retention_days: null
    max_backlog_age_secs: 300
    publish_max_retries: 3
    publish_retry_delay_ms: 100
```

1. Apply migrations and deploy the upgraded producer, HTTP client, and Fusillade
   storage code everywhere before enabling capture or durable dispatch.
2. Enable `capture_fusillade_billing_events` and a worker for shadow comparison.
   Legacy events never authorize a debit; after a grace period their token counts
   and cost are compared with legacy analytics. Investigate mismatches.
3. Run the worker either embedded (`worker_enabled: true`) or as a separate process
   (`worker_only: true`). The standalone process exposes `/healthz`, `/readyz`, and
   `/metrics` and does not start the gateway or inference daemon.
4. Enable `dispatch_enabled` after the worker is healthy and shadow results agree.
   Dispatch requires a fresh heartbeat and a bounded age of pending durable work.

For rollback, disable new durable dispatch but keep upgraded producers and workers
running until durable requests and events drain. The persisted mode cannot switch
an existing durable request back to legacy billing. Rolling back binaries before
that drain would violate the acknowledgement and acceptance contract.

## Retention and operations

Postgres does not automatically expire rows in this design. The worker deletes a
bounded page of **processed** events older than `retention_hours` on each cycle.
The default is 24 hours after processing, not after inference. Pending and unresolved
rows never expire. Billing receipts are retained independently of queue cleanup
and must survive the full replay horizon. Set `analytics_retention_days` to match
the external analytics cleanup policy if missing recent analytics should raise an
integrity issue. With an unknown policy, or after that horizon, absent analytics
is reported as unverifiable rather than corruption; receipt/ledger checks continue.

Monitor `dwctl_billing_events_pending`, `dwctl_billing_events_unresolved`,
`dwctl_billing_events_oldest_pending_seconds`, worker retry counters, and
reconciliation issue/cycle metrics. Heartbeats run independently every five seconds
and stop with the supervised worker on database-loop failure. A missing heartbeat
or old pending backlog pauses new durable dispatch; it does not discard queued work.

Resolve an event only after repairing its missing attribution/pricing or establishing
the accepted execution and ledger state. Requeue the existing event with its existing
UUID; do not invent a new logical request UUID or remove a receipt to retry billing.
Unresolved and audit records are intentionally retained for this investigation.

## Reliability boundary

Once the event commits, producer or worker pod loss cannot silently lose the queued
billing obligation. A pod can still die after upstream execution but before usage
publication. This queue does not make the upstream stream replayable or atomically
couple inference execution to Postgres. Closing that earlier gap requires upstream
usage persistence/replay or another durable execution record. The charging policy
here is one accepted logical result, not every discarded physical attempt.
