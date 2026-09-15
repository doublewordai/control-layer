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
| `on_rate_limit` | bool | `false` | Fallback when hitting local rate limits |
| `first_token_timeout_ms` | int | -- | Failover deadline for the first token of a streamed response; `0` disables (see below) |

Status code wildcards:

- `5` matches all 5xx (500-599)
- `50` matches 500-509
- `502` matches exact 502

When fallback triggers, the next provider is selected based on strategy (weighted random resamples from remaining pool; priority uses definition order).

### First-token failover

A provider can accept a streamed request and then stall: the headers arrive, but no token ever does. `first_token_timeout_ms` bounds that wait, so the request fails over instead of hanging. Set a proxy-wide default with `AppState::with_first_token_timeout`; a pool's own value overrides it.

The deadline is only armed when all of these hold, so it can reroute a stalled request but never fail one that would otherwise have succeeded:

- The request is `"stream": true`. A non-streaming response only sends headers once the whole completion is done, so a deadline would cut off long answers.
- The pool has more than one provider, and the attempt is not the last one the attempt budget allows.
- The request doesn't carry the header set with `AppState::with_first_token_timeout_exempt_header`.

It bounds the wait for response headers and, in strict mode, the wait for the first real SSE frame. Keep-alive comments don't count. Nothing has reached the client at that point, so the next provider starts cleanly.

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
