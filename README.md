# Control Layer

**Enterprise Access Control for AI.**

Complete with web dashboard, user authentication, and API gateway.

## Getting Started

### 1. Install Prerequisites

```bash
# Install CLI tools (macOS)
brew install just jwt-cli hurl mkcert kind kubectl helm gh

# Or install manually:
# just: https://github.com/casey/just
# jwt-cli: https://github.com/mike-engel/jwt-cli
# hurl: https://hurl.dev/docs/installation.html
# mkcert: https://github.com/FiloSottile/mkcert
```

**Important**: Rust version 1.88 or higher is required for SQLx compatibility. If you encounter SQLx prepare issues, verify your Rust version with `rustc --version`.

### 2. Initial Setup

**For local development**: Update the `admin_email` in `clay_config.yaml` to your own email address instead of the default. This email will be used as the admin account for testing.

### 3. Start the System

```bash
# Development mode (with hot reload)
just dev
```

The system will be available at:

- **Web Dashboard**: <https://localhost:5173> (main interface)
- **API**: <https://localhost:5173/api/v1/> (REST endpoints)
- **LLM API**: <https://localhost:5173/ai/> (OpenAI-compatible API. Accessible with tokens provided by the dashboard).
- **Database**: localhost:5432 (PostgreSQL, dev mode only)

### 4. Programmatically testing the REST API

The following command generates a JWT token that will bypass SSO for API testing:

```bash
# Generate JWT for API testing
TOKEN=$(just jwt your-email@company.com)

# Use in API requests
curl https://localhost/api/v1/users -b "VouchCookie=$TOKEN"
```

Alternatively, the REST API is exposed at port `3001` when docker compose is running in dev mode. Behind the proxy, all authorization is handled by the (trusted) `X-Doubleword-User` header: for example, you can use the following command to get the list of users:

```bash
curl -H "X-Doubleword-User: your-email@company.com" https://localhost:3001/api/v1/users
```

## Development Workflow

In development mode, both frontend and backend services automatically reload on file changes:

- **Frontend** (clay-frontend): Uses Vite dev server with hot module replacement
- **Backend** (clay): Uses `cargo watch` to rebuild and restart on Rust code changes

### Running Tests

```bash
just test           # Run tests against running services
```

## Project Overview

This system consists of several interconnected services:

```bash
control-layer/
├── clay/              # Rust API server (user/group/model management)
├── dashboard/         # React/TypeScript web frontend
```

**Service Documentation:**

- **[clay](application/clay/README.md)** - API server setup and development
- **[dashboard](application/dashboard/README.md)** - Frontend development

**System Architecture:** See [ARCHITECTURE.md](ARCHITECTURE.md) for detailed system diagrams

### Common Tasks

```bash
just setup               # Setup development environment
just dev                 # Start development environment with hot reload
just up                  # Start production stack (docker)
just down                # Stop docker services
just test                # Run tests against running services
just test docker         # Start docker, test, then stop
just jwt <email>         # Generate auth token
```

## CI Metrics

View real-time build and performance metrics for [this project](https://charts.somnial.co/doubleword-control-layer).

## FAQ

**How do I view service logs?**

```bash
# View all service logs
docker compose logs -f

# View specific service logs
docker compose logs -f clay
docker compose logs -f clay-frontend
```

**How do I reset the database?**

```bash
# Stop services and remove volumes (clears database)
just down -v
just up
```

**How do I stop all services?**

```bash
just down
```

## Troubleshooting

**"Command not found" errors**
→ Run `just setup` to check for missing tools and get installation instructions

**Tests failing with 401**
→ Ensure services are running: `just dev` or `just up`

**"Port already in use" errors**
→ Stop conflicting services: `just down` or change ports in docker-compose.yml

**Database connection errors**
→ Reset database: `just down -v && just up`

**SSL certificate errors**
→ Run `just setup` to regenerate certificates and restart services

**HTTPS returns 400 Bad Request**
→ Clear your browser cache and cookies for localhost, then try again. This often occurs when switching between different authentication configurations.

**Strange sqlx build errors, referencing SQL queries, when building `clay` image**
→ Navigate to the `application/clay` directory and run `cargo sqlx prepare` to
ensure prepared SQL queries are up to date. Ensure you're using Rust 1.88 or higher (`rustc --version`).

If you see something like "error returned from database: password authentication failed for user "postgres""
then you'll need to change your [pg_hba.conf file](https://stackoverflow.com/a/55039419).
N.B. I needed to use sudo vim pg_hba.conf and then run `sudo service postgresql restart` afterwards.

**"Test database missing or inaccessible" from check-db, and db-setup doesn't fix it**
→ Try creating the databases manually with `createdb onwards_pilot_test` and `createdb test`.
If you get "createdb: error: database creation failed: ERROR: permission denied to create database" then try executing them as postgres.
i.e. do `sudo -u postgres -i` and then run them.
