# Load Balancing

Onwards supports load balancing across multiple providers for a single alias, with automatic failover, weighted distribution, and configurable retry behavior.

## Configuration

```json
{
  "targets": {
    "gpt-4": {
      "strategy": "weighted_random",
      "fallback": {
        "enabled": true,
        "on_status": [429, 5],
        "on_rate_limit": true
      },
      "providers": [
        { "url": "https://api.openai.com", "onwards_key": "sk-key-1", "weight": 3 },
        { "url": "https://api.openai.com", "onwards_key": "sk-key-2", "weight": 1 }
      ]
    }
  }
}
```

## Strategy

- **`weighted_random`** (default): Distributes traffic randomly based on weights. A provider with `weight: 3` receives ~3x the traffic of `weight: 1`.
- **`priority`**: Always routes to the first provider. Falls through to subsequent providers only when fallback is triggered.

## Fallback

Controls automatic retry on other providers when requests fail:

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch for fallback |
| `on_status` | int[] | -- | Status codes that trigger fallback (supports wildcards) |
| `realtime_on_status` | int[] | `[]` | Extra statuses that trigger fallback for realtime requests only |
| `on_rate_limit` | bool | `false` | Fallback when hitting local rate limits |
| `first_token_timeout_ms` | int | -- | Failover deadline for the first token of a streamed response; `0` disables (see below) |
| `affinity` | object | -- | Priority pools only: choose preferred-first once per conversation instead of per request (see below) |

Status code wildcards:

- `5` matches all 5xx (500-599)
- `50` matches 500-509
- `502` matches exact 502

When fallback triggers, the next provider is selected based on strategy (weighted random resamples from remaining pool; priority uses definition order).

### Fallback metrics

| Metric | Type | Meaning |
|--------|------|---------|
| `onwards_failovers_total` | Counter | Attempts the pool failed over from, whether or not another provider was left to try |
| `onwards_fallback_rescues_total` | Counter | Requests answered with a 2xx after at least one failover |
| `onwards_upstream_failed_total` | Counter | Requests that ended without an answer, by reason and the last upstream status |

The first two have `model`, `cause` and `traffic` labels. `cause` is the upstream status that triggered the failover (for example `400` or `529`), or the failure kind when there is none: `timeout`, `first_token_timeout`, `network_error`, `empty_body` or `rate_limited`. On a rescue, `cause` is that of the first failed attempt. `traffic` is `dispatched` for requests carrying the header set with `AppState::with_first_token_timeout_exempt_header`, and `realtime` otherwise.

Each rescue is also logged at `info` ("Request served after failing over") with the model, cause, attempt count, account and API key ID, so rescued traffic can be attributed to callers. Individual failovers stay at `debug`.

### First-token failover

A provider can accept a streamed request and then stall: the headers arrive, but no token ever does. `first_token_timeout_ms` bounds that wait, so the request fails over instead of hanging. Set a proxy-wide default with `AppState::with_first_token_timeout`; a pool's own value overrides it.

The deadline is only armed when all of these hold, so it can reroute a stalled request but never fail one that would otherwise have succeeded:

- The request is `"stream": true`. A non-streaming response only sends headers once the whole completion is done, so a deadline would cut off long answers.
- The pool has more than one provider, and the attempt is not the last one the attempt budget allows.
- The request doesn't carry the header set with `AppState::with_first_token_timeout_exempt_header`.

It bounds the wait for response headers and, in strict mode, the wait for the first real SSE frame. Keep-alive comments don't count. Nothing has reached the client at that point, so the next provider starts cleanly.

### First-token observations

Two metrics expose the existing first-token checks without changing provider
selection or failover:

| Metric | Type | Meaning |
|--------|------|---------|
| `onwards_first_token_seconds` | Histogram | Time from the start of an attempt to its first observed non-`[DONE]` SSE data frame |
| `onwards_first_token_breaches_total` | Counter | First-token failover deadlines that expired, while waiting for headers or an SSE frame |

Both have `model`, `pool`, and `role` labels. `model` is the requested, validated
alias, as for `onwards_model_inflight`; `pool` is the resolved pool name
(`default` or a named pool such as `completions`, including after a routing
redirect). `role` is `preferred` for the first provider in definition order
and `alternate` for every other provider, regardless of attempt order or
selection strategy. Alternate providers are aggregated; provider URLs are
never labels.

Samples come only from the existing strict-mode 2xx SSE lead-frame check.
They include the wait for headers and skip keep-alive comments, embedded
errors, and `[DONE]`. A `[DONE]`-only stream remains valid and does not become
retryable. The measurement is time to a data frame, which can contain metadata
such as a role delta; it does not require generated text.

This histogram has partial coverage: non-strict streams and non-SSE responses
produce no samples. Without an armed first-token deadline, the existing peek
has time and event limits; a first frame arriving after those limits is also
unobserved. Observing those streams would require additional instrumentation.
Do not interpret the histogram as the full latency distribution or combine its
count with the breach counter to estimate an overall breach rate.

A deadline expiry is a censored observation, so it increments only the breach
counter, never the histogram. The counter measures the configured **failover
deadline**, not a separate latency budget. Network errors, HTTP or embedded
upstream errors, and the provider's independent request timeout do not increment
it. Non-strict requests can still increment it while waiting for headers.

## Pool-level options

Settings that apply to the entire alias:

| Option | Description |
|--------|-------------|
| `keys` | Access control keys for this alias |
| `rate_limit` | Rate limit for all requests to this alias |
| `concurrency_limit` | Max concurrent requests to this alias |
| `response_headers` | Headers added to all responses |
| `strategy` | `weighted_random` or `priority` |
| `fallback` | Retry configuration (see above) |
| `providers` | Array of provider configurations |

## Provider-level options

Settings specific to each provider:

| Option | Description |
|--------|-------------|
| `url` | Provider endpoint URL |
| `onwards_key` | API key for this provider |
| `onwards_model` | Model name override |
| `weight` | Traffic weight (default: 1) |
| `rate_limit` | Provider-specific rate limit |
| `concurrency_limit` | Provider-specific concurrency limit |
| `response_headers` | Provider-specific headers |
| `trusted` | Override pool-level trust for strict mode error sanitization (`true`/`false`; omit to inherit from pool) |
| `propagate_trace_context` | Whether to inject W3C `traceparent` / `tracestate` headers on outbound requests to this provider (`true`/`false`; omit to inherit from the resolved `trusted` value). Useful for preventing trace IDs from leaking to third-party providers whose downstream HTTP fetches would re-emit them. See [Trace context propagation](#trace-context-propagation) below. |

## Trace context propagation

`onwards` forwards W3C trace context (`traceparent` and `tracestate`
headers) on outbound requests to upstream providers, so a downstream
service that participates in your distributed tracing fabric can stitch
its spans into the calling trace.

Whether the headers are sent is controlled by `propagate_trace_context`:

- `propagate_trace_context: true` — always propagate
- `propagate_trace_context: false` — never propagate
- *omitted* (default) — inherit from the resolved `trusted` value:
  - per-provider `trusted: true|false` overrides
  - falling back to the pool-level `trusted` (default `false`)

In effect: **trusted upstreams receive trace context by default;
untrusted upstreams do not**. This prevents trace IDs from leaking to
third-party services that may re-emit them on their own outbound
calls (e.g., a provider's image fetcher echoing your `traceparent`
back to whatever URL the caller supplied).

> **Migration note.** Prior to onwards v0.28, `traceparent` was
> propagated to every upstream unconditionally. After this change,
> non-trusted upstreams no longer propagate by default (and any inbound
> trace context is stripped before forwarding to them). If you rely on
> trace continuity across `onwards → upstream` and the upstream isn't
> marked `trusted: true`, set `propagate_trace_context: true` on that
> provider. The field is **provider-scoped**: set it on each relevant
> entry of a pool's `providers` array, or on a legacy single-provider
> target. There is no pool-level `propagate_trace_context` key — for a
> whole pool, mark the pool `trusted: true` (which both bypasses
> error sanitization and enables propagation) or set the field on each
> provider entry.

## Examples

### Primary/backup failover

```json
{
  "targets": {
    "gpt-4": {
      "strategy": "priority",
      "fallback": { "enabled": true, "on_status": [5], "on_rate_limit": true },
      "providers": [
        { "url": "https://primary.example.com", "onwards_key": "sk-primary" },
        { "url": "https://backup.example.com", "onwards_key": "sk-backup" }
      ]
    }
  }
}
```

### Multiple API keys with pool-level rate limit

```json
{
  "targets": {
    "gpt-4": {
      "rate_limit": { "requests_per_second": 100, "burst_size": 200 },
      "providers": [
        { "url": "https://api.openai.com", "onwards_key": "sk-key-1" },
        { "url": "https://api.openai.com", "onwards_key": "sk-key-2" }
      ]
    }
  }
}
```

## Backwards compatibility

Single-provider configs still work unchanged:

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-key"
    }
  }
}
```

## Load-aware priority share

Priority pools with enabled fallback and multiple providers automatically adjust
the share of requests that try the preferred provider first. The share decreases
when the preferred provider's breach rate — first frames later than the budget,
plus overload statuses such as 429, 503 and 529 — rises above a target, holds
inside a hysteresis band, and recovers in steps once the rate stays low or the
pool has too few samples to judge. A pool that temporarily drops to one provider
keeps its controller and resumes it when the preferred provider returns. Set
`fallback.aimd.enabled: false` to opt out, or override the default parameters.
The controller applies only to eligible strict-mode streams, and it preserves the
preferred provider in subsequent failover attempts. See
[load-aware failover](load-aware-failover.md) for configuration, eligibility,
sampling limits, reload behavior and rollout.

### Conversation affinity

An agentic conversation sends many turns, each repeating the conversation so
far. The provider that served the previous turn usually still holds that prefix
in its cache; another provider has to process it again. When the preferred
provider cannot take every request, choosing per request moves conversations
back and forth and loses the cache on both sides.

`fallback.affinity` makes the choice per conversation. Each realtime request is
keyed by an explicit identifier when the client sends one (`x-session-id`
header, or `session_id` / `prompt_cache_key` in the body), otherwise by the
conversation's opening: its first system or developer message and its first
other message. The key maps to a point in `[0, 1)`, and the preferred provider is
tried first when the point is below the pool's affinity share. The share admits
about `target_conversations` of the conversations active in the last
`active_window_ms`, and moves only when the admitted count leaves
`target_conversations ± margin`, so admitted conversations stay put while the
population is steady.

```json
"fallback": {
  "enabled": true,
  "affinity": { "target_conversations": 50 }
}
```

| Option | Default | Description |
|--------|---------|-------------|
| `target_conversations` | required | Conversations to keep on the preferred provider |
| `margin` | a tenth of the target | Drift allowed before the share moves; at most the target |
| `active_window_ms` | `600000` | How long a conversation stays active after its last request |
| `update_interval_ms` | `60000` | The share is recomputed at multiples of this wall-clock interval |
| `max_tracked` | `100000` | Conversations tracked per process; when full, the tracker keeps the ones that decide the share |
| `enabled` | `true` | Set `false` to keep per-request selection |

Replicas share no state. Each records the conversations it sees and recomputes
the share at the same wall-clock instants; because a conversation's requests are
spread across replicas, they see the same conversations and reach the same
share. The [load-aware share](#load-aware-priority-share) still caps the
preferred side for eligible streams, so an overloaded preferred provider sheds
conversations. Continuation pools and requests without a key keep ordinary
priority selection.

`target_conversations` does not adapt: set it to what the preferred provider can
serve concurrently, and change it when that capacity changes.

### Realtime-only failover statuses

`fallback.realtime_on_status` adds statuses that fail a request over to the next
provider only when the request is realtime — it lacks the header set with
`AppState::with_first_token_timeout_exempt_header` — on top of `fallback.on_status`.
It accepts the same wildcards. Use it for a provider's over-capacity status:
realtime callers are rerouted, while dispatched traffic that runs its own retries
receives the upstream response.
