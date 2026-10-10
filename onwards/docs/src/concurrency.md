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

Every request checked against the limit is counted in
`onwards_inflight_limit_checks_total{model, account}` and every refusal in
`onwards_inflight_limit_refusals_total{model, account}`, so the share of an
account's realtime requests refused on an alias is the ratio of the two. A
series appears with its first increment, in each process: the checks counter
has one for each account and alias with realtime traffic on a limited alias,
and the refusals counter one only for the accounts that have been refused. An
account that was never refused has no refusal series, so the ratio is missing
for it rather than zero.

## Batch in-flight cap

Realtime and batch are deliberately asymmetric. Realtime has no alias-wide total
in-flight cap: only the per-account limits described above, which divide an
alias's capacity between tenants rather than capping the alias as a whole. A
full downstream answers `529`, and realtime
traffic is allowed to grow into whatever capacity exists, with the per-account
limit stopping one tenant from taking it all. Batch instead gets a per-model
cap, because batch must never crowd realtime out of a model and can always be
processed later — a refused batch request is rescheduled, not lost.

A pool can also cap how many **batch** (dispatched) requests are in flight on
its alias. Unlike the per-account limit above, this is a single count for the
alias rather than a per-tenant one. It is shared across every dispatcher and
proxy instance only when a shared limiter is plugged in; with the default local
limiter the count is per process (see below). It is configured with
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
absent field) means batch is uncapped. The control layer resolves this field for
each virtual model before writing the onwards config: a positive
`batch_capacity` is used, otherwise it falls back to
`limits.batch_inflight.default_capacity` (default `200`), so an enforced
deployment caps virtual models that never set a value unless that default is
turned off.

The cap is read from the alias's default pool and counts against the alias the
request named, even when a routing rule redirects it, so a redirected request
still consumes a slot on the model it asked for. The slot is held across
failover attempts for the life of the response body and released when the body
finishes or the client disconnects.

A request over the cap receives `529` (the shared overload status) with
type `overloaded_error`, code `batch_capacity_exceeded`, and a
`Retry-After: 1` header. It is deliberately not a `429`: the dispatcher treats
`529` as a downstream overload and reduces its adaptive concurrency, whereas a
`429` is a per-key rate limit and would not. A `batch_capacity_exceeded` `529`
is admission control rather than a failed attempt, so the dispatcher
reschedules it with normal backoff without spending a retry attempt; an
ordinary `529` still spends one. The refusal is counted in
`onwards_batch_inflight_refusals_total{model}`.

Counts are per process by default. `AppState::with_batch_inflight_limiter`
plugs in a shared counter (the control layer passes the same Redis-backed
limiter used for realtime `inflight_limit`, under a reserved `__batch__` scope
so the two key spaces never collide). Enforcement is switched with
`AppState::with_batch_inflight_enforce`; when it is off, batch requests are
never refused and the limiter is not consulted at all.

`batch_inflight_limit` is consumed only by an embedding process that both plugs
in a limiter and turns enforcement on — the control layer does this when it
resolves virtual models. The standalone `onwards` binary does not: it loads
`batch_inflight_limit` from its config but never calls
`AppState::with_batch_inflight_limiter` or `AppState::with_batch_inflight_enforce`,
so enforcement stays off and the field has no effect there. To use the cap in a
standalone process, call both builders on the constructed `AppState`.

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
