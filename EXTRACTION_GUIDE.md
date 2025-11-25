# Fusillade Repository Extraction Guide (Simple Version)

This guide details how to extract the `fusillade` component from the control-layer monorepo into its own standalone GitHub repository.

## Overview

**Goal:** Split `fusillade` into its own repository while:
- Maintaining CI/CD workflows in both repositories
- Publishing fusillade to crates.io
- Updating control-layer to use published fusillade crate
- Keeping both repositories functional independently

**Note:** This approach creates a fresh repository without preserving git history for simplicity.

## Step 1: Create New Fusillade Repository

```bash
# Create a new directory for the fusillade repository
mkdir fusillade-repo
cd fusillade-repo

# Initialize git
git init
git branch -M main

# Copy fusillade files from control-layer
cp -r ../control-layer/fusillade/* .

# Initial commit
git add .
git commit -m "Initial commit: fusillade v0.3.0

Extracted from doublewordai/control-layer monorepo.
"
```

## Step 2: Set Up GitHub Repository

```bash
# Create new GitHub repository (via gh CLI or web UI)
gh repo create doublewordai/fusillade --public --description "A daemon implementation for sending batched LLM requests with efficient request coalescing"

# Add remote and push
git remote add origin https://github.com/doublewordai/fusillade.git
git push -u origin main
```

## Step 3: Adapt Workflows for Fusillade Repository

Create `.github/workflows/` directory in the fusillade repository:

### `.github/workflows/ci.yaml`

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
  release:
    types: [published, edited]
  workflow_dispatch:

jobs:
  test:
    runs-on: depot-ubuntu-24.04

    services:
      postgres:
        image: postgres:latest
        env:
          POSTGRES_DB: test
          POSTGRES_USER: postgres
          POSTGRES_PASSWORD: postgres
          POSTGRES_HOST_AUTH_METHOD: trust
        options: >-
          --health-cmd pg_isready
          --health-interval 1s
          --health-timeout 5s
          --health-retries 5
        ports:
          - 5432:5432

    steps:
      - name: Checkout code
        uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          components: llvm-tools-preview

      - uses: Swatinem/rust-cache@v2

      - name: Install sqlx-cli and cargo-llvm-cov
        run: |
          cargo install sqlx-cli --no-default-features --features native-tls,postgres --locked
          cargo install cargo-llvm-cov --locked

      - name: Setup database
        env:
          DATABASE_URL: postgres://postgres:postgres@localhost:5432/fusillade
        run: |
          createdb -h localhost -U postgres fusillade
          sqlx migrate run

      - name: Run tests
        env:
          DATABASE_URL: postgres://postgres:postgres@localhost:5432/fusillade
        run: cargo test

      - name: Run tests with coverage
        env:
          DATABASE_URL: postgres://postgres:postgres@localhost:5432/fusillade
        run: cargo llvm-cov --lcov --output-path lcov.info

      - name: Lint
        run: |
          cargo fmt --check
          cargo clippy -- -D warnings
          cargo sqlx prepare --check

  publish:
    if: github.event_name == 'release' && !github.event.release.prerelease
    runs-on: depot-ubuntu-24.04
    needs: test

    steps:
      - name: Checkout code
        uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2

      - name: Publish to crates.io
        run: cargo publish --token ${{ secrets.CARGO_REGISTRY_TOKEN }}
```

### `.github/workflows/release-please.yaml`

```yaml
name: Release Please

on:
  push:
    branches:
      - main

permissions:
  contents: write
  pull-requests: write
  issues: write

jobs:
  release-please:
    runs-on: ubuntu-latest
    steps:
      - uses: googleapis/release-please-action@v4
        with:
          release-type: rust
          token: ${{ secrets.RELEASE_TOKEN }}
```

### `.github/workflows/autolabel.yaml`

```yaml
name: autolabel issues
on:
  issues:
    types:
      - reopened
      - opened
jobs:
  label_issues:
    runs-on: ubuntu-latest
    permissions:
      issues: write
    steps:
      - run: gh issue edit "$NUMBER" --add-label "$LABELS"
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GH_REPO: ${{ github.repository }}
          NUMBER: ${{ github.event.issue.number }}
          LABELS: fusillade
```

### Additional Files Needed

**`justfile`** (for consistency with control-layer):
```just
# Display available commands
default:
    @just --list

# Setup database
db-setup:
    #!/usr/bin/env bash
    set -euo pipefail

    DB_HOST="${DB_HOST:-localhost}"
    DB_PORT="${DB_PORT:-5432}"
    DB_USER="${DB_USER:-postgres}"
    DB_PASS="${DB_PASS:-password}"

    echo "Creating fusillade database..."
    PGPASSWORD="$DB_PASS" psql -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d postgres -c "CREATE DATABASE fusillade;" 2>/dev/null || echo "Database already exists"

    echo "DATABASE_URL=postgres://$DB_USER:$DB_PASS@$DB_HOST:$DB_PORT/fusillade" > .env

    echo "Running migrations..."
    sqlx migrate run

    echo "✅ Database setup complete!"

# Run tests
test *args="":
    cargo test {{args}}

# Run tests with coverage
test-coverage:
    cargo llvm-cov --lcov --output-path lcov.info

# Lint
lint:
    cargo fmt --check
    cargo clippy -- -D warnings
    cargo sqlx prepare --check

# Format
fmt:
    cargo fmt

# CI pipeline
ci:
    just db-setup
    cargo test
    cargo llvm-cov --lcov --output-path lcov.info
    just lint
```

**`.github/dependabot.yml`**:
```yaml
version: 2
updates:
  - package-ecosystem: "cargo"
    directory: "/"
    schedule:
      interval: "weekly"
  - package-ecosystem: "github-actions"
    directory: "/"
    schedule:
      interval: "weekly"
```

## Step 4: Update Control-Layer Repository

After fusillade is extracted and published to crates.io, update the control-layer repository:

### Update `Cargo.toml`

Remove fusillade from workspace members:

```toml
[workspace]
members = ["dwctl"]
resolver = "2"
```

### Update `dwctl/Cargo.toml`

Change from path dependency to published crate:

```toml
[dependencies]
# Before:
# fusillade = { path = "../fusillade", version = "0.3.0" }

# After:
fusillade = "0.3.0"
```

### Remove Fusillade Directory

```bash
git rm -r fusillade/
git commit -m "chore: extract fusillade into separate repository

Fusillade has been moved to its own repository at:
https://github.com/doublewordai/fusillade

The control-layer now depends on the published crate from crates.io."
```

### Update `release-please-config.json`

```json
{
  "packages": {
    "dwctl": {
      "release-type": "rust"
    }
  },
  "plugins": [
    {
      "type": "cargo-workspace"
    }
  ],
  "include-component-in-tag": false
}
```

### Update `.github/workflows/ci.yaml`

Remove fusillade-specific steps:
1. The backend tests will now test only dwctl
2. The publish job should only publish dwctl
3. Keep all other jobs unchanged

The justfile commands (`just test rust`, `just ci rust`) will automatically work with the reduced workspace.

### Update Documentation

**`README.md`**: Add a note about fusillade being extracted:

```markdown
## Architecture

The Doubleword Control Layer (dwctl) is built with these components:

- **dwctl** (this repository): Core API server for user/group/model management
- **[fusillade](https://github.com/doublewordai/fusillade)**: Batch processing system (separate repository)
- **dashboard**: Web frontend for management

### Related Repositories

- [fusillade](https://github.com/doublewordai/fusillade): Batch processing daemon for LLM requests
```

**`CLAUDE.md`**: Update architecture section to reflect the separation

## Step 5: Post-Extraction Tasks

### For Fusillade Repository:

1. **Set up GitHub repository settings:**
   - Enable branch protection for `main`
   - Configure required CI checks
   - Set up crates.io publishing secrets (`CARGO_REGISTRY_TOKEN`)
   - Set up release-please token (`RELEASE_TOKEN`)

2. **Create initial issues/milestones:**
   - Port any fusillade-specific issues from control-layer
   - Set up project board if needed

3. **Update README.md:**
   - Add installation instructions
   - Add usage examples
   - Link to control-layer as a consumer
   - Add badges (CI status, crates.io version, etc.)

4. **Publish first standalone release:**
   ```bash
   # Create a release PR via release-please
   # Merge it to trigger a release
   # This will publish v0.3.0 (or next version) to crates.io
   ```

### For Control-Layer Repository:

1. **Update all references:**
   - Search for "fusillade" in docs and update links
   - Update architecture diagrams if any

2. **Test the integration:**
   ```bash
   # After fusillade is published to crates.io
   cargo update fusillade
   cargo build
   cargo test
   ```

3. **Create migration PR:**
   - Include all control-layer updates
   - Update workflows
   - Remove fusillade directory
   - Update documentation

## Timeline

Recommended execution order:

1. **Step 1 (15 min):** Copy fusillade to new directory, create GitHub repo, push initial commit
2. **Step 2 (15 min):** Add workflow files to fusillade repo, commit and push
3. **Step 3 (30 min):** Test CI in fusillade repo, publish v0.3.0 to crates.io
4. **Step 4 (15 min):** Update control-layer to use published crate (on a branch)
5. **Step 5 (30 min):** Test control-layer changes, update docs, create PR

**Total time: ~2 hours**

## Rollback Plan

If issues arise:

1. **Before removing fusillade from control-layer:** Simply don't merge the removal PR - control-layer continues to work as-is
2. **After merging removal PR:** Revert the commit that removed fusillade
3. **If fusillade crate has issues:** Use git dependency temporarily:
   ```toml
   fusillade = { git = "https://github.com/doublewordai/fusillade", version = "0.3.0" }
   ```
4. **Worst case:** The original fusillade code is still in control-layer's git history and can be restored

## Verification Checklist

### Fusillade Repository:
- [ ] All fusillade git history preserved
- [ ] CI/CD workflows running successfully
- [ ] Tests passing
- [ ] Published to crates.io
- [ ] README updated with installation/usage
- [ ] Dependabot configured

### Control-Layer Repository:
- [ ] Workspace updated to remove fusillade
- [ ] dwctl/Cargo.toml uses published crate
- [ ] All tests passing
- [ ] CI/CD workflows updated and passing
- [ ] Documentation updated
- [ ] No broken links to fusillade code

### Integration:
- [ ] dwctl builds successfully with published fusillade
- [ ] All tests pass in control-layer
- [ ] Docker builds work
- [ ] E2E tests pass

## Quick Start Script

For convenience, here's a single script that does the extraction:

```bash
# From control-layer root directory
./scripts/quick-extract-fusillade.sh
```

This will:
1. Copy fusillade to `../fusillade-repo`
2. Add all necessary workflow files
3. Create initial commit
4. Print instructions for pushing to GitHub

## References

- [Cargo workspaces](https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html)
- [Publishing to crates.io](https://doc.rust-lang.org/cargo/reference/publishing.html)
- [Release Please](https://github.com/googleapis/release-please)
