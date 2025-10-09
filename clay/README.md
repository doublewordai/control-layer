# Clay API Server

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
createdb clay

# Or connect to existing PostgreSQL instance
psql -c "CREATE DATABASE clay;"
```

### 2. Environment Configuration

Create `.env` file in the current directory:

```bash
# .env
DATABASE_URL=postgres://your-username@localhost:5432/clay
```

Replace `your-username` with your PostgreSQL username.

### 3. Run Database Migrations

```bash
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
# Run with live database connection
cargo run

# Run tests (requires database)
cargo test
```

## Configuration

The service uses `config.yaml` (or `CLAY_*` environment variables):

- `CLAY_HOST`: Server host (default: 0.0.0.0)
- `CLAY_PORT`: Server port (default: 3001)
- `DATABASE_URL`: PostgreSQL connection string

## User Roles and Permissions

Clay uses an additive role-based access control system where users can have multiple roles that combine to provide different levels of access.

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
- Verify database exists: `psql -l | grep clay`

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
