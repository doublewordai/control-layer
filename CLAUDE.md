# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

The Doubleword Control Layer (dwctl) is a high-performance AI model gateway
that provides unified routing, management, and security for inference across
multiple model providers. It's the world's fastest AI gateway with 450x less
overhead than LiteLLM.

### Architecture

The project consists of three main components in a Rust/TypeScript monorepo:

1. **dwctl** (Rust): Core API server handling user/group/model management, authentication, and request routing
2. **fusillade** (Rust): Batch processing system for HTTP requests with retry logic and per-model concurrency control
3. **dashboard** (React/TypeScript): Web frontend for managing the control layer

### Request Flow Architecture

The application handles two distinct request flows:

**AI Proxy Requests** (`/ai/v1/*`):

- Handled by the `onwards` routing layer with synchronized cache of valid API keys
- Cache updated in real-time via PostgreSQL LISTEN/NOTIFY
- `onwards` validates API key, maps model alias to inference endpoint, forwards request
- Optional `outlet`/`outlet-postgres` middleware logs request/response data
- Credits deducted based on token usage

**Management API Requests** (`/admin/api/v1/*`):

- Authentication middleware validates credentials (session cookies, proxy headers, or API keys)
- Request reaches handler which performs authorization based on user roles
- Handlers interact with database through repository pattern
- Changes trigger PostgreSQL NOTIFY events to update `onwards` routing cache

### Database Layer

Uses SQLx with PostgreSQL following the Repository pattern:

```
Handlers (API) → Repositories (db::handlers) → Models (db::models) → PostgreSQL
```

- Migrations run automatically on startup from `dwctl/migrations/`
- Schema uses PostgreSQL LISTEN/NOTIFY for real-time config updates
- Advisory locks for leader election - leader election is used to choose who
runs the probe service, and (configurably) the batch daemon
- SQLx performs compile-time SQL validation (requires database connection during compilation)

## Development Commands

### Initial Setup

```bash
# Install prerequisites (macOS)
brew install just hurl postgresql

# Check all dependencies installed
just check

# Start Docker postgres (optimized for testing with fsync disabled)
just db-start

# Setup databases and run migrations
# Creates dwctl and fusillade databases, writes .env files
just db-setup
```

**Important**: Rust compilation requires a running PostgreSQL database due to SQLx's compile-time query verification.

### Running Services

```bash
# Start backend (from project root)
cargo run

# Start frontend dev server (from dashboard/)
npm run dev
```

### Testing

```bash
# Backend unit tests (requires database)
just test rust
just test rust --watch      # Watch mode
just test rust --coverage   # With coverage

# Frontend tests
just test ts
just test ts --watch        # Watch mode
```

### Linting and Formatting

```bash
# Lint
just lint rust
just lint ts
just lint ts --fix          # Auto-fix issues

# Format
just fmt rust
just fmt ts

# CI pipeline (lint + test + coverage)
just ci rust
just ci ts
```

### Database Management

```bash
# Start/stop test postgres container
just db-start
just db-stop
just db-stop --remove       # Also remove container

# Database connection settings (override with env vars)
# DB_HOST (default: localhost)
# DB_PORT (default: 5432)
# DB_USER (default: postgres)
# DB_PASS (default: password)

# Reset database migrations
cd dwctl
sqlx database reset -y
```

### Single Test Execution

```bash
# Run specific Rust test
cargo test test_name

# Run specific Hurl test file
hurl --variables-file test.env tests/authenticated/specific-test.hurl

# Watch specific Rust test
cargo watch -x "test test_name"
```

## Configuration

The system uses `config.yaml` with environment variable overrides prefixed with `DWCTL_`. Nested config can be specified by joining keys with double underscore:

```bash
# Example: disable native auth
DWCTL_AUTH__NATIVE__ENABLED=false

# Set secret key
DWCTL_SECRET_KEY="your-secret-key"
```

### Important Config Sections

- `admin_email` / `admin_password`: Initial admin user credentials
- `auth.native.enabled`: Toggle username/password authentication
- `auth.proxy_header.enabled`: Toggle proxy header authentication for SSO
- `database.type`: `external` (default) or `embedded` (requires embedded-db feature)
- `batches.enabled`: Enable OpenAI-compatible batch processing API
- `background_services.batch_daemon.enabled`: When batch daemon runs (`leader`, `always`, `never`)
- `background_services.leader_election.enabled`: Enable leader election for multi-instance deployments
- `background_services.onwards_sync.enabled`: Enable onwards config sync (syncs DB changes to AI proxy)
- `background_services.probe_scheduler.enabled`: Enable health probe scheduler

## Demo Mode

The dashboard supports a demo mode for showcasing features without a live backend. Demo mode uses Mock Service Worker (MSW) to intercept API requests and return mock data.

### Enabling Demo Mode

Demo mode can be enabled in three ways:

1. **URL parameter**: Add `?flags=demo` to the URL (e.g., `https://control-layer.pages.dev?flags=demo`)
2. **Settings page**: Toggle demo mode in the Settings UI (persisted to localStorage)
3. **localStorage**: Set `app-settings` key with `features.demo: true`

### How Demo Mode Works

- **MSW Service Worker**: When demo mode is enabled, the app initializes a service worker that intercepts fetch requests
- **Mock handlers**: Located in `dashboard/src/api/control-layer/mocks/handlers.ts`, these handlers return static JSON data
- **Demo data**: Mock data includes users, groups, models, endpoints, API keys, requests, batches, and transactions
- **State persistence**: Demo state (like user-group associations) is stored in localStorage and survives page refreshes
- **DemoOnlyRoute**: Some features may be wrapped in `DemoOnlyRoute` component to only show in demo mode

### Demo Data Files

Mock data is stored in `dashboard/src/api/control-layer/mocks/`:

- `users.json`, `groups.json`, `endpoints.json`, `models.json`
- `api-keys.json`, `transactions.json`
- `files.json`, `batches.json`, `batch-requests.json`, `file-requests.json`
- `demoState.ts`: Manages stateful operations (adding/removing users from groups, etc.)

### Feature Flags

Demo mode is part of the broader feature flag system. Available flags:

- `demo`: Enable demo mode with mock data
- `use_billing`: Enable billing and cost management features

Flags can be combined: `?flags=demo,use_billing`

## Code Architecture

### dwctl Structure

- `api/handlers/`: HTTP request handlers for each resource (users, groups, deployments, etc.)
- `api/models/`: API request/response models
- `auth/`: Authentication middleware and permission checking
- `db/handlers/`: Repository implementations following Repository pattern
- `db/models/`: Database record structures matching table schemas
- `sync/`: Real-time sync between database and onwards routing layer via LISTEN/NOTIFY
- `request_logging/`: Request/response logging powered by outlet/outlet-postgres

### Key External Dependencies

- `onwards`: High-performance routing layer for AI proxy requests
- `outlet`/`outlet-postgres`: Request/response logging middleware
- `fusillade`: Batch processing system for async job execution

### User Roles

The system uses additive RBAC where users can have multiple roles:

- **StandardUser** (required, cannot be removed): Base role enabling authentication, profile access, model usage, API key creation
- **PlatformManager**: Administrative access to most functionality (users, groups, models, settings) but NOT request logs
- **RequestViewer**: Read-only access to request logs and analytics

Role changes take effect immediately except for native auth JWT-based API access (requires re-login).

## Production Deployment

1. Use production-grade PostgreSQL database via `DATABASE_URL` environment variable
2. Set secure random `DWCTL_SECRET_KEY` (e.g., `openssl rand -base64 32`)
3. Configure CORS for your frontend in `auth.security.cors.allowed_origins`
4. Review user registration settings (`auth.native.allow_registration`)

## CI/CD

- GitHub Actions runs `just ci rust` and `just ci ts`
- Rust CI requires Rust 1.88+ for SQLx compatibility
- SQLx prepare checks ensure offline query metadata is up to date: `cargo sqlx prepare --check --workspace`
- Frontend CI checks TypeScript compilation, linting, tests with coverage, and production build

## Docker Build

Multi-platform builds use Docker Buildx:

```bash
# Build with specific tags
TAGS=latest PLATFORMS=linux/amd64 docker buildx bake --load

# Production multi-platform build
TAGS=v1.0.0 PLATFORMS=linux/amd64,linux/arm64 docker buildx bake --push
```

## Testing Philosophy

- **Backend**: `cargo test` provides comprehensive API integration tests using `axum-test` to spin up the full API server and make ~real HTTP requests
- **Frontend**: `npm test` in `dashboard/` runs Vitest unit tests for React components and utilities. Always scope queries using `within(container)` instead of `screen` to avoid querying multiple test instances in the DOM (see TypeScript/React Testing section for details)
- Hurl and Playwright E2E tests exist but are currently minimal (not the main test suite)
- All Rust tests require a running PostgreSQL database due to SQLx integration
- Test cleanup scripts in `scripts/` (drop-test-users.sh, drop-test-groups.sh) for Hurl tests
- Tests run in parallel where possible for performance

## Code Conventions & Best Practices

### Rust

**Import Style:**

- Use unqualified names for imports, and put identifier imports at the top of the file. DON'T use fully qualified names unless absolutely necessary, to
prevent name clashes.
- Organize imports in groups: std → external crates → internal modules
- Example: `use crate::errors::{Error, Result};` then use `Result` directly, not `errors::Result`

**Error Handling:**

- Return `Result<T, Error>` from handlers (automatically converts to HTTP responses)
- Use specific error variants: `Error::NotFound`, `Error::BadRequest`, `Error::InsufficientPermissions`
- Database errors wrap `DbError` which auto-converts to appropriate HTTP status codes
- Provide descriptive error messages focused on what went wrong, not implementation details

**API Handlers:**

- Add `#[tracing::instrument(skip_all)]` to all handler functions for observability
- Use `#[utoipa::path(...)]` macro for OpenAPI documentation
- Follow RESTful conventions: list (GET /resources), get (GET /resources/:id), create (POST /resources), update (PUT/PATCH /resources/:id), delete (DELETE /resources/:id)

**Database Access:**

- Always use the Repository pattern via `db::handlers`
- Create a transaction, instantiate repository, perform operations, commit
- Keep repository instantiation scoped in blocks to avoid borrow checker issues
- Repositories are thin wrappers around transactions using `&mut Transaction`

**Testing:**

- Test incrementally: write one test, make it pass, write the next test (not all at once)
- Prefer orthogonality and simplicity in test design - test one thing clearly per test
- Use `#[sqlx::test]` attribute to get a fresh database per test (automatic setup/teardown)
- Use `create_test_user()`, `create_test_app()` helpers from `test_utils` module
- Integration tests use `axum_test::TestServer` to make real HTTP requests

**Documentation:**

- Module-level documentation (`//!`) explains purpose and architecture
- Public APIs have doc comments (`///`) with examples where helpful
- Focus documentation on "why" and non-obvious behavior, not just "what"

### TypeScript/React

**Testing:**

- Use Vitest with describe/it blocks for unit tests
- Test files colocated with source: `Component.tsx` → `Component.test.tsx`
- Focus on testing utility functions, context behavior, and complex component logic
- Prefer testing user-facing behavior over implementation details
- To select components, always use aria labels or roles rather than class names or tag names, to both improve accessibility and reduce brittleness
- **Query Scoping Pattern**: When tests run in parallel, multiple component instances may exist in the DOM. Always scope queries to avoid "Found multiple elements" errors:
  - Use `const { container } = render(...)` to get the component's container
  - Use `within(container).getByRole(...)` instead of `screen.getByRole(...)`
  - Import `within` from `@testing-library/react` when using this pattern
  - **Exception - Portal-rendered elements**: For elements that render outside the component container (modals, dialogs, dropdown menus, popovers), use `screen` instead of `within(container)`:
    - Modals and dialogs: `screen.getByRole("dialog")`
    - Dropdown menus: `screen.getByRole("menu")` and `screen.getByRole("menuitem")`
    - Popover content: `screen.getByPlaceholderText(...)` for inputs inside popovers
    - These elements use portals and render at the document root, so they won't be found within the component container

**Component Structure:**

- Use functional components with TypeScript
- Keep components focused and composable
- Extract complex logic to custom hooks or utility functions

**Design Principles:**

- **Consistency over flashiness**: Use existing components from `components/ui/` where possible (based on Radix UI primitives)
- **Avoid bright colors for emphasis**: Rely on clear UX and layout instead of vibrant colors
- **Subtle animations only**: Never animate without user behavior (no autoplay); when animating, keep it subtle (e.g., `transition-colors`, `transition-transform`)
- **Pragmatic DRY**: A little copying is better than slavish dedication to DRY - prefer clarity over abstraction for smaller chunks
- **Color palette**: Use the `doubleword-*` color scheme (neutrals, muted tones) defined in `tailwind.config.js`
- **Spacing & typography**: Consistent use of Tailwind utilities; Space Grotesk font family
- **Loading states**: Use subtle spinners (`animate-spin`) only when needed, prefer skeleton states for content loading
- **Mobile responsiveness**: When working on the frontend, always keep mobile responsiveness in mind

### General

**Configuration:**

- All config lives in `config.yaml` with `DWCTL_` environment variable overrides
- Use double underscores for nested config: `DWCTL_AUTH__NATIVE__ENABLED=false`
- Document new config options in the config file with comments

**Git Commits:**

- Write clear, descriptive commit messages
- Focus on "why" not "what" (the diff shows what changed)
- Reference issue numbers when applicable
- Don't coauthor commits
- Before commiting, run `just lint rust`, `just test rust` (if you've changed
any rust code), `just lint ts`, and `just test ts` (if you've changed any ts
code) to ensure code quality. NEVER PUSH ANY CODE THAT DOESN'T PASS ALL LINTS
AND TESTS.

**Performance:**

- Database queries should use appropriate indexes (check migrations)
- Use PostgreSQL LISTEN/NOTIFY for real-time cache invalidation (see `sync/` module)
- Avoid N+1 queries - batch fetch related data when possible
- To run sqlx migrations, navigate to the appropriate directory (dwctl, or fusillade/) and run `cargo sqlx migrate run`. NEVER try to run sqlx migrate run --source ... --database-url from the root.
- Instead of calling 'tokio::time::sleep' in tests, try to poll until the condition you're waiting for becomes true. Assert both against the state its in at first, then the state it changes to. Then the tests 1. Aren't slow - because they can change state immediately, and 2. test against the whole flow - both before and after the condition becomes true

