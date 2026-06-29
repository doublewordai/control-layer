# Getting Started

By the end of this tutorial, you'll have the Control Layer running locally and will have sent your first request to an LLM through it.

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/) and Docker Compose installed
- An API key from a model provider. If you don't have one:
  - **Doubleword**: [app.doubleword.ai/api-keys](https://app.doubleword.ai/api-keys)
  - **OpenAI**: [platform.openai.com/api-keys](https://platform.openai.com/api-keys)
  - **Anthropic**: [console.anthropic.com/settings/keys](https://console.anthropic.com/settings/keys)
  - Or any OpenAI-compatible endpoint (Together, Groq, local vLLM, etc.)

## Step 1: Start the Control Layer

Download the Docker Compose file and start the stack:

```bash
wget https://raw.githubusercontent.com/doublewordai/control-layer/refs/heads/main/docker-compose.yml
docker compose up -d
```

Wait about 30 seconds for the services to initialize[^1].

> **Verify**
>
> Open `http://localhost:3001` in your browser. You should see the login page.

## Step 2: Log in to the dashboard

Sign in with the default admin credentials:

| Field | Value |
|-------|-------|
| Email | `test@doubleword.ai` |
| Password | `hunter2` |

> **Verify**
>
> You see the Control Layer dashboard with Models, Endpoints, Playground, and other items in the sidebar.

## Step 3: Add an endpoint

Click **Endpoints** in the sidebar, then click **Add Endpoint**.

In the dialog:

1. Use the dropdown in the **Base URL** field to select a popular endpoint (OpenAI, Anthropic, Google), or enter a custom URL
2. Paste your API key in the **API Key** field
3. Click **Discover Models**

The Control Layer connects to your provider and fetches available models.

4. Select the models you want to enable, then click **Save**

> **Verify**
>
> Go to **Models** in the sidebar. You should see your provider's models listed.

## Step 4: Grant access to a group

Models must be added to a group before users can access them.

On any model card, click **+ Add groups** in the top right corner. Select **Everyone** (the default group that includes all users), then click **Done**.

> **Verify**
>
> The model card now shows the group badge.

## Step 5: Test in the Playground

On a model card, click **Playground**.

Type a message and press Enter.

> **Verify**
>
> You receive a response from the model.

## Step 6: Send a request via the API

### Create an API key

1. Click **API Keys** in the sidebar
2. Click **Create API Key**
3. Enter a name (e.g., "test-key") and click **Create Key**
4. **Copy the key now** - you won't see it again

### Make a request

Using curl (replace `YOUR_API_KEY` with the key you copied):

```bash
curl http://localhost:3001/ai/v1/chat/completions \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

Or using Python:

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:3001/ai/v1",
    api_key="YOUR_API_KEY"
)

response = client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "Hello!"}]
)

print(response.choices[0].message.content)
```

> **Verify**
>
> You receive a JSON response (curl) or printed output (Python) with the model's reply.

## Done!

You have a working Control Layer instance routing requests to your AI provider.

Next steps:

- [Add more endpoints](how-to/endpoints.md) to access additional providers
- [Set up users and groups](how-to/users-and-groups.md) to manage team access
- [Configure for production](reference/configuration.md)

> **Before deploying to production**
>
> The default configuration is for local development only. Before exposing the Control Layer:
>
> 1. **Change the admin password** - `hunter2` is not secure
> 2. **Set a secret key** - generate with `openssl rand -base64 32` and set via `SECRET_KEY` environment variable
> 3. **Use a production database** - set `DATABASE_URL` to a real PostgreSQL instance
> 4. **Configure CORS** - update `auth.security.cors.allowed_origins` for your domain
>
> See [Configuration Reference](reference/configuration.md) for details.

[^1]: On first run, Docker downloads the required images which may take several minutes. Run `docker compose logs -f` to watch startup progress.
