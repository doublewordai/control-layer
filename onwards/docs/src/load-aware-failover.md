# Load-Aware Failover

> Status: AIMD is enabled by default for priority pools with fallback enabled
> and at least two providers. Nonstrict, nonstreaming and exempt requests retain
> ordinary routing. Set `aimd.enabled: false` to opt a pool/model out.

## Operating the controller

Existing eligible pools immediately use these defaults without a database backfill:

```json
{
  "enabled": true,
  "latency_budget_ms": 10000,
  "breach_rate_target": 0.10,
  "recovery_breach_rate": 0.03,
  "window_samples": 100,
  "min_samples": 20,
  "share_step": 0.05,
  "share_decay": 0.8,
  "share_floor": 0.05,
  "dwell_ms": 30000,
  "idle_recovery_ms": 300000,
  "overload_statuses": [429, 503, 529]
}
```

The default 10-second budget matches the application's default first-token
failover deadline. Controllers and sample minima are per process, not aggregated
across replicas. With the defaults:

- **Decrease** the share by 20% when more than 10% of the window's completed
  samples breach.
- **Hold** while the breach rate is above 3% and at most 10%.
- **Increase** by five percentage points once the rate has stayed at or below 3%
  for the 30-second dwell, and again each dwell while it stays there.
- **Idle recovery** adds a step for each five minutes the pool goes without the
  20 samples needed to judge, so a pool that sees little traffic after an
  incident climbs back instead of staying degraded until a restart.
- The share never drops below 0.05, so real traffic keeps measuring the
  preferred provider. There are no synthetic probes.

Override `fallback.aimd` on a native onwards priority pool, or the top-level
`aimd` field when creating/updating a dwctl priority composite. An override object
replaces the prior object; omitted object members use the defaults above. For an
enabled override, set an explicit `first_token_timeout_ms`: either `0` to disable
that deadline, or at least the latency budget. An absent AIMD override inherits
the controller defaults and permits an inherited deadline. If that inherited
deadline is shorter than the budget, its censored result is **unknown**, not a
budget breach. The controller never silently lengthens a configured deadline.

Dwctl PATCH semantics: omitted fields are unchanged; `aimd: null` restores the
default controller; `aimd: {"enabled": false}` disables it. A null
`first_token_timeout_ms` restores deadline inheritance. Overrides appear under
`fallback` in model responses; null means inherited defaults, not disabled.
The dashboard has no AIMD editor; use the model API for overrides and opt-out.

Only strict-mode `stream: true` requests without the configured timeout-exempt
header use the share or contribute observations. Single-provider and weighted-selection pools, and pools without enabled fallback,
are inert. Eligible preferred attempts at any position in the retry cascade,
including the final attempt, can contribute; alternate attempts never do.
Nonstrict/nonstreaming/exempt traffic uses ordinary selection even when the
pool has a demoted share. Each preferred attempt ends as one of:

- **healthy:** its first non-sentinel data frame arrived within the budget;
- **breach:** that frame arrived after the budget, the first-token deadline
  expired at or after the budget, or the provider answered with a status listed
  in `overload_statuses`, either on the response line or embedded in a 2xx
  stream. A provider shedding load is the clearest overload signal there is;
- **unknown:** anything else — a shorter inherited deadline, other upstream
  errors, network errors and provider request timeouts, non-SSE responses,
  empty or DONE-only streams, and cancellation.

Unknown outcomes are excluded from the breach rate. They count as neither healthy
nor a breach, and they never block a decision.

Controller observations inspect the already parsed strict SSE stream, including
content arriving after the lead peek's event/time caps. They do not add buffering,
change bytes, or move the failover deadline. The exported first-token histogram
still has its documented lead-peek coverage; it is not the controller's
denominator. No token-content parsing is added: the first non-sentinel data frame
can still be metadata. A stream that never produces data and has no armed failover
deadline remains unknown until it ends or is cancelled. Latency is measured when
the gateway polls the frame, so gateway scheduling or downstream backpressure
can contribute; it is not a measurement of backend execution time alone.

The window holds up to `window_samples` completed healthy/breach outcomes, and
decisions need at least `min_samples`. Attempts still in flight never hold a
decision back, and a full window keeps admitting samples, so the controller keeps
deciding under sustained concurrency. A decrease clears the window and starts a
new generation: attempts that began at the old share are ignored when they
complete. An increase keeps the window, so a window that is still healthy supports
the next step one dwell later. Updates are constant-time under a shared pool
mutex, and each process controls its own share independently.

Reloads keep a controller when its AIMD parameters, explicit deadline and
preferred provider identity (URL, key, upstream model) are unchanged. When a pool
is still configured for AIMD but drops to a single provider — for example because
an autoscaler disabled the preferred provider — its controller is **parked**
rather than discarded. It resumes with its learned share when a later reload
restores the same preferred provider, and idle recovery credits the time it spent
parked. Changing the settings, replacing or reordering the preferred provider, or
opting out retires the controller, and its successor starts at 1.0. Cloned request
pools share state; process restarts reset it.

Validation bounds: budget 1–3,600,000 ms; dwell 1–86,400,000 ms; idle recovery
0–86,400,000 ms (`0` disables it); `2 <= min_samples <= window_samples <= 100000`;
target in `[0,1)` and `0 <= recovery_breach_rate <= breach_rate_target`; decay in
`(0,1)`; step and floor in `(0,1]`; at most 32 `overload_statuses`, each 400–599.
Choose windows and dwell for sample volume across individual gateway replicas,
including traffic remaining at the floor.

### Realtime-only failover statuses

Separately from the controller, a pool's `fallback.realtime_on_status` lists
upstream statuses that fail a **realtime** request over to the next provider, on
top of `on_status`. Requests carrying the exempt header keep the upstream response
and retry on their own terms. Dwctl stores this per model as
`fallback_realtime_on_status` (the dashboard's "Overloaded (529, realtime only)"
switch); new composite models default to `[529]`. Whether or not a status fails
over, a listed overload status from the preferred provider still counts as a
controller breach.

Monitor client latency, errors, preferred-first share, adjustment rate and alternate
spend after deployment. A low share is an indicator of capacity shortfall, not proof.
Disable with `aimd: {"enabled": false}` to restore ordinary priority selection for
new requests after the routing configuration reloads.

## Problem

`first_token_timeout_ms` bounds how long a streamed attempt may go without
producing its first frame, then fails over. It is a good hang detector and a
poor latency control, because every firing is a *tax*: the client waits the
full deadline on the first provider and then waits again for the second.

That cost is acceptable when the first provider is broken. It is not acceptable
when the first provider is merely slow, which is the common case for a
self-hosted upstream whose time-to-first-token is **load-dependent**:

- Under load, first-token latency rises. A deadline chosen to catch stalls
  starts catching the ordinary upper tail instead.
- Each failover then adds the whole deadline to a request that would have
  completed shortly after it.
- The result is a cluster of client latencies just above the deadline — caused
  by the mitigation rather than the upstream.

Raising the deadline removes the manufactured cluster but abandons the slow
tail. Lowering it converts more ordinary requests into double-waits. There is
no good value, because a per-request deadline can only ever react *after*
paying its own cost.

## Why a binary circuit breaker is not enough

The obvious fix is a circuit breaker: watch first-token latency, and when it
degrades, send traffic to the next provider instead of paying the deadline per
request. That removes the per-request tax — the cost collapses from "every
affected request" to "one probe per interval".

But a naive breaker oscillates when the upstream's latency is load-dependent:

1. Latency degrades under full load; the breaker opens.
2. All traffic moves away, so the upstream goes idle.
3. A probe arrives at an idle upstream and is fast, so the breaker closes.
4. Full load returns, latency degrades, and the breaker opens again.

The measurement taken at trickle load does not predict behaviour at full load,
so the breaker hunts forever. This is not a tuning problem: the stable answer
is usually a *split* — the upstream serves the share it can serve within
budget, and the remainder goes elsewhere — and a two-position control cannot
express a split.

## Design: a controlled share

Replace the binary open/closed state with a **share** `f ∈ [0, 1]`: the
fraction of eligible requests for which the preferred provider is tried first.

### The control signal is a rate, not an event

A single slow request must not move the share. Any realistic first-token
distribution has a tail, so at *every* sustainable share some requests exceed
any fixed budget. If each breach triggered a decrease, the share would decay to
its floor regardless of actual capacity, and it would decay faster at higher
request volumes — making the control a function of traffic rather than of
service quality.

The controller therefore targets a **breach rate against a latency budget**,
which is a statement of intent that can actually be met:

- A **breach** is an attempt whose first token did not arrive within
  `latency_budget_ms`, or whose provider answered with an overload status.
- Over a sliding window of at least `min_samples` completed observations,
  compute the observed breach rate.
- If it exceeds `breach_rate_target`, decrease: `f *= share_decay`.
- If it has stayed at or below `recovery_breach_rate` for `dwell_ms`, increase:
  `f += share_step`.
- Between the two, hold.

The gap between the thresholds is hysteresis. A single threshold judged on a
small sample decreases on noise: at a true breach rate exactly on a 5% target,
50 samples exceed it about half the time. Separating "clearly overloaded" from
"clearly healthy" lets the controller decide on smaller windows without hunting.

Decrease fast, increase slowly. The asymmetry is intended to reduce oscillation
and approach the largest share whose breach rate stays within the band.

A decrease clears the window: samples taken at the old share say nothing about
the new one, and a stale overloaded window must not trigger a second decrease.
An increase keeps the window, because a window that is still healthy after a
step is evidence for the next step too. Each adjustment is still followed by a
dwell before the next, so every increase is re-measured at the higher share
before another is allowed.

### Properties worth preserving

- **Never remove a provider from the pool.** `f` biases which provider is tried
  *first*. A demoted provider is still reachable, and existing failover
  semantics are untouched.
- **Keep a floor on `f`.** A small non-zero share preserves a live measurement
  of the preferred provider, so recovery is observed from real traffic rather
  than synthetic probes. This matters more than it looks — see the scoping rule
  below, which makes preferred-provider samples the *only* control input.
- **Disabled means today's behaviour.** With the controller off, selection and
  failover behave exactly as they do now.

## Where it hooks into the code

### Two kinds of observation

The controller needs two distinct inputs, and conflating them would corrupt it:

- An **uncensored sample** — an observed first-token latency.
- A **breach** — the deadline expired. This establishes only that first-token
  latency exceeded the deadline, a censored lower bound. It is *not* a latency
  measurement and must never be recorded as one; feeding the deadline value
  into a latency histogram would bias every statistic drawn from it.

Both feed the breach-rate calculation. Only uncensored samples feed the latency
histogram.

The implemented breach counter counts **failover deadline** expiries. The
controller's latency budget is a separate threshold: a timeout earlier than the
budget cannot establish a budget breach. Controller configuration must therefore
require any armed failover deadline to be at least the latency budget. A slow
observed frame can establish a budget breach without firing the failover
deadline. Neither outcome may count twice in the controller's denominator.

The exported observation metrics are not sufficient controller input: even
strict streams can outlast the existing peek's event or time limits and be
forwarded unobserved. The controller therefore observes eligible frames after the peek and tracks
unknown outcomes explicitly, rather than counting them as healthy or estimating
a rate from the exported histogram.

### Only the preferred provider's attempts are control input

Once `1 - f` of traffic is being sent to alternates, those alternates are also
producing first-token observations. They must **not** update `f`. A slow
alternate would otherwise demote a healthy preferred provider, and a fast
alternate would ramp up a struggling one — in both cases the controller would
be steering on a signal from the wrong upstream.

Observations are therefore attributed to the provider actually attempted, and
only attempts against the preferred provider adjust the share. Observations
from alternates are still exported, because they are useful for comparing
providers, but they are inert as control input.

### Where an uncensored sample can be taken

| Site | What it establishes | Available for |
|---|---|---|
| Response headers arrive | Headers only — not a first token | All responses |
| Lead-frame read (`read_lead_frames`) | First decisive SSE frame | Strict-mode 2xx SSE only |
| Deadline expiry | Breach (censored) | Wherever the deadline is armed |

Header arrival is **not** a first-token sample. For a streamed response the
headers can arrive long before the first token, so it cannot stand in for one.

The lead-frame read is where the exported histogram observes first frames, and
it is gated: the enclosing branch requires `(200..300).contains(&status) &&
state.targets.strict_mode`. Non-strict SSE is forwarded without a lead-frame
peek, deliberately — the pass-through path avoids forcing buffering and SSE
re-framing onto streams that would otherwise stream straight through.

Consequences to accept explicitly:

- The first-token histogram is populated **only for strict-mode SSE traffic**.
  For non-strict traffic there are no uncensored samples, so a controller there
  would have breaches and nothing else, and could never ramp back up.
- Extending coverage to non-strict traffic needs a pass-through-safe observer
  that timestamps the first `data:` frame **without** re-framing or buffering
  the stream, and that ignores keep-alive comments. That is a separate opt-in
  step, not a free extension.

### What counts as a first token

`classify_sse_event` distinguishes `Data` from the `[DONE]` sentinel's `Done`
variant. `read_lead_frames` sets `saw_data` for both, preserving the existing
non-empty verdict, but sets `saw_content` only for `Data`. The histogram uses
`saw_content`, so `[DONE]`-only streams neither produce samples nor become
retryable empty responses. Keep-alive comments do not set either flag.

A sample measures the first non-sentinel data frame, which may contain metadata
rather than generated text. Observing literal token content is not implemented.

### Decision: scoped to the priority strategy

Provider selection lives in `load_balancer.rs`: `select_iter` yields providers
lazily, and `select_excluding` dispatches to `select_priority` (definition
order, first available) or `select_least_connections` (lowest `active/weight`,
weighted-random tiebreak).

**The controller is defined for `LoadBalanceStrategy::Priority` only.** Under
`Priority` the preferred provider is unambiguous — first in definition order —
and skipping it genuinely hands the first attempt to the next provider.

`WeightedRandom` is out of scope, for two concrete reasons:

- Leaving the preferred provider *eligible* does not make it *first*.
  `select_least_connections` ranks by lowest `active/weight` and consults
  weights only to break ties, so the realised preferred-first rate would sit
  below `f` by an amount that varies with load. Folding `f` into weights does
  not fix this — weights there are a least-connections normaliser, not a
  proportional splitter.
- Seeding the shared exclusion set is unsafe. `SelectIter::next` only clears
  exclusions when `select_excluding` returns `None`, so while any alternate
  remains selectable a seeded exclusion persists and the preferred provider is
  unreachable for the rest of that request.

The bias must therefore be a **first-attempt-only choice**, expressed as an
explicit override of the first provider rather than by mutating the exclusion
set: with probability `1 - f`, begin at the next provider, then let subsequent
attempts proceed exactly as they do today, with the preferred provider still
reachable. Supporting `WeightedRandom` would require that override to carry a
provider identity into a freshly initialised iterator, and is deferred.

### State and lifetime

`ProviderPool` is cloned per request out of the `Targets` map, and its fields
are plain values, so shared mutable state must sit behind an `Arc` — exactly as
`Provider`'s active-connection counter already does.

Config reloads rebuild pools. The watcher calls `adopt_provider_state` on the new
pool before inserting it, which carries live state across the reload.

Controller state is carried the same way, but **not unconditionally**. A single
pool-level share has no per-provider matching, so adopting blindly would apply a
share learned about one upstream to whatever now sits first in definition order.
Adoption therefore requires the same preferred provider identity, AIMD
parameters and explicit deadline.

A pool can also lose eligibility without its configuration changing: an
autoscaler disabling a self-hosted preferred provider leaves one provider. The
controller is parked in that pool, carried through further reloads, and resumed
when the same preferred provider returns. Retiring it instead would reset the
share every time capacity is scaled down and back up, which for a frequently
scaled model means the controller never keeps what it learned.

## Configuration

Configure `FallbackConfig.aimd` alongside `first_token_timeout_ms`. AIMD defaults are applied to eligible pools; model/pool overrides use these fields:

| Option | Meaning |
|---|---|
| `latency_budget_ms` | First-token latency defining a breach |
| `breach_rate_target` | Breach rate above which the share decreases |
| `recovery_breach_rate` | Breach rate at or below which the share may increase |
| `window_samples` | Completed outcomes over which the rate is measured |
| `min_samples` | Completed outcomes required before a latency-driven decision |
| `share_step` | Additive increase per healthy dwell or idle interval |
| `share_decay` | Multiplicative decrease when the rate is exceeded |
| `share_floor` | Minimum share retained for measurement |
| `dwell_ms` | Minimum time between adjustments, and healthy time before an increase |
| `idle_recovery_ms` | Interval that earns a step while samples are too few to judge; `0` disables |
| `overload_statuses` | Upstream error statuses from the preferred provider counted as breaches |

Dwell and window size matter more than they look: a first-token observation
only exists once the attempt produces its first token or breaches, so decisions
lag the traffic that caused them. And because only preferred-provider attempts
are control input, a low share yields observations slowly — which is what the
share floor and idle recovery protect.

### Per-model values

Dwctl stores `first_token_timeout_ms` and nullable JSONB `aimd` on deployed
models. Create/update/read and both standard/composite sync paths carry them.
AIMD is accepted by the API only on priority composites with fallback enabled.
Rows with null overrides use the defaults above, so default changes apply to them
on the next routing reload; stored overrides keep their values, and members they
omit use the defaults.

## Observability

The metrics recorder deliberately runs with idle-timeout and eviction
**disabled**, because the autoscaler reads an absent `onwards_model_inflight`
series as "genuinely zero in-flight" and evicting a long-lived stream's gauge
would tear a worker down mid-stream. Every label combination therefore persists
for the lifetime of the process, so every label must be bounded by
configuration rather than by traffic.

That rules out provider URLs as labels. It does **not** allow alias-only labels
either: a composite alias can carry several named `ProviderPool`s, whose
independent controllers would otherwise write the same series and aggregate
unrelated observations.

| Metric | Type | Labels |
|---|---|---|
| `onwards_provider_share` | gauge | model, pool |
| `onwards_share_adjustments_total` | counter | model, pool, direction |
| `onwards_aimd_active` | gauge | model, pool |
| `onwards_aimd_window_samples` | gauge | model, pool |
| `onwards_aimd_window_breach_rate` | gauge | model, pool |
| `onwards_aimd_in_flight` | gauge | model, pool |
| `onwards_aimd_unknown_total` | counter | model, pool |
| `onwards_aimd_overload_breaches_total` | counter | model, pool, status |
| `onwards_first_token_seconds` | histogram | model, pool, role |
| `onwards_first_token_breaches_total` | counter | model, pool, role |

`model` is the alias, matching the existing `onwards_model_inflight{model}`
convention. `pool` is the resolved pool name, bounded by configuration. `role`
is `preferred` or `alternate`, and `status` is bounded by `overload_statuses`.
The controller series carry no `role`, since one controller governs one pool.

`onwards_first_token_seconds` has real histogram buckets, including an exact
10-second edge, in both the onwards recorder and dwctl's; without them it would
render as a per-process summary whose quantiles cannot be aggregated across
replicas. Embedders installing their own recorder can reuse
`onwards::FIRST_TOKEN_SECONDS_BUCKETS`.

### Reading the share

A share that settles well below 1.0 is an **indicator** of capacity shortfall,
not proof of it. The controller's inputs are classified to keep it meaningful:

- Slow first frames, deadline expiries at or after the budget, and overload
  statuses are breaches — each says the preferred provider could not serve the
  request in time.
- Other failures — connection errors, non-overload error statuses, cancellations
  — are unknown and excluded, so an outage of a different kind does not read as a
  capacity limit.

Use the window gauges to tell a healthy steady state from one that has too little
signal. A share at 1.0 with a low `onwards_aimd_window_samples` and a rising
`onwards_aimd_unknown_total` means the controller cannot see enough outcomes to
judge, not that the provider is healthy. `onwards_aimd_window_breach_rate` shows
where the pool sits relative to the two thresholds.

Named continuation pools keep their deterministic failover order and explicitly opt out of AIMD.
Catalog provisioning preserves API overrides while routing remains compatible, and clears enabled
AIMD overrides when the catalog changes to weighted routing or disables fallback.

The controller series are updated by eligible requests and observations.
`onwards_aimd_active` drops to 0 when a controller is parked or retired. As with
the other non-evicting metrics, a disabled or idle pool retains its last published
share; read it together with `onwards_aimd_active` and recent adjustment activity.

## Validation

Deterministic controller tests cover the hysteresis band, dwell, fresh generations
after decreases, window retention after increases, idle recovery, parking and
resumption, unknown exclusion, decisions with attempts in flight, overload
statuses, share limits and a load-dependent capacity simulation. Selection tests
cover alternate-first cascades, concurrency guards, reload adoption and parking
across a disabled preferred provider. HTTP tests cover late frames, censored
deadlines, overload statuses from the preferred provider, realtime-only failover
statuses, exempt/nonstrict traffic, byte preservation and upstream errors.
Database/API tests cover round trips, merged PATCH validation, clearing and
onwards sync.
