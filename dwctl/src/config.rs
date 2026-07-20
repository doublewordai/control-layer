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
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
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
    pub config: PathBuf,

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
    /// Onwards proxy configuration
    pub onwards: OnwardsConfig,
    /// Optional URL to redirect new users to for onboarding (e.g., "https://onboarding.doubleword.ai")
    /// When set, users with a null `last_login` will receive this URL in the `/users/current` response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onboarding_url: Option<String>,
    /// Email address where support requests are sent (default: "support@doubleword.ai")
    pub support_email: String,
    /// External data source connections configuration
    #[serde(default)]
    pub connections: ConnectionsConfig,
    /// Multi-step Open Responses orchestration configuration. Only the
    /// safety caps are exposed today; storage and dispatch are wired
    /// implicitly when the multi-step processor is registered with
    /// fusillade.
    #[serde(default)]
    pub responses: ResponsesConfig,
    /// Image-input normalisation configuration.
    ///
    /// When enabled, image references in `/v1/chat/completions` and
    /// `/v1/responses` request bodies are routed through a hardened
    /// content-addressed store before being forwarded to upstream
    /// providers. See `crate::image_normalizer::config` for the full
    /// shape and defaults.
    #[serde(default)]
    pub image_normalizer: crate::image_normalizer::ImageNormalizerConfig,
    /// Encrypted key custody (Redis-backed wrapped-key store). Currently used by
    /// zero-data-retention flex requests to hold per-request body keys; absent
    /// (the default) leaves ZDR disabled.
    #[serde(default)]
    pub keystore: Option<crate::keystore::KeystoreConfig>,
    /// OpenAPI spec exposure controls. Defaults disable the Admin spec
    /// (which describes internal management endpoints) and enable the
    /// AI spec (which mirrors the publicly-documented OpenAI surface).
    /// Both surfaces require authentication regardless of these flags.
    #[serde(default)]
    pub openapi: OpenApiConfig,
    /// Cached-input pricing (the dwctl-owned cache layer): the on/off flag, the
    /// tokenizer-svc URL, and the default pricing multipliers. See [`CacheConfig`].
    #[serde(default)]
    pub cache: CacheConfig,
}

/// Controls exposure of the OpenAPI specs and Scalar doc UIs.
///
/// Both surfaces are mounted by default but always require
/// authentication. The Admin spec additionally requires an admin-level
/// identity (PlatformManager role, the admin user, or a `platform`-
/// purpose API key) — inference `sk-*` keys, StandardUsers, and
/// RequestViewers are rejected. Operators who want to remove the route
/// entirely can set the relevant flag to `false`; disabled routes
/// return an explicit 404 so probes can't tell the route exists.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenApiConfig {
    /// Expose `/admin/openapi.json` and `/admin/docs`. Defaults to
    /// `true`; the routes require an admin-level identity. Set to
    /// `false` to remove the routes entirely (they return 404).
    pub admin_enabled: bool,
    /// Expose `/ai/openapi.json` and `/ai/docs`. Defaults to `true`;
    /// the routes require any authenticated identity.
    pub ai_enabled: bool,
}

impl Default for OpenApiConfig {
    fn default() -> Self {
        Self {
            admin_enabled: true,
            ai_enabled: true,
        }
    }
}

/// Configuration for `/v1/responses` multi-step orchestration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResponsesConfig {
    /// Maximum sub-agent recursion depth (per plan §C11). A
    /// `tool_call` step whose sub-agent dispatch would exceed this
    /// depth is failed with `max_depth_exceeded`.
    pub max_response_step_depth: u32,
    /// Maximum model_call ↔ tool_call iterations within a single loop
    /// level (per plan §C11). A loop level that hits this cap fails
    /// with `max_iterations_exceeded`.
    pub max_response_iterations: u32,
}

impl Default for ResponsesConfig {
    fn default() -> Self {
        Self {
            max_response_step_depth: 8,
            max_response_iterations: 10,
        }
    }
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

/// Default underway task worker pool settings (small — only needs PgListener + task processing)
pub fn default_underway_pool() -> PoolSettings {
    PoolSettings {
        max_connections: 100,
        min_connections: 0,
        ..Default::default()
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
        /// Underway task worker pool (separate from main because the worker
        /// holds long-lived PgListener connections)
        #[serde(default = "default_underway_pool")]
        underway_pool: PoolSettings,
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
        /// Underway task worker pool (separate from main because the worker
        /// holds long-lived PgListener connections)
        #[serde(default = "default_underway_pool")]
        underway_pool: PoolSettings,
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
                underway_pool: default_underway_pool(),
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
                underway_pool: default_underway_pool(),
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

    /// Get the underway task worker pool settings
    pub fn underway_pool_settings(&self) -> &PoolSettings {
        match self {
            DatabaseConfig::Embedded { underway_pool, .. } => underway_pool,
            DatabaseConfig::External { underway_pool, .. } => underway_pool,
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
    /// Custom text displayed for terms of service acceptance during auto top-up setup.
    /// If not set, no terms of service acceptance text is shown.
    pub auto_topup_terms_of_service_text: Option<String>,
    /// Stripe tax code for auto top-up tax calculations (e.g. "txcd_10000000").
    /// If not set, falls back to the account-level default tax code in Stripe Tax settings.
    pub tax_code: Option<String>,
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
            docs_url: "https://doublewordai.github.io/control-layer/".to_string(),
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
    /// Default rate-limit tiers applied to API keys based on the owning user's
    /// `verified` flag. Only used when the api_key has no explicit per-key
    /// override. Leaving either tier as `None` means "no limit for that tier".
    pub rate_limits: RateLimitTiersConfig,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            native: NativeAuthConfig::default(),
            proxy_header: ProxyHeaderAuthConfig::default(),
            security: SecurityConfig::default(),
            default_user_roles: vec![Role::StandardUser],
            rate_limits: RateLimitTiersConfig::default(),
        }
    }
}

/// Per-tier defaults for API key rate limits. A `None` tier means no default
/// limit is applied, preserving the legacy "unlimited unless overridden"
/// behaviour for that tier.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitTiersConfig {
    pub verified: Option<RateLimitTierConfig>,
    pub unverified: Option<RateLimitTierConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitTierConfig {
    pub requests_per_second: f32,
    pub burst_size: Option<i32>,
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
    /// Optional Domain attribute for cookies (e.g. ".doubleword.ai" for cross-subdomain)
    pub cookie_domain: Option<String>,
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

/// Security configuration for JWT, CORS, and browser security response headers.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    /// JWT token expiry duration
    #[serde(with = "humantime_serde")]
    pub jwt_expiry: Duration,
    /// CORS configuration for browser clients
    pub cors: CorsConfig,
    /// Browser security response headers (CSP, X-Frame-Options, etc.)
    pub headers: SecurityHeadersConfig,
}

/// Browser security response headers added to every HTTP response.
///
/// Defence-in-depth at the application layer: these are emitted even when a
/// reverse proxy or ingress in front of the server does not add them. Each
/// header is set only if not already present on the response, so any
/// stricter per-route header (e.g. a more restrictive `Referrer-Policy` on
/// sensitive endpoints) is preserved.
///
/// `content_security_policy`, `content_security_policy_report_only` and
/// `strict_transport_security` are opt-in (empty = not sent): a CSP that does
/// not match the deployed frontend can break it, and HSTS is usually owned by
/// whatever terminates TLS. The remaining headers are safe defaults and are
/// on unless `enabled` is set to false.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityHeadersConfig {
    /// Master switch for all security response headers below.
    pub enabled: bool,
    /// `X-Frame-Options` value (e.g. `DENY`, `SAMEORIGIN`). Empty = not sent.
    pub frame_options: String,
    /// `Referrer-Policy` value. Empty = not sent.
    pub referrer_policy: String,
    /// `Permissions-Policy` value. Empty = not sent.
    pub permissions_policy: String,
    /// `Content-Security-Policy` value. Empty = not sent (opt-in).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content_security_policy: String,
    /// `Content-Security-Policy-Report-Only` value. Empty = not sent (opt-in).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content_security_policy_report_only: String,
    /// `Strict-Transport-Security` value. Empty = not sent (opt-in; usually
    /// owned by whatever terminates TLS).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub strict_transport_security: String,
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
    /// When set, in addition to the credentialed `allowed_origins` allowlist,
    /// allow ANY other origin to make CORS requests *without* credentials.
    /// First-party allowlisted origins still receive
    /// `Access-Control-Allow-Credentials: true`; every other origin gets a
    /// reflected `Access-Control-Allow-Origin` and no credentials. This lets
    /// third-party browser apps call the public API without ever exposing
    /// cookie-credentialed access to arbitrary sites.
    pub allow_any_origin_without_credentials: bool,
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
    /// Directory to load email templates from at runtime.
    /// If not set, uses templates embedded at compile time.
    pub templates_dir: Option<String>,
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
/// Controls per-request body size limits for individual requests, both within
/// batch JSONL files and on the realtime AI proxy path, to prevent individual
/// requests from overwhelming inference providers.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestLimitsConfig {
    /// Maximum body size in bytes for individual requests. Applies to requests
    /// within batch JSONL files and to realtime request bodies on the onwards
    /// AI proxy path (/ai/v1/*).
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

/// Onwards AI proxy configuration.
///
/// Controls behavior of the onwards routing layer used for AI proxy requests.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OnwardsConfig {
    /// Enable strict mode with schema validation and typed handlers.
    /// When false (default), all requests are passed through transparently.
    /// When true, only known OpenAI API paths are accepted and validated.
    pub strict_mode: bool,
}

/// Cached-input pricing — the dwctl-owned cache tower layer. All cache configuration lives
/// here (formerly split across `onwards.*` and a top-level `cache_pricing`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Enable cached-input pricing. When false (the default), the cache layer is not added
    /// to the stack: onwards is byte-identical to today with zero request-path overhead
    /// (no body read, no classify fork, no injection).
    ///
    /// When true, [`crate::prompt_cache::cache_middleware`] wraps the onwards router (inner
    /// to outlet, so billing sees the injected fields): on each cacheable request it forks
    /// the classifier concurrently with the upstream call, strips `cache_control` markers,
    /// injects the `cache_*` usage fields, and commits prefix writes on success. Per-model
    /// activation is still gated by an active `model_cache_tariffs` row, so flipping this on
    /// does nothing until a model is enabled. classify races the (slower) model call under a
    /// deadline, so it adds no request latency.
    ///
    /// Set via environment: `DWCTL_CACHE__ENABLED=true`
    pub enabled: bool,

    /// Base URL of the tokenizer-svc used to count cache-prefix tokens. Only consulted when
    /// `enabled` is true; the classifier calls `{tokenizer_url}/v1/models` and
    /// `{tokenizer_url}/v1/tokenize`. A namespace-relative service name (e.g.
    /// `http://tokenizer-svc:8088`) resolves to the tokenizer-svc in the pod's own namespace.
    ///
    /// Set via environment: `DWCTL_CACHE__TOKENIZER_URL=http://tokenizer-svc:8088`
    pub tokenizer_url: String,

    /// Default pricing multipliers (pre-fill when enabling caching on a model without
    /// explicit per-tier values). See [`CachePricingConfig`].
    pub pricing: CachePricingConfig,

    /// The cache TTL tiers the platform currently offers, as Anthropic-style strings
    /// (`"5m"`, `"1h"`, `"24h"`). A request whose `cache_control` marker names a tier NOT in
    /// this list is rejected with a 400 (like an unknown parameter) — not silently un-cached,
    /// so billing stays honest. Restrict it to roll out a subset (e.g. drop `"24h"` until the
    /// KV-store mechanism exists). A model may still carry a tariff for a disabled tier; it
    /// just can't be reached until the tier is re-enabled here. Default: `["5m", "1h"]`.
    ///
    /// Set via environment: `DWCTL_CACHE__ENABLED_TTLS=5m,1h`
    pub enabled_ttls: Vec<String>,

    /// The tier a `cache_control: {type: "ephemeral"}` marker with no explicit `ttl` defaults
    /// to (Anthropic's default is `"5m"`). Must be one of `enabled_ttls`.
    ///
    /// Set via environment: `DWCTL_CACHE__DEFAULT_TTL=5m`
    pub default_ttl: String,

    /// Handling of provider-injected per-request *telemetry* blocks (e.g. the Claude Code SDK's
    /// `x-anthropic-billing-header` line, whose nonce changes every request). Such a block sits
    /// ahead of the caller's `cache_control` breakpoint, so leaving it in would change the prefix
    /// hash every turn — forcing write-only caching (no read discount) and defeating the upstream
    /// KV/prefix cache. Matched blocks are always excluded from our cache prefix; see
    /// [`TelemetryBlockConfig`] for whether they're also removed from the forwarded prompt.
    pub telemetry_blocks: TelemetryBlockConfig,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tokenizer_url: "http://localhost:8088".to_string(),
            pricing: CachePricingConfig::default(),
            enabled_ttls: vec!["5m".to_string(), "1h".to_string()],
            default_ttl: "5m".to_string(),
            telemetry_blocks: TelemetryBlockConfig::default(),
        }
    }
}

/// How provider-injected telemetry blocks are handled. A block counts as "telemetry" only when it
/// is an **unmarked** (`cache_control`-free) **`system`** message content block whose text starts
/// with one of `prefixes` — the role/marker constraints mean `prefixes` never applies to
/// user/assistant content or to a block the caller has marked. Matched blocks are **always**
/// excluded from our cache prefix (the fix for the write-only-caching bug); `strip_from_prompt`
/// additionally controls whether they're removed from the request forwarded to the model.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryBlockConfig {
    /// When true (the default), remove matched telemetry blocks from the forwarded request too —
    /// not just from our cache prefix. This also lets the upstream (sglang/Dynamo) prefix-cache
    /// the real prompt and drops noise the model would otherwise see.
    ///
    /// When false, the block is left in the forwarded request (still excluded from our cache
    /// prefix). Our cache is billing-only — no KV reuse, every request still runs in full upstream
    /// — so this can't produce wrong outputs, and the read discount stays correct because the
    /// excluded prefix genuinely recurs. The only cost is that the per-request telemetry stays in
    /// the model's prompt, defeating the upstream prefix cache and billing those tokens as uncached
    /// each turn. Prefer the default.
    ///
    /// Set via environment: `DWCTL_CACHE__TELEMETRY_BLOCKS__STRIP_FROM_PROMPT=false`
    pub strip_from_prompt: bool,

    /// Leading text prefixes that identify a telemetry block (matched after trimming leading
    /// whitespace). An **empty list disables the feature entirely** (nothing excluded or
    /// stripped). Default: the Claude Code SDK's `x-anthropic-billing-header:` line.
    ///
    /// Set via environment: `DWCTL_CACHE__TELEMETRY_BLOCKS__PREFIXES=x-anthropic-billing-header:`
    pub prefixes: Vec<String>,
}

impl Default for TelemetryBlockConfig {
    fn default() -> Self {
        Self {
            strip_from_prompt: true,
            prefixes: vec!["x-anthropic-billing-header:".to_string()],
        }
    }
}

/// Default cache-pricing multipliers, used when enabling caching on a model without
/// explicit per-tier values. The `model_cache_tariffs` row remains the source of truth
/// (and what billing reads as of inference time) — these only pre-fill it at creation.
///
/// Defaults mirror Anthropic's published premiums: 5m write 1.25×, 1h write 2×, read
/// 0.1×; 24h defaults to 2.5×. Floor 1024 tokens.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CachePricingConfig {
    pub default_write_multiplier_5m: rust_decimal::Decimal,
    pub default_write_multiplier_1h: rust_decimal::Decimal,
    pub default_write_multiplier_24h: rust_decimal::Decimal,
    pub default_read_multiplier: rust_decimal::Decimal,
    pub default_min_prefix_tokens: i32,
}

impl Default for CachePricingConfig {
    fn default() -> Self {
        use rust_decimal::Decimal;
        Self {
            default_write_multiplier_5m: Decimal::new(125, 2), // 1.25
            default_write_multiplier_1h: Decimal::new(2, 0),   // 2.0
            default_write_multiplier_24h: Decimal::new(25, 1), // 2.5
            default_read_multiplier: Decimal::new(1, 1),       // 0.1
            default_min_prefix_tokens: 1024,
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
    /// Allowed completion windows for batch processing.
    /// These define the maximum time from batch creation to completion (e.g., "24h", "1h").
    /// Default: vec!["24h".to_string()]
    pub allowed_completion_windows: Vec<String>,
    /// Per-completion-window relaxation factors for capacity checks.
    ///
    /// A multiplier applied to the model's throughput capacity when deciding
    /// whether to accept a batch for a given completion window. This allows
    /// deliberate over-acceptance when there is enough time to provision
    /// additional capacity before requests are due.
    ///
    /// - `1.0` (default): strict — only accept what the model can handle
    /// - `1.5`: accept up to 50% more than current capacity
    /// - `0.0`: block all new batches for this window
    ///
    /// Keys must match entries in `allowed_completion_windows`. Any allowed
    /// window without an explicit entry defaults to `1.0`. Specifying a window
    /// that is not in `allowed_completion_windows` is a configuration error.
    #[serde(default, deserialize_with = "deserialize_relaxation_factors")]
    pub window_relaxation_factors: HashMap<String, f32>,
    /// Allowed OpenAI-compatible URL paths for batch requests.
    /// These paths are validated during file upload and batch creation.
    pub allowed_url_paths: Vec<String>,
    /// Async request configuration.
    /// Controls whether the async UI is shown and which completion window is used for async requests.
    #[serde(default)]
    pub async_requests: AsyncRequestsConfig,
    /// Files configuration for batch file uploads/downloads
    pub files: FilesConfig,
    /// Default throughput (requests/second) for models without explicit throughput configured.
    /// Used for capacity calculations when accepting new batches.
    /// If not specified or null, defaults to 100.0 req/s. This is quite high, in favour of over-acceptance.
    /// Must be positive (> 0) when specified.
    #[serde(default = "default_batch_throughput", deserialize_with = "deserialize_positive_throughput")]
    pub default_throughput: f32,
    /// TTL for batch capacity reservations (seconds).
    /// Used to prevent stale reservations from reducing capacity forever.
    /// Must be positive (> 0). Setting this too low risks disabling the race guard.
    #[serde(
        default = "default_reservation_ttl_secs",
        deserialize_with = "deserialize_positive_reservation_ttl"
    )]
    pub reservation_ttl_secs: i64,
    /// Optional realtime priority decay window (seconds) for queue monitoring.
    /// When set, completed FLEX requests within this lookback are included
    /// in the 1h pending-request-counts bucket. When null or omitted, no decay
    /// count is applied.
    #[serde(default, deserialize_with = "deserialize_non_negative_optional_i64")]
    pub priority_decay_window_secs: Option<i64>,
    /// Upload-volume cap for *unverified* creditors, expressed as requests per
    /// hour of completion window. The effective cap for a submission scales with
    /// its completion window: `unverified_requests_per_completion_hour *
    /// window_hours` (e.g. 1000 for a 1h async window, 24000 for a 24h batch
    /// window). It bounds how much an unverified user can queue before they
    /// verify (add a payment method / make a payment), which removes the cap.
    ///
    /// Applies to both batch creation and flex/async request submission.
    /// Verified creditors are never limited. Set to 0 to disable the cap.
    /// Default: 1000.
    pub unverified_requests_per_completion_hour: usize,

    /// Include committed pending/claimed/processing requests in batch admission capacity checks.
    /// When false, admission capacity checks only include active in-flight reservations.
    /// Default: false.
    #[serde(default)]
    pub pending_capacity_counts_enabled: bool,
}

/// Configuration for the async requests feature.
///
/// Async requests provide a simplified UI for submitting individual requests
/// that are processed as batches with a fast completion window.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsyncRequestsConfig {
    /// Enable async requests UI and API functionality (default: true)
    pub enabled: bool,
    /// Completion window used for async requests (e.g., "1h").
    /// Must be present in the parent `allowed_completion_windows` list.
    /// Default: "1h"
    pub completion_window: String,
}

impl Default for AsyncRequestsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            completion_window: "1h".to_string(),
        }
    }
}

fn default_batch_throughput() -> f32 {
    100.0
}

fn default_reservation_ttl_secs() -> i64 {
    10 * 60
}

fn deserialize_relaxation_factors<'de, D>(deserializer: D) -> Result<HashMap<String, f32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let map: HashMap<String, f32> = HashMap::deserialize(deserializer)?;
    for (window, &factor) in &map {
        if !factor.is_finite() {
            return Err(D::Error::custom(format!(
                "window_relaxation_factors[{}] must be a finite number, got {}",
                window, factor
            )));
        }
        if factor < 0.0 {
            return Err(D::Error::custom(format!(
                "window_relaxation_factors[{}] must be >= 0.0, got {}",
                window, factor
            )));
        }
    }
    Ok(map)
}

impl BatchConfig {
    /// Get the relaxation factor for a completion window.
    /// Returns 1.0 if no explicit factor is configured (strict mode).
    pub fn relaxation_factor(&self, completion_window: &str) -> f32 {
        self.window_relaxation_factors.get(completion_window).copied().unwrap_or(1.0)
    }
}

fn deserialize_positive_reservation_ttl<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let opt: Option<i64> = Option::deserialize(deserializer)?;

    match opt {
        None => Ok(default_reservation_ttl_secs()),
        Some(value) if value <= 0 => Err(D::Error::custom(format!(
            "reservation_ttl_secs must be positive (> 0), got {}",
            value
        ))),
        Some(value) => Ok(value),
    }
}

fn deserialize_non_negative_optional_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let opt: Option<i64> = Option::deserialize(deserializer)?;
    match opt {
        Some(value) if value < 0 => Err(D::Error::custom(format!(
            "priority_decay_window_secs must be non-negative, got {}",
            value
        ))),
        value => Ok(value),
    }
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
            window_relaxation_factors: HashMap::new(),
            allowed_url_paths: vec![
                "/v1/chat/completions".to_string(),
                "/v1/completions".to_string(),
                "/v1/embeddings".to_string(),
                "/v1/responses".to_string(),
                "/v1/messages".to_string(),
            ],
            async_requests: AsyncRequestsConfig::default(),
            files: FilesConfig::default(),
            default_throughput: default_batch_throughput(),
            reservation_ttl_secs: default_reservation_ttl_secs(),
            priority_decay_window_secs: None,
            unverified_requests_per_completion_hour: 1000,
            pending_capacity_counts_enabled: false,
        }
    }
}

impl BatchConfig {
    /// Validate batch config consistency at startup.
    pub fn validate(&self) {
        if self.async_requests.enabled && !self.allowed_completion_windows.contains(&self.async_requests.completion_window) {
            tracing::error!(
                async_window = %self.async_requests.completion_window,
                allowed = ?self.allowed_completion_windows,
                "async_requests.completion_window is not in allowed_completion_windows — async requests will fail"
            );
        }
    }
}

/// Which fusillade claim loops should run inside this daemon process.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonMode {
    /// Run both batchless request claims and batch claims.
    #[default]
    Both,
    /// Run only the batchless request claim loop.
    RequestOnly,
    /// Run only the batch claim loop.
    BatchOnly,
}

impl From<DaemonMode> for fusillade::DaemonMode {
    fn from(mode: DaemonMode) -> Self {
        match mode {
            DaemonMode::Both => fusillade::DaemonMode::Both,
            DaemonMode::RequestOnly => fusillade::DaemonMode::RequestOnly,
            DaemonMode::BatchOnly => fusillade::DaemonMode::BatchOnly,
        }
    }
}

/// The daemon processes batch requests asynchronously in the background.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// When to run the daemon (default: "leader")
    /// - "always": Always run the daemon
    /// - "never": Never run the daemon
    /// - "leader": Only run if this instance is the leader
    pub enabled: DaemonEnabled,

    /// Which claim loops this daemon process should run (default: "both").
    /// - "both": Run request and batch claim loops
    /// - "request_only": Run only batchless request claims
    /// - "batch_only": Run only batch claims
    #[serde(default)]
    pub mode: DaemonMode,

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

    /// HTTP error statuses retried in addition to Fusillade's built-in predicate.
    /// Defaults to `[499]`; use `[]` to disable additional status retries.
    #[serde(default = "default_additional_retryable_statuses")]
    pub additional_retryable_statuses: Vec<u16>,

    /// Deprecated: use first_chunk_timeout_ms, chunk_timeout_ms, and body_timeout_ms instead.
    /// If set, splits into 90% first_chunk_timeout_ms and 10% body_timeout_ms.
    /// Ignored when the granular timeout fields are explicitly set.
    pub timeout_ms: Option<u64>,

    /// Timeout for receiving response headers (connect + time-to-first-token) in milliseconds.
    /// This should be generous enough to cover slow model inference starts.
    /// Default: 86,400,000 (24 hours).
    pub first_chunk_timeout_ms: u64,

    /// Timeout for receiving the next chunk of response body in milliseconds.
    /// Once the server starts streaming, each inter-chunk gap must be shorter
    /// than this value or the request is considered stalled.
    /// Default: 86,400,000 (24 hours).
    pub chunk_timeout_ms: u64,

    /// Timeout for the entire response body in milliseconds.
    /// Catches slow-drip responses that never trip the per-chunk timeout
    /// but take an unreasonable total time.
    /// Default: 86,400,000 (24 hours).
    pub body_timeout_ms: u64,

    /// Maximum time without progress while sending the request body, in milliseconds.
    /// This only covers upload; keep it below first_chunk_timeout_ms so an upload
    /// stall is reported before the broader first-response timeout. Default: 60,000.
    pub upload_stall_timeout_ms: u64,

    /// Request-body bytes per upload progress unit. Smaller values detect progress
    /// more finely at the cost of more body frames. Must be greater than zero.
    /// Default: 65,536 (64 KiB).
    pub upload_chunk_bytes: usize,

    /// How often the upload watchdog checks progress, in milliseconds. Keep this
    /// well below upload_stall_timeout_ms. Must be greater than zero. Default: 100.
    pub upload_stall_poll_ms: u64,

    /// Interval for logging daemon status (requests in flight) in milliseconds
    /// Set to None to disable periodic status logging (default: Some(2000))
    pub status_log_interval_ms: Option<u64>,

    /// Maximum time a request can stay in "claimed" state before being unclaimed
    /// and returned to pending (milliseconds). This handles daemon crashes. (default: 60000 = 1 minute)
    pub claim_timeout_ms: u64,

    /// Maximum time a request can stay in "processing" state before being unclaimed
    /// and returned to pending (milliseconds). This handles daemon crashes during execution. (default: 600000 = 10 minutes)
    pub processing_timeout_ms: u64,

    /// PostgreSQL statement timeout for pending request count queries (milliseconds).
    /// This bounds internal queue-depth monitoring work so a slow count query
    /// fails without accumulating behind callers' poll cadence. (default: 60000 = 1 minute)
    pub pending_request_counts_timeout_ms: u64,

    /// Per-model configurations for completion window escalation via route-at-claim-time.
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

    /// Request paths that should use SSE streaming for usage tracking.
    /// When a request's path matches, an `X-Fusillade-Stream` header is sent
    /// and the response is read as SSE, then reassembled into non-streaming JSON.
    /// Example: `["/v1/chat/completions", "/v1/completions"]`
    #[serde(default)]
    pub streamable_endpoints: Vec<String>,

    /// Weight controlling how much SLA urgency influences claim scheduling (0.0–1.0).
    /// Blends per-user fairness with batch deadline urgency when ordering claims.
    /// 0.0 = pure user-fairness, 1.0 = pure deadline urgency. Default: 0.5.
    #[serde(default = "default_urgency_weight", deserialize_with = "deserialize_urgency_weight")]
    pub urgency_weight: f64,

    /// When true, the daemon injects a deadline-derived priority hint into
    /// each outbound request body at `nvext.agent_hints.priority` (NVIDIA
    /// Dynamo's unified priority extension; `i32`, where higher values mean
    /// "more important" at the API layer and Dynamo normalizes per backend).
    /// The injected value is the negated Unix timestamp of the batch SLA
    /// deadline so earlier deadlines produce larger numbers. Default: false.
    #[serde(default)]
    pub inject_deadline_priority: bool,

    /// Maximum request rows the batch claim daemon takes per iteration.
    /// 0 inherits `claim_batch_size`, so an existing tuned cap carries over
    /// to the split batch daemon unchanged. Default: 0 (inherit).
    #[serde(default)]
    pub batch_claim_size: usize,

    /// Maximum batches selected per model per batch-claim iteration. Values
    /// above 1 let a model's leftover capacity spill into the next-ranked
    /// batches instead of idling when the top batch can't fill it.
    /// Default: 4.
    #[serde(default = "default_batch_claim_batch_size")]
    pub batch_claim_batch_size: usize,

    /// Sleep between batch-claim iterations in milliseconds.
    /// 0 inherits `claim_interval_ms`. Default: 0 (inherit).
    #[serde(default)]
    pub batch_claim_interval_ms: u64,

    /// Require an explicit `live` model_filters event before batch-claiming a
    /// model. When false, models with NO filter events (external / always-on
    /// providers that scouter does not manage) are treated as live — the
    /// historical claim behaviour. Not-live (`coming`/`absent`) models remain
    /// claimable only via the deadline ramp in either mode. Default: false.
    #[serde(default)]
    pub batch_claim_require_live: bool,

    /// Exponent of the deadline-ramp curve: batches on not-live models become
    /// claimable at full capacity within `window_minutes ^ exponent` minutes
    /// of their deadline (~59 min for 24h windows, ~10 min for 1h at the
    /// default). Values ≥ 1.0 make the ramp cover the whole window (batches
    /// claimable immediately regardless of liveness). Default: 0.56.
    #[serde(default = "default_claim_ramp_exponent", deserialize_with = "deserialize_claim_ramp_exponent")]
    pub claim_ramp_exponent: f64,

    /// Consecutive claim-cycle failures a claim loop tolerates (retrying with
    /// exponential backoff, capped at 30s) before it gives up and takes the
    /// daemon down. Transient DB blips no longer kill the daemon outright.
    /// Default: 10.
    #[serde(default = "default_claim_loop_max_consecutive_failures")]
    pub claim_loop_max_consecutive_failures: u32,

    /// Upper bound on a single claim-cycle database query in milliseconds — a
    /// deadness detector, not a performance guardrail. A connection severed
    /// silently (nothing delivered to the client) otherwise blocks the claim
    /// loop until TCP keepalive. On expiry the connection is dropped and the
    /// attempt counts as a transient claim failure for the retry machinery.
    /// Keep comfortably above any legitimate claim duration. Default: 180000
    /// (3 minutes).
    #[serde(default = "default_claim_query_timeout_ms")]
    pub claim_query_timeout_ms: u64,

    /// Batch-archive sweeper (fusillade phase 3): moves frozen terminal
    /// batches' request rows from `fusillade.requests` into the partitioned
    /// `batch_requests_archive`. OFF by default. The blue/green invariant:
    /// deploys never move data — only flipping this flag does, and only once
    /// every pod in the fleet runs location-routing-aware code AND this
    /// dwctl version is the rollback floor (deny_unknown_fields: an older
    /// pod parsing a config that contains these keys crash-loops).
    #[serde(default)]
    pub batch_archive_sweep_enabled: bool,
    /// Sweep tick interval. 0 disables the worker (guarded in fusillade).
    #[serde(default = "default_batch_archive_sweep_interval_ms")]
    pub batch_archive_sweep_interval_ms: u64,
    /// Bounded moves per sweep tick (never drain-until-empty).
    #[serde(default = "default_batch_archive_moves_per_tick")]
    pub batch_archive_sweep_moves_per_tick: i64,
    /// Post-freeze dwell before a batch becomes a sweep candidate. Default 0
    /// (move immediately — reads are mid-move safe by construction).
    #[serde(default, deserialize_with = "deserialize_non_negative_secs")]
    pub batch_archive_sweep_dwell_secs: f64,
    /// Cancellation grace: batches with canceled rows that were in flight at
    /// cancel are not archived while those rows are younger than this, so
    /// late billed results can still supersede the cancel on the live row.
    /// Default 600s, mirroring processing_timeout_ms.
    #[serde(
        default = "default_batch_archive_cancel_grace_secs",
        deserialize_with = "deserialize_cancel_grace_secs"
    )]
    pub batch_archive_cancel_grace_secs: f64,
    /// Historical backfill worker: same mover as the sweeper on its own
    /// pacing, oldest-first. Enable after the sweeper is live and steady;
    /// flip off to pause instantly (resumable by construction).
    #[serde(default)]
    pub batch_archive_backfill_enabled: bool,
    #[serde(default = "default_batch_archive_backfill_interval_ms")]
    pub batch_archive_backfill_interval_ms: u64,
    #[serde(default = "default_batch_archive_moves_per_tick")]
    pub batch_archive_backfill_moves_per_tick: i64,
    /// Weekly archive-partition runway maintained by the daemon's daily tick
    /// (runs regardless of the flags above so partitions exist before any
    /// flip; fusillade_archive_partitions_ahead gauges it).
    #[serde(default = "default_batch_archive_partitions_weeks_ahead")]
    pub batch_archive_partitions_weeks_ahead: i32,
}

fn default_batch_archive_sweep_interval_ms() -> u64 {
    5_000
}

fn default_batch_archive_moves_per_tick() -> i64 {
    4
}

fn default_batch_archive_cancel_grace_secs() -> f64 {
    600.0
}

fn default_batch_archive_backfill_interval_ms() -> u64 {
    1_000
}

fn default_batch_archive_partitions_weeks_ahead() -> i32 {
    4
}

fn default_claim_loop_max_consecutive_failures() -> u32 {
    10
}

fn default_additional_retryable_statuses() -> Vec<u16> {
    vec![499]
}

fn default_claim_query_timeout_ms() -> u64 {
    180_000
}

fn default_batch_claim_batch_size() -> usize {
    4
}

fn default_claim_ramp_exponent() -> f64 {
    0.56
}

fn default_urgency_weight() -> f64 {
    0.5
}

/// Custom deserializer that validates urgency_weight is in the 0.0–1.0 range and finite.
fn deserialize_urgency_weight<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let opt: Option<f64> = Option::deserialize(deserializer)?;

    match opt {
        None => Ok(default_urgency_weight()),
        Some(value) if !value.is_finite() => Err(D::Error::custom(format!("urgency_weight must be a finite number, got {}", value))),
        Some(value) if !(0.0..=1.0).contains(&value) => Err(D::Error::custom(format!(
            "urgency_weight must be between 0.0 and 1.0, got {}",
            value
        ))),
        Some(value) => Ok(value),
    }
}

/// Custom deserializer that validates claim_ramp_exponent is finite and non-negative.
/// (NaN/inf/negative exponents would make the deadline-ramp predicate undefined.)
fn deserialize_claim_ramp_exponent<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let opt: Option<f64> = Option::deserialize(deserializer)?;

    match opt {
        None => Ok(default_claim_ramp_exponent()),
        Some(value) if !value.is_finite() => Err(D::Error::custom(format!(
            "claim_ramp_exponent must be a finite number, got {}",
            value
        ))),
        Some(value) if value < 0.0 => Err(D::Error::custom(format!("claim_ramp_exponent must be non-negative, got {}", value))),
        Some(value) => Ok(value),
    }
}

/// Seconds knobs (archive dwell): finite and non-negative — NaN/inf/negative
/// values would make the SQL interval predicates undefined.
fn deserialize_non_negative_secs<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let opt: Option<f64> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(0.0),
        Some(value) if !value.is_finite() => Err(D::Error::custom(format!("seconds value must be a finite number, got {}", value))),
        Some(value) if value < 0.0 => Err(D::Error::custom(format!("seconds value must be non-negative, got {}", value))),
        Some(value) => Ok(value),
    }
}

/// Same validation as [`deserialize_non_negative_secs`] but defaulting to the
/// cancellation-grace default when the key is absent.
fn deserialize_cancel_grace_secs<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let opt: Option<f64> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(default_batch_archive_cancel_grace_secs()),
        Some(value) if !value.is_finite() => Err(D::Error::custom(format!(
            "batch_archive_cancel_grace_secs must be a finite number, got {}",
            value
        ))),
        Some(value) if value < 0.0 => Err(D::Error::custom(format!(
            "batch_archive_cancel_grace_secs must be non-negative, got {}",
            value
        ))),
        Some(value) => Ok(value),
    }
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
            mode: DaemonMode::Both,
            claim_batch_size: 100,
            default_model_concurrency: 10,
            claim_interval_ms: 1000,
            max_retries: Some(1000),
            stop_before_deadline_ms: Some(900_000),
            backoff_ms: 1000,
            backoff_factor: 2,
            max_backoff_ms: 10000,
            additional_retryable_statuses: default_additional_retryable_statuses(),
            timeout_ms: None,
            first_chunk_timeout_ms: 86_400_000,
            chunk_timeout_ms: 86_400_000,
            body_timeout_ms: 86_400_000,
            upload_stall_timeout_ms: 60_000,
            upload_chunk_bytes: 64 * 1024,
            upload_stall_poll_ms: 100,
            status_log_interval_ms: Some(2000),
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
            pending_request_counts_timeout_ms: 60000,
            batch_metadata_fields: default_batch_metadata_fields_dwctl(),
            model_escalations: HashMap::new(),
            purge_interval_ms: 600_000,
            purge_batch_size: 1000,
            purge_throttle_ms: 100,
            streamable_endpoints: Vec::new(),
            urgency_weight: default_urgency_weight(),
            inject_deadline_priority: false,
            batch_claim_size: 0,
            batch_claim_batch_size: default_batch_claim_batch_size(),
            batch_claim_interval_ms: 0,
            batch_claim_require_live: false,
            claim_ramp_exponent: default_claim_ramp_exponent(),
            claim_loop_max_consecutive_failures: default_claim_loop_max_consecutive_failures(),
            claim_query_timeout_ms: default_claim_query_timeout_ms(),
            batch_archive_sweep_enabled: false,
            batch_archive_sweep_interval_ms: default_batch_archive_sweep_interval_ms(),
            batch_archive_sweep_moves_per_tick: default_batch_archive_moves_per_tick(),
            batch_archive_sweep_dwell_secs: 0.0,
            batch_archive_cancel_grace_secs: default_batch_archive_cancel_grace_secs(),
            batch_archive_backfill_enabled: false,
            batch_archive_backfill_interval_ms: default_batch_archive_backfill_interval_ms(),
            batch_archive_backfill_moves_per_tick: default_batch_archive_moves_per_tick(),
            batch_archive_partitions_weeks_ahead: default_batch_archive_partitions_weeks_ahead(),
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
        // If the deprecated timeout_ms is set and the granular fields are at their
        // defaults, split it: 90% header (connect + TTFT), 10% body.
        let (first_chunk_timeout_ms, chunk_timeout_ms, body_timeout_ms) = if let Some(timeout) = self.timeout_ms {
            if self.first_chunk_timeout_ms == 86_400_000 && self.chunk_timeout_ms == 86_400_000 && self.body_timeout_ms == 86_400_000 {
                tracing::warn!(
                    timeout_ms = timeout,
                    "batch_daemon.timeout_ms is deprecated; \
                         use first_chunk_timeout_ms, chunk_timeout_ms, and body_timeout_ms instead"
                );
                (timeout * 9 / 10, 86_400_000, timeout / 10)
            } else {
                // Granular fields were explicitly set — ignore deprecated field
                (self.first_chunk_timeout_ms, self.chunk_timeout_ms, self.body_timeout_ms)
            }
        } else {
            (self.first_chunk_timeout_ms, self.chunk_timeout_ms, self.body_timeout_ms)
        };

        fusillade::daemon::DaemonConfig {
            mode: self.mode.into(),
            claim_batch_size: self.claim_batch_size,
            model_concurrency_limits: model_capacity_limits.unwrap_or_else(|| std::sync::Arc::new(dashmap::DashMap::new())),
            model_escalations: Arc::new(DashMap::from_iter(self.model_escalations.clone())),
            claim_interval_ms: self.claim_interval_ms,
            max_retries: self.max_retries,
            stop_before_deadline_ms: self.stop_before_deadline_ms,
            backoff_ms: self.backoff_ms,
            backoff_factor: self.backoff_factor,
            max_backoff_ms: self.max_backoff_ms,
            additional_retryable_statuses: self.additional_retryable_statuses.clone(),
            first_chunk_timeout_ms,
            chunk_timeout_ms,
            body_timeout_ms,
            upload_stall_timeout_ms: self.upload_stall_timeout_ms,
            upload_chunk_bytes: self.upload_chunk_bytes,
            upload_stall_poll_ms: self.upload_stall_poll_ms,
            status_log_interval_ms: self.status_log_interval_ms,
            claim_timeout_ms: self.claim_timeout_ms,
            processing_timeout_ms: self.processing_timeout_ms,
            pending_request_counts_timeout_ms: self.pending_request_counts_timeout_ms,
            batch_metadata_fields: self.batch_metadata_fields.clone(),
            purge_interval_ms: self.purge_interval_ms,
            purge_batch_size: self.purge_batch_size,
            purge_throttle_ms: self.purge_throttle_ms,
            streamable_endpoints: self.streamable_endpoints.clone(),
            urgency_weight: self.urgency_weight,
            inject_deadline_priority: self.inject_deadline_priority,
            batch_claim_size: self.batch_claim_size,
            batch_claim_batch_size: self.batch_claim_batch_size,
            batch_claim_interval_ms: self.batch_claim_interval_ms,
            batch_claim_require_live: self.batch_claim_require_live,
            claim_ramp_exponent: self.claim_ramp_exponent,
            claim_loop_max_consecutive_failures: self.claim_loop_max_consecutive_failures,
            claim_query_timeout_ms: self.claim_query_timeout_ms,
            batch_archive_sweep_enabled: self.batch_archive_sweep_enabled,
            batch_archive_sweep_interval_ms: self.batch_archive_sweep_interval_ms,
            batch_archive_sweep_moves_per_tick: self.batch_archive_sweep_moves_per_tick,
            batch_archive_sweep_dwell_secs: self.batch_archive_sweep_dwell_secs,
            batch_archive_cancel_grace_secs: self.batch_archive_cancel_grace_secs,
            batch_archive_backfill_enabled: self.batch_archive_backfill_enabled,
            batch_archive_backfill_interval_ms: self.batch_archive_backfill_interval_ms,
            batch_archive_backfill_moves_per_tick: self.batch_archive_backfill_moves_per_tick,
            batch_archive_partitions_weeks_ahead: self.batch_archive_partitions_weeks_ahead,
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
    /// Configuration for the usage-aggregate refresh daemon
    pub usage_refresh: UsageRefreshConfig,
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
    /// Configuration for connection sync workers (file ingestion, batch activation)
    pub sync_workers: SyncWorkersConfig,
    /// Worker counts for core batch task processing (always run, not gated by sync)
    pub task_workers: TaskWorkersConfig,
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
    /// Fallback sync interval in milliseconds (default: 300000ms = 5 minutes)
    ///
    /// Even when LISTEN/NOTIFY is working, this provides periodic full syncs to guarantee
    /// eventual consistency. Prevents issues from dropped notifications or connection problems.
    ///
    /// Each fallback tick triggers a FULL routing-table reload (all models, endpoints,
    /// secrets, and authorized keys), which is expensive in DB egress. Because LISTEN/NOTIFY
    /// already propagates real changes in real time, this only needs to be frequent enough to
    /// recover from a *missed* notification — minutes, not seconds.
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
            fallback_interval_milliseconds: 300_000, // 5 minutes (NOTIFY handles real changes; this is only a missed-notification safety net)
        }
    }
}

/// Usage-aggregate refresh daemon configuration.
///
/// The daemon incrementally folds new `http_analytics` rows into the
/// `user_model_usage_daily` rollup. It's woken by the analytics batcher after each
/// flush (in-process, no LISTEN/NOTIFY — emitter and consumer share the pod) and, as a
/// safety net, on a periodic fallback tick. Cross-pod duplicate runs are made cheap
/// no-ops by an advisory lock in the refresh itself.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageRefreshConfig {
    /// Enable the usage-refresh daemon (default: true).
    pub enabled: bool,
    /// Fallback tick interval in milliseconds (default: 60000ms = 1 minute).
    ///
    /// The batcher nudge drives the refresh in real time whenever there's traffic; this
    /// tick only backstops a missed nudge or drains the cursor after a restart. It is
    /// cheap — a tick with no new rows finds `MAX(id)` unchanged and no-ops. Set to `0`
    /// to disable the fallback entirely.
    pub fallback_interval_milliseconds: u64,
    /// Minimum interval between refreshes in milliseconds (default: 30000ms = 30s).
    ///
    /// Bounds refresh frequency and coalesces bursts of batcher nudges: after a refresh
    /// the daemon waits at least this long before running again.
    pub min_interval_milliseconds: u64,
}

impl Default for UsageRefreshConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fallback_interval_milliseconds: 60_000,
            min_interval_milliseconds: 30_000,
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
    /// First-payment match promotion: a user's first ever payment (checkout or
    /// auto-topup) is matched with bonus credits, up to this amount (in dollars).
    /// 0 disables the promotion. Eligibility is derived from the ledger (no prior
    /// `purchase`), so existing paying customers are never matched.
    #[serde(default)]
    pub first_payment_match_up_to: rust_decimal::Decimal,
}

impl Default for CreditsConfig {
    fn default() -> Self {
        Self {
            // Default to 0 credits (no credits given on creation)
            initial_credits_for_standard_users: rust_decimal::Decimal::ZERO,
            // Default to 0 (first-payment match promotion disabled)
            first_payment_match_up_to: rust_decimal::Decimal::ZERO,
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
    /// Deprecated and unused: balance depletion notifications are now
    /// edge-triggered (one per zero crossing, sent by the charging writers),
    /// which needs no global rate limit. Kept so existing config files still
    /// parse.
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

/// External data source connections configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectionsConfig {
    /// Encryption key for connection credentials (base64 or 32-byte string).
    /// Falls back to `secret_key` if not set.
    pub encryption_key: Option<String>,
    /// Sync pipeline configuration.
    pub sync: SyncPipelineConfig,
}

/// Configuration for the sync ingestion/activation pipeline.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncPipelineConfig {
    /// Default completion window for sync-created batches (default: "24h").
    pub default_completion_window: String,
    /// Default endpoint for sync-created batches (default: "/v1/chat/completions").
    pub default_endpoint: String,
}

impl Default for SyncPipelineConfig {
    fn default() -> Self {
        Self {
            default_completion_window: "24h".to_string(),
            default_endpoint: "/v1/chat/completions".to_string(),
        }
    }
}

/// Configuration for connection sync background workers.
///
/// Controls whether sync workers run on this instance and how many concurrent
/// workers process each stage of the pipeline.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncWorkersConfig {
    /// Enable sync workers on this instance (default: true).
    /// Set to false for API-only replicas that should not process sync jobs.
    /// Jobs are still enqueued to Postgres and picked up by other replicas.
    pub enabled: bool,
    /// Number of concurrent file ingestion workers (default: 4).
    /// Controls how many files are streamed from S3 and written to fusillade
    /// simultaneously. Higher values increase throughput but use more memory.
    pub ingest_workers: usize,
    /// Number of concurrent batch activation workers (default: 1).
    /// Kept low to avoid overwhelming capacity reservation checks.
    pub activate_workers: usize,
    /// Number of sync discovery workers (default: 1).
    /// Typically only one is needed since discovery is fast.
    #[serde(alias = "sync_workers")]
    pub discovery_workers: usize,
}

impl Default for SyncWorkersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ingest_workers: 4,
            activate_workers: 1,
            discovery_workers: 1,
        }
    }
}

/// Worker counts for core batch task processing.
///
/// These workers always run regardless of the sync `enabled` flag — they
/// handle API-triggered work (batch population, state cascade on cancel).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskWorkersConfig {
    /// Number of batch population workers (default: 1).
    /// Populates requests from templates after a batch is created.
    pub create_batch_workers: usize,
    /// Number of cascade-batch-state workers (default: 1).
    /// Updates child request states after a batch is cancelled or deleted.
    pub cascade_batch_state_workers: usize,
    /// Number of purge-user-data workers (default: 1, minimum: 1).
    /// Erases a deleted user's fusillade data (batches, files, requests).
    pub purge_user_data_workers: usize,
    /// Maximum records per flush in the in-process responses writer
    /// (default: 100). Larger values amortise commit overhead across the
    /// batch; smaller values reduce per-record latency from outlet send
    /// to row visible. Replaces the previous underway-based
    /// `response_workers` setting.
    pub response_writer_batch_size: usize,
}

impl Default for TaskWorkersConfig {
    fn default() -> Self {
        Self {
            create_batch_workers: 1,
            cascade_batch_state_workers: 1,
            purge_user_data_workers: 1,
            response_writer_batch_size: 100,
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
            onwards: OnwardsConfig::default(),
            onboarding_url: None,
            support_email: "support@doubleword.ai".to_string(),
            connections: ConnectionsConfig::default(),
            responses: ResponsesConfig::default(),
            image_normalizer: crate::image_normalizer::ImageNormalizerConfig::default(),
            keystore: None,
            openapi: OpenApiConfig::default(),
            cache: CacheConfig::default(),
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
            cookie_domain: None,
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
            headers: SecurityHeadersConfig::default(),
        }
    }
}

impl Default for SecurityHeadersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            frame_options: "DENY".to_string(),
            referrer_policy: "strict-origin-when-cross-origin".to_string(),
            permissions_policy:
                "accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()".to_string(),
            // Opt-in: empty means the header is not sent.
            content_security_policy: String::new(),
            content_security_policy_report_only: String::new(),
            strict_transport_security: String::new(),
        }
    }
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec![
                CorsOrigin::Url(Url::parse("http://localhost:3001").unwrap()), // Development frontend (Vite)
            ],
            allow_credentials: true,
            max_age: Some(3600), // Cache preflight for 1 hour
            exposed_headers: vec!["location".to_string()],
            allow_any_origin_without_credentials: false,
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
            templates_dir: None,
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
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, figment::Error> {
        Self::load(&Args {
            config: path.as_ref().to_path_buf(),
            validate: false,
        })
    }

    #[allow(clippy::result_large_err)]
    pub fn load(args: &Args) -> Result<Self, figment::Error> {
        let mut config: Self = Self::figment(args).extract()?;

        // if database_url is set, use it (preserving existing pool and component settings)
        if let Some(url) = config.database_url.take() {
            let pool = config.database.main_pool_settings().clone();
            let fusillade = config.database.fusillade().clone();
            let outlet = config.database.outlet().clone();
            let underway_pool = config.database.underway_pool_settings().clone();

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
                underway_pool,
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

        // Normalize empty cookie_domain to None (allows env var override with "" to clear it)
        if config.auth.native.session.cookie_domain.as_deref() == Some("") {
            config.auth.native.session.cookie_domain = None;
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

        // Cached-input pricing needs a tokenizer-svc URL to count cache-prefix tokens.
        // Without it, every cacheable request silently degrades to no caching — fail fast
        // at startup instead, so an operator who flips the flag gets a clear error.
        if self.cache.enabled && self.cache.tokenizer_url.trim().is_empty() {
            return Err(Error::Internal {
                operation: "Config validation: cache.enabled is true but cache.tokenizer_url is empty. \
                     Set DWCTL_CACHE__TOKENIZER_URL to the tokenizer-svc base URL, or disable caching."
                    .to_string(),
            });
        }

        // Cache TTL tiers: every enabled tier must be a known tier (5m/1h/24h), the set must be
        // non-empty, and the default tier must be one of them — otherwise a no-ttl marker would
        // default straight into a rejected tier. Fail fast at startup with a clear message.
        if self.cache.enabled {
            for ttl in &self.cache.enabled_ttls {
                if crate::prompt_cache::TtlTier::parse(ttl).is_none() {
                    return Err(Error::Internal {
                        operation: format!(
                            "Config validation: cache.enabled_ttls contains an unknown tier {ttl:?}; allowed values are \"5m\", \"1h\", \"24h\"."
                        ),
                    });
                }
            }
            if self.cache.enabled_ttls.is_empty() {
                return Err(Error::Internal {
                    operation: "Config validation: cache.enabled_ttls is empty; enable at least one tier (\"5m\", \"1h\", \"24h\")."
                        .to_string(),
                });
            }
            if !self.cache.enabled_ttls.iter().any(|t| t == &self.cache.default_ttl) {
                return Err(Error::Internal {
                    operation: format!(
                        "Config validation: cache.default_ttl {:?} is not in cache.enabled_ttls {:?}.",
                        self.cache.default_ttl, self.cache.enabled_ttls
                    ),
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

        // Validate cookie_domain if set — must produce a valid Set-Cookie header fragment
        if let Some(ref domain) = self.auth.native.session.cookie_domain {
            let invalid = domain.is_empty() || domain.chars().any(|c| c.is_whitespace() || c.is_control()) || domain.contains(';');
            if invalid {
                return Err(Error::Internal {
                    operation: format!(
                        "Config validation: Invalid cookie_domain '{domain}'. \
                         Must not be empty or contain semicolons, whitespace, or control characters."
                    ),
                });
            }
            // Verify the resulting fragment is a valid HTTP header value
            let fragment = format!("; Domain={domain}");
            if axum::http::HeaderValue::from_str(&fragment).is_err() {
                return Err(Error::Internal {
                    operation: format!("Config validation: cookie_domain '{domain}' produces an invalid HTTP header value."),
                });
            }
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
            let unknown_windows: Vec<&str> = self
                .batches
                .window_relaxation_factors
                .keys()
                .filter(|w| !self.batches.allowed_completion_windows.contains(w))
                .map(|w| w.as_str())
                .collect();

            if !unknown_windows.is_empty() {
                return Err(Error::Internal {
                    operation: format!(
                        "Config validation: window_relaxation_factors contains window(s) not in \
                        allowed_completion_windows: {}. Add them to allowed_completion_windows or \
                        remove them from window_relaxation_factors.",
                        unknown_windows.join(", ")
                    ),
                });
            }

            if self.batches.allowed_url_paths.is_empty() {
                return Err(Error::Internal {
                    operation: "Config validation: batches.allowed_url_paths cannot be empty. Add at least one supported URL path."
                        .to_string(),
                });
            }

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
        let config_path: OsString = args.config.as_os_str().to_owned();
        Figment::new()
            // Load base config file
            .merge(Yaml::file(config_path))
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
                config: "test.yaml".into(),
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
    fn cors_allow_any_origin_without_credentials_parses() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: hello
auth:
  security:
    cors:
      allowed_origins:
        - "https://app.doubleword.ai"
      allow_credentials: true
      allow_any_origin_without_credentials: true
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;

            assert!(config.auth.security.cors.allow_any_origin_without_credentials);
            assert!(config.auth.security.cors.allow_credentials);

            Ok(())
        });
    }

    #[test]
    fn cors_allow_any_origin_without_credentials_defaults_false() {
        Jail::expect_with(|jail| {
            jail.create_file("test.yaml", "secret_key: hello\n")?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;

            assert!(!config.auth.security.cors.allow_any_origin_without_credentials);

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
                config: "test.yaml".into(),
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
                config: "test.yaml".into(),
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
                config: "test.yaml".into(),
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
                config: "test.yaml".into(),
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

    /// Helper: a config that passes validation up to the cache checks, with caching on.
    fn cache_test_config() -> Config {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());
        config.cache.enabled = true;
        config
    }

    #[test]
    fn test_cache_tiers_not_validated_when_disabled() {
        let mut config = cache_test_config();
        config.cache.enabled = false; // disabled → tier config is not validated
        config.cache.enabled_ttls = vec!["99h".to_string()]; // bogus, but ignored
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_valid_tiers_ok() {
        let mut config = cache_test_config();
        config.cache.enabled_ttls = vec!["5m".to_string(), "1h".to_string()];
        config.cache.default_ttl = "5m".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_unknown_tier_rejected() {
        let mut config = cache_test_config();
        config.cache.enabled_ttls = vec!["5m".to_string(), "99h".to_string()];
        config.cache.default_ttl = "5m".to_string();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("unknown tier"), "{err}");
    }

    #[test]
    fn test_cache_empty_tiers_rejected() {
        let mut config = cache_test_config();
        config.cache.enabled_ttls = vec![];
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("enabled_ttls is empty"), "{err}");
    }

    #[test]
    fn test_cache_default_ttl_must_be_enabled() {
        let mut config = cache_test_config();
        config.cache.enabled_ttls = vec!["5m".to_string()];
        config.cache.default_ttl = "1h".to_string(); // not in enabled_ttls
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("default_ttl"), "{err}");
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
                config: "test.yaml".into(),
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
                config: "test.yaml".into(),
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
                config: "test.yaml".into(),
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
                config: "test.yaml".into(),
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
                config: "test.yaml".into(),
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
                config: "test.yaml".into(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.batches.default_throughput, 75.5);

            Ok(())
        });
    }

    #[test]
    fn test_reservation_ttl_zero_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  reservation_ttl_secs: 0
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("reservation_ttl_secs must be positive"));

            Ok(())
        });
    }

    #[test]
    fn test_reservation_ttl_negative_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  reservation_ttl_secs: -60
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("reservation_ttl_secs must be positive"));

            Ok(())
        });
    }

    #[test]
    fn test_reservation_ttl_null_uses_default() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  reservation_ttl_secs: null
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            assert_eq!(config.batches.reservation_ttl_secs, 600);

            Ok(())
        });
    }

    #[test]
    fn test_reservation_ttl_default() {
        let config = Config::default();
        assert_eq!(config.batches.reservation_ttl_secs, 600);
    }

    #[test]
    fn test_priority_decay_window_default_disabled() {
        let config = Config::default();
        assert_eq!(config.batches.priority_decay_window_secs, None);
    }

    #[test]
    fn test_pending_capacity_counts_default_disabled() {
        let config = Config::default();
        assert!(!config.batches.pending_capacity_counts_enabled);
    }

    #[test]
    fn test_priority_decay_window_explicit_value() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  priority_decay_window_secs: 600
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            assert_eq!(config.batches.priority_decay_window_secs, Some(600));

            Ok(())
        });
    }

    #[test]
    fn test_priority_decay_window_negative_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  priority_decay_window_secs: -1
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("priority_decay_window_secs"));

            Ok(())
        });
    }

    #[test]
    fn test_relaxation_factor_defaults_to_one() {
        let config = Config::default();
        assert_eq!(config.batches.relaxation_factor("1h"), 1.0);
        assert_eq!(config.batches.relaxation_factor("24h"), 1.0);
    }

    #[test]
    fn test_relaxation_factor_explicit_value() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  allowed_completion_windows: ["1h", "24h"]
  window_relaxation_factors:
    "1h": 1.0
    "24h": 1.5
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            assert_eq!(config.batches.relaxation_factor("1h"), 1.0);
            assert_eq!(config.batches.relaxation_factor("24h"), 1.5);
            Ok(())
        });
    }

    #[test]
    fn test_relaxation_factor_unknown_window_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  allowed_completion_windows: ["1h", "24h"]
  window_relaxation_factors:
    "12h": 1.5
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("12h"));
            Ok(())
        });
    }

    #[test]
    fn test_relaxation_factor_negative_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  allowed_completion_windows: ["1h", "24h"]
  window_relaxation_factors:
    "24h": -0.5
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("window_relaxation_factors"));
            Ok(())
        });
    }

    #[test]
    fn test_relaxation_factor_zero_allowed() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  allowed_completion_windows: ["1h", "24h"]
  window_relaxation_factors:
    "1h": 0.0
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            assert_eq!(config.batches.relaxation_factor("1h"), 0.0);
            Ok(())
        });
    }

    #[test]
    fn test_relaxation_factor_empty_map_backwards_compatible() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
batches:
  allowed_completion_windows: ["1h", "24h"]
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            // No relaxation_factors key — all windows default to 1.0
            assert_eq!(config.batches.relaxation_factor("1h"), 1.0);
            assert_eq!(config.batches.relaxation_factor("24h"), 1.0);
            Ok(())
        });
    }

    #[test]
    fn test_empty_cookie_domain_env_override_normalized_to_none() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
auth:
  native:
    session:
      cookie_domain: ".doubleword.ai"
"#,
            )?;

            // Staging overrides cookie_domain with empty string to clear it
            jail.set_env("DWCTL_AUTH__NATIVE__SESSION__COOKIE_DOMAIN", "");

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.auth.native.session.cookie_domain, None);

            Ok(())
        });
    }

    #[test]
    fn test_additional_retryable_statuses_default_override_and_mapping() {
        Jail::expect_with(|jail| {
            jail.create_file("test.yaml", "secret_key: test-secret-key\n")?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.additional_retryable_statuses, vec![499]);
            assert_eq!(
                config
                    .background_services
                    .batch_daemon
                    .to_fusillade_config()
                    .additional_retryable_statuses,
                vec![499]
            );

            jail.create_file(
                "test.yaml",
                r#"
secret_key: test-secret-key
background_services:
  batch_daemon:
    additional_retryable_statuses: []
"#,
            )?;
            let config = Config::load(&args)?;
            assert!(config.background_services.batch_daemon.additional_retryable_statuses.is_empty());
            assert!(
                config
                    .background_services
                    .batch_daemon
                    .to_fusillade_config()
                    .additional_retryable_statuses
                    .is_empty()
            );

            Ok(())
        });
    }

    #[test]
    fn test_urgency_weight_default() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.urgency_weight, 0.5);

            Ok(())
        });
    }

    #[test]
    fn test_urgency_weight_yaml_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    urgency_weight: 0.8
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.urgency_weight, 0.8);

            Ok(())
        });
    }

    #[test]
    fn test_urgency_weight_negative_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    urgency_weight: -0.1
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };

            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("urgency_weight must be between 0.0 and 1.0"));

            Ok(())
        });
    }

    #[test]
    fn test_urgency_weight_above_one_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    urgency_weight: 1.5
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };

            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("urgency_weight must be between 0.0 and 1.0"));

            Ok(())
        });
    }

    #[test]
    fn test_daemon_mode_default_and_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.mode, DaemonMode::Both);
            assert_eq!(
                fusillade::DaemonMode::from(config.background_services.batch_daemon.mode),
                fusillade::DaemonMode::Both
            );

            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    mode: request_only
"#,
            )?;
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.mode, DaemonMode::RequestOnly);
            assert_eq!(
                fusillade::DaemonMode::from(config.background_services.batch_daemon.mode),
                fusillade::DaemonMode::RequestOnly
            );

            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    mode: batch_only
"#,
            )?;
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.mode, DaemonMode::BatchOnly);
            assert_eq!(
                fusillade::DaemonMode::from(config.background_services.batch_daemon.mode),
                fusillade::DaemonMode::BatchOnly
            );

            Ok(())
        });
    }

    #[test]
    fn test_upload_watchdog_defaults_override_and_mapping() {
        Jail::expect_with(|jail| {
            jail.create_file("test.yaml", "secret_key: test-secret-key\n")?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };

            let config = Config::load(&args)?;
            let daemon = &config.background_services.batch_daemon;
            assert_eq!(daemon.upload_stall_timeout_ms, 60_000);
            assert_eq!(daemon.upload_chunk_bytes, 64 * 1024);
            assert_eq!(daemon.upload_stall_poll_ms, 100);

            jail.create_file(
                "test.yaml",
                r#"
secret_key: test-secret-key
background_services:
  batch_daemon:
    upload_stall_timeout_ms: 30000
    upload_chunk_bytes: 8192
    upload_stall_poll_ms: 25
"#,
            )?;
            let config = Config::load(&args)?;
            let daemon = &config.background_services.batch_daemon;
            assert_eq!(daemon.upload_stall_timeout_ms, 30_000);
            assert_eq!(daemon.upload_chunk_bytes, 8 * 1024);
            assert_eq!(daemon.upload_stall_poll_ms, 25);

            let fusillade = daemon.to_fusillade_config();
            assert_eq!(fusillade.upload_stall_timeout_ms, 30_000);
            assert_eq!(fusillade.upload_chunk_bytes, 8 * 1024);
            assert_eq!(fusillade.upload_stall_poll_ms, 25);

            Ok(())
        });
    }

    #[test]
    fn test_claim_ramp_exponent_default_and_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.claim_ramp_exponent, 0.56);

            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    claim_ramp_exponent: 0.9
"#,
            )?;
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.claim_ramp_exponent, 0.9);

            Ok(())
        });
    }

    #[test]
    fn test_claim_loop_max_consecutive_failures_default_and_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.claim_loop_max_consecutive_failures, 10);

            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    claim_loop_max_consecutive_failures: 3
"#,
            )?;
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.claim_loop_max_consecutive_failures, 3);

            Ok(())
        });
    }

    #[test]
    fn test_claim_query_timeout_default_and_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.claim_query_timeout_ms, 180_000);

            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    claim_query_timeout_ms: 60000
"#,
            )?;
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.claim_query_timeout_ms, 60_000);

            Ok(())
        });
    }

    #[test]
    fn test_claim_ramp_exponent_negative_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    claim_ramp_exponent: -0.5
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("claim_ramp_exponent must be non-negative"));

            Ok(())
        });
    }

    #[test]
    fn test_claim_ramp_exponent_non_finite_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    claim_ramp_exponent: .nan
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("claim_ramp_exponent must be a finite number") || err.contains("invalid"),
                "unexpected error: {err}"
            );

            Ok(())
        });
    }

    #[test]
    fn test_urgency_weight_null_uses_default() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    urgency_weight: null
"#,
            )?;

            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };

            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.urgency_weight, 0.5);

            Ok(())
        });
    }

    #[test]
    fn test_batch_archive_cancel_grace_secs_negative_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    batch_archive_cancel_grace_secs: -1.0
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("batch_archive_cancel_grace_secs must be non-negative"),
                "unexpected error: {err}"
            );

            Ok(())
        });
    }

    #[test]
    fn test_batch_archive_cancel_grace_secs_non_finite_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    batch_archive_cancel_grace_secs: .inf
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("batch_archive_cancel_grace_secs must be a finite number") || err.contains("invalid"),
                "unexpected error: {err}"
            );

            Ok(())
        });
    }

    #[test]
    fn test_batch_archive_cancel_grace_secs_null_uses_default() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    batch_archive_cancel_grace_secs: null
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let config = Config::load(&args)?;
            assert_eq!(config.background_services.batch_daemon.batch_archive_cancel_grace_secs, 600.0);

            Ok(())
        });
    }

    #[test]
    fn test_batch_archive_sweep_dwell_secs_negative_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    batch_archive_sweep_dwell_secs: -0.001
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("seconds value must be non-negative"), "unexpected error: {err}");

            Ok(())
        });
    }

    #[test]
    fn test_batch_archive_sweep_dwell_secs_non_finite_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key"
background_services:
  batch_daemon:
    batch_archive_sweep_dwell_secs: .nan
"#,
            )?;
            let args = Args {
                config: "test.yaml".into(),
                validate: false,
            };
            let result = Config::load(&args);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("seconds value must be a finite number") || err.contains("invalid"),
                "unexpected error: {err}"
            );

            Ok(())
        });
    }
}
