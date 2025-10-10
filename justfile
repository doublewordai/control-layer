set dotenv-load

# Display available commands
default:
    @just --list

# Helper function to get admin email from clay_config.yaml
# Usage: ADMIN_EMAIL=$(just get-admin-email)
get-admin-email:
    @grep 'admin_email:' clay_config.yaml | sed 's/.*admin_email:[ ]*"\(.*\)"/\1/'

# Setup development environment for local development
#
# This command prepares your local environment by:
# - Checking for required development tools (docker, hurl, jwt-cli, etc.)
# - Generating self-signed certificates for HTTPS
# - Decrypting environment configuration (if enc.env exists)
# - Checking database setup status
#
# Prerequisites:
# - macOS or Linux
# - Homebrew (recommended for tool installation)
# - Access to doublewordai GCP project (for environment decryption)
#
# First-time setup:
#   brew install docker hurl jwt-cli mkcert kind kubectl helm gh postgresql
#   gcloud auth login  # Required for environment decryption
#   just setup
#
# For Rust development, also run:
#   just db-setup
setup:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Setting up development environment..."

    # Check for required tools
    echo "Checking for required tools..."
    missing_tools=()

    # Required tools
    required_tools=("docker" "hurl" "jwt" "mkcert" "kind" "kubectl" "helm" "gh" "psql" "createdb")
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
        echo ""
        echo "Install with:"
        echo "  brew install docker hurl jwt-cli mkcert kind kubectl helm gh postgresql"
        echo ""
        echo "Note: docker compose-plugin is included with Docker Desktop"
        echo ""
        echo "Individual installation guides:"
        echo "  jwt-cli: https://github.com/mike-engel/jwt-cli"
        echo "  hurl: https://hurl.dev/docs/installation.html"
        echo "  mkcert: https://github.com/FiloSottile/mkcert"
        exit 1
    fi

    echo "✅ All required tools found!"

    echo "✅ Development setup complete!"
    echo ""
    echo "Checking database setup..."
    just check-db || echo "💡 To develop Rust code, run 'just db-setup'"

# Check database setup status for Rust tests
#
# IMPORTANT: Rust development requires a running PostgreSQL database!
#
# The clay service stores user/group/model data in PostgreSQL, &:
#
# - SQLx (our database library) performs compile-time SQL validation, & so even
#   compiling Rust code requires database connectivity.
# - For testing, we uses sqlx's test harness which requires a database to run.
#
# This command verifies:
# - PostgreSQL client tools are installed (psql, createdb)
# - 'postgres' user exists and can create databases
# - 'test' database is accessible for running tests
#
# If checks fail, run 'just db-setup' to fix the configuration
check-db:
    #!/usr/bin/env bash
    set -euo pipefail

    # Check if PostgreSQL tools are available
    if ! command -v psql >/dev/null 2>&1; then
        echo "❌ PostgreSQL tools not found. Install with:"
        echo "  brew install postgresql"
        exit 1
    fi

    # Check if postgres user exists
    postgres_user_exists=false
    if psql -U postgres -d postgres -c '\q' 2>/dev/null; then
        postgres_user_exists=true
    fi

    # Check if test database exists and is accessible
    test_db_exists=false
    if psql -U postgres -d test -c '\q' 2>/dev/null; then
        test_db_exists=true
    fi

    echo "Database setup status:"
    if [ "$postgres_user_exists" = true ]; then
        echo "  ✅ postgres user exists"
    else
        echo "  ❌ postgres user missing"
    fi

    if [ "$test_db_exists" = true ]; then
        echo "  ✅ test database accessible"
        echo ""
        echo "🎉 Database setup is ready for Rust tests!"
    else
        echo "  ❌ test database missing or inaccessible"
        echo ""
        echo "💡 Run 'just db-setup' to fix database configuration"
        exit 1
    fi

# Setup PostgreSQL databases for Rust development
#
# IMPORTANT: Rust development requires a running PostgreSQL database!
#
# The clay service stores user/group/model data in PostgreSQL, &:
#
# - SQLx (our database library) performs compile-time SQL validation, & so even
#   compiling Rust code requires database connectivity.
# - For testing, we uses sqlx's test harness which requires a database to run.
#
# This command verifies:
# - PostgreSQL client tools are installed (psql, createdb)
# - 'postgres' user exists and can create databases
# - 'test' database is accessible for running tests
#
# If checks fail, run 'just db-setup' to fix the configuration
db-setup:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Setting up test databases..."
    if command -v createdb >/dev/null 2>&1; then
        # Ensure postgres user exists with appropriate privileges
        echo "Checking postgres user..."
        if ! psql -d postgres -c "SELECT 1 FROM pg_roles WHERE rolname='postgres';" | grep -q 1; then
            echo "Creating postgres user with createdb privileges..."
            createuser -s postgres 2>/dev/null || createuser --createdb postgres 2>/dev/null || echo "  - postgres user creation failed, may already exist"
        else
            echo "  - postgres user already exists"
            # Ensure postgres user has createdb privileges
            psql -d postgres -c "ALTER USER postgres CREATEDB;" 2>/dev/null || echo "  - postgres user already has necessary privileges"
        fi

        # Create test database if it doesn't exist
        echo "Creating test database..."
        createdb -O postgres test 2>/dev/null || echo "  - test database already exists"
        echo "✅ Test database ready"
        echo ""
        echo "Database URLs configured:"
        echo "  DATABASE_URL=postgres://postgres:postgres@localhost:5432/test"
        echo "  TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/test"
    else
        echo "❌ Error: createdb not found. Install PostgreSQL tools:"
        echo "  brew install postgresql"
        echo ""
        echo "Or manually create test databases:"
        echo "  createdb onwards_pilot_test"
        echo "  createdb test"
        exit 1
    fi

# Start the full development stack with hot reload
#
# Uses docker-compose.yml (base) + docker-compose.override.yml (dev overrides):
# - docker-compose.yml: Production-ready service definitions
# - docker-compose.override.yml: Development-specific settings (ports, volumes, hot reload)
#
# Services running in development mode:
# - clay: Rust API server (port 3001) - hot reloads via volume mounts
# - clay-frontend: React dev server (port 5173) - Vite HMR enabled
# - postgres: Database (port 5432) - exposed for direct access
#
# The --watch flag enables hot reload. File changes trigger container rebuilds.
#
# Access the app at: https://localhost
# Direct API access: http://localhost:3001
# Database: postgres://clay:clay_password@localhost:5432/clay
#
#
# Examples:
#   just dev                    # Standard development stack
dev *args="":
    #!/usr/bin/env bash
    set -euo pipefail

    # Pass all arguments directly to docker compose
    echo "Starting development stack..."
    docker compose -f docker-compose.yml -f docker-compose.override.yml up --build --watch {{args}}

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
    BUILD_LOCAL=false
    docker_args=""
    for arg in {{args}}; do
        if [ "$arg" = "--build" ]; then
            BUILD_LOCAL=true
        else
            docker_args="$docker_args $arg"
        fi
    done

    if [ "$BUILD_LOCAL" = "true" ]; then
        echo "🔨 Building local images with latest tag..."
        TAGS=latest PLATFORMS=linux/amd64 ATTESTATIONS=false docker buildx bake --load
        echo "🚀 Starting docker services with local images..."
        TAG=latest PULL_POLICY=never docker compose -f docker-compose.yml up $docker_args
    else
        docker compose -f docker-compose.yml up $docker_args
    fi


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
                --reporter=*)
                    CUSTOM_REPORTER=true
                    TEST_ARGS="$TEST_ARGS $arg"
                    ;;
                *)
                    TEST_ARGS="$TEST_ARGS $arg"
                    ;;
            esac
        done

        # Add default reporter if none specified
        if [ "$CUSTOM_REPORTER" = false ]; then
            TEST_ARGS="--reporter=list $TEST_ARGS"
        fi

        echo "Cleaning up any leftover test data from previous runs..."
        ./scripts/drop-test-users.sh > /dev/null 2>&1 || echo "  (no previous test users to clean up)"
        ./scripts/drop-test-groups.sh > /dev/null 2>&1 || echo "  (no previous test groups to clean up)"

        echo "Generating test cookies..."
        # Get admin credentials from clay_config.yaml
        ADMIN_EMAIL=$(just get-admin-email)
        ADMIN_PASSWORD=$(grep 'admin_password:' clay_config.yaml | sed 's/.*admin_password:[ ]*"\(.*\)"/\1/')
        echo "Using admin email: $ADMIN_EMAIL"

        # Check for required passwords
        if [ -z "$ADMIN_PASSWORD" ]; then
            echo "❌ Error: admin_password not set in clay_config.yaml"
            exit 1
        fi

        # Generate admin JWT
        ADMIN_JWT=$(USERNAME=$ADMIN_EMAIL PASSWORD=$ADMIN_PASSWORD ./scripts/generate-jwt.sh 2>&1)
        if [ $? -eq 0 ]; then
            echo "admin_jwt=$ADMIN_JWT" > test.env
            echo "✅ Admin JWT generated successfully"
        else
            echo "❌ Failed to generate admin JWT:"
            echo "$ADMIN_JWT"
            exit 1
        fi

        # Delete and recreate test user to ensure clean state
        echo "Ensuring clean test user..."
        docker compose exec -T postgres psql -U clay -d clay -c "DELETE FROM users WHERE email = 'user@example.org';" > /dev/null 2>&1 || true
        curl -s -X POST http://localhost:3001/authentication/register \
            -H "Content-Type: application/json" \
            -d '{"email":"user@example.org","username":"testuser","password":"user_password","display_name":"Test User"}' \
            > /dev/null 2>&1

        # Generate user JWT
        echo "Generating user JWT..."
        if USER_JWT=$(USERNAME=user@example.org PASSWORD=user_password ./scripts/generate-jwt.sh); then
            echo "user_jwt=$USER_JWT" >> test.env
            echo "✅ User JWT generated successfully"
        else
            echo "❌ Failed to generate user JWT - see error above"
            exit 1
        fi

        echo "Test cookies written to test.env"

        if [ "$RUN_API_TESTS" = true ]; then
            echo "Running: hurl --variables-file test.env --test --jobs 1 tests/"
            hurl --variables-file test.env --test --jobs 1 tests/
        fi

        if [ "$RUN_E2E_TESTS" = true ]; then
            echo ""
            echo "Running Playwright E2E tests..."
            cd dashboard && ADMIN_EMAIL=$ADMIN_EMAIL ADMIN_PASSWORD=$ADMIN_PASSWORD USER_PASSWORD=user_password npm run test:e2e -- $TEST_ARGS
            cd ..
        fi

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
                echo "🔨 [$(date '+%H:%M:%S')] Building local images with latest tag..."
                TAGS=latest PLATFORMS=linux/amd64 ATTESTATIONS=false docker buildx bake --load
                BUILD_TIME=$(date +%s)
                echo "🚀 [$(date '+%H:%M:%S')] Starting docker services with local images... (build took: $((BUILD_TIME - START_TIME))s)"
                TAG=latest PULL_POLICY=never just up -d --wait
            else
                echo "🚀 [$(date '+%H:%M:%S')] Starting docker services..."
                just up -d --wait
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
                cd clay && cargo watch -x "test $remaining_args"
            elif [[ "{{args}}" == *"--coverage"* ]]; then
                if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
                    echo "❌ Error: cargo-llvm-cov not found. Install with:"
                    echo "  cargo install cargo-llvm-cov"
                    echo "  # or"
                    echo "  cargo binstall cargo-llvm-cov"
                    exit 1
                fi
                cd clay && cargo llvm-cov --fail-under-lines 60 --lcov --output-path lcov.info
            else
                cd clay && cargo test {{args}}
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
# - Checks all Rust projects (clay)
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
            cd clay
            echo "Checking Cargo.lock sync..."
            cargo metadata --locked > /dev/null
            echo "Running cargo fmt --check..."
            cargo fmt --check
            echo "Running cargo clippy..."
            cargo clippy {{args}}
            echo "Checking SQLx prepared queries..."
            cargo sqlx prepare --check
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
# - Formats all Rust projects (clay)
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
            echo "Running cargo fmt for clay..."
            cd clay && cargo fmt {{args}}
            ;;
        *)
            echo "Usage: just fmt [ts|rust]"
            exit 1
            ;;
    esac

# Generate JWT token for API testing: 'just jwt user@example.com'
#
# Creates a signed JWT token for testing authenticated API endpoints. The token
# is formatted for use with curl as a VouchCookie.
#
# Usage with curl: TOKEN=$(just jwt admin@company.com) curl -b
# "VouchCookie=$TOKEN" https://localhost/api/v1/users
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
    @./scripts/generate-jwt.sh

# Generate cookie for the configured admin user
# Requires ADMIN_PASSWORD environment variable
jwt-admin:
    @USERNAME="$(just get-admin-email)" PASSWORD="${ADMIN_PASSWORD}" ./scripts/generate-jwt.sh

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
            echo "📋 Setting up llvm-cov environment for consistent compilation..."
            cd clay
            echo "🧪 Step 1/1: Running tests with coverage..."
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
# Uses Grype to scan the clay image and provides detailed vulnerability reports by severity level.
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
    CLAY_TAG="${REGISTRY}clay:$SCAN_TAG"

    echo "🔍 Scanning published container images for vulnerabilities..."
    echo "Tag: $SCAN_TAG"
    echo "Images: $CLAY_TAG"

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
    echo "Scanning clay image: $CLAY_TAG"
    grype "$CLAY_TAG" --output json --file clay-vulnerabilities.json --quiet || {
        echo "⚠️  Clay scan failed, skipping..."
        echo '{"matches": []}' > clay-vulnerabilities.json
    }
    
    # Calculate metrics for each component
    CLAY_CRITICAL=$(calculate_vulns clay-vulnerabilities.json "Critical")
    CLAY_HIGH=$(calculate_vulns clay-vulnerabilities.json "High")
    CLAY_MEDIUM=$(calculate_vulns clay-vulnerabilities.json "Medium")
    CLAY_LOW=$(calculate_vulns clay-vulnerabilities.json "Low")
    CLAY_TOTAL=$(jq '.matches | length' clay-vulnerabilities.json 2>/dev/null || echo "0")
    
    # Calculate totals
    TOTAL_CRITICAL=$((CLAY_CRITICAL))
    TOTAL_HIGH=$((CLAY_HIGH))
    TOTAL_MEDIUM=$((CLAY_MEDIUM))
    TOTAL_LOW=$((CLAY_LOW))
    TOTAL_VULNS=$((CLAY_TOTAL))
    
    # Display results
    echo ""
    echo "🛡️  Security Scan Results"
    echo "========================="
    printf "%-10s %-9s %-6s %-8s %-5s %-7s\n" "Component" "Critical" "High" "Medium" "Low" "Total"
    echo "-------------------------------------------------------"
    printf "%-10s %-9s %-6s %-8s %-5s %-7s\n" "Clay" "$CLAY_CRITICAL" "$CLAY_HIGH" "$CLAY_MEDIUM" "$CLAY_LOW" "$CLAY_TOTAL"
    echo "-------------------------------------------------------"
    printf "%-10s %-9s %-6s %-8s %-5s %-7s\n" "Total" "$TOTAL_CRITICAL" "$TOTAL_HIGH" "$TOTAL_MEDIUM" "$TOTAL_LOW" "$TOTAL_VULNS"
    
    echo ""
    echo "📁 Detailed reports saved:"
    echo "  - clay-vulnerabilities.json"
    
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

# Hidden recipes for internal use
_drop-test-users:
    @./scripts/drop-test-users.sh
