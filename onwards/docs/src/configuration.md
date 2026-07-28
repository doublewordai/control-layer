# Configuration

Onwards is configured through a JSON file. Each key in the `targets` object defines a model alias that clients can request.

## Global options

| Option | Type | Required | Description |
|--------|------|----------|-------------|
| `strict_mode` | bool | No | Enable strict mode globally for all targets (see [Strict Mode](strict-mode.md)). Default: false |
| `auth` | object | No | Global authentication configuration (see [Authentication](authentication.md)) |
| `targets` | object | Yes | Map of model aliases to target configurations |

## Target options

| Option | Type | Required | Description |
|--------|------|----------|-------------|
| `url` | string | Yes | Base URL of the AI provider |
| `onwards_key` | string | No | API key to include in requests to the target |
| `onwards_model` | string | No | Model name to use when forwarding requests |
| `keys` | string[] | No | API keys required for authentication to this target |
| `rate_limit` | object | No | Per-target rate limiting (see [Rate Limiting](rate-limiting.md)) |
| `concurrency_limit` | object | No | Per-target concurrency limiting (see [Concurrency Limiting](concurrency.md)) |
| `upstream_auth_header_name` | string | No | Custom header name for upstream auth (default: `Authorization`) |
| `upstream_auth_header_prefix` | string | No | Custom prefix for upstream auth header value (default: `Bearer `) |
| `response_headers` | object | No | Key-value pairs to add or override in the response headers |
| `sanitize_response` | bool | No | Enforce strict OpenAI schema compliance for responses only (see [Sanitization](sanitization.md)) |
| `propagate_trace_context` | optional bool | No | Inject W3C `traceparent` / `tracestate` headers on outbound requests; omit to inherit from the resolved `trusted` value. **Provider-scoped:** valid on a single-provider target and on each entry of a pool's `providers` array — *not* as a top-level key on a pool that uses `providers`. See [Trace context propagation](load-balancing.md#trace-context-propagation). |
| `reasoning_translation` | object | No | Translate canonical OpenAI reasoning efforts into this provider's request shape. Provider-scoped in load-balanced pools. |
| `strategy` | string | No | Load balancing strategy: `weighted_random` or `priority` |
| `fallback` | object | No | Retry configuration (see [Load Balancing](load-balancing.md)) |
| `providers` | array | No | Array of provider configurations for load balancing |

## Reasoning translation

Clients use `reasoning_effort` on Chat Completions and `reasoning.effort` on Responses. The complete OpenAI-compatible effort set is `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`.

Each configured surface has a required `writes` array and a required `unsupported_efforts` array. Every write must map the same supported efforts. The mapped efforts and `unsupported_efforts` must be disjoint and together explicitly account for all seven values. This makes accepting, collapsing, or disabling an OpenAI effort a conscious provider configuration choice.

For a model with native effort levels, preserve `reasoning_effort` and reject levels the model does not support:

```json
{
  "targets": {
    "gpt-oss": {
      "url": "https://inference.example.com/v1",
      "reasoning_translation": {
        "chat_completions": {
          "unsupported_efforts": ["none", "minimal", "xhigh", "max"],
          "writes": [{
            "target_path": "/reasoning_effort",
            "values": {
              "low": "low",
              "medium": "medium",
              "high": "high"
            }
          }]
        }
      }
    }
  }
}
```

For vLLM or SGLang models controlled by an absolute reasoning budget, retain `reasoning_effort` to activate thinking and add `thinking_token_budget`:

```json
{
  "chat_completions": {
    "unsupported_efforts": [],
    "writes": [
      {
        "target_path": "/reasoning_effort",
        "values": {
          "none": "none",
          "minimal": "minimal",
          "low": "low",
          "medium": "medium",
          "high": "high",
          "xhigh": "xhigh",
          "max": "max"
        }
      },
      {
        "target_path": "/thinking_token_budget",
        "values": {
          "none": 0,
          "minimal": 512,
          "low": 1024,
          "medium": 4096,
          "high": 8192,
          "xhigh": 12288,
          "max": 16384
        }
      }
    ]
  }
}
```

Budget values are model-specific and should be established through evaluation; the values above are illustrative, not defaults. Chat Completions requests using a budget mapping must set non-null `max_completion_tokens` (or legacy `max_tokens`) above the selected budget. Responses requests must set `max_output_tokens` above it. A missing limit returns `422`; an equal or smaller limit returns `400`. Budgets are never silently clipped.

Binary providers can deliberately collapse all enabled efforts to one boolean while still naming every mapping:

```json
{
  "chat_completions": {
    "unsupported_efforts": [],
    "writes": [{
      "target_path": "/thinking",
      "values": {
        "none": false,
        "minimal": true,
        "low": true,
        "medium": true,
        "high": true,
        "xhigh": true,
        "max": true
      }
    }]
  }
}
```

An omitted client effort injects nothing. If a requested effort is unsupported by any provider in a fallback pool, or its absolute budget is incompatible with the request limit, the request is rejected before an upstream attempt.

Provider-native reasoning controls in client requests, including `thinking_token_budget`, are rejected. Legacy Completions does not support reasoning controls. Per-model capability discovery through `/v1/models` is intentionally left to the control layer.

## Rate limit object

| Field | Type | Description |
|-------|------|-------------|
| `requests_per_second` | float | Number of requests allowed per second |
| `burst_size` | integer | Maximum burst size of requests |

## Concurrency limit object

| Field | Type | Description |
|-------|------|-------------|
| `max_concurrent_requests` | integer | Maximum number of concurrent requests |

## Auth configuration

The top-level `auth` object configures global authentication:

| Field | Type | Description |
|-------|------|-------------|
| `global_keys` | string[] | Keys that grant access to all authenticated targets |
| `key_definitions` | object | Named key definitions with per-key rate/concurrency limits |

See [Authentication](authentication.md) for details.

## Minimal example

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key"
    }
  }
}
```

## Full example

```json
{
  "strict_mode": true,
  "auth": {
    "global_keys": ["global-api-key-1"],
    "key_definitions": {
      "premium_user": {
        "key": "sk-premium-67890",
        "rate_limit": {
          "requests_per_second": 100,
          "burst_size": 200
        },
        "concurrency_limit": {
          "max_concurrent_requests": 10
        }
      }
    }
  },
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "onwards_model": "gpt-4-turbo",
      "keys": ["premium_user"],
      "rate_limit": {
        "requests_per_second": 50,
        "burst_size": 100
      },
      "concurrency_limit": {
        "max_concurrent_requests": 20
      },
      "response_headers": {
        "Input-Price-Per-Token": "0.0001",
        "Output-Price-Per-Token": "0.0002"
      },
      "sanitize_response": true
    }
  }
}
```
