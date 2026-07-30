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

Full documentation is available at **[doublewordai.github.io/control-layer/onwards](https://doublewordai.github.io/control-layer/onwards/)**, covering:

- [Configuration reference](https://doublewordai.github.io/control-layer/onwards/configuration.html)
- [Authentication](https://doublewordai.github.io/control-layer/onwards/authentication.html)
- [Rate limiting](https://doublewordai.github.io/control-layer/onwards/rate-limiting.html)
- [Load balancing](https://doublewordai.github.io/control-layer/onwards/load-balancing.html)
- [Strict mode](https://doublewordai.github.io/control-layer/onwards/strict-mode.html)
- [Response sanitization](https://doublewordai.github.io/control-layer/onwards/sanitization.html)
- [Contributing](https://doublewordai.github.io/control-layer/onwards/contributing.html)
