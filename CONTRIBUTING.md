# Contributing to doubleword control layer

## Workflow

If you encounter an issue, or have a feature request, please open an issue on
[github](https://github.com/doubleword/dwctl/issues). If you'd like to
contribute, try to see first if there's an open issue for what you'd like to
work on. If not, please open one to discuss it before starting work!

Some issues will be tagged as "good first issue" for newcomers.

When submitting a pull request, please ensure that all lints & tests pass. To
run linting locally, run

```bash
just lint rust
```

```bash
just lint ts
```

All tests can be run with.

```bash
just test rust
```

```bash
just test ts
```

```bash
just test docker --build
```

## Developing

### 1. Install Prerequisites

```bash
# Install CLI tools (macOS)
brew install just hurl

# Or install manually:
# just: https://github.com/casey/just
# hurl: https://hurl.dev/docs/installation.html
```

You'll need rust installed to develop the backend, and `npm` for the frontend.
We use [sqlx](https://github.com/launchbadge/sqlx) for rust development, so
you'll need a running postgres database to compile the project. Alternatively,
if you're not changing the postgres queries, you can build with
`SQLX_OFFLINE=1` to skip the database check at compile time.

**Important**: Rust version 1.88 or higher is required for SQLx compatibility.
If you encounter SQLx prepare issues, verify your Rust version with `rustc
--version`.

### 2. Initial Setup

**For local development**: Update the `admin_email` in `config.yaml` to your
own email address instead of the default. This email will be used as the admin
account for testing.

Run `just dev` to setup an all-in-one docker compose development environment.

Alternatively, run:

```bash
cargo run
```

in one terminal (to setup the backend with an embedded postgres database - see
the config docs in the README for how to use another database), and

```bash
npm run dev 
```

from the `dashboard/` folder, in another terminal, to start the frontend.

## Project Overview

This system has two components:

```bash
dwctl/
├── dwctl/           # Rust API server (user/group/model management)
├── dashboard/         # React/TypeScript web frontend
```

**Service Documentation:**

- **[dwctl](application/dwctl/README.md)** - API server setup and development
- **[dashboard](application/dashboard/README.md)** - Frontend development

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

View real-time build and performance metrics for [this project](https://charts.somnial.co/doubleword-dwctl)

## FAQ

**How do I view service logs?**

```bash
# View all service logs
docker compose logs -f

# View specific service logs
docker compose logs -f dwctl
docker compose logs -f dwctl-frontend
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

**Strange sqlx build errors, referencing SQL queries, when building `dwctl` image**
→ Navigate to the `application/dwctl` directory and run `cargo sqlx prepare` to
ensure prepared SQL queries are up to date. Ensure you're using Rust 1.88 or higher (`rustc --version`).

If you see something like "error returned from database: password authentication failed for user "postgres""
then you'll need to change your [pg_hba.conf file](https://stackoverflow.com/a/55039419).
N.B. I needed to use sudo vim pg_hba.conf and then run `sudo service postgresql restart` afterwards.

**"Test database missing or inaccessible" from check-db, and db-setup doesn't fix it**
→ Try creating the databases manually. See the `justfile` for details.
If you get "createdb: error: database creation failed: ERROR: permission denied
to create database" then try executing them as postgres. i.e. do `sudo -u
postgres -i` and then run them.
