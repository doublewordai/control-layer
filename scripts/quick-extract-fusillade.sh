#!/usr/bin/env bash
# Simple script to extract fusillade into a separate repository
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
EXTRACTION_DIR="$REPO_ROOT/../fusillade-repo"

echo "🚀 Fusillade Simple Extraction Script"
echo "======================================"
echo ""
echo "This will create a new fusillade repository at: $EXTRACTION_DIR"
echo ""

# Check if extraction directory already exists
if [ -d "$EXTRACTION_DIR" ]; then
    echo "❌ Directory $EXTRACTION_DIR already exists"
    read -p "Remove it and continue? (y/N) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        rm -rf "$EXTRACTION_DIR"
    else
        echo "Extraction cancelled."
        exit 0
    fi
fi

echo "Step 1: Creating new repository directory..."
mkdir -p "$EXTRACTION_DIR"
cd "$EXTRACTION_DIR"

echo "Step 2: Initializing git repository..."
git init
git branch -M main

echo "Step 3: Copying fusillade files..."
cp -r "$REPO_ROOT/fusillade/"* .

echo "Step 4: Creating GitHub workflows..."
mkdir -p .github/workflows

# Create CI workflow
cat > .github/workflows/ci.yaml << 'EOFCI'
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
          PGPASSWORD=postgres createdb -h localhost -U postgres fusillade
          echo "DATABASE_URL=postgres://postgres:postgres@localhost:5432/fusillade" > .env
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
EOFCI

# Create Release Please workflow
cat > .github/workflows/release-please.yaml << 'EOFRP'
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
EOFRP

# Create autolabel workflow
cat > .github/workflows/autolabel.yaml << 'EOFAL'
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
EOFAL

# Create dependabot config
cat > .github/dependabot.yml << 'EOFDEP'
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
EOFDEP

# Create justfile
cat > justfile << 'EOFJUST'
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

    echo "Setting up fusillade database..."

    if ! pg_isready -h "$DB_HOST" -p "$DB_PORT" >/dev/null 2>&1; then
        echo "❌ PostgreSQL is not running on $DB_HOST:$DB_PORT"
        exit 1
    fi

    echo "Creating fusillade database..."
    PGPASSWORD="$DB_PASS" psql -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d postgres -c "CREATE DATABASE fusillade;" 2>/dev/null || echo "  - database already exists"

    echo "Writing .env file..."
    echo "DATABASE_URL=postgres://$DB_USER:$DB_PASS@$DB_HOST:$DB_PORT/fusillade" > .env

    echo "Running migrations..."
    sqlx migrate run

    echo "✅ Database setup complete!"

# Run tests
test *args="":
    cargo test {{args}}

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
EOFJUST

# Create .gitignore
cat > .gitignore << 'EOFGIT'
# Rust
/target
Cargo.lock

# SQLx
.env
.sqlx

# IDEs
.vscode/
.idea/
*.swp
*.swo
*~

# OS
.DS_Store
Thumbs.db
EOFGIT

echo "Step 5: Creating initial commit..."
git add .
git commit -m "Initial commit: fusillade v0.3.0

Extracted from doublewordai/control-layer monorepo.

This is a standalone Rust crate for batched LLM request processing
with efficient request coalescing and per-model concurrency control."

echo ""
echo "✅ Extraction complete!"
echo ""
echo "📁 New repository created at: $EXTRACTION_DIR"
echo ""
echo "Next steps:"
echo "  1. cd $EXTRACTION_DIR"
echo "  2. Create GitHub repository:"
echo "     gh repo create doublewordai/fusillade --public --description \"Batched LLM request processing daemon\""
echo "  3. Add remote and push:"
echo "     git remote add origin https://github.com/doublewordai/fusillade.git"
echo "     git push -u origin main"
echo "  4. Configure repository secrets:"
echo "     - CARGO_REGISTRY_TOKEN (for publishing to crates.io)"
echo "     - RELEASE_TOKEN (for release-please)"
echo "  5. Publish to crates.io:"
echo "     cargo publish"
echo ""
