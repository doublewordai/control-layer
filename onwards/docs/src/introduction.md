# Onwards

[![Crates.io](https://img.shields.io/crates/v/onwards)](https://crates.io/crates/onwards)
[![Documentation](https://docs.rs/onwards/badge.svg)](https://docs.rs/onwards)
[![GitHub](https://img.shields.io/badge/GitHub-doublewordai%2Fcontrol--layer-blue)](https://github.com/doublewordai/control-layer/tree/main/onwards)

A Rust-based AI Gateway that provides a unified interface for routing requests to OpenAI-compatible targets. The goal is to be as "transparent" as possible.

## Features

- **Unified routing** to any OpenAI-compatible provider
- **Hot-reloading** configuration with automatic file watching
- **Authentication** with global and per-target API keys
- **Rate limiting** per-target and per-API-key with token bucket algorithm
- **Concurrency limiting** per-target and per-API-key
- **Load balancing** with weighted random and priority strategies
- **Automatic failover** across multiple providers
- **Strict mode** for request validation, response sanitization, and error standardization
- **Response sanitization** for OpenAI schema compliance
- **Prometheus metrics** for monitoring
- **Custom response headers** for pricing and metadata
- **Upstream auth customization** for non-standard providers
