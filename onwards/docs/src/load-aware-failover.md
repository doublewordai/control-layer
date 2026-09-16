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
  "breach_rate_target": 0.05,
  "window_samples": 200,
  "min_samples": 50,
  "share_step": 0.02,
  "share_decay": 0.8,
  "share_floor": 0.05,
  "dwell_ms": 30000
}
```

The default 10-second budget matches the application's default first-token
failover deadline. Controllers and sample minima are per process, not aggregated
across replicas. A 5% rate allows ordinary tail events; overload decreases share
by 20%, and healthy recovery adds two percentage points per fresh healthy dwell.

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
pool has a demoted share. Non-SSE responses, provider request timeouts, network
and upstream errors, cancellation, empty streams and DONE-only streams are
unknown outcomes, not healthy samples or capacity breaches.

Controller observations inspect the already parsed strict SSE stream, including
content arriving after the lead peek's event/time caps. They do not add buffering,
change bytes, or move the failover deadline. The existing exported first-token
histogram still has its documented lead-peek coverage; it is not the controller's
denominator. No token-content parsing is added: the first non-sentinel data frame
can still be metadata. A stream that never produces data and has no armed failover
deadline remains unknown until it ends or is cancelled. Latency is measured when
the gateway polls the frame, so gateway scheduling or downstream backpressure
can contribute; it is not a measurement of backend execution time alone.

The bounded window is ordered by attempt start, with at least `min_samples`
required. Pending and unknown outcomes inhibit adjustments. At capacity, admission
pauses while any sampled attempt remains pending; slow samples are never evicted
in favor of newer fast completions. After resolution, new admissions displace the
oldest outcomes. Unknowns must age out before adjustment resumes. This conservative
policy can freeze the share during incomplete coverage or outages, leaving ordinary
failover to handle errors. It is not an outage circuit breaker.

A healthy dwell begins with a sufficiently sampled, fully resolved healthy window.
An unknown or overloaded window observed on completion resets that dwell; pending
work blocks decisions without restarting the last established healthy dwell.
No traffic means no adjustments. Every adjustment clears the window and advances a
generation; late completions from older generations are ignored. Fresh samples and
a new dwell are needed for recovery. Updates are constant-time under a shared pool
mutex, and each process controls its own share independently.

Reloads preserve state only when both pools have an active controller, preferred
identity (URL, key, upstream model), AIMD parameters and explicit deadline are
unchanged. Replacing/reordering the preferred provider, enabling a previously
inert controller, or changing its settings starts at share 1.0. Cloned request
pools share state. Process restarts also reset the share.

Validation bounds: budget 1–3,600,000 ms; dwell 1–86,400,000 ms;
`2 <= min_samples <= window_samples <= 100000`; target in `[0,1)`; decay in
`(0,1)`; step and floor in `(0,1]`. Choose windows/dwell for sample volume across
individual gateway replicas, including traffic remaining at the floor.

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
  `latency_budget_ms`.
- Over a sliding window of at least `min_samples` observations, compute the
  observed breach rate.
- If it exceeds `breach_rate_target`, decrease: `f *= share_decay`.
- If it stays at or below target for `dwell_ms`, increase: `f += share_step`.

Decrease fast, increase slowly. The asymmetry is intended to reduce oscillation and approach the largest
share whose breach rate still meets the target — a defined service level, not an artefact of volume.

Because each increase is one increment followed by re-measurement *at that
share*, the controller never infers full-load behaviour from a trickle sample,
which is the specific failure the binary design cannot avoid.

After each adjustment, require `min_samples` fresh observations from attempts
started at the new share before adjusting again. Old-window observations and
late completions from earlier shares must not trigger repeated decreases or
establish a healthy dwell at the new share. Dwell is a minimum interval between
adjustments, including decreases, rather than a per-request decision trigger.

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

Config reloads rebuild pools. There is an established mechanism for carrying
live state across a reload: the watcher calls `adopt_provider_state` on the new
pool before inserting it, which delegates to `adopt_active_counter` — sharing
the previous `Arc` while taking limits from the *new* config.

Controller state must be adopted the same way, but **not unconditionally**.
`adopt_active_counter` is safe because it matches counters per provider
identity; a single pool-level share has no such matching, so adopting blindly
would apply a share learned about one upstream to whatever now sits first in
definition order. Adoption must therefore be conditional on the preferred
provider's identity being unchanged, and reset the controller otherwise.

Without adoption at all, the share resets on every configuration change, and a
controller that resets faster than it converges is worse than no controller.

## Configuration

Configure `FallbackConfig.aimd` alongside `first_token_timeout_ms`. AIMD defaults are applied to eligible pools; model/pool overrides use these fields:

| Option | Meaning |
|---|---|
| `latency_budget_ms` | First-token latency defining a breach |
| `breach_rate_target` | Breach rate the controller holds the share to |
| `window_samples` | Sliding window over which the rate is measured |
| `min_samples` | Observations required before any adjustment |
| `share_step` | Additive increase per healthy dwell |
| `share_decay` | Multiplicative decrease when the rate is exceeded |
| `share_floor` | Minimum share retained for measurement |
| `dwell_ms` | Minimum time between adjustments |

Dwell and window size matter more than they look: a first-token observation
only exists once the attempt produces its first token or breaches, so decisions
lag the traffic that caused them. And because only preferred-provider attempts
are control input, a low share yields observations slowly — which is what the
share floor protects.

### Per-model values

Dwctl stores `first_token_timeout_ms` and nullable JSONB `aimd` on deployed
models. Create/update/read and both standard/composite sync paths carry them.
AIMD is accepted by the API only on priority composites with fallback enabled.
Existing rows have null overrides, preserving global deadlines while enabling the
default controller on eligible priority pools.

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
| `onwards_first_token_seconds` | histogram | model, pool, role |
| `onwards_first_token_breaches_total` | counter | model, pool, role |
| `onwards_share_adjustments_total` | counter | model, pool, direction |

`model` is the alias, matching the existing `onwards_model_inflight{model}`
convention. `pool` is the resolved pool name, bounded by configuration. `role`
is `preferred` or `alternate` — two values, which is what makes per-provider
comparison possible without unbounded provider identities. The share gauge
carries no `role`, since one controller governs one pool.

### Reading the share

A share that settles well below 1.0 is an **indicator** of capacity shortfall,
not proof of it. Whether it means that depends on classifying what caused the
decay:

- Connection errors, non-2xx responses and embedded upstream errors are
  failures. They already drive the existing failover path and should **not**
  count as latency breaches, or an outage will look like a capacity limit.
- Only latency breaches and slow uncensored samples should adjust `f`.

With that classification in place, a persistently low `f` is meaningful evidence
that the preferred upstream cannot carry its own demand within budget, measured
from served traffic. Without it, the number conflates slowness with failure.

Named continuation pools keep their deterministic failover order and explicitly opt out of AIMD.
Catalog provisioning preserves API overrides while routing remains compatible, and clears enabled
AIMD overrides when the catalog changes to weighted routing or disables fallback.

The share gauge is updated by eligible requests and observations. Replaced or removed
controllers are retired so their in-flight observations cannot publish stale metrics. As with the
other non-evicting metrics, a disabled or idle pool can retain its last published
value; consult configuration and recent adjustment activity when interpreting it.

## Validation

Deterministic controller tests cover rate thresholds, dwell, fresh generations,
unknown/pending outcomes, bounded sampling, share limits and a load-dependent
capacity simulation. Selection tests cover alternate-first cascades, concurrency
guards and reload adoption. HTTP tests cover late frames, censored deadlines,
exempt/nonstrict traffic, byte preservation and upstream errors. Database/API tests
cover round trips, merged PATCH validation, clearing and onwards sync.
