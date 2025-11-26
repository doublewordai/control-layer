# Copilot Instructions for Control Layer

## Overview
The Doubleword Control Layer (dwctl) is a high-performance AI model gateway providing unified routing, management, and security for LLM inference. It's a Rust/TypeScript monorepo with:
- **dwctl** (Rust): Core API server at `dwctl/`
- **dashboard** (React/TypeScript): Web frontend at `dashboard/`

## Build Requirements

### Prerequisites
- **Rust 1.88+** (SQLx compatibility requirement)
- **Node.js 20+** with npm
- **PostgreSQL** (running database required for Rust compilation due to SQLx)
- **just** (command runner): `cargo install just`
- **Docker** with Compose plugin

### Critical: Database Required for Rust Builds
SQLx performs compile-time SQL validation. Always ensure PostgreSQL is running before building Rust code:
```bash
# Start test postgres container
just db-start

# Setup databases and .env files
just db-setup
```

**Offline mode alternative**: Set `SQLX_OFFLINE=true` to use cached query metadata from `.sqlx/` directory for builds without database access.

## Build Commands

### Rust Backend
```bash
# Check/compile (requires database OR SQLX_OFFLINE=true)
cargo check --no-default-features    # Skip embedded-db feature to avoid GitHub API calls
cargo build --no-default-features

# For offline builds:
SQLX_OFFLINE=true cargo check --no-default-features
```

### TypeScript Frontend
```bash
cd dashboard
npm ci           # Install dependencies (use ci, not install, for reproducible builds)
npm run build    # Build production bundle
```

## Test Commands

### Rust Tests (require running PostgreSQL)
```bash
just test rust                    # Run all Rust tests
just test rust --coverage         # With coverage (requires cargo-llvm-cov)
cargo test <test_name>            # Run specific test
```

### TypeScript Tests
```bash
just test ts                      # Run frontend tests
cd dashboard && npm test -- --run # Direct vitest execution
```

## Lint Commands

### Rust
```bash
just lint rust                    # Full lint (fmt + clippy + sqlx prepare check)
cargo fmt --check                 # Check formatting only
cargo clippy                      # Clippy only
cargo sqlx prepare --check --workspace  # Verify SQLx offline metadata
```

### TypeScript
```bash
just lint ts                      # Full lint (tsc + eslint)
just lint ts --fix                # Auto-fix ESLint issues
cd dashboard && npx tsc -b --noEmit  # TypeScript check only
```

## CI Pipeline
CI runs `just ci rust` and `just ci ts`. These commands combine linting, testing with coverage, and (for TypeScript) building.

### Reproduce CI locally:
```bash
# Backend CI
just db-start && just db-setup    # Required first
DB_PASS=password just ci rust -- -D warnings

# Frontend CI  
just ci ts
```

## Project Layout

### Root Files
- `justfile` - Build/test/lint command definitions
- `Cargo.toml` - Rust workspace config
- `config.yaml` - Application configuration
- `docker-compose.yml` - Production container setup
- `.sqlx/` - SQLx offline query cache (required for offline builds)

### Rust Backend (`dwctl/`)
- `src/main.rs` - Entry point
- `src/lib.rs` - Core application and router setup
- `src/api/handlers/` - HTTP request handlers
- `src/db/handlers/` - Database repositories
- `src/auth/` - Authentication middleware
- `migrations/` - PostgreSQL migrations (run automatically on startup)
- `Cargo.toml` - Dependencies and features

### Frontend (`dashboard/`)
- `src/main.tsx` - React entry point
- `src/App.tsx` - Root component with routing
- `src/api/` - API client and hooks
- `src/components/` - React components
- `package.json` - npm dependencies
- `vite.config.ts` - Build configuration

### GitHub Workflows (`.github/workflows/`)
- `ci.yaml` - Main CI: frontend tests, backend tests, Docker build, security scan, E2E tests

## Common Issues and Workarounds

### SQLx "database required" errors
Set `SQLX_OFFLINE=true` or ensure PostgreSQL is running with `just db-start && just db-setup`.

### embedded-db feature fails (GitHub API rate limit)
Use `--no-default-features` flag to skip embedded PostgreSQL:
```bash
cargo check --no-default-features
```

### Migrations
Navigate to `dwctl/` before running migrations:
```bash
cd dwctl && sqlx migrate run
```
**Never** run `sqlx migrate run --source ... --database-url ...` from root.

### SQLx prepared queries out of sync
If queries change, regenerate: `cargo sqlx prepare --workspace`

## Configuration
- Environment variables: Prefix with `DWCTL_`, use `__` for nested keys (e.g., `DWCTL_AUTH__NATIVE__ENABLED=false`)
- Database URL: `DATABASE_URL` env var or `config.yaml`
- Test database: Default `postgres://postgres:password@localhost:5432/dwctl`

## Trust These Instructions
The commands above have been validated. Only search for additional information if these instructions are incomplete or produce errors.
