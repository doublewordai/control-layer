# Load-Aware Failover (design)

> Status: design, not implemented. This page describes the intended successor to
> the per-request first-token deadline documented in
> [Load Balancing](load-balancing.md#first-token-failover).

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

## Design: additive-increase / multiplicative-decrease

Replace the binary open/closed state with a **share** `f ∈ [0, 1]`: the
fraction of eligible requests for which the preferred provider is tried first.

- **Increase** additively (`f += step`) after a dwell period in which observed
  first-token latency stayed within budget.
- **Decrease** multiplicatively (`f *= factor`, factor < 1) as soon as it does
  not.

Decrease fast, increase slowly. The asymmetry is what stops the hunt, and the
controller converges on the largest share the upstream can carry within the
latency budget rather than flapping between none and all.

Because each increase is one increment followed by re-measurement *at that
share*, the controller never infers full-load behaviour from a trickle sample —
which is the specific failure the binary design cannot avoid.

### Properties worth preserving

- **Never remove a provider from the pool.** `f` biases which provider is tried
  *first*. A demoted provider is still reachable, and existing failover
  semantics are untouched.
- **Keep a floor on `f`.** A small non-zero share preserves a live measurement,
  so recovery is observed from real traffic rather than synthetic probes.
- **Disabled means today's behaviour.** With the controller off, selection and
  failover behave exactly as they do now.

## Where it hooks into the code

### Observation

First-token latency is already bounded in two places; both need to record a
sample rather than only detect the failure case.

| Site | Today | Needed |
|---|---|---|
| `handlers.rs` (header deadline) | `header_deadline` is the min of the request deadline and the first-token deadline; a breach records reason `first_token_timeout` | also record elapsed time on the success path |
| `handlers.rs` (lead frames) | when armed, `timeout_at(deadline, read_lead_frames(..))`; the error arm records `onwards.fallback = "first_token_timeout"` and returns `LoopAction::Continue` | the `Ok` arm is the first-token success — currently untimed, and the only source of good samples |

Sampling only the timeout path would feed the controller failures alone, so it
could never ramp back up. The success arm is load-bearing.

### Decision

Provider selection lives in `load_balancer.rs`: `select_iter` yields providers
lazily, and `select_excluding` dispatches to `select_priority` (definition
order, first available) or `select_least_connections` (lowest `active/weight`,
weighted-random tiebreak).

The share should bias *ordering*, not add a strategy:

- **Priority** — with probability `1 - f`, skip the preferred provider on the
  first attempt and start at the next one. Failover order is otherwise intact.
- **Weighted random** — fold `f` into the existing weight arithmetic, which
  already expresses proportional split.

Keeping this inside the existing selection path means the attempt budget,
exclusion set, and cascade-restart behaviour all continue to work unchanged.

### State and lifetime

`ProviderPool` is cloned per request out of the `Targets` map, and its fields
are plain values, so shared mutable state must sit behind an `Arc` — exactly as
`Provider`'s active-connection counter already does.

Config reloads rebuild pools. There is an established mechanism for carrying
live state across a reload: the watcher calls `adopt_provider_state` on the new
pool before inserting it, which delegates to `adopt_active_counter` — sharing
the previous `Arc` while taking limits from the *new* config.

Controller state must be adopted the same way. Without it the share resets on
every configuration change, and a controller that resets faster than it
converges is worse than no controller at all.

## Configuration

Extend `FallbackConfig` alongside `first_token_timeout_ms`, with a proxy-wide
default on `AppState` mirroring `with_first_token_timeout`:

| Option | Meaning |
|---|---|
| `latency_budget_ms` | First-token latency the controller targets |
| `share_step` | Additive increase per healthy dwell |
| `share_decay` | Multiplicative decrease on breach |
| `share_floor` | Minimum share retained for measurement |
| `dwell_ms` | Minimum time between adjustments |
| `min_samples` | Samples required before a decision |

Dwell and minimum-sample count matter more than they look: a first-token sample
only exists once the request produces its first token, so decisions lag the
traffic that caused them.

### Per-model values are a prerequisite

`first_token_timeout_ms` is currently hardcoded to `None` in both
`OnwardsFallbackConfig` literals in dwctl's onwards-config sync — the composite
arm and the single-model arm — so only the proxy-wide default is reachable. The
correct budget is model-specific, so this plumbing is a prerequisite for the
controller, not a follow-up.

## Observability

Following existing naming (`onwards_*_total` counters, `onwards_*_inflight`
gauges):

- `onwards_provider_share` — gauge, per alias and provider. The controller's
  current `f`.
- `onwards_first_token_seconds` — histogram, per alias and provider. The
  samples the controller acts on.
- `onwards_share_adjustments_total` — counter, labelled by direction.

A share that settles well below 1.0 is a capacity signal: it is direct evidence
the preferred upstream cannot carry its own demand within budget, measured from
served traffic rather than inferred.

## Implementation order

1. Plumb per-model fallback values through the onwards-config sync so a budget
   can be set per alias. Behaviour unchanged.
2. Record first-token samples on both success paths, and export the histogram.
   Observation only, no control.
3. Add the controller and its state, adopted across reloads. Default disabled.
4. Bias selection by share, behind the same switch.
5. Enable per alias, starting with one whose upstream latency is known to be
   load-dependent.

Each step is independently shippable, and steps 1–2 are useful on their own:
they answer what the latency distribution actually is per provider, which is
currently not measured.

## Testing

- **Controller unit tests** — convergence to a stable share under a simulated
  load-dependent upstream; no oscillation when fast-at-idle and slow-at-load;
  decrease outpaces increase.
- **Reload** — share survives a config reload via the adopt path, and a new
  budget from new config takes effect.
- **Selection** — share biases first-attempt ordering without changing the
  attempt budget, exclusion set, or cascade restart.
- **Disabled default** — with the controller off, selection and failover are
  byte-for-byte today's behaviour.
- **Sampling** — success and timeout paths both produce samples; keep-alive
  comments do not count as a first token.

## Open questions

- **Per-process state.** Each gateway process runs its own controller, so with
  N processes there are N independent controllers converging separately. Shared
  state would be consistent but adds coordination; per-process is simpler and
  probably adequate, but the multiplier should be a conscious choice.
- **Interaction with the existing deadline.** The per-request deadline should
  remain as a hang detector once the controller handles systematic slowness,
  which argues for a longer deadline. The right value depends on how much of
  the tail the controller absorbs in practice.
- **Cost asymmetry.** When providers differ in cost, the budget is really a
  latency-versus-cost knob rather than a pure latency control. Making that
  explicit may be worthwhile.
