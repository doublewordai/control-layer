# Command Line Options

| Flag | Description | Default |
|------|-------------|---------|
| `--targets <file>` / `-f <file>` | Path to configuration file | Required |
| `--port <port>` | Port to listen on | `3000` |
| `--watch` | Enable configuration file watching for hot-reloading | `true` |
| `--metrics` | Enable Prometheus metrics endpoint | `true` |
| `--metrics-port <port>` | Port for metrics and `/healthz`, `/readyz` | `9090` |
| `--metrics-prefix <prefix>` | Prefix for metric names | `onwards` |
| `--shutdown-delay-secs <seconds>` | Continue serving after readiness fails, before closing admission | `5` |
| `--shutdown-timeout-secs <seconds>` | Deadline for draining accepted requests after closing admission; must be positive | `300` |

The shutdown settings also accept `ONWARDS_SHUTDOWN_DELAY_SECS` and
`ONWARDS_SHUTDOWN_TIMEOUT_SECS`. See [Graceful shutdown](shutdown.md) for the
signal sequence, health probes and Kubernetes termination budget.

## Examples

Start with defaults:

```bash
cargo run -- -f config.json
```

Custom port, metrics disabled:

```bash
cargo run -- -f config.json --port 8080 --metrics false
```

Custom metrics configuration:

```bash
cargo run -- -f config.json --metrics-port 9100 --metrics-prefix gateway
```
