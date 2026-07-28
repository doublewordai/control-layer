# Response Headers

Onwards can include custom headers in the response for each target. These can override existing headers or add new ones.

## Configuration

```json
{
  "targets": {
    "model-with-headers": {
      "url": "https://api.provider.com",
      "onwards_key": "your-api-key",
      "response_headers": {
        "X-Custom-Header": "custom-value",
        "X-Provider": "my-gateway"
      }
    }
  }
}
```

## Pricing headers

One use of this feature is to set pricing information. If you have a dynamic token price, when a user's request is accepted the price is agreed and can be recorded in the HTTP headers:

```json
{
  "targets": {
    "priced-model": {
      "url": "https://api.provider.com",
      "onwards_key": "your-api-key",
      "response_headers": {
        "Input-Price-Per-Token": "0.0001",
        "Output-Price-Per-Token": "0.0002"
      }
    }
  }
}
```

When using [load balancing](load-balancing.md), response headers can be configured at both the pool level and provider level. Provider-level headers take precedence.
