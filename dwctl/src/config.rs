//! Application configuration management.
//!
//! Configuration is loaded from a YAML file with environment variable overrides. The configuration
//! file path defaults to `config.yaml` but can be specified via `-f` flag or `DWCTL_CONFIG`
//! environment variable.
//!
//! ## Loading Priority
//!
//! Configuration sources are merged in the following order (later sources override earlier ones):
//!
//! 1. **YAML config file** - Base configuration (default: `config.yaml`)
//! 2. **Environment variables** - Variables prefixed with `DWCTL_` override YAML values
//! 3. **DATABASE_URL** - Special case: overrides `database.url` if set
//!
//! For nested config values, use double underscores in environment variables. For example,
//! `DWCTL_DATABASE__TYPE=external` sets the `database.type` field.
//!
//! ## Usage
//!
//! ```no_run
//! use clap::Parser;
//! use dwctl::config::{Args, Config};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Parse CLI arguments
//! let args = Args::parse();
//!
//! // Load configuration from file and environment
//! let config = Config::load(&args)?;
//!
//! println!("Server will bind to {}:{}", config.host, config.port);
//! # Ok(())
//! # }
//! ```
//!
//! ## Configuration Structure
//!
//! The configuration file is structured in YAML format. See the repository's `config.yaml` for a
//! complete example with all available options. Key sections include:
//!
//! - **Server**: `host`, `port` - HTTP server binding configuration
//! - **Database**: `database.type`, `database.url` - PostgreSQL connection settings
//! - **Admin User**: `admin_email`, `admin_password` - Initial admin user created on first startup
//! - **Authentication**: `auth.native`, `auth.proxy_header` - Authentication method configuration
//! - **Security**: `secret_key`, `auth.security.cors` - Security and CORS settings
//! - **Credits**: `credits.initial_credits_for_standard_users` - Credit system configuration
//! - **Features**: `enable_metrics`, `enable_request_logging` - Optional feature toggles
//! - **Batches**: `batches.enabled` - Batch API configuration
//! - **Background Services**: `background_services.batch_daemon`, `background_services.leader_election` - Background service configuration
//!
//! ## Environment Variable Examples
//!
//! ```bash
//! # Override server port
//! DWCTL_PORT=8080
//!
//! # Set database connection (preferred method)
//! DATABASE_URL="postgresql://user:pass@localhost/dwctl"
//!
//! # Or use DWCTL_DATABASE__URL
//! DWCTL_DATABASE__URL="postgresql://user:pass@localhost/dwctl"
//!
//! # Override nested values
//! DWCTL_AUTH__NATIVE__ENABLED=false
//! DWCTL_ENABLE_METRICS=true
//! ```

use clap::Parser;
use dashmap::DashMap;
use figment::{
    Figment,
    providers::{Env, Format, Yaml},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use url::Url;

use crate::api::models::users::Role;
use crate::errors::Error;
use crate::sample_files::SampleFilesConfig;

// DB sync channel name
pub static ONWARDS_CONFIG_CHANGED_CHANNEL: &str = "auth_config_changed";

/// Simple CLI args - just for specifying config file
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Path to configuration file
    #[arg(short = 'f', long, env = "DWCTL_CONFIG", default_value = "config.yaml")]
    pub config: String,

    /// Validate configuration and exit without starting the server.
    /// Useful for CI/CD pipelines to catch config errors before deployment.
    #[arg(long)]
    pub validate: bool,
}

/// Main application configuration.
///
/// This is the root configuration structure loaded from YAML and environment variables.
/// All fields have sensible defaults defined in the `Default` implementation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// HTTP server host to bind to (e.g., "0.0.0.0" for all interfaces)
    pub host: String,
    /// HTTP server port to bind to
    pub port: u16,
    /// Base URL where the dashboard is accessible (e.g., "https://app.example.com")
    /// Used for password reset links, payment redirect URLs, and batch notification emails.
    pub dashboard_url: String,
    /// Deprecated: Use `database` field instead. Kept for backward compatibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_url: Option<String>,
    /// Optional: Database replica URL override via environment variable
    /// Use DATABASE_REPLICA_URL or DWCTL_DATABASE_REPLICA_URL to set this
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_replica_url: Option<String>,
    /// Database configuration - either embedded or external PostgreSQL
    pub database: DatabaseConfig,
    /// Threshold in milliseconds for logging slow SQL statements (default: 1000ms)
    pub slow_statement_threshold_ms: u64,
    /// Email address for the initial admin user (created on first startup)
    pub admin_email: String,
    /// Password for the initial admin user (optional, can be set via environment)
    pub admin_password: Option<String>,
    /// Secret key for JWT signing and encryption (required for production)
    pub secret_key: Option<String>,
    /// Model sources for syncing available models
    pub model_sources: Vec<ModelSource>,
    /// Frontend metadata displayed in the UI
    pub metadata: Metadata,
    /// Payment provider configuration (Stripe, PayPal, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment: Option<PaymentConfig>,
    /// Authentication configuration for various auth methods
    pub auth: AuthConfig,
    /// Batch API configuration (endpoints and file handling)
    pub batches: BatchConfig,
    /// Background services configuration (daemons, leader election, etc.)
    pub background_services: BackgroundServicesConfig,
    /// Enable Prometheus metrics endpoint at `/internal/metrics`
    pub enable_metrics: bool,
    /// Enable request/response logging to PostgreSQL (outlet-postgres)
    ///
    /// When enabled, raw request and response bodies are stored in the
    /// `http_requests` and `http_responses` tables for debugging and auditing.
    pub enable_request_logging: bool,
    /// Enable analytics and billing (http_analytics table, credit deduction, Prometheus metrics)
    ///
    /// Can be enabled independently of `enable_request_logging`. When enabled without
    /// request logging, analytics data is still recorded but raw request/response
    /// bodies are not stored.
    ///
    /// When disabled, no analytics, billing, or GenAI metrics are recorded.
    pub enable_analytics: bool,
    /// Analytics batching configuration
    #[serde(default)]
    pub analytics: AnalyticsConfig,
    /// Enable OpenTelemetry OTLP export for distributed tracing
    pub enable_otel_export: bool,
    /// Credit system configuration
    pub credits: CreditsConfig,
    /// Sample file generation configuration for new users
    pub sample_files: SampleFilesConfig,
    /// Resource limits for protecting system capacity
    pub limits: LimitsConfig,
    /// Email configuration for password resets and notifications
    pub email: EmailConfig,
}

/// Individual pool configuration with all SQLx parameters.
///
/// These settings control connection pool behavior for optimal performance.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolSettings {
    /// Maximum number of connections in the pool
    pub max_connections: u32,
    /// Minimum number of idle connections to maintain
    pub min_connections: u32,
    /// Maximum time to wait for a connection (seconds)
    pub acquire_timeout_secs: u64,
    /// Time before idle connections are closed (seconds, 0 = never)
    pub idle_timeout_secs: u64,
    /// Maximum lifetime of a connection (seconds, 0 = never)
    pub max_lifetime_secs: u64,
}

impl Default for PoolSettings {
    /// Production defaults: balanced for reliability and resource usage
    fn default() -> Self {
        Self {
            max_connections: 10,
            min_connections: 0,
            acquire_timeout_secs: 30,
            idle_timeout_secs: 600,  // 10 minutes
            max_lifetime_secs: 1800, // 30 minutes
        }
    }
}

/// How a component (fusillade/outlet) connects to its database.
///
/// Components can either share the main database using a separate PostgreSQL schema,
/// or use a completely dedicated database with its own connection settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ComponentDb {
    /// Share the main database using a separate PostgreSQL schema.
    /// This is the default and recommended for most deployments.
    Schema {
        /// Schema name (e.g., "fusillade", "outlet")
        name: String,
        /// Connection pool settings for this component (primary and replica if not specified)
        #[serde(default)]
        pool: PoolSettings,
        /// Optional separate pool settings for replica connections
        /// If not specified, uses the same settings as `pool`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replica_pool: Option<PoolSettings>,
    },
    /// Use a dedicated database with its own connection.
    /// Useful for isolating workloads or using read replicas.
    Dedicated {
        /// Primary database URL
        url: String,
        /// Optional read replica URL for read-heavy operations
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replica_url: Option<String>,
        /// Connection pool settings for primary (and replica if not specified)
        #[serde(default)]
        pool: PoolSettings,
        /// Optional separate pool settings for replica connections
        /// If not specified, uses the same settings as `pool`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replica_pool: Option<PoolSettings>,
    },
}

impl ComponentDb {
    /// Get the primary pool settings for this component
    pub fn pool_settings(&self) -> &PoolSettings {
        match self {
            ComponentDb::Schema { pool, .. } => pool,
            ComponentDb::Dedicated { pool, .. } => pool,
        }
    }

    /// Get the replica pool settings for this component
    /// Returns the replica_pool if specified, otherwise returns the primary pool settings
    pub fn replica_pool_settings(&self) -> &PoolSettings {
        match self {
            ComponentDb::Schema { pool, replica_pool, .. } => replica_pool.as_ref().unwrap_or(pool),
            ComponentDb::Dedicated { pool, replica_pool, .. } => replica_pool.as_ref().unwrap_or(pool),
        }
    }
}

/// Default fusillade component configuration (schema mode with "fusillade" schema)
pub fn default_fusillade_component() -> ComponentDb {
    ComponentDb::Schema {
        name: "fusillade".into(),
        pool: PoolSettings {
            max_connections: 20,
            min_connections: 2,
            acquire_timeout_secs: 30,
            idle_timeout_secs: 600,
            max_lifetime_secs: 1800,
        },
        replica_pool: None,
    }
}

/// Default outlet component configuration (schema mode with "outlet" schema)
pub fn default_outlet_component() -> ComponentDb {
    ComponentDb::Schema {
        name: "outlet".into(),
        pool: PoolSettings {
            max_connections: 5,
            min_connections: 0,
            acquire_timeout_secs: 30,
            idle_timeout_secs: 600,
            max_lifetime_secs: 1800,
        },
        replica_pool: None,
    }
}

/// Database configuration.
///
/// Supports either an embedded PostgreSQL instance (for development) or an external
/// PostgreSQL database (recommended for production).
///
/// Components (fusillade, outlet) can either share the main database using separate
/// schemas, or use dedicated databases with their own connection settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DatabaseConfig {
    /// Use embedded PostgreSQL database (requires embedded-db feature)
    Embedded {
        /// Directory where database data will be stored (default: .dwctl_data/postgres)
        #[serde(skip_serializing_if = "Option::is_none")]
        data_dir: Option<PathBuf>,
        /// Whether to persist data between restarts (default: false/ephemeral)
        #[serde(default)]
        persistent: bool,
        /// Main database connection pool settings for primary (and replica if not specified)
        #[serde(default)]
        pool: PoolSettings,
        /// Optional separate pool settings for replica connections
        /// If not specified, uses the same settings as `pool`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replica_pool: Option<PoolSettings>,
        /// Fusillade batch processing database configuration
        #[serde(default = "default_fusillade_component")]
        fusillade: ComponentDb,
        /// Outlet request logging database configuration
        #[serde(default = "default_outlet_component")]
        outlet: ComponentDb,
    },
    /// Use external PostgreSQL database
    External {
        /// Connection string for the main database
        url: String,
        /// Optional read replica URL for the main database
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replica_url: Option<String>,
        /// Main database connection pool settings for primary (and replica if not specified)
        #[serde(default)]
        pool: PoolSettings,
        /// Optional separate pool settings for replica connections
        /// If not specified, uses the same settings as `pool`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replica_pool: Option<PoolSettings>,
        /// Fusillade batch processing database configuration
        #[serde(default = "default_fusillade_component")]
        fusillade: ComponentDb,
        /// Outlet request logging database configuration
        #[serde(default = "default_outlet_component")]
        outlet: ComponentDb,
    },
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        // Default to embedded when feature is enabled, otherwise external
        #[cfg(feature = "embedded-db")]
        {
            DatabaseConfig::Embedded {
                data_dir: None,
                persistent: false,
                pool: PoolSettings::default(),
                replica_pool: None,
                fusillade: default_fusillade_component(),
                outlet: default_outlet_component(),
            }
        }
        #[cfg(not(feature = "embedded-db"))]
        {
            DatabaseConfig::External {
                url: "postgres://localhost:5432/control_layer".to_string(),
                replica_url: None,
                pool: PoolSettings::default(),
                replica_pool: None,
                fusillade: default_fusillade_component(),
                outlet: default_outlet_component(),
            }
        }
    }
}

impl DatabaseConfig {
    /// Check if using embedded database
    pub fn is_embedded(&self) -> bool {
        matches!(self, DatabaseConfig::Embedded { .. })
    }

    /// Get external URL if available
    pub fn external_url(&self) -> Option<&str> {
        match self {
            DatabaseConfig::External { url, .. } => Some(url),
            DatabaseConfig::Embedded { .. } => None,
        }
    }

    /// Get external replica URL if available
    pub fn external_replica_url(&self) -> Option<&str> {
        match self {
            DatabaseConfig::External { replica_url, .. } => replica_url.as_deref(),
            DatabaseConfig::Embedded { .. } => None,
        }
    }

    /// Get embedded data directory if configured
    pub fn embedded_data_dir(&self) -> Option<PathBuf> {
        match self {
            DatabaseConfig::Embedded { data_dir, .. } => data_dir.clone(),
            DatabaseConfig::External { .. } => None,
        }
    }

    /// Get embedded persistence flag if configured
    pub fn embedded_persistent(&self) -> bool {
        match self {
            DatabaseConfig::Embedded { persistent, .. } => *persistent,
            DatabaseConfig::External { .. } => false,
        }
    }

    /// Get the main database primary pool settings
    pub fn main_pool_settings(&self) -> &PoolSettings {
        match self {
            DatabaseConfig::Embedded { pool, .. } => pool,
            DatabaseConfig::External { pool, .. } => pool,
        }
    }

    /// Get the main database replica pool settings
    /// Returns the replica_pool if specified, otherwise returns the primary pool settings
    pub fn main_replica_pool_settings(&self) -> &PoolSettings {
        match self {
            DatabaseConfig::Embedded { pool, replica_pool, .. } => replica_pool.as_ref().unwrap_or(pool),
            DatabaseConfig::External { pool, replica_pool, .. } => replica_pool.as_ref().unwrap_or(pool),
        }
    }

    /// Get the fusillade component database configuration
    pub fn fusillade(&self) -> &ComponentDb {
        match self {
            DatabaseConfig::Embedded { fusillade, .. } => fusillade,
            DatabaseConfig::External { fusillade, .. } => fusillade,
        }
    }

    /// Get the outlet component database configuration
    pub fn outlet(&self) -> &ComponentDb {
        match self {
            DatabaseConfig::Embedded { outlet, .. } => outlet,
            DatabaseConfig::External { outlet, .. } => outlet,
        }
    }
}

/// Payment provider configuration.
///
/// Supports different payment providers via an enum. Credentials should be
/// set via environment variables for security.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PaymentConfig {
    /// Stripe payment processing
    /// Set credentials via:
    /// - `DWCTL_PAYMENT__STRIPE__API_KEY` - Stripe secret API key
    /// - `DWCTL_PAYMENT__STRIPE__WEBHOOK_SECRET` - Webhook signing secret
    /// - `DWCTL_PAYMENT__STRIPE__PRICE_ID` - Price ID for the payment product
    Stripe(StripeConfig),
    /// Dummy payment provider for testing
    /// Set configuration via:
    /// - `DWCTL_PAYMENT__DUMMY__AMOUNT` - Amount to add (defaults to $50)
    Dummy(DummyConfig),
}

/// Stripe payment configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StripeConfig {
    /// Stripe API key (secret key starting with sk_)
    pub api_key: String,
    /// Stripe webhook signing secret (starts with whsec_)
    pub webhook_secret: String,
    /// Stripe price ID for the payment (starts with price_)
    pub price_id: String,
    /// Whether to enable invoice creation for checkout sessions (default: false)
    #[serde(default)]
    pub enable_invoice_creation: bool,
}

/// Dummy payment configuration for testing.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DummyConfig {
    /// Amount to add in dollars (required)
    pub amount: rust_decimal::Decimal,
}

/// Frontend metadata displayed in the UI.
///
/// These values are exposed to the frontend and shown in the user interface.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metadata {
    /// Region name displayed in the UI (e.g., "UK South", "US East")
    pub region: Option<String>,
    /// Organization name displayed in the UI
    pub organization: Option<String>,
    /// Documentation URL shown in the UI header
    pub docs_url: String,

    /// JSONL documentation URL displayed in batch modals (e.g., "https://docs.example.com/batches/jsonl-files")
    pub docs_jsonl_url: Option<String>,

    /// Custom HTML title for the dashboard (e.g., "ACME Corp Control Layer")
    pub title: Option<String>,

    /// Base URL for AI API endpoints (files, batches, daemons)
    /// If not set, the frontend uses relative paths (same-origin requests)
    /// Example: "https://api.doubleword.ai"
    pub ai_api_base_url: Option<String>,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            region: None,
            organization: None,
            docs_url: "https://docs.doubleword.ai/control-layer".to_string(),
            docs_jsonl_url: None,
            title: None,
            ai_api_base_url: None,
        }
    }
}

/// External model source configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelSource {
    /// Name identifier for this model source
    pub name: String,
    /// Base URL of the model source API
    pub url: Url,
    /// Optional API key for authenticating with the model source
    pub api_key: Option<String>,
    #[serde(default = "ModelSource::default_sync_interval")]
    #[serde(with = "humantime_serde")]
    pub sync_interval: Duration,
    /// Models to seed during initial database setup from this source
    #[serde(default)]
    pub default_models: Option<Vec<DefaultModel>>,
}

/// External model details.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DefaultModel {
    pub name: String,
    pub add_to_everyone_group: bool,
}

/// Authentication configuration for all supported auth methods.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Native username/password authentication
    pub native: NativeAuthConfig,
    /// Proxy header-based authentication (for SSO integration)
    pub proxy_header: ProxyHeaderAuthConfig,
    /// Security settings (JWT, CORS, etc.)
    pub security: SecurityConfig,
    /// Default roles assigned to newly created non-admin users
    /// Applies to user registration and proxy header auth auto-creation
    /// StandardUser role is always guaranteed to be present even if not specified
    pub default_user_roles: Vec<Role>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            native: NativeAuthConfig::default(),
            proxy_header: ProxyHeaderAuthConfig::default(),
            security: SecurityConfig::default(),
            default_user_roles: vec![Role::StandardUser],
        }
    }
}

/// Native username/password authentication configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeAuthConfig {
    /// Enable native authentication (login/registration)
    pub enabled: bool,
    /// Allow new users to self-register
    pub allow_registration: bool,
    /// Password validation rules
    pub password: PasswordConfig,
    /// Session cookie configuration
    pub session: SessionConfig,
    /// How long password reset tokens are valid
    #[serde(with = "humantime_serde")]
    pub password_reset_token_duration: Duration,
}

/// Proxy header-based authentication configuration.
///
/// This authentication method reads user identity from HTTP headers set by an upstream
/// proxy (e.g., SSO proxy). Enables integration with external authentication systems.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyHeaderAuthConfig {
    /// Enable proxy header authentication
    ///
    /// This configuration is for deploying the control layer
    /// with trusted HTTP headers from an upstream proxy
    /// (for example oauth2-proxy or vouch).
    pub enabled: bool,
    /// The name of the HTTP header containing a unique user identifier.
    /// This serves as a unique identifier for the user.
    /// It's possible to use an email address here, but make sure if
    /// you do so that all distinct users have unique email addresses.
    ///
    /// For example, if you have multiple authentication providers
    /// configured upstream, the accounts with different providers
    /// might have the same email address - a nefarious user could
    /// signup at a different provider and perform an account takeover.
    pub header_name: String,
    /// HTTP header name containing the user's email.
    /// Optional per-request - if not provided, the value from header_name
    /// will be used as the email (for backwards compatibility).
    /// For federated authentication where users can log in via multiple
    /// providers, send both headers to keep users separate.
    pub email_header_name: String,
    /// HTTP header name containing user groups (comma-separated)
    /// Not required, but will be respected if auto_create_users
    /// is enabled, and import_idp_groups is true.
    pub groups_field_name: String,
    /// Import and sync user groups from groups_field_name header.
    pub import_idp_groups: bool,
    /// SSO groups to exclude from import
    pub blacklisted_sso_groups: Vec<String>,
    /// HTTP header name containing SSO provider name.
    /// Stored per-user in the database.
    pub provider_field_name: String,
    /// Automatically create users if they don't exist.
    /// Per-request, look up 'header_name' in the
    /// external_user_id table, and if not found, creates
    /// a new user with email taken from 'email_header_name',
    /// and groups taken from groups_field_name.
    pub auto_create_users: bool,
}

/// Session cookie configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// Session timeout duration
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    /// Cookie name for session token
    pub cookie_name: String,
    /// Set Secure flag on cookies (HTTPS only)
    pub cookie_secure: bool,
    /// SameSite cookie attribute ("strict", "lax", or "none")
    pub cookie_same_site: String,
}

/// Password validation rules.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PasswordConfig {
    /// Minimum password length
    pub min_length: usize,
    /// Maximum password length
    pub max_length: usize,
    /// Argon2 memory cost in KiB (default: 19456 KiB = 19 MB, secure for production)
    pub argon2_memory_kib: u32,
    /// Argon2 iterations (default: 2, secure for production)
    pub argon2_iterations: u32,
    /// Argon2 parallelism (default: 1)
    pub argon2_parallelism: u32,
}

/// Security configuration for JWT and CORS.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    /// JWT token expiry duration
    #[serde(with = "humantime_serde")]
    pub jwt_expiry: Duration,
    /// CORS configuration for browser clients
    pub cors: CorsConfig,
}

/// CORS (Cross-Origin Resource Sharing) configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CorsConfig {
    /// Allowed origins for CORS requests
    pub allowed_origins: Vec<CorsOrigin>,
    /// Allow credentials (cookies) in CORS requests
    pub allow_credentials: bool,
    /// Cache preflight requests for this many seconds
    pub max_age: Option<u64>,
    /// Custom headers to expose to the browser (in addition to CORS-safelisted headers)
    pub exposed_headers: Vec<String>,
}

/// Email configuration for password resets and notifications.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
// Note: Cannot use deny_unknown_fields here due to #[serde(flatten)] on transport
pub struct EmailConfig {
    /// Email transport method
    #[serde(flatten)]
    pub transport: EmailTransportConfig,
    /// Sender email address
    pub from_email: String,
    /// Sender display name
    pub from_name: String,
    /// Who to set the reply to field from
    pub reply_to: Option<String>,
}

/// Email transport configuration - either SMTP or file-based for testing.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EmailTransportConfig {
    /// Send emails via SMTP server
    Smtp {
        /// SMTP server hostname
        host: String,
        /// SMTP server port
        port: u16,
        /// SMTP authentication username
        username: String,
        /// SMTP authentication password
        password: String,
        /// Use TLS encryption
        use_tls: bool,
    },
    /// Write emails to files (for development/testing)
    File {
        /// Directory path where email files will be written
        path: String,
    },
}

/// File upload/download configuration for batch processing.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesConfig {
    /// Default expiration time in seconds (default: 24 hours)
    pub default_expiry_seconds: i64,
    /// Minimum expiration time in seconds (default: 1 hour)
    pub min_expiry_seconds: i64,
    /// Maximum expiration time in seconds (default: 30 days)
    pub max_expiry_seconds: i64,
    /// Buffer size for file upload streams (default: 100)
    pub upload_buffer_size: usize,
    /// Buffer size for file download streams (default: 100)
    pub download_buffer_size: usize,
    /// Number of templates to insert in each batch during file upload (default: 5000)
    pub batch_insert_size: usize,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            default_expiry_seconds: 24 * 60 * 60,  // 24 hours
            min_expiry_seconds: 60 * 60,           // 1 hour
            max_expiry_seconds: 30 * 24 * 60 * 60, // 30 days
            upload_buffer_size: 100,
            download_buffer_size: 100,
            batch_insert_size: 5000,
        }
    }
}

/// Resource limits for protecting system capacity.
///
/// These limits help prevent resource exhaustion under high load by rejecting
/// requests that would exceed capacity rather than degrading performance for all users.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// File limits (size, request count, and upload concurrency)
    pub files: FileLimitsConfig,
    /// Request limits (per-request body size within batch files)
    pub requests: RequestLimitsConfig,
}

/// Request limits configuration.
///
/// Controls per-request body size limits within batch JSONL files
/// to prevent individual requests from overwhelming inference providers.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestLimitsConfig {
    /// Maximum body size in bytes for individual requests within batch JSONL files.
    /// Set to 0 for unlimited (not recommended for production).
    /// Default: 10MB
    pub max_body_size: u64,
}

impl Default for RequestLimitsConfig {
    fn default() -> Self {
        Self {
            max_body_size: 10 * 1024 * 1024, // 10MB
        }
    }
}

/// File limits configuration.
///
/// Controls file size limits, request count limits, and upload concurrency
/// to protect database connection pools and system resources.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileLimitsConfig {
    /// Maximum file size in bytes.
    /// Set to 0 for unlimited (not recommended for production).
    /// Default: 100MB
    pub max_file_size: u64,
    /// Maximum number of requests (JSONL lines) allowed per file.
    /// Set to 0 for unlimited (not recommended for production).
    /// Default: 0 (unlimited)
    pub max_requests_per_file: usize,
    /// Maximum number of concurrent file uploads allowed system-wide.
    /// Set to 0 for unlimited (not recommended for production).
    /// Default: 0 (unlimited)
    pub max_concurrent_uploads: usize,
    /// Maximum number of uploads that can wait in queue for a slot.
    /// When this limit is reached, new uploads receive HTTP 429 immediately.
    /// Set to 0 for unlimited waiting queue (not recommended).
    /// Default: 20
    pub max_waiting_uploads: usize,
    /// Maximum time in seconds to wait for an upload slot before returning HTTP 429.
    /// Set to 0 to reject immediately when no slot is available.
    /// Default: 60
    pub max_upload_wait_secs: u64,
}

impl Default for FileLimitsConfig {
    fn default() -> Self {
        Self {
            max_file_size: 100 * 1024 * 1024, // 100MB
            max_requests_per_file: 0,         // 0 = unlimited
            // 0 = unlimited (existing behavior)
            max_concurrent_uploads: 0,
            max_waiting_uploads: 20,
            max_upload_wait_secs: 60,
        }
    }
}

/// Batch API configuration.
///
/// The batch API provides OpenAI-compatible batch processing endpoints for asynchronous
/// request processing. Note: The batch processing daemon configuration has been moved
/// to `background_services.batch_daemon`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BatchConfig {
    /// Enable batches API endpoints (default: true)
    pub enabled: bool,
    /// Allowed completion windows (SLAs) for batch processing.
    /// These define the maximum time from batch creation to completion.
    /// Default: vec!["24h".to_string()]
    pub allowed_completion_windows: Vec<String>,
    /// Files configuration for batch file uploads/downloads
    pub files: FilesConfig,
    /// Default throughput (requests/second) for models without explicit throughput configured.
    /// Used for SLA capacity calculations when accepting new batches.
    /// If not specified or null, defaults to 100.0 req/s. This is quite high, in favour of over-acceptance.
    /// Must be positive (> 0) when specified.
    #[serde(default = "default_batch_throughput", deserialize_with = "deserialize_positive_throughput")]
    pub default_throughput: f32,
}

fn default_batch_throughput() -> f32 {
    100.0
}

/// Custom deserializer that validates throughput is positive, with null/missing defaulting to 100.0
fn deserialize_positive_throughput<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    // First, try to deserialize as Option<f32> to handle null
    let opt: Option<f32> = Option::deserialize(deserializer)?;

    match opt {
        None => Ok(default_batch_throughput()), // null or missing -> use default
        Some(value) if value <= 0.0 => Err(D::Error::custom(format!(
            "default_throughput must be positive (> 0), got {}",
            value
        ))),
        Some(value) => Ok(value),
    }
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_completion_windows: vec!["24h".to_string()],
            files: FilesConfig::default(),
            default_throughput: default_batch_throughput(),
        }
    }
}

/// Batch processing daemon configuration.
///
/// The daemon processes batch requests asynchronously in the background.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// When to run the daemon (default: "leader")
    /// - "always": Always run the daemon
    /// - "never": Never run the daemon
    /// - "leader": Only run if this instance is the leader
    pub enabled: DaemonEnabled,

    /// Maximum number of requests to claim in each iteration (default: 100)
    pub claim_batch_size: usize,

    /// Default concurrency limit per model (default: 10)
    pub default_model_concurrency: usize,

    /// How long to sleep between claim iterations in milliseconds (default: 1000)
    pub claim_interval_ms: u64,

    /// Maximum number of retry attempts before giving up
    /// If None, retries will run until stop_before_deadline_ms
    pub max_retries: Option<u32>,

    /// Stop retrying this many milliseconds before batch deadline
    /// Positive values stop before the deadline (safety buffer)
    /// Negative values allow retrying after the deadline
    /// If None, retries are not deadline-aware
    pub stop_before_deadline_ms: Option<i64>,

    /// Base backoff duration in milliseconds (will be exponentially increased) (default: 1000)
    pub backoff_ms: u64,

    /// Factor by which the backoff_ms is increased with each retry (default: 2)
    pub backoff_factor: u64,

    /// Maximum backoff time in milliseconds (default: 10000)
    pub max_backoff_ms: u64,

    /// Timeout for each individual request attempt in milliseconds (default: 600000 = 10 minutes)
    pub timeout_ms: u64,

    /// Interval for logging daemon status (requests in flight) in milliseconds
    /// Set to None to disable periodic status logging (default: Some(2000))
    pub status_log_interval_ms: Option<u64>,

    /// Maximum time a request can stay in "claimed" state before being unclaimed
    /// and returned to pending (milliseconds). This handles daemon crashes. (default: 60000 = 1 minute)
    pub claim_timeout_ms: u64,

    /// Maximum time a request can stay in "processing" state before being unclaimed
    /// and returned to pending (milliseconds). This handles daemon crashes during execution. (default: 600000 = 10 minutes)
    pub processing_timeout_ms: u64,

    /// Per-model configurations for SLA escalation via route-at-claim-time.
    /// When a request is claimed with less than `escalation_threshold_seconds` remaining
    /// before batch expiry, it's routed to the `escalation_model` instead.
    ///
    /// Parameters:
    ///     * escalation_model: model to route to for late-stage requests
    ///     * escalation_threshold_seconds: time before batch expiry to trigger routing (default: 900 = 15 minutes)
    ///
    /// Note: Batch API keys automatically have access to escalation models in the routing cache.
    pub model_escalations: HashMap<String, fusillade::ModelEscalationConfig>,

    /// Batch table column names to include as request headers.
    /// These values are sent as `x-fusillade-batch-{column}` headers with each request.
    /// Example: ["id", "created_by", "endpoint"] produces headers like:
    ///   - x-fusillade-batch-id
    ///   - x-fusillade-batch-created-by
    ///   - x-fusillade-batch-endpoint
    #[serde(default = "default_batch_metadata_fields_dwctl")]
    pub batch_metadata_fields: Vec<String>,

    /// Interval for running the orphaned row purge task (milliseconds).
    /// Deletes orphaned request_templates and requests whose parent file/batch
    /// has been soft-deleted, for right-to-erasure compliance.
    /// Set to 0 to disable purging. Default: 600000 (10 minutes).
    pub purge_interval_ms: u64,

    /// Maximum number of orphaned rows to delete per purge iteration.
    /// Each iteration deletes up to this many requests and this many request_templates.
    /// Default: 1000.
    pub purge_batch_size: i64,

    /// Throttle delay between consecutive purge batches within a single drain
    /// cycle (milliseconds). Prevents sustained high DB load when many orphans
    /// exist. Default: 100.
    pub purge_throttle_ms: u64,
}

fn default_batch_metadata_fields_dwctl() -> Vec<String> {
    vec![
        "id".to_string(),
        "endpoint".to_string(),
        "created_at".to_string(),
        "completion_window".to_string(),
        "request_source".to_string(),
    ]
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: DaemonEnabled::Leader,
            claim_batch_size: 100,
            default_model_concurrency: 10,
            claim_interval_ms: 1000,
            max_retries: Some(1000),
            stop_before_deadline_ms: Some(900_000),
            backoff_ms: 1000,
            backoff_factor: 2,
            max_backoff_ms: 10000,
            timeout_ms: 600000,
            status_log_interval_ms: Some(2000),
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
            batch_metadata_fields: default_batch_metadata_fields_dwctl(),
            model_escalations: HashMap::new(),
            purge_interval_ms: 600_000,
            purge_batch_size: 1000,
            purge_throttle_ms: 100,
        }
    }
}

impl DaemonConfig {
    /// Convert to fusillade daemon config
    pub fn to_fusillade_config(&self) -> fusillade::daemon::DaemonConfig {
        self.to_fusillade_config_with_limits(None)
    }

    pub fn to_fusillade_config_with_limits(
        &self,
        model_capacity_limits: Option<std::sync::Arc<dashmap::DashMap<String, usize>>>,
    ) -> fusillade::daemon::DaemonConfig {
        fusillade::daemon::DaemonConfig {
            claim_batch_size: self.claim_batch_size,
            default_model_concurrency: self.default_model_concurrency,
            model_concurrency_limits: model_capacity_limits.unwrap_or_else(|| std::sync::Arc::new(dashmap::DashMap::new())),
            model_escalations: Arc::new(DashMap::from_iter(self.model_escalations.clone())),
            claim_interval_ms: self.claim_interval_ms,
            max_retries: self.max_retries,
            stop_before_deadline_ms: self.stop_before_deadline_ms,
            backoff_ms: self.backoff_ms,
            backoff_factor: self.backoff_factor,
            max_backoff_ms: self.max_backoff_ms,
            timeout_ms: self.timeout_ms,
            status_log_interval_ms: self.status_log_interval_ms,
            claim_timeout_ms: self.claim_timeout_ms,
            processing_timeout_ms: self.processing_timeout_ms,
            batch_metadata_fields: self.batch_metadata_fields.clone(),
            purge_interval_ms: self.purge_interval_ms,
            purge_batch_size: self.purge_batch_size,
            purge_throttle_ms: self.purge_throttle_ms,
            ..Default::default()
        }
    }
}

/// Controls when the batch processing daemon runs.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DaemonEnabled {
    /// Always run the daemon on this instance
    Always,
    /// Never run the daemon on this instance
    Never,
    /// Only run the daemon if this instance is elected leader
    Leader,
}

/// Leader election configuration for multi-instance deployments.
///
/// Leader election uses PostgreSQL advisory locks to elect a single leader instance that
/// runs background services like health probes and batch processing.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LeaderElectionConfig {
    /// Enable leader election (default: true)
    /// When false, this instance always runs as leader (useful for single-instance deployments and testing)
    pub enabled: bool,
}

impl Default for LeaderElectionConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Batch completion notification configuration.
///
/// When enabled, polls for completed/failed/cancelled batches and sends email notifications
/// to batch creators. Safe to run on all replicas — uses atomic `notification_sent_at` claim
/// to prevent duplicate emails.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Enable batch completion notifications (default: true)
    pub enabled: bool,
    /// How often to poll for completed batches (default: 30s)
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    /// Webhook delivery configuration for Standard Webhooks-compliant
    /// notifications for batch terminal state events (completed, failed).
    pub webhooks: WebhookConfig,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval: Duration::from_secs(30),
            webhooks: WebhookConfig::default(),
        }
    }
}

/// Background services configuration.
///
/// Controls which background services are enabled on this instance.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct BackgroundServicesConfig {
    /// Configuration for onwards config sync service
    pub onwards_sync: OnwardsSyncConfig,
    /// Configuration for probe scheduler service
    pub probe_scheduler: ProbeSchedulerConfig,
    /// Configuration for batch processing daemon
    pub batch_daemon: DaemonConfig,
    /// Leader election configuration for multi-instance deployments
    pub leader_election: LeaderElectionConfig,
    /// Configuration for database pool metrics sampling
    pub pool_metrics: PoolMetricsSamplerConfig,
    /// Configuration for batch completion notifications (email + webhooks)
    pub notifications: NotificationsConfig,
}

/// Database pool metrics sampling configuration.
///
/// Controls how often database connection pool metrics are sampled and recorded.
/// Metrics include connection counts (total, idle, in-use, max) for each pool.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolMetricsSamplerConfig {
    /// How often to sample pool metrics (default: 5s)
    #[serde(with = "humantime_serde")]
    pub sample_interval: Duration,
}

impl Default for PoolMetricsSamplerConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_secs(5),
        }
    }
}

/// Onwards configuration sync service configuration.
///
/// This service syncs database configuration changes to the onwards routing layer via PostgreSQL LISTEN/NOTIFY.
/// Disabling this will prevent the AI proxy from receiving config updates (not recommended for production).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OnwardsSyncConfig {
    /// Enable onwards config sync service (default: true)
    pub enabled: bool,
    /// Fallback sync interval in milliseconds (default: 10000ms = 10 seconds)
    ///
    /// Even when LISTEN/NOTIFY is working, this provides periodic full syncs to guarantee
    /// eventual consistency. Prevents issues from dropped notifications or connection problems.
    ///
    /// Set to `0` to disable periodic fallback syncs entirely. Disabling the fallback interval
    /// removes protection against missed notifications and is generally not recommended
    /// in production environments.
    pub fallback_interval_milliseconds: u64,
}

impl Default for OnwardsSyncConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fallback_interval_milliseconds: 10_000, // 10 seconds
        }
    }
}

/// Probe scheduler service configuration.
///
/// The probe scheduler periodically checks inference endpoint health and removes failing backends from rotation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProbeSchedulerConfig {
    /// Enable probe scheduler service (default: true)
    /// When leader election is enabled, the probe scheduler only runs on the elected leader
    pub enabled: bool,
}

impl Default for ProbeSchedulerConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Webhook delivery service configuration.
///
/// The webhook service delivers Standard Webhooks-compliant notifications
/// for batch terminal state events (completed, failed, cancelled).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    /// Enable webhook delivery service (default: true)
    pub enabled: bool,
    /// HTTP timeout for webhook deliveries in seconds (default: 30)
    pub timeout_secs: u64,
    /// Retry backoff schedule in seconds. Each entry is the delay before the
    /// corresponding attempt. The length of this list is the maximum number of
    /// attempts — once exhausted, the delivery is marked as exhausted.
    ///
    /// Default: [0, 5, 300, 1800, 7200, 28800, 86400]
    ///          (immediate, 5s, 5m, 30m, 2h, 8h, 24h)
    pub retry_schedule_secs: Vec<i64>,
    /// Number of consecutive failures before disabling a webhook (default: 10)
    pub circuit_breaker_threshold: i32,
    /// Maximum deliveries to claim from the database per tick (default: 50)
    pub claim_batch_size: i64,
    /// Maximum concurrent outbound HTTP requests (default: 20)
    pub max_concurrent_sends: usize,
    /// Internal channel buffer capacity for send requests and results (default: 200)
    pub channel_capacity: usize,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: 30,
            retry_schedule_secs: vec![0, 5, 300, 1800, 7200, 28800, 86400],
            circuit_breaker_threshold: 10,
            claim_batch_size: 50,
            max_concurrent_sends: 20,
            channel_capacity: 200,
        }
    }
}

/// CORS origin specification.
///
/// Can be either a wildcard (`*`) to allow all origins, or a specific URL.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CorsOrigin {
    /// Allow all origins (`*`)
    #[serde(deserialize_with = "parse_wildcard")]
    Wildcard,
    /// Specific origin URL (e.g., `https://app.example.com`)
    #[serde(deserialize_with = "parse_url")]
    Url(Url),
}

fn parse_wildcard<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    if s == "*" {
        Ok(())
    } else {
        Err(serde::de::Error::custom("Expected '*'"))
    }
}

fn parse_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    Url::parse(&s).map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
/// Credit system configuration.
///
/// Controls how credits are allocated to users for tracking AI usage.
pub struct CreditsConfig {
    /// Initial credits given to standard users when they are created (default: 0)
    pub initial_credits_for_standard_users: rust_decimal::Decimal,
}

impl Default for CreditsConfig {
    fn default() -> Self {
        Self {
            // Default to 0 credits (no credits given on creation)
            initial_credits_for_standard_users: rust_decimal::Decimal::ZERO,
        }
    }
}

/// Analytics batching configuration.
///
/// The batcher uses a write-through strategy:
/// 1. Block until at least one record arrives
/// 2. Drain all available records (up to batch_size)
/// 3. Write immediately (with retry on failure)
///
/// This minimizes latency at low load while getting batching efficiency at high load.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyticsConfig {
    /// Maximum number of records to write in a single batch.
    /// At high load, records queue while writing, naturally forming larger batches.
    /// Default: 100
    pub batch_size: usize,
    /// Maximum number of retry attempts for failed batch writes.
    /// After all retries are exhausted, the batch is dropped and an error is logged.
    /// Default: 3
    pub max_retries: u32,
    /// Base delay in milliseconds for exponential backoff between retries.
    /// Actual delay is: base_delay * 2^attempt (e.g., 100ms, 200ms, 400ms for base=100).
    /// Default: 100
    pub retry_base_delay_ms: u64,
    /// Minimum interval in milliseconds between balance depletion notifications globally.
    ///
    /// When any user's balance goes negative, we send a pg_notify to invalidate their API keys.
    /// This rate limit prevents notification storms when users continue making requests
    /// with negative balances. At most one notification is sent per interval, even if
    /// multiple users become depleted during that time.
    ///
    /// Default: 5000ms (5 seconds)
    pub balance_notification_interval_milliseconds: u64,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            max_retries: 3,
            retry_base_delay_ms: 100,
            balance_notification_interval_milliseconds: 5000,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 3001,
            dashboard_url: "http://localhost:5173".to_string(),
            database_url: None, // Deprecated field
            database_replica_url: None,
            database: DatabaseConfig::default(),
            slow_statement_threshold_ms: 1000,
            admin_email: "test@doubleword.ai".to_string(),
            admin_password: Some("hunter2".to_string()),
            secret_key: None,
            model_sources: vec![],
            metadata: Metadata::default(),
            payment: None,
            auth: AuthConfig::default(),
            batches: BatchConfig::default(),
            background_services: BackgroundServicesConfig::default(),
            enable_metrics: true,
            enable_request_logging: true,
            enable_analytics: true,
            analytics: AnalyticsConfig::default(),
            enable_otel_export: false,
            credits: CreditsConfig::default(),
            sample_files: SampleFilesConfig::default(),
            limits: LimitsConfig::default(),
            email: EmailConfig::default(),
        }
    }
}

impl Default for ModelSource {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: Url::parse("http://localhost:8080").unwrap(),
            api_key: None,
            sync_interval: Duration::from_secs(10),
            default_models: None,
        }
    }
}

impl Default for NativeAuthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_registration: false,
            password: PasswordConfig::default(),
            session: SessionConfig::default(),
            password_reset_token_duration: Duration::from_secs(30 * 60), // 30 minutes
        }
    }
}

impl Default for ProxyHeaderAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            header_name: "x-doubleword-user".to_string(),
            email_header_name: "x-doubleword-email".to_string(),
            groups_field_name: "x-doubleword-user-groups".to_string(),
            provider_field_name: "x-doubleword-sso-provider".to_string(),
            auto_create_users: true,
            blacklisted_sso_groups: Vec::new(),
            import_idp_groups: false,
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(24 * 60 * 60), // 24 hours
            cookie_name: "dwctl_session".to_string(),
            cookie_secure: true,
            cookie_same_site: "strict".to_string(),
        }
    }
}

impl Default for PasswordConfig {
    fn default() -> Self {
        Self {
            min_length: 8,
            max_length: 64,
            // Secure defaults for production (Argon2id RFC recommendations)
            argon2_memory_kib: 19456, // 19 MB
            argon2_iterations: 2,
            argon2_parallelism: 1,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            jwt_expiry: Duration::from_secs(24 * 60 * 60), // 24 hours
            cors: CorsConfig::default(),
        }
    }
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec![
                CorsOrigin::Url(Url::parse("htt://localhost:3001").unwrap()), // Development frontend (Vite)
            ],
            allow_credentials: true,
            max_age: Some(3600), // Cache preflight for 1 hour
            exposed_headers: vec!["location".to_string()],
        }
    }
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            transport: EmailTransportConfig::default(),
            from_email: "noreply@example.com".to_string(),
            from_name: "Control Layer".to_string(),
            reply_to: None,
        }
    }
}

impl Default for EmailTransportConfig {
    fn default() -> Self {
        Self::File {
            path: "./emails".to_string(),
        }
    }
}

impl ModelSource {
    fn default_sync_interval() -> Duration {
        Duration::from_secs(10)
    }
}

impl Config {
    #[allow(clippy::result_large_err)]
    pub fn load(args: &Args) -> Result<Self, figment::Error> {
        let mut config: Self = Self::figment(args).extract()?;

        // if database_url is set, use it (preserving existing pool and component settings)
        if let Some(url) = config.database_url.take() {
            let pool = config.database.main_pool_settings().clone();
            let fusillade = config.database.fusillade().clone();
            let outlet = config.database.outlet().clone();

            // Preserve original replica_pool if it was explicitly configured (not using fallback)
            let original_replica_pool = match &config.database {
                DatabaseConfig::External { replica_pool, .. } => replica_pool.clone(),
                DatabaseConfig::Embedded { replica_pool, .. } => replica_pool.clone(),
            };

            // Check if replica_url was set via environment variable
            let replica_url = config.database_replica_url.take();

            config.database = DatabaseConfig::External {
                url,
                replica_url,
                pool,
                replica_pool: original_replica_pool, // Always preserve original replica_pool if it existed
                fusillade,
                outlet,
            };
        } else if let Some(replica_url) = config.database_replica_url.take() {
            // Only replica_url is set via environment variable, apply it to existing config
            match &mut config.database {
                DatabaseConfig::External {
                    replica_url: current_replica,
                    ..
                } => {
                    *current_replica = Some(replica_url);
                }
                DatabaseConfig::Embedded { .. } => {
                    // Can't set replica for embedded database
                }
            }
        }

        config.validate().map_err(|e| figment::Error::from(e.to_string()))?;
        Ok(config)
    }

    /// Get the database connection string
    /// Returns None if using embedded database (connection string will be set at runtime)
    pub fn database_url(&self) -> Option<&str> {
        self.database.external_url()
    }

    /// Validate the configuration for consistency and required fields
    pub fn validate(&self) -> Result<(), Error> {
        // Validate native authentication requirements
        if self.auth.native.enabled {
            if self.secret_key.is_none() {
                return Err(Error::Internal {
                    operation: "Config validation: Native authentication is enabled but secret_key is not configured. \
                     Please set DWCTL_SECRET_KEY environment variable or add secret_key to config file."
                        .to_string(),
                });
            }

            // Validate password requirements
            if self.auth.native.password.min_length > self.auth.native.password.max_length {
                return Err(Error::Internal {
                    operation: format!(
                        "Config validation: Invalid password configuration: min_length ({}) cannot be greater than max_length ({})",
                        self.auth.native.password.min_length, self.auth.native.password.max_length
                    ),
                });
            }

            if self.auth.native.password.min_length < 1 {
                return Err(Error::Internal {
                    operation: "Config validation: Invalid password configuration: min_length must be at least 1".to_string(),
                });
            }
        }

        // Validate JWT expiry duration is reasonable
        if self.auth.security.jwt_expiry.as_secs() < 300 {
            // Less than 5 minutes
            return Err(Error::Internal {
                operation: "Config validation: JWT expiry duration is too short (minimum 5 minutes)".to_string(),
            });
        }

        if self.auth.security.jwt_expiry.as_secs() > 86400 * 30 {
            // More than 30 days
            return Err(Error::Internal {
                operation: "Config validation: JWT expiry duration is too long (maximum 30 days)".to_string(),
            });
        }

        // Validate that at least one auth method is enabled
        if !self.auth.native.enabled && !self.auth.proxy_header.enabled {
            return Err(Error::Internal {
                operation:
                    "Config validation: No authentication methods are enabled. Please enable either native or proxy_header authentication."
                        .to_string(),
            });
        }

        // Validate CORS configuration
        if self.auth.security.cors.allowed_origins.is_empty() {
            return Err(Error::Internal {
                operation: "Config validation: CORS allowed_origins cannot be empty. Add at least one allowed origin.".to_string(),
            });
        }

        // Validate that wildcard is not used with credentials
        let has_wildcard = self
            .auth
            .security
            .cors
            .allowed_origins
            .iter()
            .any(|origin| matches!(origin, CorsOrigin::Wildcard));
        if has_wildcard && self.auth.security.cors.allow_credentials {
            return Err(Error::Internal {
                operation: "Config validation: CORS cannot use wildcard origin '*' with allow_credentials=true. Specify explicit origins."
                    .to_string(),
            });
        }

        // Validate batch file configuration whenever the request manager could be used.
        // The PostgresRequestManager is always constructed and uses these values for its batch
        // insert strategy and buffer sizes. These settings are required when:
        // - The batches API is enabled (file uploads/downloads use the request manager)
        // - The batch daemon can run (processes batch requests)
        let daemon_can_run = self.background_services.batch_daemon.enabled != DaemonEnabled::Never;
        let validate_request_manager_config = self.batches.enabled || daemon_can_run;

        if validate_request_manager_config {
            // batch_insert_size is used by PostgresRequestManager for database insertion strategy
            if self.batches.files.batch_insert_size == 0 {
                return Err(Error::Internal {
                    operation: "Config validation: batch_insert_size cannot be 0. Set a positive integer value (recommended: 1000-10000). \
                               This setting is used by the request manager when batches are enabled or the daemon runs."
                        .to_string(),
                });
            }

            // download_buffer_size is used by PostgresRequestManager for file download streams
            if self.batches.files.download_buffer_size == 0 {
                return Err(Error::Internal {
                    operation: "Config validation: download_buffer_size cannot be 0. Set a positive integer value (default: 100). \
                               This setting is used by the request manager when batches are enabled or the daemon runs."
                        .to_string(),
                });
            }
        }

        // Validate batches API-specific configuration (only if batches API is enabled)
        if self.batches.enabled {
            // upload_buffer_size is only used during file uploads (batches API specific)
            if self.batches.files.upload_buffer_size == 0 {
                return Err(Error::Internal {
                    operation: "Config validation: upload_buffer_size cannot be 0. Set a positive integer value (default: 100)."
                        .to_string(),
                });
            }

            // Validate file size limits are sensible (0 = unlimited is allowed but not recommended)
            // Note: max_file_size is now in limits.files, not batches.files

            // Validate expiry times are positive and in sensible order
            if self.batches.files.min_expiry_seconds <= 0 {
                return Err(Error::Internal {
                    operation: "Config validation: min_expiry_seconds must be positive (default: 3600 = 1 hour).".to_string(),
                });
            }

            if self.batches.files.default_expiry_seconds <= 0 {
                return Err(Error::Internal {
                    operation: "Config validation: default_expiry_seconds must be positive (default: 86400 = 24 hours).".to_string(),
                });
            }

            if self.batches.files.max_expiry_seconds <= 0 {
                return Err(Error::Internal {
                    operation: "Config validation: max_expiry_seconds must be positive (default: 2592000 = 30 days).".to_string(),
                });
            }

            // Validate expiry times are in correct order
            if self.batches.files.min_expiry_seconds > self.batches.files.default_expiry_seconds {
                return Err(Error::Internal {
                    operation: format!(
                        "Config validation: min_expiry_seconds ({}) cannot be greater than default_expiry_seconds ({})",
                        self.batches.files.min_expiry_seconds, self.batches.files.default_expiry_seconds
                    ),
                });
            }

            if self.batches.files.default_expiry_seconds > self.batches.files.max_expiry_seconds {
                return Err(Error::Internal {
                    operation: format!(
                        "Config validation: default_expiry_seconds ({}) cannot be greater than max_expiry_seconds ({})",
                        self.batches.files.default_expiry_seconds, self.batches.files.max_expiry_seconds
                    ),
                });
            }

            if self.batches.files.min_expiry_seconds > self.batches.files.max_expiry_seconds {
                return Err(Error::Internal {
                    operation: format!(
                        "Config validation: min_expiry_seconds ({}) cannot be greater than max_expiry_seconds ({})",
                        self.batches.files.min_expiry_seconds, self.batches.files.max_expiry_seconds
                    ),
                });
            }
        }

        Ok(())
    }

    pub fn figment(args: &Args) -> Figment {
        Figment::new()
            // Load base config file
            .merge(Yaml::file(&args.config))
            // Environment variables can still override specific values
            .merge(Env::prefixed("DWCTL_").split("__"))
            // Common DATABASE_URL and DATABASE_REPLICA_URL patterns
            // Accept both DATABASE_REPLICA_URL and DWCTL_DATABASE_REPLICA_URL
            .merge(Env::raw().only(&["DATABASE_URL", "DATABASE_REPLICA_URL"]))
            .merge(
                Env::raw()
                    .only(&["DWCTL_DATABASE_REPLICA_URL"])
                    .map(|_| "database_replica_url".into()),
            )
    }

    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Jail;

    #[test]
    fn test_model_sources_config() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: hello
model_sources:
  - name: openai
    url: https://api.openai.com
    api_key: sk-test
    sync_interval: 30s
  - name: internal
    url: http://internal:8080
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let config = Config::load(&args)?;

            assert_eq!(config.model_sources.len(), 2);

            let openai = &config.model_sources[0];
            assert_eq!(openai.name, "openai");
            assert_eq!(openai.url.as_str(), "https://api.openai.com/");
            assert_eq!(openai.api_key.as_deref(), Some("sk-test"));
            assert_eq!(openai.sync_interval, Duration::from_secs(30));

            let internal = &config.model_sources[1];
            assert_eq!(internal.name, "internal");
            assert_eq!(internal.sync_interval, Duration::from_secs(10)); // default

            Ok(())
        });
    }

    #[test]
    fn test_env_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: hello
metadata:
  region: US East
  organization: Test Corp
"#,
            )?;

            jail.set_env("DWCTL_HOST", "127.0.0.1");
            jail.set_env("DWCTL_PORT", "8080");

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let config = Config::load(&args)?;

            // Env vars should override
            assert_eq!(config.host, "127.0.0.1");
            assert_eq!(config.port, 8080);

            // YAML values should be preserved
            assert_eq!(config.metadata.region, Some("US East".to_string()));
            assert_eq!(config.metadata.organization, Some("Test Corp".to_string()));

            Ok(())
        });
    }

    #[test]
    fn test_auth_config_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key-for-testing"
auth:
  native:
    enabled: true
    allow_registration: false
    password:
      min_length: 12
  proxy_header:
    enabled: false
    header_name: "x-custom-user"
  security:
    jwt_expiry: "2h"
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let config = Config::load(&args)?;

            // Check overridden values
            assert!(config.auth.native.enabled);
            assert!(!config.auth.native.allow_registration);
            assert_eq!(config.auth.native.password.min_length, 12);
            assert_eq!(config.auth.native.password.max_length, 64); // still default

            assert!(!config.auth.proxy_header.enabled);
            assert_eq!(config.auth.proxy_header.header_name, "x-custom-user");

            assert_eq!(config.auth.security.jwt_expiry, Duration::from_secs(2 * 60 * 60));

            Ok(())
        });
    }

    #[test]
    fn test_config_validation_native_auth_missing_secret() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = None;

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("secret_key is not configured"));
    }

    #[test]
    fn test_config_validation_invalid_password_length() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-key".to_string());
        config.auth.native.password.min_length = 10;
        config.auth.native.password.max_length = 5;

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("min_length"));
    }

    #[test]
    fn test_config_validation_no_auth_methods_enabled() {
        let mut config = Config::default();
        config.auth.native.enabled = false;
        config.auth.proxy_header.enabled = false;

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No authentication methods"));
    }

    #[test]
    fn test_config_validation_valid_config() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());

        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn test_batch_insert_size_default() {
        let config = Config::default();
        assert_eq!(config.batches.files.batch_insert_size, 5000);
    }

    #[test]
    fn test_batch_insert_size_yaml_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  files:
    batch_insert_size: 10000
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.batches.files.batch_insert_size, 10000);

            Ok(())
        });
    }

    #[test]
    fn test_batch_insert_size_env_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
"#,
            )?;

            jail.set_env("DWCTL_BATCHES__FILES__BATCH_INSERT_SIZE", "7500");

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.batches.files.batch_insert_size, 7500);

            Ok(())
        });
    }

    #[test]
    fn test_batch_insert_size_zero_validation() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = true;
        config.batches.files.batch_insert_size = 0;

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("batch_insert_size cannot be 0"));
    }

    #[test]
    fn test_upload_buffer_size_zero_validation() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = true;
        config.batches.files.upload_buffer_size = 0;

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("upload_buffer_size cannot be 0"));
    }

    #[test]
    fn test_download_buffer_size_zero_validation() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = true;
        config.batches.files.download_buffer_size = 0;

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("download_buffer_size cannot be 0"));
    }

    #[test]
    fn test_expiry_times_positive_validation() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = true;

        // Test min_expiry_seconds
        config.batches.files.min_expiry_seconds = 0;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("min_expiry_seconds must be positive"));

        // Test default_expiry_seconds
        config.batches.files.min_expiry_seconds = 3600;
        config.batches.files.default_expiry_seconds = 0;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("default_expiry_seconds must be positive"));

        // Test max_expiry_seconds
        config.batches.files.default_expiry_seconds = 86400;
        config.batches.files.max_expiry_seconds = 0;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max_expiry_seconds must be positive"));
    }

    #[test]
    fn test_expiry_times_order_validation() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = true;

        // Test min > default
        config.batches.files.min_expiry_seconds = 86400;
        config.batches.files.default_expiry_seconds = 3600;
        config.batches.files.max_expiry_seconds = 2592000;
        let result = config.validate();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("min_expiry_seconds") && err_msg.contains("default_expiry_seconds"));

        // Test default > max
        config.batches.files.min_expiry_seconds = 3600;
        config.batches.files.default_expiry_seconds = 2592000;
        config.batches.files.max_expiry_seconds = 86400;
        let result = config.validate();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("default_expiry_seconds") && err_msg.contains("max_expiry_seconds"));

        // Test min > max (should also fail)
        config.batches.files.min_expiry_seconds = 2592000;
        config.batches.files.default_expiry_seconds = 86400;
        config.batches.files.max_expiry_seconds = 3600;
        let result = config.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_batch_validation_skipped_when_disabled() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = false; // Disabled
        config.background_services.batch_daemon.enabled = DaemonEnabled::Never; // Daemon also disabled
        config.batches.files.batch_insert_size = 0; // Invalid, but should be ignored when daemon is Never

        let result = config.validate();
        assert!(result.is_ok()); // Should pass because both batches AND daemon are disabled
    }

    #[test]
    fn test_batch_insert_size_validated_when_daemon_enabled() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = false; // Batches API disabled
        config.background_services.batch_daemon.enabled = DaemonEnabled::Leader; // But daemon can run
        config.batches.files.batch_insert_size = 0; // Invalid

        let result = config.validate();
        assert!(result.is_err()); // Should fail because daemon can run and needs valid batch_insert_size
        assert!(result.unwrap_err().to_string().contains("batch_insert_size cannot be 0"));
    }

    #[test]
    fn test_download_buffer_validated_when_daemon_enabled() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = false; // Batches API disabled
        config.background_services.batch_daemon.enabled = DaemonEnabled::Always; // Daemon always runs
        config.batches.files.download_buffer_size = 0; // Invalid

        let result = config.validate();
        assert!(result.is_err()); // Should fail because daemon uses download_buffer_size
        assert!(result.unwrap_err().to_string().contains("download_buffer_size cannot be 0"));
    }

    #[test]
    fn test_batch_insert_size_validated_when_batches_enabled_daemon_never() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = true; // Batches API enabled
        config.background_services.batch_daemon.enabled = DaemonEnabled::Never; // Daemon disabled
        config.batches.files.batch_insert_size = 0; // Invalid

        let result = config.validate();
        assert!(result.is_err()); // Should fail because batches API needs valid batch_insert_size
        assert!(result.unwrap_err().to_string().contains("batch_insert_size cannot be 0"));
    }

    #[test]
    fn test_download_buffer_validated_when_batches_enabled_daemon_never() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.batches.enabled = true; // Batches API enabled
        config.background_services.batch_daemon.enabled = DaemonEnabled::Never; // Daemon disabled
        config.batches.files.download_buffer_size = 0; // Invalid

        let result = config.validate();
        assert!(result.is_err()); // Should fail because batches API needs valid download_buffer_size
        assert!(result.unwrap_err().to_string().contains("download_buffer_size cannot be 0"));
    }

    #[test]
    fn test_default_throughput_default_value() {
        let config = Config::default();
        assert_eq!(config.batches.default_throughput, 100.0);
    }

    #[test]
    fn test_default_throughput_yaml_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  default_throughput: 100.0
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.batches.default_throughput, 100.0);

            Ok(())
        });
    }

    #[test]
    fn test_default_throughput_null_uses_default() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  default_throughput: null
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.batches.default_throughput, 100.0); // Should use default

            Ok(())
        });
    }

    #[test]
    fn test_default_throughput_missing_uses_default() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  enabled: true
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.batches.default_throughput, 100.0); // Should use default

            Ok(())
        });
    }

    #[test]
    fn test_default_throughput_zero_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  default_throughput: 0
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("default_throughput must be positive"));

            Ok(())
        });
    }

    #[test]
    fn test_default_throughput_negative_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  default_throughput: -10.0
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("default_throughput must be positive"));

            Ok(())
        });
    }

    #[test]
    fn test_default_throughput_env_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
"#,
            )?;

            jail.set_env("DWCTL_BATCHES__DEFAULT_THROUGHPUT", "75.5");

            let args = Args {
                config: "test.yaml".to_string(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.batches.default_throughput, 75.5);

            Ok(())
        });
    }
}
