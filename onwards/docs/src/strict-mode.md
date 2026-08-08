# Strict Mode

Strict mode provides enhanced security and API compliance by using typed request/response handlers instead of the default wildcard passthrough router. This feature:

- **Validates all requests** against OpenAI API schemas before forwarding
- **Sanitizes all responses** by removing third-party provider metadata
- **Standardizes error messages** to prevent information leakage
- **Ensures model field consistency** between requests and responses
- **Supports streaming and non-streaming** for all endpoints

This is useful when you need guaranteed API compatibility, security hardening, or protection against third-party response variations.

## Enabling strict mode

Strict mode is a **global configuration** that applies to all targets in your gateway. Add `strict_mode: true` at the top level of your configuration (not inside individual targets).

```json
{
  "strict_mode": true,
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-openai-key"
    },
    "claude": {
      "url": "https://api.anthropic.com",
      "onwards_key": "sk-ant-key"
    }
  }
}
```

When enabled, all requests to all targets will use strict mode validation and sanitization.

## How it works

When `strict_mode: true` is enabled:

1. **Request validation**: Incoming requests are deserialized through OpenAI schemas. Invalid requests receive immediate `400 Bad Request` errors with clear messages.
2. **Response sanitization**: Third-party responses are deserialized (automatically dropping unknown fields). If deserialization fails, a standard error is returned - malformed responses are never passed through. The model field is rewritten to match the original request, then re-serialized as clean OpenAI responses with correct Content-Length headers.
3. **Error standardization**: Third-party errors are logged internally but never forwarded to clients. Clients receive standardized OpenAI-format errors based only on HTTP status codes.
4. **Streaming support**: SSE streams are parsed line-by-line to handle multi-line data events and strip comment lines, each chunk is sanitized, and re-emitted as clean events.

## Security benefits

**Prevents information leakage:**
- Third-party stack traces, database errors, and debug information are never exposed
- Error responses contain only standard HTTP status codes and generic messages
- No provider-specific metadata (trace IDs, internal IDs, costs) reaches clients
- Malformed provider responses fail closed with standard errors (never leaked)
- SSE comment lines stripped to prevent metadata leakage in streaming responses

**Ensures consistency:**
- Responses always match OpenAI's API format exactly
- The `model` field always reflects what the client requested, not what the provider returned
- Extra fields like `provider`, `cost`, `trace_id` are automatically dropped

**Fast failure:**
- Invalid requests fail immediately with clear, actionable error messages
- No wasted upstream requests for malformed input
- Reduces debugging time for integration issues

## Error standardization

When strict mode is enabled, all error responses follow OpenAI's error format exactly:

```json
{
  "error": {
    "message": "Invalid request",
    "type": "invalid_request_error",
    "param": null,
    "code": null
  }
}
```

**Status code mapping:**

| HTTP Status | Error Type | Message |
|------------|------------|---------|
| 400 | `invalid_request_error` | Invalid request |
| 401 | `authentication_error` | Authentication failed |
| 403 | `permission_error` | Permission denied |
| 404 | `not_found_error` | Not found |
| 429 | `rate_limit_error` | Rate limit exceeded |
| 500 | `api_error` | Internal server error |
| 502 | `api_error` | Bad gateway |
| 503 | `api_error` | Service unavailable |

For **untrusted** upstreams, account-class status codes are remapped *before* this table is applied — see [Account-class status code masking](#account-class-status-code-masking).

Third-party error details are always logged server-side but never sent to clients.

## Account-class status code masking

Untrusted providers can return status codes that describe the *operator's* relationship with the provider — not the *caller's* relationship with onwards. Surfacing those would let callers probe the operator's account state. Onwards rewrites:

| Upstream status | Surfaced as | Rationale |
|---|---|---|
| `401 Unauthorized` | `502 Bad Gateway` | The caller's auth is fine; ours isn't — don't expose that |
| `402 Payment Required` | `502 Bad Gateway` | Provider billing state is internal |
| `403 Forbidden` | `502 Bad Gateway` | Permission failures are operator-scoped |
| `451 Unavailable For Legal Reasons` | `502 Bad Gateway` | Jurisdictional restrictions are operator-scoped |
| `408 Request Timeout` | `504 Gateway Timeout` | Upstream timeouts are gateway timeouts from the caller's view |

User-facing codes (`400`, `404`, `413`, `422`, `429`) pass through unchanged — they're real signal about the caller's request and downstream retry logic depends on seeing them.

For untrusted providers, the embedded error's `code` is read whether the provider encodes it as a number (`429`), a numeric string (`"429"`), or a named string — the rate-limit family (`rate_limit`, `rate_limit_error`, `rate_limit_exceeded`) maps to `429` so retry semantics survive, and unrecognized names fall back to `500`. The resulting code is then masked per the table above.

Masking is applied to both non-streaming error responses (where the upstream status is the outer HTTP status) and to embedded `error.code` values in SSE streams (so a downstream reassembler that reclassifies on the embedded code surfaces the masked code as the HTTP status). Trusted targets bypass masking entirely.

## Errors embedded in 2xx SSE streams

Some providers return `HTTP 200 OK` and start an SSE stream even when the *upstream* of the upstream has failed. The failure surfaces as a chunk with shape:

```text
data: {"id":"...","object":"chat.completion.chunk","choices":[],"error":{"code":429,"message":"..."}}
```

The naive strict deserializer would parse this as a valid (empty) chunk and silently drop the `error` field, leaving a downstream stream reassembler with no completion content and no signal that anything went wrong.

Onwards detects the embedded `error` envelope *before* strict deserialization and forwards it as a stand-alone event:

```text
data: {"error":{"message":"Rate limit exceeded","type":"rate_limit_error","param":null,"code":429}}
```

The chunk wrapper is stripped so the emitted SSE data line begins with `{"error"` — that prefix is what the [`fusillade`](https://crates.io/crates/fusillade) reassembler matches on to reclassify the response from HTTP 200 to the embedded `code`. End-to-end, an upstream-of-upstream 429 becomes a real HTTP 429 to the client, with retry semantics intact.

This applies to both bare error chunks and chunks that carry completion fields alongside the error. Untrusted upstreams additionally have the embedded `error.code` masked (per the table above) and the prose replaced with a generic message; trusted targets forward the envelope verbatim.

## Supported endpoints

Strict mode currently supports:

- `/v1/chat/completions` (streaming and non-streaming) - Full sanitization
- `/v1/embeddings` - Full sanitization
- `/v1/responses` (Open Responses API, non-streaming) - Full sanitization
- `/v1/models` - Model listing (no sanitization needed)

All supported endpoints include:
- **Request validation** - Invalid requests fail immediately with clear error messages
- **Response sanitization** - Third-party metadata automatically removed
- **Model field rewriting** - Ensures consistency with client request
- **Error standardization** - Third-party error details never exposed

Requests to unsupported endpoints will return `404 Not Found` when strict mode is enabled.

## Comparison with response sanitization

| Feature | Response Sanitization | Strict Mode |
|---------|----------------------|-------------|
| Request validation | ✗ No | ✓ Yes |
| Response sanitization | ✓ Yes | ✓ Yes |
| Error standardization | ✗ No | ✓ Yes |
| Endpoint coverage | `/v1/chat/completions` only | Chat, Embeddings, Responses, Models |
| Router type | Wildcard passthrough | Typed handlers |
| Use case | Simple response cleaning | Production security & compliance |

**Important:** When strict mode is enabled globally, the per-target `sanitize_response` flag is automatically ignored. Strict mode handlers perform complete sanitization themselves, so enabling `sanitize_response: true` on individual targets has no effect and won't cause double sanitization.

**When to use strict mode:**
- Production deployments requiring security hardening
- Compliance requirements around error message content
- Multi-provider setups needing guaranteed response consistency
- Applications that need request validation before forwarding

**When to use response sanitization:**
- Simple use cases where you only need response cleaning
- Non-security-critical deployments
- Maximum flexibility with endpoint coverage

## Trusted Providers

In strict mode, you can mark providers as trusted to bypass error sanitization while keeping success response sanitization. This is useful when you have providers you fully control (e.g., your own OpenAI account) and want their detailed error messages to help with debugging, while still ensuring response consistency.

Trust can be set at two levels:

- **Pool level** (`trusted` on the target) — default for all providers in the pool
- **Provider level** (`trusted` inside a provider entry) — overrides the pool default for that specific provider

This is the only exception to strict mode's error standardization guarantees: when a provider is effectively trusted, its errors may be forwarded with full third-party details instead of being standardized.

### Configuration

**Single-provider (pool-level trusted):**

```json
{
  "strict_mode": true,
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-...",
      "trusted": true
    },
    "third-party": {
      "url": "https://some-provider.com",
      "onwards_key": "sk-..."
    }
  }
}
```

**Uniform pool-level trusted:**

```json
{
  "strict_mode": true,
  "targets": {
    "gpt-4-pool": {
      "trusted": true,
      "providers": [
        { "url": "https://api.openai.com", "onwards_key": "sk-primary-..." },
        { "url": "https://api.openai.com", "onwards_key": "sk-backup-..." }
      ]
    }
  }
}
```

**Mixed trust within a pool (per-provider override):**

```json
{
  "strict_mode": true,
  "targets": {
    "gpt-4": {
      "trusted": false,
      "providers": [
        { "url": "https://internal.example.com", "trusted": true },
        { "url": "https://external.example.com" }
      ]
    }
  }
}
```

Here, the internal provider's error responses pass through unchanged. The external provider omits `trusted`, so it inherits the pool default (`false`) and has its error responses sanitized. This lets you mix trusted internal infrastructure with untrusted external providers inside a single pool.

### Behavior

When a pool is marked as `trusted: true`:

**Success responses (200 OK) are STILL sanitized:**
- Model field IS rewritten to match the client's request
- Provider-specific metadata IS removed (costs, trace IDs, custom fields)
- Response IS validated against OpenAI schemas
- Content-Length headers ARE updated correctly
- Streaming responses ARE parsed and sanitized line-by-line

**Error responses (4xx, 5xx) bypass sanitization:**
- Original error messages and metadata forwarded to clients
- Provider-specific error details preserved (stack traces, debug info)
- Custom error fields passed through unchanged

This allows you to get detailed debugging information from errors while maintaining response consistency for successful requests.

### Security Warning

⚠️ **Use trusted providers carefully.** Marking a provider as trusted bypasses error sanitization for that provider:

**What is exposed for trusted providers:**
- Error details and stack traces from the provider
- Provider-specific error metadata (trace IDs, internal error codes)
- Debug information in error responses

**What is NOT exposed (still sanitized):**
- Success responses are fully sanitized (model rewritten, metadata removed)
- Provider metadata in successful requests (costs, trace IDs) is still stripped
- Responses still match OpenAI schema exactly for successful requests

**Only mark providers as trusted when you fully control or trust them.** This typically means:
- Your own OpenAI/Anthropic accounts (providers using your API keys)
- Self-hosted models you operate
- Internal services you maintain

**Do not mark third-party providers as trusted** unless you want their detailed error messages exposed to your clients. Trusted providers are designed for debugging your own infrastructure, not for production use with external providers.

### Interaction with Model Override Header

Onwards supports the `model-override` header to route requests to different pools than specified in the request body. Trust is resolved from the **provider that actually handles the request** (after routing and provider selection), so it correctly reflects the resolved model rather than what the client specified in the body.

This means if a client sends:
- Request body with `"model": "trusted-pool"`
- Header with `model-override: untrusted-pool`

The request will route to `untrusted-pool` and **sanitization will be applied** based on that pool's provider trust settings, preventing metadata leakage. Clients cannot bypass sanitization by exploiting mismatches between body and header model resolution.

## Implementation details

For developers working on the Onwards codebase:

**Router architecture:**
- Strict mode uses typed Axum handlers defined in `src/strict/handlers.rs`
- Each endpoint has dedicated request/response schema types in `src/strict/schemas/`
- Requests are deserialized using serde, which automatically validates structure
- `response_transform_fn` is skipped when strict mode is enabled to prevent double sanitization

**Response sanitization:**
- Responses are deserialized through strict schemas (extra fields automatically dropped by serde)
- Malformed responses fail closed with standard errors - never passed through
- Model field is rewritten to match the original request model
- Re-serialized to ensure only defined fields are present
- Content-Length headers updated to match sanitized response size
- Applies to both non-streaming responses and SSE chunks
- SSE streams processed line-by-line to handle multi-line events and strip comments

**Error handling:**
- Third-party errors are intercepted in `sanitize_error_response()`
- Original error logged with `error!()` macro for server-side debugging
- Standard error generated based only on HTTP status code
- OpenAI-compatible format guaranteed via `error_response()` helper
- Deserialization failures return standard errors, never leak malformed responses

**Trust resolution:**

- `target_message_handler` resolves effective trust as `provider.trusted.unwrap_or(pool.trusted)` after provider selection
- The resolved trust is attached to the response via a `ResolvedTrust` extension
- Strict mode handlers read it via `ForwardResult.trusted` — no separate pool lookup needed
- Ensures trust reflects the actual provider that handled the request, including after fallback retries

**Testing:**

- Request/response schema tests in each schema module
- Integration tests in `src/strict/handlers.rs` verify sanitization behavior
- Tests verify fail-closed behavior on malformed responses (no passthrough)
- Tests verify SSE multi-line events and comment stripping
- Tests verify Content-Length header correctness after sanitization
- Tests verify per-provider trusted overrides pool-level setting in both directions
