# Display available commands
default:
    @just --list

# Helper function to get admin email from config.yaml
# Usage: ADMIN_EMAIL=$(just get-admin-email)
get-admin-email:
    @grep 'admin_email:' config.yaml | sed 's/.*admin_email:[ ]*"\(.*\)"/\1/'

# Check that you've got all the dependencies for development installed
#
# Prerequisites:
# - macOS or Linux
# - Homebrew (recommended for tool installation)
#
# First-time setup:
#   brew install docker hurl postgresql
#   just setup
check:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Setting up development environment..."

    # Check for required tools
    echo "Checking for required tools..."
    missing_tools=()

    # Required tools
    required_tools=("docker" "hurl" "psql" "createdb", "cargo", "npm")
    for tool in "${required_tools[@]}"; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing_tools+=("$tool")
        fi
    done

    # Check docker compose (subcommand, not separate binary)
    if ! docker compose version >/dev/null 2>&1; then
        missing_tools+=("docker compose-plugin")
    fi

    # Report missing tools
    if [ ${#missing_tools[@]} -ne 0 ]; then
        echo "❌ Error: Missing required tools:"
        for tool in "${missing_tools[@]}"; do
            echo "  - $tool"
        done
        exit 1
    fi

    echo "✅ All required tools found!"

    echo "✅ Development setup complete!"


# Setup PostgreSQL databases for Rust development
#
# IMPORTANT: Rust development requires a running PostgreSQL database!
#
# The Control Layer service stores user/group/model data in PostgreSQL:
#
# - SQLx (our database library) performs compile-time SQL validation, so even
#   compiling Rust code requires database connectivity.
# - For testing, we use sqlx's test harness which requires a database to run.
#
# This command:
# - Creates dwctl and fusillade databases
# - Writes DATABASE_URL to .env files for sqlx compile-time verification
# - Runs migrations to set up the schema
#
# Connection settings can be overridden with environment variables:
# - DB_HOST (default: localhost)
# - DB_PORT (default: 5432)
# - DB_USER (default: postgres)
# - DB_PASS (default: password)
#
# Examples:
#   just db-setup                          # Use defaults (localhost:5432, postgres/password)
#   DB_PASS=postgres just db-setup         # Override password (for CI)
#   just db-start && just db-setup         # Start local Docker postgres and setup
db-setup:
    #!/usr/bin/env bash
    set -euo pipefail

    echo "Setting up development databases..."

    # Database connection settings (can be overridden with environment variables)
    DB_HOST="${DB_HOST:-localhost}"
    DB_PORT="${DB_PORT:-5432}"
    DB_USER="${DB_USER:-postgres}"
    DB_PASS="${DB_PASS:-password}"

    # Check if postgres is running
    if ! pg_isready -h "$DB_HOST" -p "$DB_PORT" >/dev/null 2>&1; then
        echo "❌ PostgreSQL is not running on $DB_HOST:$DB_PORT"
        echo "Run 'just db-start' to start Docker postgres"
        exit 1
    fi

    echo "✅ PostgreSQL is running"

    # Create databases
    echo "Creating databases..."
    PGPASSWORD="$DB_PASS" psql -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d postgres -c "CREATE DATABASE dwctl;" 2>/dev/null || echo "  - dwctl database already exists"
    PGPASSWORD="$DB_PASS" psql -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d postgres -c "CREATE DATABASE fusillade;" 2>/dev/null || echo "  - fusillade database already exists"

    # Write .env files for sqlx compile-time verification
    echo "Writing .env files..."
    echo "DATABASE_URL=postgres://$DB_USER:$DB_PASS@$DB_HOST:$DB_PORT/dwctl" > dwctl/.env

    # Run migrations
    echo "Running migrations..."
    echo "Running dwctl migrations..."
    if (cd dwctl && sqlx migrate run); then
        echo "  ✅ dwctl migrations complete"
    else
        echo "  ❌ dwctl migrations failed"
        exit 1
    fi

    echo ""
    echo "✅ Database setup complete!"
    echo ""
    echo "Database URLs configured:"
    echo "  dwctl:     postgres://$DB_USER:$DB_PASS@$DB_HOST:$DB_PORT/dwctl"

# Start the full development stack with hot reload
#
# Uses docker-compose.yml (base) + docker-compose.override.yml (dev overrides):
# - docker-compose.yml: Production-ready service definitions
# - docker-compose.override.yml: Development-specific settings (ports, volumes, hot reload)
#
# Services running in development mode:
# - control-layer: Rust API server (port 3001) - hot reloads via volume mounts
# - control-layer-frontend: React dev server (port 5173) - Vite HMR enabled
# - postgres: Database (port 5432) - exposed for direct access
#
# The --watch flag enables hot reload. File changes trigger container rebuilds.
#
# Access the app at: https://localhost
# Direct API access: http://localhost:3001
# Database: postgres://control_layer:control_layer_password@localhost:5432/control_layer
#
#
# Examples:
#   just dev                    # Standard development stack
dev *args="":
    #!/usr/bin/env bash
    set -euo pipefail

    # Pass all arguments directly to docker compose
    echo "Starting development stack..."
    docker compose up --build --watch {{args}}

# Start production stack: 'just up'
#
# Production-like deployment using docker-compose.yml only
# - Uses pre-built container images (no development overrides)
# - Services run exactly as they would in production
# - Good for testing production configuration locally
# - Uses docker-compose.yml without docker-compose.override.yml
#
#
# Examples:
#   just up
up *args="":
    #!/usr/bin/env bash
    set -euo pipefail

    docker compose -f docker-compose.yml up {{args}}


# Stop services: 'just down'
#
# Stops services based on how they were started:
#
# Stop docker-compose services
# - Stops all containers started by 'just up' or 'just dev'
# - Removes containers but preserves volumes and networks by default
# - Add --volumes to remove data volumes as well
# - Add --remove-orphans to clean up any leftover containers
#
# Examples:
#   just down                    # Stop containers, keep volumes
#   just down --volumes          # Stop containers and remove volumes
down *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    docker compose down {{args}}


# Run tests: 'just test' or 'just test [docker|rust|ts]'
#
# Test targets available:
#
# (no target): Run integration tests against already-running services
# - Assumes services are running (via 'just dev' or 'just up')
# - Runs hurl-based HTTP API tests and Playwright E2E browser tests
# - Generates JWT tokens for authenticated endpoints
# - Fast - no stack startup/teardown time
# - Flags: --api-only (hurl only), --e2e-only (Playwright only)
#
# docker: Full docker stack test with lifecycle management
# - Starts clean docker stack ('just up')
# - Runs integration tests against the stack
# - Automatically tears down stack when done
# - Good for CI or testing against production-like environment
#
# rust: Unit and integration tests for Rust services
# - Requires PostgreSQL database (run 'just db-setup' first)
#
# ts: Frontend unit tests and type checking
# - Runs TypeScript compiler checks and ESLint
# - Executes Vitest unit tests for React components
#
# Examples:
#   just test                    # Test against running services
#   just test docker             # Full docker integration test
#   just test rust               # Backend unit tests
#   just test ts                 # Frontend tests
test target="" *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    # Check if target is actually a flag (starts with --)
    if [ -z "{{target}}" ] || [[ "{{target}}" == --* ]]; then
        # Treat target as an argument if it starts with --
        ALL_ARGS="{{target}} {{args}}"
        if [ "{{target}}" = "" ]; then
            ALL_ARGS="{{args}}"
        fi
        # Just run tests against running services


        # Parse arguments for test type flags and collect remaining args
        RUN_API_TESTS=true
        RUN_E2E_TESTS=true
        TEST_ARGS=""
        CUSTOM_REPORTER=false

        for arg in $ALL_ARGS; do
            case "$arg" in
                --api-only)
                    RUN_E2E_TESTS=false
                    ;;
                --e2e-only)
                    RUN_API_TESTS=false
                    ;;
                *)
                    TEST_ARGS="$TEST_ARGS $arg"
                    ;;
            esac
        done

        echo "Cleaning up any leftover test data from previous runs..."
        ./scripts/drop-test-users.sh > /dev/null 2>&1 || echo "  (no previous test users to clean up)"
        ./scripts/drop-test-groups.sh > /dev/null 2>&1 || echo "  (no previous test groups to clean up)"

        echo "Generating test cookies..."
        # Get admin credentials from config.yaml
        ADMIN_EMAIL=$(just get-admin-email)
        ADMIN_PASSWORD=$(grep 'admin_password:' config.yaml | sed 's/.*admin_password:[ ]*"\(.*\)"/\1/')
        # Check for required passwords
        if [ -z "$ADMIN_PASSWORD" ]; then
            echo "❌ Error: admin_password not set in config.yaml"
            exit 1
        fi

        echo "Using admin email: $ADMIN_EMAIL, and admin password $ADMIN_PASSWORD"


        # Generate admin JWT
        if ADMIN_JWT=$(EMAIL=$ADMIN_EMAIL PASSWORD=$ADMIN_PASSWORD ./scripts/login.sh); then
            echo "admin_jwt=$ADMIN_JWT" > test.env
            echo "✅ Admin JWT generated successfully"
        else
            echo "❌ Failed to generate admin JWT:"
            echo "$ADMIN_JWT"
            exit 1
        fi

        # Delete and recreate test user to ensure clean state
        echo "Ensuring clean test user..."
        USER1_EMAIL="user@example.org"
        USER1_PASSWORD="user_password"
        USER2_EMAIL="user2@example.org"
        USER2_PASSWORD="user2_password"

        docker compose exec -T postgres psql -U control_layer -d control_layer -c "DELETE FROM users WHERE email = '$USER1_EMAIL';" > /dev/null 2>&1 || true
        docker compose exec -T postgres psql -U control_layer -d control_layer -c "DELETE FROM users WHERE email = '$USER2_EMAIL';" > /dev/null 2>&1 || true

        echo "Creating test user 1: $USER1_EMAIL"
        curl -s -X POST http://localhost:3001/authentication/register \
            -H "Content-Type: application/json" \
            -d '{"email":"'"$USER1_EMAIL"'","username":"testuser1","password":"'"$USER1_PASSWORD"'","display_name":"Test User 1"}' \
            > /dev/null 2>&1

        echo "Creating test user 2: $USER2_EMAIL"
        curl -s -X POST http://localhost:3001/authentication/register \
            -H "Content-Type: application/json" \
            -d '{"email":"'"$USER2_EMAIL"'","username":"testuser2","password":"'"$USER2_PASSWORD"'","display_name":"Test User 2"}' \
            > /dev/null 2>&1

        # Generate user JWTs
        echo "Generating user 1 JWT..."
        if USER1_JWT=$(EMAIL=$USER1_EMAIL PASSWORD=$USER1_PASSWORD ./scripts/login.sh); then
            echo "user_jwt=$USER1_JWT" >> test.env
            echo "✅ User 1 JWT generated successfully"
        else
            echo "❌ Failed to generate user 1 JWT - see error above"
            exit 1
        fi

        echo "Generating user 2 JWT..."
        if USER2_JWT=$(EMAIL=$USER2_EMAIL PASSWORD=$USER2_PASSWORD ./scripts/login.sh); then
            echo "user2_jwt=$USER2_JWT" >> test.env
            echo "✅ User 2 JWT generated successfully"
        else
            echo "❌ Failed to generate user 2 JWT - see error above"
            exit 1
        fi    

        echo "Test cookies written to test.env"

        if [ "$RUN_API_TESTS" = true ]; then
            echo "Running: hurl --variables-file test.env --test --jobs 1 tests/"
            hurl --variables-file test.env --test --jobs 1 tests/
        fi

        # if [ "$RUN_E2E_TESTS" = true ]; then
        #     echo ""
        #     echo "Running Playwright E2E tests..."
        #     cd dashboard && ADMIN_EMAIL=$ADMIN_EMAIL ADMIN_PASSWORD=$ADMIN_PASSWORD USER_PASSWORD=user_password npm run test:e2e -- $TEST_ARGS
        #     cd ..
        # fi

        echo ""
        echo "Cleaning up test users and groups..."
        ./scripts/drop-test-users.sh
        ADMIN_PASSWORD=$ADMIN_PASSWORD ./scripts/drop-test-groups.sh
        exit 0
    fi

    case "{{target}}" in
        docker)
            # Check for --build flag and --api-only flag  
            BUILD_LOCAL=false
            API_ONLY_FLAG=""
            for arg in {{args}}; do
                if [ "$arg" = "--build" ]; then
                    BUILD_LOCAL=true
                elif [ "$arg" = "--api-only" ]; then
                    API_ONLY_FLAG="--api-only"
                fi
            done

            # Start timing
            START_TIME=$(date +%s)
            echo "🕐 [$(date '+%H:%M:%S')] Starting docker test (total time: 0s)"

            if [ "$BUILD_LOCAL" = "true" ]; then
                echo "🔨 [$(date '+%H:%M:%S')] Building local images..."
                PULL_POLICY=never docker compose build
                BUILD_TIME=$(date +%s)
                echo "🚀 [$(date '+%H:%M:%S')] Starting docker services with local images... (build took: $((BUILD_TIME - START_TIME))s)"
                PULL_POLICY=never docker compose up -d
                
                echo "⏳ Waiting for control-layer service to be ready..."
                MAX_WAIT=300  # 5 minutes max wait
                WAITED=0
                while [ $WAITED -lt $MAX_WAIT ]; do
                    if curl -s -f http://localhost:3001/health > /dev/null 2>&1; then
                        echo "✅ Control-layer service is ready (took ${WAITED}s)"
                        break
                    fi
                    sleep 2
                    WAITED=$((WAITED + 2))
                    if [ $((WAITED % 10)) -eq 0 ]; then
                        echo "   Still waiting... (${WAITED}s elapsed)"
                    fi
                done
                
                if [ $WAITED -ge $MAX_WAIT ]; then
                    echo "❌ Service failed to become ready after ${MAX_WAIT}s"
                    echo ""
                    echo "📋 Control-layer logs:"
                    docker compose logs control-layer --tail=50
                    exit 1
                fi
            else
                echo "🚀 [$(date '+%H:%M:%S')] Starting docker services..."
                just up -d --wait
                
                echo "⏳ Waiting for control-layer service to be ready..."
                MAX_WAIT=60  # 1 minute max wait for pre-built images
                WAITED=0
                while [ $WAITED -lt $MAX_WAIT ]; do
                    if curl -s -f http://localhost:3001/health > /dev/null 2>&1; then
                        echo "✅ Control-layer service is ready (took ${WAITED}s)"
                        break
                    fi
                    sleep 1
                    WAITED=$((WAITED + 1))
                done
                
                if [ $WAITED -ge $MAX_WAIT ]; then
                    echo "❌ Service failed to become ready after ${MAX_WAIT}s"
                    echo ""
                    echo "📋 Control-layer logs:"
                    docker compose logs control-layer --tail=50
                    exit 1
                fi
            fi

            SERVICES_UP_TIME=$(date +%s)
            echo "🧪 [$(date '+%H:%M:%S')] Running tests... (startup took: $((SERVICES_UP_TIME - START_TIME))s)"
            just test $API_ONLY_FLAG || {
                FAIL_TIME=$(date +%s)
                echo "❌ [$(date '+%H:%M:%S')] Tests failed after $((FAIL_TIME - SERVICES_UP_TIME))s"
                echo ""
                echo "📋 Recent server logs:"
                docker compose logs --tail=20  # Show fewer logs
                echo "🧹 [$(date '+%H:%M:%S')] Cleaning up..."
                # Fast teardown: kill containers immediately instead of graceful shutdown
                # docker compose kill && docker compose rm -f && docker compose down --volumes --remove-orphans 2>/dev/null || true
                exit 1
            }

            TESTS_DONE_TIME=$(date +%s)
            echo "🧹 [$(date '+%H:%M:%S')] Stopping docker services... (tests took: $((TESTS_DONE_TIME - SERVICES_UP_TIME))s)"
            # Fast teardown: kill containers immediately instead of graceful shutdown
            docker compose kill && docker compose rm -f && docker compose down --volumes --remove-orphans 2>/dev/null || true

            END_TIME=$(date +%s)
            echo "✅ [$(date '+%H:%M:%S')] Docker tests completed successfully!"
            echo "📊 Timing breakdown:"
            echo "   • Startup: $((SERVICES_UP_TIME - START_TIME))s"
            echo "   • Tests:   $((TESTS_DONE_TIME - SERVICES_UP_TIME))s"
            echo "   • Cleanup: $((END_TIME - TESTS_DONE_TIME))s"
            echo "   • Total:   $((END_TIME - START_TIME))s"
            ;;
        rust)
            echo "Running Rust tests..."
            if [[ "{{args}}" == *"--watch"* ]]; then
                if ! command -v cargo-watch >/dev/null 2>&1; then
                    echo "❌ Error: cargo-watch not found. Install with:"
                    echo "  cargo install cargo-watch"
                    exit 1
                fi
                # Remove --watch from args and pass remaining to cargo test
                remaining_args=$(echo "{{args}}" | sed 's/--watch//g' | xargs)
                cargo watch -x "test $remaining_args"
            elif [[ "{{args}}" == *"--coverage"* ]]; then
                if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
                    echo "❌ Error: cargo-llvm-cov not found. Install with:"
                    echo "  cargo install cargo-llvm-cov"
                    echo "  # or"
                    echo "  cargo binstall cargo-llvm-cov"
                    exit 1
                fi
                cargo llvm-cov --fail-under-lines 60 --lcov --output-path lcov.info
            else
                cargo test {{args}}
            fi
            ;;
        ts)
            echo "Running TypeScript tests..."
            cd dashboard
            if [ -z "{{args}}" ]; then
                # No args: run once
                npm test -- --run
            elif [[ "{{args}}" == *"--watch"* ]]; then
                # Watch mode: remove --watch and let vitest handle it
                npm test -- $(echo "{{args}}" | sed 's/--watch//g')
            else
                # Has args but no watch: run once
                npm test -- --run {{args}}
            fi
            ;;
        *)
            echo "Usage: just test [docker|rust|ts]"
            exit 1
            ;;
    esac

# Run linting: 'just lint [ts|rust]'
#
# Linting targets:
#
# ts: TypeScript and JavaScript linting
# - Runs TypeScript compiler (tsc) for type checking
# - Runs ESLint for code style and best practices
# - Treats warnings as errors (--max-warnings 0)
# - Pass --fix to automatically fix ESLint issues
#
# rust: Rust code formatting and linting
# - Runs cargo fmt --check to verify formatting
# - Runs cargo clippy for Rust-specific lints and suggestions
# - Checks all Rust projects (dwctl)
# - Pass clippy args like -- -D warnings for stricter checking
#
#
# Examples:
#   just lint ts                 # Check TypeScript code
#   just lint ts --fix           # Fix TypeScript issues automatically
#   just lint rust               # Check Rust code
#   just lint rust -- -D warnings  # Treat Rust warnings as errors
lint target *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{target}}" in
        ts)
            cd dashboard
            echo "Checking package-lock.json sync..."
            npm ci --dry-run
            echo "Running TypeScript checks..."
            npx tsc -b --noEmit
            echo "Running ESLint..."
            npm run lint -- --max-warnings 0 {{args}}
            ;;
        rust)
            echo "Checking Cargo.lock sync..."
            cargo metadata --locked > /dev/null
            echo "Running cargo fmt --check..."
            cargo fmt --check
            echo "Running cargo clippy..."
            cargo clippy {{args}}
            echo "Checking SQLx prepared queries..."
            cargo sqlx prepare --check --workspace
            ;;
        *)
            echo "Usage: just lint [ts|rust]"
            exit 1
            ;;
    esac

# Format code: 'just fmt [ts|rust]'
#
# Code formatting targets:
#
# ts: TypeScript and JavaScript formatting
# - Uses Prettier to format all frontend code
# - Formats .ts, .tsx, .js, .jsx, .json, .css, .md files
# - Applies consistent style across the entire dashboard project
# - Modifies files in place to fix formatting issues
#
# rust: Rust code formatting
# - Uses cargo fmt to format all Rust code
# - Formats all Rust projects (dwctl)
# - Applies standard Rust formatting conventions
# - Modifies files in place to fix formatting issues
#
#
# Examples:
#   just fmt ts                  # Format all frontend code
#   just fmt rust                # Format all Rust code
fmt target *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{target}}" in
        ts)
            cd dashboard && npx prettier --write . {{args}}
            ;;
        rust)
            echo "Running cargo fmt for dwctl..."
            cargo fmt {{args}}
            ;;
        *)
            echo "Usage: just fmt [ts|rust]"
            exit 1
            ;;
    esac

# Generate JWT token for API testing: 'just jwt user@example.com'
#
# Creates a signed JWT token for testing authenticated API endpoints. The token
# is formatted for use with curl as a dwctl_session.
#
# Usage with curl: TOKEN=$(just jwt admin@company.com) curl -b
# "dwctl_session=$TOKEN" https://localhost/api/v1/users
#
# In order to use the token, the user e/ email EMAIL must already exist in the
# database - i.e. either be the default admin user, or later created by them.
#
# Token contains: - User email and basic profile information - Expiration time
# suitable for testing sessions - Signed with the development JWT secret
#
# Note: Only works with test/development environments, not production (i.e.
# depends on JWT_SECRET being set to the value in .env). You can extract the
# Generate authentication cookie by logging in with username and password
#
# Requires USERNAME and PASSWORD environment variables
#
# Examples:
#   USERNAME=admin@example.org PASSWORD=secret just jwt
jwt:
    @./scripts/login.sh

# Generate cookie for the configured admin user
# Requires ADMIN_PASSWORD environment variable
jwt-admin:
    @EMAIL="$(just get-admin-email)" PASSWORD="${ADMIN_PASSWORD}" ./scripts/login.sh

# Run CI pipeline locally: 'just ci [rust|ts]'
#
# Combines linting and testing for local CI validation.
# Runs the same checks as GitHub Actions to catch issues early.
#
# CI targets:
#
# rust: Backend CI pipeline
# - Runs cargo fmt --check, clippy, and sqlx prepare --check
# - Executes all Rust unit and integration tests with coverage
# - Requires PostgreSQL database (run 'just db-setup' first)
#
# ts: Frontend CI pipeline
# - Runs TypeScript compiler checks and ESLint
# - Executes Vitest unit tests with coverage
# - Builds production bundle to verify no build errors
#
#
# Examples:
#   just ci rust                 # Run backend CI checks
#   just ci ts                   # Run frontend CI checks
ci target *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{target}}" in
        rust)
            echo "🦀 Running Rust CI pipeline..."

            # Setup databases using db-setup target
            just db-setup

            echo "📋 Setting up llvm-cov environment for consistent compilation..."
            echo "🧪 Step 1/2: Running tests with coverage..."
            just test rust --coverage {{args}}
            eval "$(cargo llvm-cov show-env --export-prefix)"
            echo "📋 Step 2/2: Linting"
            just lint rust {{args}}
            echo "✅ Rust CI pipeline completed successfully!"
            ;;
        ts)
            echo "📘 Running TypeScript CI pipeline..."
            echo "📋 Step 1/3: Linting..."
            just lint ts {{args}}
            echo "🧪 Step 2/3: Testing with coverage..."
            just test ts --coverage {{args}}
            echo "🏗️  Step 3/3: Building..."
            cd dashboard && npm run build
            echo "✅ TypeScript CI pipeline completed successfully!"
            ;;
        *)
            echo "Usage: just ci [rust|ts]"
            echo ""
            echo "Available CI targets:"
            echo "  rust - Backend linting, testing with coverage"
            echo "  ts   - Frontend linting, testing with coverage, build"
            exit 1
            ;;
    esac

# Security scanning: 'just security-scan [TAG]'
#
# Scans published container images from GitHub Container Registry for vulnerabilities.
# Uses Grype to scan the control-layer image and provides detailed vulnerability reports by severity level.
#
# Arguments:
# TAG: Image tag to scan (defaults to 'latest' if not specified)
#
# Output includes vulnerability counts by severity and detailed JSON reports.
# Reports are saved as *-vulnerabilities.json files.
#
# Examples:
#   just security-scan           # Scan latest published images
#   just security-scan v1.2.3    # Scan specific version tag
#   TAG=sha-abc123 just security-scan  # Scan using environment variable
security-scan target="latest" *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    
    # Check if grype is installed
    if ! command -v grype >/dev/null 2>&1; then
        echo "❌ Error: Grype not found. Install with:"
        echo "  curl -sSfL https://get.anchore.io/grype | sudo sh -s -- -b /usr/local/bin"
        echo "  # or"
        echo "  brew install grype"
        exit 1
    fi
    
    # Use environment variable if set, otherwise use the provided target as tag
    SCAN_TAG="${TAG:-{{target}}}"
    REGISTRY="ghcr.io/doublewordai/control-layer/"
    CONTROL_LAYER_TAG="${REGISTRY}control-layer:$SCAN_TAG"

    echo "🔍 Scanning published container images for vulnerabilities..."
    echo "Tag: $SCAN_TAG"
    echo "Images: $CONTROL_LAYER_TAG"

    # Function to calculate vulnerability counts
    calculate_vulns() {
        local file=$1
        local severity=$2
        if [ -f "$file" ]; then
            jq -r --arg sev "$severity" '[.matches[] | select(.vulnerability.severity == $sev)] | length' "$file" 2>/dev/null || echo "0"
        else
            echo "0"
        fi
    }
    
    # Scan each image
    echo ""
    echo "Scanning control-layer image: $CONTROL_LAYER_TAG"
    grype "$CONTROL_LAYER_TAG" --output json --file control-layer-vulnerabilities.json --quiet || {
        echo "⚠️  Control Layer scan failed, skipping..."
        echo '{"matches": []}' > control-layer-vulnerabilities.json
    }

    # Calculate metrics for each component
    CONTROL_LAYER_CRITICAL=$(calculate_vulns control-layer-vulnerabilities.json "Critical")
    CONTROL_LAYER_HIGH=$(calculate_vulns control-layer-vulnerabilities.json "High")
    CONTROL_LAYER_MEDIUM=$(calculate_vulns control-layer-vulnerabilities.json "Medium")
    CONTROL_LAYER_LOW=$(calculate_vulns control-layer-vulnerabilities.json "Low")
    CONTROL_LAYER_TOTAL=$(jq '.matches | length' control-layer-vulnerabilities.json 2>/dev/null || echo "0")

    # Calculate totals
    TOTAL_CRITICAL=$((CONTROL_LAYER_CRITICAL))
    TOTAL_HIGH=$((CONTROL_LAYER_HIGH))
    TOTAL_MEDIUM=$((CONTROL_LAYER_MEDIUM))
    TOTAL_LOW=$((CONTROL_LAYER_LOW))
    TOTAL_VULNS=$((CONTROL_LAYER_TOTAL))

    # Display results
    echo ""
    echo "🛡️  Security Scan Results"
    echo "========================="
    printf "%-15s %-9s %-6s %-8s %-5s %-7s\n" "Component" "Critical" "High" "Medium" "Low" "Total"
    echo "-------------------------------------------------------"
    printf "%-15s %-9s %-6s %-8s %-5s %-7s\n" "Control Layer" "$CONTROL_LAYER_CRITICAL" "$CONTROL_LAYER_HIGH" "$CONTROL_LAYER_MEDIUM" "$CONTROL_LAYER_LOW" "$CONTROL_LAYER_TOTAL"
    echo "-------------------------------------------------------"
    printf "%-15s %-9s %-6s %-8s %-5s %-7s\n" "Total" "$TOTAL_CRITICAL" "$TOTAL_HIGH" "$TOTAL_MEDIUM" "$TOTAL_LOW" "$TOTAL_VULNS"

    echo ""
    echo "📁 Detailed reports saved:"
    echo "  - control-layer-vulnerabilities.json"
    
    # Warn about critical vulnerabilities
    if [ "$TOTAL_CRITICAL" -gt 0 ]; then
        echo ""
        echo "⚠️  WARNING: Found $TOTAL_CRITICAL critical vulnerabilities!"
        echo "   Review the detailed reports and consider updating vulnerable components."
    elif [ "$TOTAL_HIGH" -gt 0 ]; then
        echo ""
        echo "⚠️  Found $TOTAL_HIGH high severity vulnerabilities."
        echo "   Consider reviewing and updating vulnerable components."
    else
        echo ""
        echo "✅ No critical or high severity vulnerabilities found."
    fi

# Publish packages to crates.io: 'just release'
#
# Publishes both fusillade and dwctl packages to crates.io in an idempotent way.
# If a version is already published, it will be skipped gracefully.
#
# Prerequisites:
# - Authentication: Either run 'cargo login' or set CARGO_REGISTRY_TOKEN environment variable
# - Node.js and npm installed (for building dwctl frontend)
#
# The release process:
# 1. Attempts to publish fusillade (skips if version already exists)
# 2. Builds frontend and bundles it into dwctl/static
# 3. Attempts to publish dwctl (skips if version already exists)
#
# Examples:
#   just release                              # Use stored credentials from 'cargo login'
#   CARGO_REGISTRY_TOKEN=<token> just release # Use token from environment
release:
    #!/usr/bin/env bash
    set -euo pipefail

    echo "📦 Publishing packages to crates.io..."
    echo ""

    # Build cargo publish command with optional token
    PUBLISH_CMD="cargo publish --allow-dirty --color always"
    if [ -n "${CARGO_REGISTRY_TOKEN:-}" ]; then
        echo "Using CARGO_REGISTRY_TOKEN from environment"
        PUBLISH_CMD="$PUBLISH_CMD --token $CARGO_REGISTRY_TOKEN"
    else
        echo "Using stored credentials from 'cargo login'"
    fi
    echo ""

    # Function to publish a package and handle errors gracefully
    publish_package() {
        local package=$1

        echo "Publishing $package..."
        if $PUBLISH_CMD -p "$package" 2>&1 | tee /tmp/cargo-publish-$package.log; then
            echo "✅ Successfully published $package"
            return 0
        else
            # Check if the error is because the version already exists
            if grep -q "already uploaded" /tmp/cargo-publish-$package.log || \
               grep -q "crate version .* is already uploaded" /tmp/cargo-publish-$package.log; then
                echo "ℹ️  $package version already published, skipping"
                return 0
            else
                echo "❌ Failed to publish $package"
                cat /tmp/cargo-publish-$package.log
                return 1
            fi
        fi
    }

    # Build frontend for dwctl
    echo "Building frontend and publishing dwctl..."
    echo "Building frontend..."
    cd dashboard
    npm ci
    npm run build
    cd ..

    echo "Copying frontend to dwctl/static..."
    rm -rf dwctl/static
    cp -r dashboard/dist dwctl/static
    echo "✅ Frontend built and bundled"
    echo ""

    # Publish dwctl
    publish_package "dwctl" || exit 1

    echo ""
    echo "🎉 Release process completed successfully!"

# Start Docker PostgreSQL with fsync disabled for fast testing
#
# This starts a PostgreSQL container optimized for testing:
# - fsync disabled for faster writes (TESTING ONLY - never use in production!)
# - Runs on port 5432
# - Credentials: postgres/password
# - Container name: test-postgres
# - Uses a named volume for persistent storage
#
# Examples:
#   just db-start
db-start:
    #!/usr/bin/env bash
    set -euo pipefail

    # Check if container already exists
    if docker ps -a --format '{{{{.Names}}' | grep -q "^test-postgres$"; then
        if docker ps --format '{{{{.Names}}' | grep -q "^test-postgres$"; then
            echo "✅ test-postgres container is already running"
        else
            echo "Starting existing test-postgres container..."
            docker start test-postgres
        fi
    else
        echo "Creating new test-postgres container with fsync disabled and trust auth..."
        # Create volume if it doesn't exist
        docker volume create test-postgres-data >/dev/null 2>&1 || true
        docker run --name test-postgres \
          -e POSTGRES_PASSWORD=password \
          -e POSTGRES_HOST_AUTH_METHOD=trust \
          -p 5432:5432 \
          -v test-postgres-data:/var/lib/postgresql/ \
          -d postgres:latest \
          postgres -c fsync=off \
          -c full_page_writes=off \
          -c synchronous_commit=off \
          -c wal_level=minimal \
          -c max_wal_senders=0 \
          -c checkpoint_timeout=1h \
          -c max_wal_size=4GB \
          -c shared_buffers=256MB \
          -c work_mem=16MB \
          -c maintenance_work_mem=128MB
    fi

    echo "Waiting for postgres to be ready..."
    sleep 3

    # Verify it's up
    if pg_isready -h localhost -p 5432 >/dev/null 2>&1; then
        echo "✅ PostgreSQL is ready on localhost:5432"
    else
        echo "❌ PostgreSQL not responding"
        exit 1
    fi

# Stop Docker PostgreSQL container
#
# Stops the test-postgres container. Add --remove to also delete the container and volume.
#
# Examples:
#   just db-stop          # Stop container
#   just db-stop --remove # Stop and remove container + volume
db-stop *args="":
    #!/usr/bin/env bash
    set -euo pipefail

    if docker ps --format '{{{{.Names}}' | grep -q "^test-postgres$"; then
        echo "Stopping test-postgres container..."
        docker stop test-postgres

        if [[ "{{args}}" == *"--remove"* ]]; then
            echo "Removing test-postgres container..."
            docker rm test-postgres
            echo "Removing test-postgres-data volume..."
            docker volume rm test-postgres-data 2>/dev/null || echo "  (volume already removed)"
        fi
        echo "✅ Done"
    elif docker ps -a --format '{{{{.Names}}' | grep -q "^test-postgres$"; then
        if [[ "{{args}}" == *"--remove"* ]]; then
            echo "Removing stopped test-postgres container..."
            docker rm test-postgres
            echo "Removing test-postgres-data volume..."
            docker volume rm test-postgres-data 2>/dev/null || echo "  (volume already removed)"
            echo "✅ Done"
        else
            echo "ℹ️  test-postgres container is already stopped"
        fi
    else
        echo "ℹ️  test-postgres container does not exist"
    fi

# Hidden recipes for internal use
_drop-test-users:
    @./scripts/drop-test-users.sh
    +# Profile a test with samply timeline profiler: 'just profile [TEST_FILTER]'


# Profiles test execution with samply and opens a timeline view in Firefox Profiler.
# The profile server runs at http://127.0.0.1:3001 - press Ctrl+C to stop.
#
# Arguments:
# TEST_FILTER: Test name filter (optional, defaults to running all tests)
#
# Examples:
#   just profile auth::middleware::tests::test_jwt_session_authentication
#   just profile test_create_user
#   just profile                     # Profile all tests
profile test_filter="":
    #!/usr/bin/env basH
    set -euo pipefail

    # Check if samply is installed
    if ! command -v samply >/dev/null 2>&1; then
        echo "❌ Error: samply not found. Install with:"
        echo "  cargo install samply"
        exit 1
    fi

    # Rebuild test binary and extract the binary path from cargo output
    echo "Rebuilding test binary..."
    BUILD_OUTPUT=$(cargo test --no-run --lib 2>&1)
    echo "$BUILD_OUTPUT"

    TEST_BINARY=$(echo "$BUILD_OUTPUT" | grep -o 'target/debug/deps/dwctl-[a-f0-9]*' | head -1)

    if [ -z "$TEST_BINARY" ]; then
        echo "❌ Error: Could not find test binary in cargo output"
        exit 1
    fi

    echo "Profiling with samply: $TEST_BINARY"
    echo "Test filter: {{test_filter}}"
    echo ""

    # Run samply with the test filter (DATABASE_URL set explicitly)
    DATABASE_URL="postgres://postgres:password@127.0.0.1:5432/dwctl" samply record "$TEST_BINARY" {{test_filter}}
