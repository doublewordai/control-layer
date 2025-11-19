# Contributing to Control Layer

## Workflow

If you encounter an issue, or have a feature request, please open an issue on
[github](https://github.com/doublewordai/control-layer/issues). If you'd like to
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

All tests for code in a certain language can be run with:

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

**Important**: Rust version 1.88 or higher is required for SQLx compatibility.
If you encounter SQLx prepare issues, verify your Rust version with `rustc
--version`.

Run

```bash
just check

```

to make sure you have all prerequisites installed.

### 2. Initial Setup

1. Update the `admin_email` in `config.yaml` to your
own email address instead of the default. This email will be used as the admin
account for testing.
2. Setup a postgres database. There are just targets to help with this:

```bash
# starts a dockerized postgres instance
just db-start

# Creates two databases (dwctl, and fusillade), and writes connection strings
# into dwctl/.env and fusillade/.env. sqlx will read these files when compiling.
# The config.yaml file by default points to the dwctl database.
just db-setup
```

### 3. Start Development Environment

Run:

```bash
cargo run
```

in one terminal, and

```bash
npm run dev 
```

from the `dashboard/` folder, in another terminal, to start the frontend.

## Project Overview

This system has two components:

```bash
control-layer/
├── dwctl/             # Rust API server (user/group/model management)
├── dashboard/         # React/TypeScript web frontend
```

**Service Documentation:**

- **[dwctl](dwctl/README.md)** - API server setup and development
- **[dashboard](dashboard/README.md)** - Frontend development

## CI Metrics

View real-time build and performance metrics for [this project](https://charts.somnial.co/doubleword-control-layer)

## FAQ
