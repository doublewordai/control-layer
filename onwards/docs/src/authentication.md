# Authentication

Onwards supports bearer token authentication to control access to your AI targets. You can configure authentication keys both globally and per-target.

## Global authentication keys

Global keys apply to all targets that have authentication enabled:

```json
{
  "auth": {
    "global_keys": ["global-api-key-1", "global-api-key-2"]
  },
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "keys": ["target-specific-key"]
    }
  }
}
```

## Per-target authentication

You can specify authentication keys for individual targets:

```json
{
  "targets": {
    "secure-gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "keys": ["secure-key-1", "secure-key-2"]
    },
    "open-local": {
      "url": "http://localhost:8080"
    }
  }
}
```

In this example:

- `secure-gpt-4` requires a valid bearer token from the `keys` array
- `open-local` has no authentication requirements

If both global and local keys are supplied, either global or local keys will be valid for accessing models with local keys.

## How authentication works

When a target has `keys` configured, requests must include a valid `Authorization: Bearer <token>` header where `<token>` matches one of the configured keys. If global keys are configured, they are automatically added to each target's key set.

**Successful authenticated request:**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer secure-key-1" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "secure-gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

**Failed authentication (invalid key):**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer wrong-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "secure-gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
# Returns: 401 Unauthorized
```

**Failed authentication (missing header):**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "secure-gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
# Returns: 401 Unauthorized
```

**No authentication required:**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "open-local",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
# Success - no authentication required for this target
```
