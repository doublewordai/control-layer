# Connect to the API

> Learn how to interact with AI models using the Control Layer's OpenAI-compatible API, including configuration, request handling, and best practices.

The Control Layer provides an OpenAI-compatible API. Use any OpenAI client library by changing the base URL.

## Create an API key

1. Click **API Keys** in the sidebar
2. Click **Create API Key**
3. Enter a name and click **Create**
4. Copy the key immediately—you won't see it again

API keys inherit your user permissions. You can only access models assigned to your groups.

### Key options

When creating a key, you can optionally set:

- **Description**: Notes about what this key is for
- **Rate limit**: Maximum requests per second (1–10,000) and burst size (1–50,000)

Leave rate limits empty for unlimited requests.

## Configure your client

Point your OpenAI client to the Control Layer:

**Python**

```python
from openai import OpenAI

client = OpenAI(
    base_url="https://your-control-layer/ai/v1",
    api_key="your-api-key"
)

response = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "Hello!"}]
)
```

**Node.js**

```javascript
import OpenAI from 'openai';

const client = new OpenAI({
    baseURL: 'https://your-control-layer/ai/v1',
    apiKey: 'your-api-key'
});

const response = await client.chat.completions.create({
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'Hello!' }]
});
```

**curl**

```bash
curl https://your-control-layer/ai/v1/chat/completions \
  -H "Authorization: Bearer your-api-key" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "Hello!"}]}'
```

## Model names

Use model names exactly as shown on the Models page. The Control Layer routes requests to the correct provider automatically.

If your admin configured model aliases, you can use either the original name or the alias.

You can list models with the OpenAI-compatible `GET /ai/v1/models` endpoint. By default it returns every active model your API key can access, using the standard OpenAI response shape.

Doubleword also supports optional query-string filters for discovery:

```bash
curl "https://your-control-layer/ai/v1/models?group=<group-id>&available_for_realtime=true" \
  -H "Authorization: Bearer your-api-key"
```

- `group`: comma-separated group UUIDs. Results are still limited to models your API key can access.
- `available_for_realtime`: `true` returns models without a realtime deny rule; `false` returns models with one.
- `include_reasoning_capabilities`: disabled by default to preserve the standard OpenAI model object. Set it to `true` to add `supported_reasoning_efforts` for models whose support can be determined across every configured provider. Composite models report the intersection supported by all enabled providers.

## Streaming responses

Streaming works the same as with OpenAI directly:

```python
stream = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "Hello!"}],
    stream=True
)

for chunk in stream:
    print(chunk.choices[0].delta.content or "", end="")
```

## Managing keys

From the **API Keys** page:

- **View usage**: The "Last used" column shows when each key was last used
- **Delete keys**: Select keys and click **Delete**, or click the delete icon on a single key
- **Revoke compromised keys**: Delete them immediately—there's no separate revoke action

Create separate keys for different applications so you can revoke one without affecting others.

## Troubleshooting

**401 Unauthorized**: Your API key is invalid or deleted. Check you copied it correctly, or create a new one.

**403 Forbidden**: Your user account doesn't have access to the requested model. Ask your admin to add you to a group that has access.

**404 Model not found**: The model name doesn't match any available model. Check the exact name on the Models page.

**429 Too Many Requests**: You've hit the rate limit configured on your API key. Wait and retry, or ask your admin to increase the limit.

**502/503 errors**: The upstream model provider is having issues. Check the provider's status page.
