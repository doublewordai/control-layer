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
| `with_replacement` | bool | `false` | For `weighted_random`, allow a provider to be selected again |
| `max_attempts` | int? | provider count | Maximum pre-response failover attempts |
| `backoff` | object? | `null` | Delay between attempts; no delay when omitted |
| `max_total_backoff_ms` | int? | `null` | Maximum cumulative time spent sleeping between attempts |
| `stream_continuation` | object? | `null` | Best-effort continuation for eligible interrupted text-generation streams |

Status code wildcards:

- `5` matches all 5xx (500-599)
- `50` matches 500-509
- `502` matches exact 502

When fallback triggers, the next provider is selected based on strategy (weighted random resamples from remaining pool; priority uses definition order).

### Stream continuation

Stream continuation is opt-in for eligible `POST /v1/completions`,
`POST /v1/chat/completions`, and `POST /v1/responses` streams. When an upstream
stops before a terminal event, Onwards sends the text already emitted to a newly
selected provider, which may be the same provider. The prefix is kept in memory
for the lifetime of the request; it is not persisted and is lost if the process
restarts. An interruption or exhausted continuation budget can leave the client
with a partial stream, and the proxy does not synthesize a terminal event.

Configure it inside `fallback`:

```json
{
  "targets": {
    "gpt-4": {
      "fallback": {
        "enabled": true,
        "on_status": [429, 5],
        "on_rate_limit": true,
        "stream_continuation": {
          "enabled": true,
          "endpoints": [
            "/v1/completions",
            "/v1/chat/completions",
            "/v1/responses"
          ],
          "max_attempts": 1,
          "max_buffered_bytes": 1048576,
          "idle_timeout_ms": 30000
        }
      },
      "providers": [
        { "url": "https://primary.example.com", "onwards_key": "sk-primary" },
        { "url": "https://backup.example.com", "onwards_key": "sk-backup" }
      ]
    }
  }
}
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | bool | `false` | Enables continuation, subject to the parent `fallback.enabled` switch |
| `endpoints` | string[] | `[]` | Exact request paths eligible for continuation; supported values are `/v1/completions`, `/v1/chat/completions`, and `/v1/responses` |
| `max_attempts` | int | `1` | Fresh post-commit continuation-attempt budget |
| `max_buffered_bytes` | int | `1048576` | Maximum generated-text bytes retained for a continuation request |
| `idle_timeout_ms` | int? | `null` | Maximum idle time between upstream stream events; `null` disables this timeout |

`fallback.max_attempts` controls failover before the response starts and its
headers are committed. `stream_continuation.max_attempts` is a separate, fresh
budget after a successful stream has committed its response. Continuation
reuses the parent fallback status matching, local rate-limit behavior, provider
selection, request/header handling, and backoff settings, including
`max_total_backoff_ms`. Stream continuation also starts a fresh cumulative
backoff budget; the parent `max_total_backoff_ms` limit applies independently
to the post-commit continuation attempts.

Only narrow, single-output text shapes are supported. Completions require a
string `prompt`, `stream: true`, `n` omitted or `1`, and `echo` omitted or
`false`. Chat Completions require ordinary message content, `stream: true`, one
choice, and no logprobs. Responses require string or message-only input,
`stream: true`, plain-text output, and foreground, non-stored execution. Tool
calls, reasoning, structured output, multi-choice or multi-item output, and
provider-specific guided generation controls are excluded. Unsupported requests
pass through normally without continuation. If an initially eligible stream
later emits an unrecognized shape, continuation is disabled rather than guessing
how to splice it.

Continuation is protocol-specific. For Completions, Onwards appends the emitted
text to the original prompt. For Chat Completions, it appends one assistant
message containing the emitted text and suppresses a repeated assistant-role
chunk. For Responses, it appends an assistant `output_text` message, suppresses
repeated lifecycle setup events, and keeps response IDs, item IDs, and sequence
numbers continuous. Terminal Responses snapshots are rewritten with the full
cumulative text.

The upstream response must be successful, use the exact `text/event-stream`
media type (case-insensitive; parameters are allowed), and have no content
encoding or `Content-Encoding: identity`. Onwards sends
`Accept-Encoding: identity` for eligible requests and continues only unencoded
responses. An encoded eligible stream is left untouched; an encoded or non-SSE
continuation response is rejected rather than spliced into the client stream.
In strict mode, body sanitization cannot be guaranteed when an eligible encoded
response ignores the identity request because preserving its encoded
representation requires forwarding it unwrapped.

This is prefix-based continuation, not native token-offset resume. Chat
Completions and Responses do not expose a continuation cursor: the emitted text
is supplied as prior assistant output in a new request. The next model call may
add a preamble, repeat text, or diverge from the interrupted generation.

The buffer cap is measured in UTF-8 bytes. Once appending a recognized text
chunk would exceed the cap, continuation is disabled for that request and the
overflow text is not retained. `idle_timeout_ms` applies between upstream
events and treats an idle stream as interrupted. Final usage is forwarded from
the provider that supplies the terminal stream; it is not aggregate usage or
aggregate billing across the initial and continuation providers.

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
