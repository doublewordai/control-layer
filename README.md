# The Doubleword Control Layer (dwctl)

[Announcement](https://www.doubleword.ai/resources/doubleword-open-sources-the-worlds-fastest-ai-gateway) | [Benchmarking](https://docs.doubleword.ai/conceptual/21-19-2025-dwctl-benchmark) | [Technical Blog](https://fergusfinn.com/blog/control-layer/) |
[Documentation](https://docs.doubleword.ai/control-layer/)

The Doubleword Control Layer (dwctl) is the world’s fastest AI model gateway (450x less overhead than LiteLLM). It provides a single, high-performance interface for routing, managing, and securing inference across model providers, users and deployments - both open-source and proprietary.

- Seamlessly switch between models
- Turn any model (self-hosted or hosted) into a production-ready API with full auth and user controls
- Centrally govern, monitor, and audit all inference activity

## Getting started

The Doubleword Control Layer requries Docker to be installed. For information on how to get started with Docker see the docs [here](https://docs.docker.com/get-started/).

There are two ways to set up the Control Layer:

1. **Docker Compose** - All-in-one setup with pre-configured Postgres and dwctl. This method automatically provisions a containerized Postgres database with default credentials and connects it to the Control Layer.
2. **Docker Run** - Bring-your-own-database setup. Use this method to connect the Control Layer to an existing Postgres instance of your choice.

### Option 1. Docker Compose

With docker compose installed, the commands below will start the Control Layer.  

```bash
wget https://raw.githubusercontent.com/doublewordai/control-layer/refs/heads/main/docker-compose.yml
docker compose -f docker-compose.yml up -d
```

Navigate to `http://localhost:3001` to get started. When you get to the login page you will be prompting to sign in with a username and password. Please refer to the configuration section below for how to set up an admin user. You can then refer to the documentation [here](https://docs.doubleword.ai/control-layer/usage/models-and-access) to start playing around with Control Layer features.  

### Option 2. Docker Run

The Doubleword Control Layer requires a PostgreSQL database to run. You can read the documentation [here](https://postgresapp.com/) on how to get started with a local version of Postgres. After doing this, or if you have one already (for example, via a cloud provider), run:

```bash
docker run -p 3001:3001 \
    -e DATABASE_URL=<your postgres connection string here> \
    -e DWCTL_SECRET_KEY="mysupersecretkey" \
    ghcr.io/doublewordai/control-layer:latest
```

Your DATABASE_URL should match the following naming convention `postgres://username:password@localhost:5432/database_name`.  Make sure to replace the secret key with a secure random value in production.

Navigate to `http://localhost:3001` to get started. When you get to the login page you will be prompting to sign in with a username and password. Please refer to the configuration section below for how to set up an admin user. You can then refer to the documentation [here](https://docs.doubleword.ai/control-layer/usage/models-and-access) to start playing around with Control Layer features.  

## Configuration

Control Layer can be configured by a `config.yaml` file. To supply one, mount
it into the container at `/app/config.yaml`, like follows:

```bash
docker run -p 3001:3001 \
  -e DATABASE_URL=<your postgres connection string here> \
  -e SECRET_KEY="mysupersecretkey"  \
  -v ./config.yaml:/app/config.yaml \
  ghcr.io/doublewordai/control-layer:latest
```

The docker compose file will mount a
`config.yaml` there if you put one alongside `docker-compose.yml`

The complete default config is below.

You can override any of these settings by
either supplying your own config file, in which case your config file will be
merged with this one, or by supplying environment variables prefixed with
`DWCTL_`.

Nested sections of the configuration can be specified by joining
the keys with a double underscore, for example, to disable native
authentication, set `DWCTL_AUTH__NATIVE__ENABLED=false`.

```yaml
# dwctl configuration
# Secret key for jwt signing.
# TODO: Must be set in production! Required when native auth is enabled.
# secret_key: null  # Not set by default - must be provided via env var or config

# Admin user email - will be created on first startup
admin_email: "test@doubleword.ai"
# TODO: Change this in production!
admin_password: "hunter2"

# Authentication configuration
auth:
  # Native username/password authentication. Stores users in the local #
  # database, and allows them to login with username and password at
  # http://<host>:<port>/login
  native:
    enabled: true # Enable native login system
    # Whether users can sign up themselves. Defaults to false for security.
    # If false, the admin can create new users via the interface or API.
    allow_registration: false
    # Constraints on user passwords created during registration
    password:
      min_length: 8
      max_length: 64
    # Parameters for login session cookies.
    session:
      timeout: "24h"
      cookie_name: "dwctl_session"
      cookie_secure: true
      cookie_same_site: "strict"

  # Proxy header authentication. 
  # Will accept & autocreate users based on email addresses
  # supplied in a configurable header. Lets you use an upstream proxy to 
  # authenticate users.
  proxy_header:
    enabled: false # X-Doubleword-User header auth
    header_name: "x-doubleword-user"
    groups_field_name: "x-doubleword-user-groups" # Header from which to read out group claims
    blacklisted_sso_groups:  # Which SSO groups to ignore from the iDP
       - "t1"
       - "t2"
    provider_field_name: "x-doubleword-sso-provider" # Header from which to read the sso provider (for source column)
    import_idp_groups: false # Whether to import iDP groups or not
     # Whether users should be automatically created if their email is supplied
    # in a header, or whether they must be pre-created by an admin in the UI.
    # If false, users that aren't precreated will receive a 403 Forbidden error.
    auto_create_users: true

  # Security settings
  security:
    # How long session cookies are valid for. After this much time, users will
    # have to log in again. Note: this is related to the
    # auth.native.session.timeout # value. That one configures how long the browser
    # will set the cookie for, this one how long the server will accept it for.
    jwt_expiry: "24h"
    # CORS Settings. In production, make sure your frontend URL is listed here.
    cors:
      allowed_origins:
        - "http://localhost:3001" # Default - Control Layer server itself
      allow_credentials: true
      max_age: 3600 # Cache preflight requests for 1 hour

# Model sources - the default inference endpoints that are shown in the UI.
# These are seeded into the database on first boot, and thereafter should be 
# managed in the UI, rather than here.
model_sources: []

# Example configurations:
# model_sources:
#   # OpenAI API
#   - name: "openai"
#     url: "https://api.openai.com"
#     api_key: "sk-..."  # Required for model sync
#
#   # Internal model server (no auth required)
#   - name: "internal"
#     url: "http://localhost:8080"

# Frontend metadata. This is just for display purposes, but can be useful to
# give information to users that manage your Control Layer deployment.
metadata:
  region: "UK South"
  organization: "ACME Corp"


# Server configuration
# To advertise publically, set to "0.0.0.0", or the specific network interface
# you've exposed.
host: "0.0.0.0"
port: 3001

# Database configuration
database:
  # By default, we connect to an external postgres database
  type: external
  # Override this with your own database url. Can also be configured via the
  # DATABASE_URL environment variable.
  url: "postgres://localhost:5432/control_layer"

  # Alternatively, you can use embedded postgres (requires compiling with the
  # embedded-db feature, which is not present in the default docker image)
  # type: embedded
  # data_dir: null  # Optional: directory for database storage
  # persistent: false  # Set to true to persist data between restarts


# By default, we log all requests and responses to the database. This is
# performed asynchronously, so there's very little performance impact. # If
# you'd like to disable this (if you have sensitive data in your
# request/responses, for example), toggle this flag.
enable_request_logging: true # Enable request/response logging to database

# Batches API configuration
# The batches API provides OpenAI-compatible batch processing endpoints
# Batches can be sent containing requests to any model configured in the
# control layer, and they'll be executed asynchronously over the course of 24
# hours.
batches:
  # Enable batches API endpoints (/files, /batches)
  # These are mounted with the /admin endpoints - so can only be accessed via
  # session or header auth, for now
  # When disabled, these endpoints will not be available (default: false).
  enabled: false

  # Daemon configuration for processing batch requests
  daemon:
    # Controls when the batch processing daemon runs
    # - "leader": Only run on the elected leader instance (default, recommended for multi-instance deployments)
    # - "always": Run on all instances (use for single-instance deployments)
    # - "never": Never run the daemon (useful for testing or when using external processors)
    enabled: leader

    # Performance & Concurrency Settings
    claim_batch_size: 100 # Maximum number of requests to claim in each iteration
    default_model_concurrency: 10 # Default concurrent requests per model
    claim_interval_ms: 1000 # Milliseconds to sleep between claim iterations

    # Retry & Backoff Settings
    max_retries: 5 # Maximum retry attempts before giving up
    backoff_ms: 1000 # Initial backoff duration in milliseconds
    backoff_factor: 2 # Exponential backoff multiplier
    max_backoff_ms: 10000 # Maximum backoff duration in milliseconds

    # Timeout Settings
    timeout_ms: 600000 # Timeout per request attempt (10 minutes)
    claim_timeout_ms: 60000 # Max time in "claimed" state before auto-unclaim (1 minute)
    processing_timeout_ms: 600000 # Max time in "processing" state before auto-unclaim (10 minutes)

    # Observability
    status_log_interval_ms: 2000 # Interval for logging daemon status (set to null to disable)

  # Files configuration for batch file uploads/downloads
  files:
    max_file_size: 2147483648 # 2 GB - maximum size for file uploads
    upload_buffer_size: 100 # Buffer size for file upload streams
    download_buffer_size: 100 # Buffer size for file download streams
```

### Initial Credit Grant

The Control Layer has a credit system which allows you to assign budgets to users and prices to models. You can set the initial grant given to standard users in `config.yaml`:

```yaml
credits:
  initial_credits_for_standard_users: 50
```

## Production Checklist

1. Setup a production-grade Postgres database, and point Control Layer to it via the
   `DATABASE_URL` environment variable.
2. Make sure that the secret key is set to a secure random value. For example, run
   `openssl rand -base64 32` to generate a secure random key.
3. Make sure user registration is enabled or disabled, as per your requirements.
4. Make sure the CORS settings are correct for your frontend.
