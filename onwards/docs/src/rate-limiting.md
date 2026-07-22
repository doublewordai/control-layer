# Rate Limiting

Onwards supports rate limiting using a token bucket algorithm. You can configure limits per-target and per-API-key.

## Per-target rate limiting

Add rate limiting to any target in your `config.json`:

```json
{
  "targets": {
    "rate-limited-model": {
      "url": "https://api.provider.com",
      "onwards_key": "your-api-key",
      "rate_limit": {
        "requests_per_second": 5.0,
        "burst_size": 10
      }
    }
  }
}
```

### How it works

Each target gets its own token bucket. Tokens are refilled at a rate determined by `requests_per_second`. The maximum number of tokens in the bucket is determined by `burst_size`. When the bucket is empty, requests to that target are rejected with a `429 Too Many Requests` response.

### Examples

```json
// Allow 1 request per second with burst of 5
"rate_limit": {
  "requests_per_second": 1.0,
  "burst_size": 5
}

// Allow 100 requests per second with burst of 200
"rate_limit": {
  "requests_per_second": 100.0,
  "burst_size": 200
}
```

Rate limiting is optional -- targets without `rate_limit` configuration have no rate limiting applied.

## Per-API-key rate limiting

In addition to per-target rate limiting, Onwards supports individual rate limits for different API keys. This allows you to provide different service tiers -- for example, basic users might have lower limits while premium users get higher limits.

### Configuration

Per-key rate limiting uses a `key_definitions` section in the auth configuration:

```json
{
  "auth": {
    "global_keys": ["fallback-key"],
    "key_definitions": {
      "basic_user": {
        "key": "sk-user-12345",
        "rate_limit": {
          "requests_per_second": 10,
          "burst_size": 20
        }
      },
      "premium_user": {
        "key": "sk-premium-67890",
        "rate_limit": {
          "requests_per_second": 100,
          "burst_size": 200
        }
      },
      "enterprise_user": {
        "key": "sk-enterprise-abcdef",
        "rate_limit": {
          "requests_per_second": 500,
          "burst_size": 1000
        }
      }
    }
  },
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "keys": ["basic_user", "premium_user", "enterprise_user", "fallback-key"]
    }
  }
}
```

### Priority order

Rate limits are checked in this order:

1. **Per-key rate limits** (if the API key has limits configured)
2. **Per-target rate limits** (if the target has limits configured)

If either limit is exceeded, the request returns `429 Too Many Requests`.

### Usage examples

**Basic user request (10/sec limit):**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer sk-user-12345" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4", "messages": [{"role": "user", "content": "Hello!"}]}'
```

**Premium user request (100/sec limit):**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer sk-premium-67890" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4", "messages": [{"role": "user", "content": "Hello!"}]}'
```

**Legacy key (no per-key limits, only target limits apply):**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer fallback-key" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4", "messages": [{"role": "user", "content": "Hello!"}]}'
```
