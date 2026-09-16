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

### Two kinds of observation

The controller needs two distinct inputs, and conflating them would corrupt it:

- An **uncensored sample** — an observed first-token latency.
- A **breach** — the deadline expired. This establishes only that first-token
  latency exceeded the deadline, a censored lower bound. It is *not* a latency
  measurement and must never be recorded as one; feeding the deadline value
  into a latency histogram would bias every statistic drawn from it.

Increase requires uncensored samples within budget. Decrease is driven by
breaches and by uncensored samples that exceed budget.

### Where an uncensored sample can be taken

| Site | What it establishes | Available for |
|---|---|---|
| Response headers arrive | Headers only — not a first token | All responses |
| Lead-frame read (`read_lead_frames`) | First decisive SSE frame | Strict-mode 2xx SSE only |
| Deadline expiry | Breach (censored) | Wherever the deadline is armed |

Header arrival is **not** a first-token sample. For a streamed response the
headers can arrive long before the first token, so it cannot stand in for one.

The lead-frame read is the only place a real first token is observed today, and
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

`classify_sse_event` maps a frame whose payload is `[DONE]` to `Data`, and
`read_lead_frames` sets `saw_data` on any `Data` frame. So the first decisive
frame of a zero-token stream can be `[DONE]`, which would otherwise be recorded
as a first-token success.

A first-token sample must therefore require a data frame that is **not**
`[DONE]`. Keep-alive comment frames already do not count.

### Decision

Provider selection lives in `load_balancer.rs`: `select_iter` yields providers
lazily, and `select_excluding` dispatches to `select_priority` (definition
order, first available) or `select_least_connections` (lowest `active/weight`,
weighted-random tiebreak).

**Preferred provider** is defined as the first provider in definition order.
The controller holds a single share for "preferred versus the rest"; per-provider
shares in pools of more than two providers are out of scope.

The mechanism is uniform across both strategies: **before the first attempt,
with probability `1 - f`, seed the exclusion set with the preferred provider**,
so selection begins among the alternates. Subsequent attempts use the exclusion
set exactly as they do today.

Note that folding `f` into provider weights would *not* work.
`LoadBalanceStrategy::WeightedRandom` dispatches to `select_least_connections`,
which picks the lowest `active/weight` score and consults weights only to break
ties. Weights there are a least-connections normaliser, not a proportional
splitter, so biasing them would not yield share `f` under load.

Seeding the exclusion set keeps the attempt budget, the exclusion mechanics and
the cascade-restart behaviour working unchanged.

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
| `min_samples` | Uncensored samples required before an increase |

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

The metrics recorder deliberately runs with idle-timeout and eviction
**disabled**, because the autoscaler reads an absent `onwards_model_inflight`
series as "genuinely zero in-flight" and evicting a long-lived stream's gauge
would tear a worker down mid-stream. Every label combination therefore persists
for the lifetime of the process.

That rules out provider URLs as labels: targets are rebuilt on config reload, so
URL-labelled series would accumulate indefinitely across target churn. Alias
labels follow existing precedent (`onwards_model_inflight{model}`) and stay
bounded by configuration size.

| Metric | Type | Labels |
|---|---|---|
| `onwards_provider_share` | gauge | alias |
| `onwards_first_token_seconds` | histogram | alias — uncensored samples only |
| `onwards_first_token_breaches_total` | counter | alias — censored breaches |
| `onwards_share_adjustments_total` | counter | alias, direction |

If per-provider detail proves necessary, use a bounded role label (preferred
versus alternate) rather than a provider identity.

### Reading the share

A share that settles well below 1.0 is an **indicator** of capacity shortfall,
not proof of it. Whether it means that depends on classifying what caused the
decay:

- Connection errors, non-2xx responses and embedded upstream errors are
  failures. They already drive the existing failover path and should **not**
  decay `f`, or they will make an outage look like a capacity limit.
- Only latency breaches and slow uncensored samples should adjust `f`.

With that classification in place, a persistently low `f` is meaningful evidence
that the preferred upstream cannot carry its own demand within budget, measured
from served traffic. Without it, the number conflates slowness with failure.

## Implementation order

1. Plumb per-model fallback values through the onwards-config sync so a budget
   can be set per alias. Behaviour unchanged.
2. Record observations and export them, with no control attached: the
   uncensored histogram (strict-mode SSE, excluding `[DONE]`) and the breach
   counter.
3. Add the controller and its state, adopted across reloads, consuming both
   inputs. Default disabled.
4. Bias selection by seeding the exclusion set, behind the same switch.
5. Enable per alias, starting with one whose upstream latency is known to be
   load-dependent.

Each step is independently shippable, and steps 1–2 are useful on their own:
they answer what the first-token latency distribution actually is, and how often
the deadline fires, neither of which is measured today.

## Testing

- **Sampling** — a data frame that is not `[DONE]` produces a sample; a stream
  whose first decisive frame is `[DONE]` does not; keep-alive comments do not.
- **Censoring** — a breach increments the breach counter and never enters the
  latency histogram.
- **Coverage** — non-strict SSE produces no uncensored samples; assert this
  rather than letting it surprise someone later.
- **Controller** — converges to a stable share against a simulated
  load-dependent upstream; does not oscillate when fast-at-idle and
  slow-at-load; decrease outpaces increase.
- **Classification** — connection errors and upstream error responses do not
  decay the share.
- **Reload** — the share survives a config reload via the adopt path, and a new
  budget from new config takes effect.
- **Selection** — exclusion seeding biases the first attempt without changing
  the attempt budget, exclusion mechanics or cascade restart.
- **Disabled default** — with the controller off, selection and failover are
  byte-for-byte today's behaviour.

## Open questions

- **Non-strict coverage.** Whether a pass-through-safe first-frame observer is
  worth building, or whether the controller should simply be unavailable for
  non-strict traffic.
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
