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

### Rejected requests

Every client error that onwards decides on itself, such as an unknown model, a failed reasoning check, a strict-mode schema error or a rate limit, increments `onwards_rejections_total{model, status, code, traffic}`:

- `code` is the error code returned to the client. Strict-mode body errors carry no code, so they are counted as:
  - `invalid_json`: malformed JSON;
  - `schema_mismatch`: valid JSON that doesn't match the schema;
  - `invalid_content_type`: a `/v1/responses` body not sent as JSON;
  - `invalid_body`: a `/v1/responses` body that couldn't be read.
- `model` is the configured model the request names, without any serving-class suffix such as `:interactive`. It is empty when the request names no configured model.
- `traffic` is `dispatched` for requests carrying the first-token-timeout exempt header and `realtime` otherwise.

Each rejection also sets these attributes on the current trace span: onwards' `onwards.request` span, or the enclosing span for a strict-mode refusal made before forwarding:

- `error.type`: the same code as the metric;
- `onwards.rejection.param`: the parameter the error names, if any;
- `onwards.account` and `onwards.api_key_id`: from the labels of the key the request presented, when it has them;
- `http.response.status_code`: on `onwards.request`.

Each rejection is also logged at `info` with its status, code, parameter, model, account and API key ID, except refusals by a request limit, which are only counted and traced: a client retrying in a tight loop would otherwise log every attempt. These are:

- `rate_limit`: the key's, the model's or a provider's rate limit;
- `concurrency_limit_exceeded`: the key's concurrency limit;
- `inflight_limit_exceeded`: the account's in-flight limit on the model.

Request parameter values and request bodies are never recorded. A client error that reports an upstream's response isn't counted.
