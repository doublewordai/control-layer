# Command Line Options

| Flag | Description | Default |
|------|-------------|---------|
| `--targets <file>` / `-f <file>` | Path to configuration file | Required |
| `--port <port>` | Port to listen on | `3000` |
| `--watch` | Enable configuration file watching for hot-reloading | `true` |
| `--metrics` | Enable Prometheus metrics endpoint | `true` |
| `--metrics-port <port>` | Port for Prometheus metrics | `9090` |
| `--metrics-prefix <prefix>` | Prefix for metric names | `onwards` |

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
