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

Each rejection also sets these attributes on the current trace span, usually onwards' `onwards.request` span. Strict mode refuses some requests before that span exists (body errors, payload too large, unsupported `/v1/completions` and Responses reasoning fields). Those attributes go on the enclosing span when onwards runs inside a gateway that opens one per request, such as dwctl; the standalone server opens none, so they are only counted there:

- `error.type`: the same code as the metric;
- `onwards.rejection.param`: the parameter the error names, if any. For `schema_mismatch` it is where the body stopped matching, such as `messages[].tool_calls[].function.arguments`: array positions are left out, and so is anything the request contained. The client's error carries the same value in `param`;
- `onwards.rejection.reason`: for `schema_mismatch`, how the body failed: `invalid_type`, `invalid_value`, `invalid_length`, `unknown_variant`, `unknown_field`, `missing_field`, `duplicate_field`, `no_matching_variant` or `other`;
- `onwards.account` and `onwards.api_key_id`: from the labels of the key the request presented, when it has them;
- `http.response.status_code`: on `onwards.request`.

Request parameter values and request bodies are never recorded. A client error that reports an upstream's response isn't counted.

A gateway that embeds onwards can count the requests its own layers refuse in the same series, with `onwards::rejections::RejectionContext::for_api_key`. dwctl records the refusals of its body limit, inference routing, protocol translation, prompt cache, continuation, request preparation, image normalisation and `/models` list there. A layer that only rewrites a refusal onwards made, such as dwctl's error enrichment, doesn't record it again.
