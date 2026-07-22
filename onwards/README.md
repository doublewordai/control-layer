# Onwards

[![Crates.io](https://img.shields.io/crates/v/onwards)](https://crates.io/crates/onwards)
[![Documentation](https://docs.rs/onwards/badge.svg)](https://docs.rs/onwards)
[![GitHub](https://img.shields.io/badge/GitHub-doublewordai%2Fcontrol--layer-blue)](https://github.com/doublewordai/control-layer/tree/main/onwards)

A Rust-based AI Gateway that provides a unified interface for routing requests to OpenAI-compatible targets. The goal is to be as "transparent" as possible.

Onwards is developed in the control-layer workspace, embedded in the `dwctl` application, and independently published as a crate and container image.

## Quickstart

Create a `config.json`:

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "onwards_model": "gpt-4"
    }
  }
}
```

Start the gateway:

Install the published binary with `cargo install onwards`, then start the gateway:

```bash
onwards -f config.json
```

Send a request:

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

## Features

- Unified routing to any OpenAI-compatible provider
- Hot-reloading configuration with automatic file watching
- Authentication with global and per-target API keys
- Rate limiting and concurrency limiting (per-target and per-key)
- Load balancing with weighted random and priority strategies
- Automatic failover across multiple providers
- Strict mode for request validation and error standardization
- Response sanitization for OpenAI schema compliance
- Prometheus metrics
- Custom response headers
- Optional multi-step Open Responses orchestration loop (`multi-step` feature)

## Multi-Step Open Responses

The `multi-step` Cargo feature adds an orchestration loop that drives
`/v1/responses`-style requests through one or more upstream model calls,
optional server-side tool calls, and back — without onwards itself owning
any persistence. The loop is a free function (`run_response_loop`) over
two traits a consumer plugs in:

- **`MultiStepStore`** — records steps, walks the chain, runs the
  transition function (`next_action_for`: given the chain so far,
  emit the next step, complete, or fail), and assembles the final
  response. Implementing this trait is the host application's job;
  the reference implementation in
  [Doubleword's control-layer](https://github.com/doublewordai/control-layer)
  is backed by the workspace's `fusillade` crate for
  durable per-step rows.
- **`ToolExecutor`** — the same trait onwards already uses for
  single-step tool injection. The loop calls `execute` for
  `ToolKind::Http` tools; `ToolKind::Agent` tools cause the loop to
  recurse into a sub-loop under that tool's step id.

Model calls fire through `fusillade::HttpClient` so each per-step HTTP
request carries the `X-Fusillade-Request-Id` header that downstream
analytics keys on. Streaming responses propagate live token deltas to
an optional `EventSink` via fusillade's `StreamEventCallback` hook,
so warm-path SSE clients still see chunks arrive in real time while
fusillade owns reassembly into the canonical final body.

### Enable

```toml
[dependencies]
onwards = { version = "0.35.6", features = ["multi-step"] }
fusillade = "24.1.0"
```

The control-layer workspace resolves both crates through local paths during development.

The feature is off by default — the simple `/ai/v1/*` proxy path doesn't
need it and stays fusillade-free. Turning it on pulls in `fusillade` for
`RequestData`, `HttpClient`, and the streaming-callback types.

### Wire-up sketch

```rust
use onwards::{run_response_loop, LoopConfig, UpstreamTarget};
use std::sync::Arc;

let http_client = Arc::new(fusillade::ReqwestHttpClient::new(
    /* first_chunk_timeout */ std::time::Duration::from_secs(30),
    /* chunk_timeout */       std::time::Duration::from_secs(30),
    /* body_timeout */        std::time::Duration::from_secs(120),
    /* streamable_endpoints*/ vec!["/v1/chat/completions".into()],
));

let final_response = run_response_loop(
    &store,           // your MultiStepStore impl
    &tool_executor,   // your ToolExecutor impl
    &tool_ctx,        // RequestContext carrying resolved tool config
    &UpstreamTarget {
        endpoint: "https://api.openai.com".into(),
        path:     "/v1/chat/completions".into(),
        api_key:  Some(api_key.to_string()),
    },
    http_client,
    /* event_sink */  None, // Some(&sink) for streaming
    request_id,
    /* scope_parent */ None,
    LoopConfig::default(),
    /* depth */ 0,
).await?;
```

Each model_call step's `RecordedStep` carries a `sub_request_id` —
the `fusillade.requests` row id the store created for that step —
which the loop stamps on the outgoing HTTP fire so per-step analytics
line up with the right row in the database.

## Performance Tuning

### Connection Pooling

Onwards uses HTTP connection pooling to dramatically improve performance under load by reusing connections instead of creating new ones for each request. This eliminates the 1:1 request-to-file-descriptor ratio and prevents TIME_WAIT connection accumulation.

**Configure via config file:**

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key"
    }
  },
  "http_pool": {
    "max_idle_per_host": 100,
    "idle_timeout_secs": 90
  }
}
```

**When to increase `pool-max-idle-per-host`:**

The pool limit is applied **per upstream host**. Choose based on your deployment pattern:

#### Scenario 1: Fan-out (Multiple Upstreams)
*Example: Main server routes to 10+ different model providers*

- **Recommendation:** `max_idle_per_host: 200-300`
- **Why:** Traffic spreads across many upstreams, each gets moderate volume
- **Math:** 10 providers × 200 connections = 2,000 total pooled connections

```json
# Fan-out configuration
{
  "http_pool": {
    "max_idle_per_host": 300,
    "idle_timeout_secs": 90
  }
}
```

#### Scenario 2: Single Upstream (High Concurrency)
*Example: Gateway in front of a single vLLM server handling all traffic*

- **Recommendation:** `max_idle_per_host: 1000-2000`
- **Why:** ALL traffic goes to one host - needs high capacity to avoid creating new connections
- **Math:** Peak 2000 concurrent requests → 2000 pooled connections reused across all requests

```json
# Single upstream configuration
{
  "http_pool": {
    "max_idle_per_host": 2000,
    "idle_timeout_secs": 120
  }
}
```

Default values: If http_pool is omitted, defaults are 100 max idle connections per host and 90 second timeout.

**Rule of thumb:** Set `pool-max-idle-per-host` >= your expected peak concurrent requests per upstream host. If the pool is too small, new connections will be created beyond the pool limit, reducing the performance benefit.

### Idle Timeout

**Why 90 seconds is optimal:**

The `pool-idle-timeout-secs` setting (default: 90s) controls how long idle connections stay in the pool:

- **HTTP/2 standard:** Recommends keeping connections alive for 2+ minutes
- **Cloud load balancers:** Typically timeout after 60-120 seconds
- **90s balances:**
  - ✅ Long enough to reuse connections between request bursts
  - ✅ Short enough to avoid holding stale connections upstream LBs have closed
  - ✅ Prevents connection leak from forgotten idle connections

**When to adjust:**
- Increase to **120s** for more aggressive connection reuse (bursty traffic with gaps)
- Decrease to **60s** if you see connection errors (upstream closing connections sooner)

### Monitoring Connection Usage

```bash
# Monitor file descriptor usage (Linux/macOS)
lsof -p $(pgrep onwards) | wc -l

# Check connection states
ss -s  # Linux
netstat -an | grep TIME_WAIT | wc -l  # macOS/Linux
```

**Expected improvements with connection pooling:**
- **Before:** 1000+ file descriptors for 200 concurrent requests (1:1 ratio)
- **After:** 150-300 file descriptors for 200 concurrent requests (10:1 reuse ratio)
- TIME_WAIT connections drop from thousands to near-zero

## Documentation

Full documentation is available at **[doublewordai.github.io/onwards](https://doublewordai.github.io/onwards/)**, covering:

- [Configuration reference](https://doublewordai.github.io/onwards/configuration.html)
- [Authentication](https://doublewordai.github.io/onwards/authentication.html)
- [Rate limiting](https://doublewordai.github.io/onwards/rate-limiting.html)
- [Load balancing](https://doublewordai.github.io/onwards/load-balancing.html)
- [Strict mode](https://doublewordai.github.io/onwards/strict-mode.html)
- [Response sanitization](https://doublewordai.github.io/onwards/sanitization.html)
- [Contributing](https://doublewordai.github.io/onwards/contributing.html)
