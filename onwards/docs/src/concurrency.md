# Concurrency Limiting

In addition to [rate limiting](rate-limiting.md) (which controls *how fast* requests are made), concurrency limiting controls *how many* requests are processed simultaneously. This is useful for managing resource usage and preventing overload.

## Per-target concurrency limiting

Limit the number of concurrent requests to a specific target:

```json
{
  "targets": {
    "resource-limited-model": {
      "url": "https://api.provider.com",
      "onwards_key": "your-api-key",
      "concurrency_limit": {
        "max_concurrent_requests": 5
      }
    }
  }
}
```

With this configuration, only 5 requests will be processed concurrently for this target. Additional requests will receive a `429 Too Many Requests` response until an in-flight request completes.

## Per-API-key concurrency limiting

You can set different concurrency limits for different API keys:

```json
{
  "auth": {
    "key_definitions": {
      "basic_user": {
        "key": "sk-user-12345",
        "concurrency_limit": {
          "max_concurrent_requests": 2
        }
      },
      "premium_user": {
        "key": "sk-premium-67890",
        "concurrency_limit": {
          "max_concurrent_requests": 10
        },
        "rate_limit": {
          "requests_per_second": 100,
          "burst_size": 200
        }
      }
    }
  },
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key"
    }
  }
}
```

## Combining rate limiting and concurrency limiting

You can use both rate limiting and concurrency limiting together:

- **Rate limiting** controls how fast requests are made over time
- **Concurrency limiting** controls how many requests are active at once

```json
{
  "targets": {
    "balanced-model": {
      "url": "https://api.provider.com",
      "onwards_key": "your-api-key",
      "rate_limit": {
        "requests_per_second": 10,
        "burst_size": 20
      },
      "concurrency_limit": {
        "max_concurrent_requests": 5
      }
    }
  }
}
```

## How it works

Concurrency limits use a semaphore-based approach:

1. When a request arrives, it tries to acquire a permit
2. If a permit is available, the request proceeds (holding the permit)
3. If no permits are available, the request is rejected with `429 Too Many Requests`
4. When the request completes, the permit is automatically released

The error response distinguishes between rate limiting and concurrency limiting:

- Rate limit: `"code": "rate_limit"`
- Concurrency limit: `"code": "concurrency_limit_exceeded"`

Both use HTTP 429 status code for consistency.
