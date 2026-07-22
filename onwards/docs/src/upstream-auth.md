# Upstream Authentication

By default, Onwards sends upstream API keys using the standard `Authorization: Bearer <key>` header format. Some AI providers use different authentication header formats. You can customize both the header name and prefix per target.

## Custom header name

Some providers use custom header names for authentication:

```json
{
  "targets": {
    "custom-api": {
      "url": "https://api.custom-provider.com",
      "onwards_key": "your-api-key-123",
      "upstream_auth_header_name": "X-API-Key"
    }
  }
}
```

This sends: `X-API-Key: Bearer your-api-key-123`

## Custom header prefix

Some providers use different prefixes or no prefix at all:

```json
{
  "targets": {
    "api-with-prefix": {
      "url": "https://api.provider1.com",
      "onwards_key": "token-xyz",
      "upstream_auth_header_prefix": "ApiKey "
    },
    "api-without-prefix": {
      "url": "https://api.provider2.com",
      "onwards_key": "plain-key-456",
      "upstream_auth_header_prefix": ""
    }
  }
}
```

This sends:

- To provider1: `Authorization: ApiKey token-xyz`
- To provider2: `Authorization: plain-key-456`

## Combining custom name and prefix

You can customize both the header name and prefix:

```json
{
  "targets": {
    "fully-custom": {
      "url": "https://api.custom.com",
      "onwards_key": "secret-key",
      "upstream_auth_header_name": "X-Custom-Auth",
      "upstream_auth_header_prefix": "Token "
    }
  }
}
```

This sends: `X-Custom-Auth: Token secret-key`

## Default behavior

If these options are not specified, Onwards uses the standard OpenAI-compatible format:

```json
{
  "targets": {
    "standard-api": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-openai-key"
    }
  }
}
```

This sends: `Authorization: Bearer sk-openai-key`
