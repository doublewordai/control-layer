# API Usage

## List available models

Get a list of all configured targets in the OpenAI models format:

```bash
curl http://localhost:3000/v1/models
```

## Sending requests

Send requests to the gateway using the standard OpenAI API format:

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

The `model` field determines which target receives the request.

## Model override header

Override the target using the `model-override` header. This routes the request to a different target regardless of the `model` field in the body:

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "model-override: claude-3" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

This is also used for routing requests without bodies -- for example, to get the embeddings usage for your organization:

```bash
curl -X GET http://localhost:3000/v1/organization/usage/embeddings \
  -H "model-override: claude-3"
```

## Metrics

When the `--metrics` flag is enabled (the default), Prometheus metrics are exposed on a separate port:

```bash
curl http://localhost:9090/metrics
```

See [Command Line Options](cli.md) for metrics configuration flags.
