# Add Endpoints

> Learn how to connect the Control Layer to AI inference endpoints by configuring model sources.

Endpoints connect the Control Layer to AI providers. Add an endpoint to make models available to your users.

## Add an endpoint

1. Click **Endpoints** in the sidebar
2. Click **Add Endpoint**
3. Select a provider from the dropdown (OpenAI, Anthropic, Google) or enter a custom base URL
4. Enter your API key
5. Click **Discover Models**
6. Select which models to enable
7. Click **Save**

The Control Layer queries the provider's `/v1/models` endpoint and imports available models.

### Model aliases

During setup, you can assign aliases to models. This lets you use a custom name (like `our-gpt4`) instead of the provider's name. Users can call models by either name.

## Supported providers

Any OpenAI-compatible API works:

- **OpenAI** — `https://api.openai.com`
- **Anthropic** — `https://api.anthropic.com`
- **Google** — `https://generativelanguage.googleapis.com`
- **Together, Groq, Fireworks** — enter their API URL
- **Self-hosted** — vLLM, Ollama, or any OpenAI-compatible server

### Custom authentication

Some providers use non-standard authentication. When adding an endpoint, you can configure:

- **Auth header name**: Default is `Authorization`
- **Auth header prefix**: Default is `Bearer `

For example, some internal services might use `X-API-Key` with no prefix.

## Edit an endpoint

1. Click the endpoint in the list
2. Update the name, description, or URL
3. Click **Save**

To change the API key, delete the endpoint and create a new one.

## Re-sync models

When a provider adds new models, re-sync to discover them:

1. Click the endpoint in the list
2. Click **Synchronize**
3. The Control Layer fetches the current model list

New models appear but aren't automatically enabled. Go to **Models** to enable them and assign group access.

## Delete an endpoint

1. Select the endpoint (checkbox)
2. Click **Delete**

Or click the delete icon on a single endpoint.

> **Warning**
>
> Deleting an endpoint removes all its models from the Control Layer. Users will get "model not found" errors for any deleted models.

## API key security

Provider API keys are stored encrypted in the Control Layer database. If credentials are exposed elsewhere, rotate them immediately with your provider, then delete and recreate the endpoint.

## Troubleshooting

**"Connection failed" during discovery**: Check that the URL is correct and reachable. Test the API key directly with the provider using curl.

**No models returned**: The endpoint might not have a `/v1/models` endpoint, or your API key might lack permission to list models. Try adding models manually if you know their names.

**"Alias already exists" error**: Another endpoint already uses that alias. Choose a different alias, or remove it from the other endpoint first.

**Models not appearing after sync**: New models are discovered but disabled by default. Go to **Models** and enable them.

**Authentication errors after setup**: The API key may have been rotated or revoked. Delete the endpoint and recreate it with a fresh key.
