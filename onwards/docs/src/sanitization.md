# Response Sanitization

Onwards can enforce strict OpenAI API schema compliance for `/v1/chat/completions` responses. This feature:

- **Removes provider-specific fields** from responses
- **Rewrites the model field** to match what the client originally requested
- **Supports both streaming and non-streaming** responses
- **Validates responses** against OpenAI's official API schema
- **Sanitizes error responses** to prevent upstream provider details from leaking to clients

This is useful when proxying to non-OpenAI providers that add custom fields, or when using `onwards_model` to rewrite model names upstream.

> **Note:** For production deployments requiring additional security (request validation, error standardization), consider using [Strict Mode](strict-mode.md) instead, which includes response sanitization plus comprehensive security features.

## Enabling response sanitization

Add `sanitize_response: true` to any target or provider in your configuration.

**Single provider:**

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-key",
      "onwards_model": "gpt-4-turbo-2024-04-09",
      "sanitize_response": true
    }
  }
}
```

**Pool with multiple providers:**

```json
{
  "targets": {
    "gpt-4": {
      "sanitize_response": true,
      "providers": [
        {
          "url": "https://api1.example.com",
          "onwards_key": "sk-key-1"
        },
        {
          "url": "https://api2.example.com",
          "onwards_key": "sk-key-2"
        }
      ]
    }
  }
}
```

## How it works

When `sanitize_response: true` and a client requests `model: gpt-4`:

1. **Request sent upstream** with `model: gpt-4`
2. **Upstream responds** with custom fields and `model: gpt-4-turbo-2024-04-09`
3. **Onwards sanitizes**:
   - Parses response using OpenAI schema (removes unknown fields)
   - Rewrites `model` field to `gpt-4` (matches original request)
   - Reserializes clean response
4. **Client receives** standard OpenAI response with `model: gpt-4`

## Common use cases

**Third-party providers** (e.g., Together AI) often add extra fields like `provider`, `native_finish_reason`, `cost`, etc. Sanitization strips these.

**Provider comparison** -- normalize responses from different providers for consistent handling.

**Debugging** -- reduce noise by filtering to only standard OpenAI fields.

## Error sanitization

When `sanitize_response: true`, error responses from upstream providers are also sanitized. This prevents information leakage -- upstream error bodies can contain provider names, internal URLs, and model identifiers that you may not want exposed to clients.

### How it works

Onwards replaces the upstream error body with a generic OpenAI-compatible error, while preserving the original HTTP status code:

- **4xx errors** are replaced with:

```json
{
  "error": {
    "message": "The upstream provider rejected the request.",
    "type": "invalid_request_error",
    "param": null,
    "code": "upstream_error"
  }
}
```

- **5xx errors** (and any other non-2xx status) are replaced with:

```json
{
  "error": {
    "message": "An internal error occurred. Please try again later.",
    "type": "internal_error",
    "param": null,
    "code": "internal_error"
  }
}
```

The original error body is logged at `ERROR` level (up to 64 KB) for debugging, so operators can still investigate upstream failures without exposing details to clients.

### Errors embedded in 2xx SSE streams

Some providers return `HTTP 200 OK` and start an SSE stream even when the upstream of the upstream has failed, embedding the failure in a chunk alongside (or instead of) normal completion fields:

```text
data: {"id":"...","object":"chat.completion.chunk","choices":[],"error":{"code":429,"message":"..."}}
```

Without handling, the lenient deserializer absorbs the `error` field into its unknown-fields map and drops it on re-serialize, leaving the caller with a content-less stream and no signal that anything went wrong. With `sanitize_response: true`, Onwards detects the embedded `error` envelope and forwards it as a stand-alone event with the chunk wrapper stripped:

```text
data: {"error":{"code":429,"message":"..."}}
```

The emitted data line begins with `{"error"`, the prefix downstream reassemblers match on to reclassify the response from HTTP 200 to the embedded `code`.

**Non-strict mode forwards the error verbatim.** The provider's message and the original status `code` pass through unchanged — non-strict mode does **not** mask account-class codes (`401`/`402`/`403`/`451`) or replace the provider's prose. If you need that protection so callers can't probe the operator's auth/billing/jurisdictional state, use [Strict Mode](strict-mode.md), which masks account-class codes and replaces untrusted error messages with generic ones.

> ⚠️ **Security warning:** Verbatim forwarding passes the **entire** error object through to your clients — every field, including the provider's `message`, billing/auth state hints, and any nested `metadata`/`raw`/unknown fields. Do not enable `sanitize_response: true` on targets proxying untrusted third parties if you need to hide upstream internals. Use [Strict Mode](strict-mode.md) for production deployments requiring information-leakage prevention.

### Error format

All Onwards error responses (both sanitized upstream errors and errors generated by Onwards itself) use the OpenAI-compatible `{"error": {...}}` envelope:

```json
{
  "error": {
    "message": "...",
    "type": "...",
    "param": null,
    "code": "..."
  }
}
```

| Field | Description |
|-------|-------------|
| `message` | Human-readable error description |
| `type` | Error category (`invalid_request_error`, `rate_limit_error`, `internal_error`) |
| `param` | The request parameter that caused the error, if applicable |
| `code` | Machine-readable error code |

## Supported endpoints

Currently supports:

- `/v1/chat/completions` (streaming and non-streaming)
