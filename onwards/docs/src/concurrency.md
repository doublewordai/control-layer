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

## Per-account in-flight limits

A pool can cap how many realtime requests each account has in flight on its
alias. A key's account is its `account` label, so every key of an account shares
one count. `inflight_limit` is the default and `account_inflight_limits`
overrides it for named accounts:

```json
{
  "auth": {
    "global_keys": [],
    "key_definitions": {
      "acme_backend": { "key": "sk-acme-1", "labels": { "account": "acme" } },
      "acme_batch": { "key": "sk-acme-2", "labels": { "account": "acme" } }
    }
  },
  "targets": {
    "gpt-4": {
      "inflight_limit": 20,
      "account_inflight_limits": { "acme": 200 },
      "providers": [{ "url": "https://api.openai.com", "onwards_key": "sk-your-openai-key" }]
    }
  }
}
```

The slot is taken after routing rules and counts against the alias the request
named, even when a rule redirects it to another alias, so the named alias's
limits apply. It is held across failover attempts and released when the
response body finishes or the client disconnects. A request
over the limit receives `429` with code `inflight_limit_exceeded` and a
`Retry-After: 1` header. The limits belong to the alias as a whole: they are
read from its default pool, whichever request-class pool serves the request.
Requests carrying the header set with
`AppState::with_first_token_timeout_exempt_header` (dispatched batch and async
work) and requests from keys with no `account` label are not counted, and the
plugged-in limiter can admit others without counting them. Counts are per
process by default; `AppState::with_inflight_limiter` plugs in a shared
counter so several instances enforce one limit.

## Batch in-flight cap

A pool can also cap how many **batch** (dispatched) requests are in flight on
its alias. Unlike the per-account limit above, this is one global count for the
alias, shared across every dispatcher and proxy instance, so it is a ceiling on
batch traffic rather than a per-tenant one. It is configured with
`batch_inflight_limit`:

```json
{
  "targets": {
    "gpt-4": {
      "inflight_limit": 20,
      "batch_inflight_limit": 8,
      "providers": [{ "url": "https://api.openai.com", "onwards_key": "sk-your-openai-key" }]
    }
  }
}
```

A batch request is one onwards classifies as dispatched: it carries the header
set with `AppState::with_first_token_timeout_exempt_header`. Realtime requests
are never counted against this cap, and batch requests are never counted
against the per-account `inflight_limit`. The two counts are independent, so an
alias can serve realtime traffic while its batch slots are full. `None` (or an
absent field) means batch is uncapped.

The cap is read from the alias's default pool and counts against the alias the
request named, even when a routing rule redirects it, so a redirected request
still consumes a slot on the model it asked for. The slot is held across
failover attempts for the life of the response body and released when the body
finishes or the client disconnects.

A request over the cap receives `529` (the shared overload status) with
type `overloaded_error`, code `batch_capacity_exceeded`, and a
`Retry-After: 1` header. It is deliberately not a `429`: the dispatcher treats
`529` as a downstream overload and reduces its adaptive concurrency, whereas a
`429` is a per-key rate limit and would not. The refusal is counted in
`onwards_batch_inflight_refusals_total{model}`.

Counts are per process by default. `AppState::with_batch_inflight_limiter`
plugs in a shared counter (the control layer passes the same Redis-backed
limiter used for realtime `inflight_limit`, under a reserved `__batch__` scope
so the two key spaces never collide). Enforcement is switched with
`AppState::with_batch_inflight_enforce`; when it is off, batch requests are
never refused and the limiter is not consulted at all.

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
