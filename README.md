# Waycast

Waycast provides a single, high-performance interface for routing, managing,
and securing inference across model providers, users and deployments - both
open-source and proprietary.

- Seamlessly switch between models
- Turn any model (self-hosted or hosted) into a production-ready API with full
auth and user controls
- Centrally govern, monitor, and audit all inference activity

## Getting started

### Docker compose

With docker compose installed, these commands will start the waycast stack.

```bash
wget https://raw.githubusercontent.com/doublewordai/waycast/refs/heads/main/docker-compose.yml
docker compose up -d
```

Navigate to `http://localhost:3001` to get started.

### Docker

Waycast requires a PostgreSQL database to run. If you have one already (for
example, via a cloud provider), run:

```bash
docker run -p 3001:3001 \ 
    -e DATABASE_URL=<your postgres connection string here> \
    -e SECRET_KEY="mysupersecretkey" \
    ghcr.io/doublewordai/waycast:latest
```

Make sure to replace the secret key with a secure random value in production.

Navigate to `http://localhost:3001` to get started.

## Configuration

Waycast can be configured by a `config.yaml` file. To supply one, mount it into
the container at `/app/config.yaml`, like follows:

```bash
docker run -p 3001:3001 \
  -e DATABASE_URL=<your postgres connection string here> \
  -e SECRET_KEY="mysupersecretkey"  \
  -v ./config.yaml:/app/config.yaml \
  ghcr.io/doublewordai/waycast:latest
```

The docker compose file will mount a
`config.yaml` there if you put one alongside `docker-compose.yml`

The complete default config is below.

You can override any of these settings by
either supplying your own config file, in which case your config file will be
merged with this one, or by supplying environment variables prefixed with
`WAYCAST_`.

Nested sections of the configuration can be specified by joining
the keys with a double underscore, for example, to disable native
authentication, set `WAYCAST_AUTH__NATIVE__ENABLED=false`.

```yaml
# Clay configuration
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
      cookie_name: "waycast_session"
      cookie_secure: true
      cookie_same_site: "strict"

  # Proxy header authentication. 
  # Will accept & autocreate users based on email addresses
  # supplied in a configurable header. Lets you use an upstream proxy to 
  # authenticate users.
  proxy_header:
    enabled: false # X-Doubleword-User header auth
    header_name: "x-doubleword-user"
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
        - "http://localhost:3001" # Default - waycast server itself
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
# give information to  users that manage your waycast deployment.
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
  url: "postgres://localhost:5432/waycast"

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
```

## Production checklist

1. Setup a production-grade Postgres database, and point `waycast` to it via the
   `DATABASE_URL` environment variable.
2. Make sure that the secret key is set to a secure random value. For example, run
   `openssl rand -base64 32` to generate a secure random key.
3. Make sure user registration is enabled or disabled, as per your requirements.
4. Make sure the CORS settings are correct for your frontend.
