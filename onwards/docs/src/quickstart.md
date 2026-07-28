# Quickstart

## Create a configuration file

Create a `config.json` file with your target configurations:

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "onwards_model": "gpt-4"
    },
    "claude-3": {
      "url": "https://api.anthropic.com",
      "onwards_key": "sk-ant-your-anthropic-key"
    },
    "local-model": {
      "url": "http://localhost:8080"
    }
  }
}
```

## Start the gateway

```bash
cargo run -- -f config.json
```

Modifying the file will automatically and atomically reload the configuration. To disable this, set the `--watch` flag to false.

## Send a request

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```
