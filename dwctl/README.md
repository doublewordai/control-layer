# Control Layer API Server (dwctl)

Rust-based API server for user, group, and model management with PostgreSQL database.

## Local Development Setup

### Prerequisites

- Rust (latest stable)
- PostgreSQL running locally
- sqlx-cli: `cargo install sqlx-cli`

### 1. Database Setup

```bash
# Start PostgreSQL (macOS with Homebrew)
brew services start postgresql

# Create database
createdb control_layer

# Or connect to existing PostgreSQL instance
psql -c "CREATE DATABASE control_layer;"
```

### 2. Environment Configuration

Create `.env` file in the `dwctl` directory:

```bash
# dwctl/.env
DATABASE_URL=postgres://your-username@localhost:5432/control_layer
```

Replace `your-username` with your PostgreSQL username.

### 3. Run Database Migrations

```bash
cd dwctl
sqlx migrate run
```

### 4. Generate Query Cache (for builds without database)

```bash
# Generate offline query cache
cargo sqlx prepare

# This creates .sqlx/ directory with cached query metadata
```

## Running the Service

```bash
cd dwctl

# Run with live database connection
cargo run

# Run tests (requires database)
cargo test
```

## Configuration

The service uses `config.yaml` (or `DWCTL_*` environment variables):

- `DWCTL_HOST`: Server host (default: 0.0.0.0)
- `DWCTL_PORT`: Server port (default: 3001)
- `DATABASE_URL`: PostgreSQL connection string

### Database Configuration

The control layer uses PostgreSQL for data storage. By default, all components share a single database using separate schemas:

- **Main schema** (`public`): Core application data (users, groups, models, etc.)
- **Fusillade schema** (`fusillade`): Batch processing data
- **Outlet schema** (`outlet`): Request logging data

#### Basic Configuration

```yaml
database:
  type: external
  url: postgres://user:pass@localhost:5432/dwctl
```

#### With Read Replica

For high-traffic deployments, you can configure a read replica for read-heavy operations:

```yaml
database:
  type: external
  url: postgres://user:pass@primary:5432/dwctl
  replica_url: postgres://user:pass@replica:5432/dwctl
```

#### Component Database Isolation

Components can optionally use dedicated databases instead of schemas. This is useful for:
- Isolating workloads (batch processing won't affect main app)
- Independent scaling and backups
- Using different database instances for different components

```yaml
database:
  type: external
  url: postgres://user:pass@localhost:5432/dwctl

  # Use dedicated database for batch processing
  fusillade:
    mode: dedicated
    url: postgres://user:pass@localhost:5432/fusillade
    replica_url: postgres://user:pass@replica:5432/fusillade
    pool:
      max_connections: 30

  # Keep outlet in main database (default behavior)
  outlet:
    mode: schema
    name: outlet
    pool:
      max_connections: 5
```

#### Connection Pool Settings

Each component can have its own pool configuration:

```yaml
database:
  type: external
  url: postgres://user:pass@localhost:5432/dwctl
  pool:
    max_connections: 10
    min_connections: 0
    acquire_timeout_secs: 30
    idle_timeout_secs: 600
    max_lifetime_secs: 1800
```

## Authentication

Control Layer supports two authentication methods:

### Native Authentication
Username/password authentication with session cookies. Users can register and log in directly through the Control Layer UI or API.

### Proxy Header Authentication

Accepts user identity from HTTP headers set by an upstream authentication proxy (e.g., oauth2-proxy, Vouch, Authentik, Auth0).

#### Header Configuration

You can configure authentication in two ways:

**Single Header Mode (Simple)**
- Send only `x-doubleword-user` header with the user's email
- The email must be unique per user
- Example: `x-doubleword-user: "user@example.com"`

**Dual Header Mode (Federated Identity)**
- Send both `x-doubleword-user` (unique identifier from IdP) and `x-doubleword-email` (email address)
- Allows multiple accounts with the same email from different identity providers
- The combination of (email, external_user_id) must be unique
- Examples:
  - GitHub user: `x-doubleword-user: "github|user123"`, `x-doubleword-email: "user@example.com"`
  - Google user: `x-doubleword-user: "google-oauth2|456"`, `x-doubleword-email: "user@example.com"`

In dual header mode, the same email can belong to different users as long as they have different external user IDs from their identity providers.

#### Configuration

See `config.yaml` for proxy header authentication settings:

```yaml
auth:
  proxy_header:
    enabled: true
    header_name: "x-doubleword-user"           # User identifier or email (single mode)
    email_header_name: "x-doubleword-email"    # User's email (dual mode, optional)
    auto_create_users: true                     # Auto-create users on first login
```

## User Roles and Permissions

Control Layer uses an additive role-based access control system where users can have multiple roles that combine to provide different levels of access.

### Role Types

#### StandardUser (Base Role)
- **Required for all users** - Cannot be removed
- Enables basic authentication and login functionality
- Provides access to user's own profile and data
- Allows model access, API key creation, and playground usage
- Foundation role that all other roles build upon

#### PlatformManager
- **Administrative access** to most platform functionality
- Can create, update, and delete users
- Can manage groups and group memberships
- Can control access to models and manage inference endpoints
- Can configure system settings
- **Cannot** view private request data (requires RequestViewer)

#### RequestViewer
- **Read-only access** to request logs and analytics
- Can view all requests that have transited the gateway
- Useful for auditing, monitoring, and analytics purposes
- Often combined with other roles for full administrative access

### Role Combinations

Roles are additive, meaning users gain the combined permissions of all their assigned roles:

- **StandardUser only**: Basic user with profile access and model usage
- **StandardUser + PlatformManager**: Full administrative access except request viewing
- **StandardUser + RequestViewer**: Basic user who can also view request logs
- **StandardUser + PlatformManager + RequestViewer**: Full system administrator with all permissions

### Role Management

- All users automatically receive and retain the `StandardUser` role
- Additional roles can be assigned/removed via the admin interface
- The system automatically ensures `StandardUser` is preserved during role updates
- Role changes take effect immediately without requiring user re-authentication, unless using native auth with jwts, whereby a user needs to logout and back for API access effects to take place

## Troubleshooting

**Database connection errors**

- Ensure PostgreSQL is running: `brew services start postgresql`
- Check DATABASE_URL in `.env` file
- Verify database exists: `psql -l | grep control_layer`

**Migration errors**

```bash
# Reset database
sqlx database reset # add `-y` to skip confirmation and `-f` if you get a
                    # 'other user are connected' error (usually your IDE is also connected)
```

## Database Schema

Migrations are stored in the `migrations/` directory, and run automatically on startup.

- `001_initial.sql` - Users, groups, models tables
- `002_listen_notify.sql` - PostgreSQL notify triggers
- `003_make_hosted_on_not_null.sql` - Schema updates

## API Endpoints

- See OpenAPI docs at `/admin/docs` when running
