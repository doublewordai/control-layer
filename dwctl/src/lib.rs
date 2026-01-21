//! # dwctl: Control Layer for AI Model Management
//!
//! `dwctl` is a comprehensive control plane for managing AI model deployments, access control,
//! and request routing. It provides a RESTful API for managing users, groups, deployments,
//! and access policies, along with an OpenAI-compatible proxy for routing AI requests.
//!
//! ## Overview
//!
//! `dwctl` acts as a centralized control plane sitting between AI model consumers and multiple
//! AI inference endpoints. Organizations deploying AI models face challenges around multi-tenancy,
//! cost tracking, and managing access to diverse model deployments. This crate addresses these
//! challenges by providing a unified control layer that handles authentication, authorization,
//! request routing, and observability.
//!
//! The system is designed for platforms that need to expose AI capabilities to multiple users or
//! teams while maintaining isolation, tracking usage, and ensuring high availability. It's
//! particularly suited for organizations running their own inference infrastructure or aggregating
//! multiple AI providers behind a single interface.
//!
//! ### What It Does
//!
//! At its core, `dwctl` receives requests from clients using the OpenAI-compatible API format,
//! authenticates the user, checks their permissions against the requested model, routes the
//! request to the appropriate inference endpoint, and optionally logs the request/response for
//! audit and analytics. It manages a credit system for usage tracking and rate limiting, monitors
//! endpoint health to remove failing backends from rotation, and supports batch processing for
//! asynchronous workloads.
//!
//! ## Architecture
//!
//! The application is built on [Axum](https://github.com/tokio-rs/axum) for the HTTP layer and
//! uses PostgreSQL for all persistence needs. It can operate with either an embedded PostgreSQL
//! instance (useful for development) or an external PostgreSQL database (recommended for production).
//!
//! ### Request Flow
//!
//! The application handles two distinct request flows depending on the endpoint accessed.
//!
//! #### AI Proxy Requests (`/ai/v1/*`)
//!
//! When a client makes a request to `/ai/v1/chat/completions`, the request is handled by the
//! [onwards] routing layer. The system maintains a synchronized cache of valid API keys for each
//! model deployment—only keys with sufficient credits and appropriate group access are included.
//! This cache is continuously updated via PostgreSQL LISTEN/NOTIFY whenever database state changes.
//! onwards validates the incoming API key against this cache, maps the model alias to an inference
//! endpoint, and forwards the request. Optional middleware powered by [outlet] and [outlet-postgres]
//! can log request and response data to PostgreSQL for auditing, and credits are deducted based on
//! token usage.
//!
//! #### Management API Requests (`/admin/api/v1/*`)
//!
//! Requests to the management API follow a traditional web application flow. The request first
//! passes through authentication middleware that validates credentials through multiple methods:
//! session cookies (for browser clients), trusted proxy headers (for SSO integration), or API keys
//! with "platform" purpose. The authentication system tries these methods in priority order, falling
//! back to the next if one is unavailable or invalid. Once authenticated, the request reaches the
//! appropriate handler which performs authorization checks based on the user's roles and permissions.
//! Handlers interact with the database through repository interfaces to perform CRUD operations on
//! resources like users, groups, deployments, and endpoints. Changes to deployments or API keys
//! trigger PostgreSQL NOTIFY events that update the [onwards] routing cache in real-time.
//!
//! [onwards]: https://github.com/doublewordai/onwards
//! [outlet]: https://github.com/doublewordai/outlet
//! [outlet-postgres]: https://github.com/doublewordai/outlet-postgres
//!
//! ### Core Components
//!
//! The **API layer** ([`api`]) exposes two main surfaces: a management API for administrators at
//! `/admin/api/v1/*` and an OpenAI-compatible proxy at `/ai/v1/*`. The management API uses RESTful
//! conventions for CRUD operations on users, groups, deployments, and other resources, while the
//! proxy API mimics OpenAI's interface to maximize compatibility with existing clients.
//!
//! The **authentication layer** ([`auth`]) handles session-based authentication for the management
//! API and can integrate with SSO proxy implementations for federated authentication. It includes
//! permission checking logic and role-based access control for administrative operations. The AI
//! proxy endpoints use a separate authentication mechanism where valid API keys are synced to
//! [onwards].
//!
//! The **database layer** ([`db`]) uses the repository pattern to abstract data access. Each entity
//! (users, groups, deployments, etc.) has a corresponding repository that handles queries and
//! mutations. The schema uses PostgreSQL features like advisory locks for leader election and
//! LISTEN/NOTIFY for real-time configuration updates.
//!
//! **Background services** run alongside the HTTP server and include a health probe scheduler that
//! periodically checks inference endpoint availability, a batch processing daemon powered by
//! fusillade for async job execution, and an [onwards] configuration sync process that watches for
//! database changes and updates the routing layer in real-time.
//!
//! ## Quick Start
//!
//! ```no_run
//! use clap::Parser;
//! use dwctl::{Application, Config};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Parse CLI arguments and load configuration
//!     let args = dwctl::config::Args::parse();
//!     let config = Config::load(&args)?;
//!
//!     // Initialize telemetry (structured logging and optional OpenTelemetry)
//!     dwctl::telemetry::init_telemetry(config.enable_otel_export)?;
//!
//!     // Create and start the application
//!     let app = Application::new(config).await?;
//!
//!     // Run with graceful shutdown on Ctrl+C
//!     app.serve(async {
//!         tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
//!     }).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Database Setup
//!
//! The application requires a PostgreSQL database and automatically runs migrations on startup:
//!
//! ```no_run
//! # use sqlx::PgPool;
//! # async fn example(pool: PgPool) -> Result<(), sqlx::Error> {
//! // Run migrations
//! dwctl::migrator().run(&pool).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Configuration
//!
//! See the [`config`] module for configuration options.
//!
// TODO: This file has gotten way too big. We need to refactor it into smaller modules.
// The constructors in test_utils should be unified with the actual constructors: right now they're
// actually the best lib way to construct things, which is bad.
pub mod api;
pub mod auth;
pub mod config;
mod crypto;
pub mod db;
mod email;
mod error_enrichment;
pub mod errors;
mod leader_election;
mod metrics;
mod openapi;
mod payment_providers;
mod probes;
mod request_logging;
pub mod sample_files;
mod static_assets;
mod sync;
pub mod telemetry;
mod types;

// Test modules
#[cfg(test)]
mod test;

use crate::{
    api::models::{
        deployments::{DeployedModelCreate, StandardModelCreate},
        users::Role,
    },
    auth::password,
    config::CorsOrigin,
    db::handlers::{Deployments, Groups, Repository, Users},
    db::models::{deployments::DeploymentCreateDBRequest, users::UserCreateDBRequest},
    metrics::GenAiMetrics,
    openapi::{AdminApiDoc, AiApiDoc},
    request_logging::serializers::{AnalyticsResponseSerializer, parse_ai_request},
};
use anyhow::Context;
use auth::middleware::admin_ai_proxy_middleware;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderValue;
use axum::{
    Router, ServiceExt, http, middleware,
    routing::{delete, get, patch, post},
};
use axum_prometheus::PrometheusMetricLayerBuilder;
use bon::Builder;
pub use config::Config;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use outlet::{RequestLoggerConfig, RequestLoggerLayer};
use outlet_postgres::PostgresHandler;
use request_logging::{AiResponse, ParsedAIRequest};
use sqlx::{Executor, PgPool};
use std::sync::{Arc, OnceLock};
use tokio::net::TcpListener;
use tower::Layer;
use tower_http::{
    cors::CorsLayer,
    trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use tracing::{Level, debug, info, instrument};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};
use uuid::Uuid;

pub use types::{ApiKeyId, DeploymentId, GroupId, InferenceEndpointId, UserId};

/// Application state shared across all request handlers.
///
/// This struct contains all the shared resources needed by the API handlers,
/// including database connections, configuration, and background service managers.
///
/// # Fields
///
/// - `db`: Main PostgreSQL connection pool for application data
/// - `config`: Application configuration loaded from environment/files
/// - `outlet_db`: Optional connection pool for request logging (when enabled)
/// - `metrics_recorder`: Optional Prometheus metrics recorder (when enabled)
/// - `is_leader`: Whether this instance is the elected leader (for distributed deployments)
/// - `request_manager`: Fusillade batch request manager for async processing
///
/// # Example
///
/// ```ignore
/// let state = AppState::builder()
///     .db(db_pools)
///     .config(config)
///     .request_manager(request_manager)
///     .build();
/// ```
#[derive(Clone, Builder)]
pub struct AppState {
    /// Database pools (primary + optional replica).
    /// Implements `Deref<Target = PgPool>` for backwards compatibility.
    /// Use `.read()` for read-only queries, `.write()` for writes.
    pub db: db::DbPools,
    pub config: Config,
    pub outlet_db: Option<PgPool>,
    pub metrics_recorder: Option<GenAiMetrics>,
    #[builder(default = false)]
    pub is_leader: bool,
    pub request_manager: Arc<fusillade::PostgresRequestManager<fusillade::ReqwestHttpClient>>,
}

/// Get the dwctl database migrator
pub fn migrator() -> sqlx::migrate::Migrator {
    sqlx::migrate!("./migrations")
}

/// Global Prometheus handle - ensures recorder is only installed once
static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Get or install the Prometheus metrics recorder.
///
/// This function is idempotent - the first call installs the global recorder,
/// subsequent calls return the existing handle. This allows both production code
/// (which calls early for background service metrics) and tests (which may call
/// later via build_router) to work correctly.
fn get_or_install_prometheus_handle() -> PrometheusHandle {
    PROMETHEUS_HANDLE
        .get_or_init(|| {
            // Custom histogram buckets for analytics lag (100ms to 10 minutes)
            const ANALYTICS_LAG_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0];

            // Custom histogram buckets for cache sync lag (1ms to 10s)
            const CACHE_SYNC_LAG_BUCKETS: &[f64] = &[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

            // Custom histogram buckets for fusillade retry attempts (0-10 retries)
            const RETRY_ATTEMPTS_BUCKETS: &[f64] = &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];

            PrometheusBuilder::new()
                .set_buckets_for_metric(Matcher::Full("dwctl_analytics_lag_seconds".to_string()), ANALYTICS_LAG_BUCKETS)
                .expect("Failed to set custom buckets for dwctl_analytics_lag_seconds")
                .set_buckets_for_metric(Matcher::Full("dwctl_cache_sync_lag_seconds".to_string()), CACHE_SYNC_LAG_BUCKETS)
                .expect("Failed to set custom buckets for dwctl_cache_sync_lag_seconds")
                .set_buckets_for_metric(
                    Matcher::Full("fusillade_retry_attempts_on_success".to_string()),
                    RETRY_ATTEMPTS_BUCKETS,
                )
                .expect("Failed to set custom buckets for fusillade_retry_attempts_on_success")
                .install_recorder()
                .expect("Failed to install Prometheus recorder")
        })
        .clone()
}

/// Create the initial admin user if it doesn't exist.
///
/// This function is idempotent - it will create a new admin user if one doesn't exist,
/// or update the password if the user already exists. This is typically called during
/// application startup to ensure there's always an admin user available.
///
/// # Arguments
///
/// - `email`: Email address for the admin user (also used as username)
/// - `password`: Optional password. If `None`, the user will have no password set
/// - `db`: PostgreSQL connection pool
///
/// # Returns
///
/// Returns the user ID of the created or existing admin user.
///
/// # Errors
///
/// Returns an error if database operations fail.
///
/// # Example
///
/// ```no_run
/// # use dwctl::create_initial_admin_user;
/// # use sqlx::PgPool;
/// # use dwctl::auth::password::Argon2Params;
/// # async fn example(pool: PgPool) -> Result<(), sqlx::Error> {
/// let user_id = create_initial_admin_user(
///     "admin@example.com",
///     Some("secure_password"),
///     Argon2Params::default(),
///     &pool
/// ).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip_all)]
pub async fn create_initial_admin_user(
    email: &str,
    password: Option<&str>,
    argon2_params: password::Argon2Params,
    db: &PgPool,
) -> Result<UserId, sqlx::Error> {
    // Hash password if provided
    let password_hash = if let Some(pwd) = password {
        Some(
            password::hash_string_with_params(pwd, Some(argon2_params))
                .map_err(|e| sqlx::Error::Encode(format!("Failed to hash admin password: {e}").into()))?,
        )
    } else {
        None
    };

    // Use a transaction to ensure atomicity
    let mut tx = db.begin().await?;
    let mut user_repo = Users::new(&mut tx);

    // Check if user already exists
    if let Some(existing_user) = user_repo
        .get_user_by_email(email)
        .await
        .map_err(|e| sqlx::Error::Protocol(format!("Failed to check existing user: {e}")))?
    {
        // User exists - update password if provided
        if let Some(password_hash) = password_hash {
            // Update password using raw SQL since we don't have a password update method
            sqlx::query!("UPDATE users SET password_hash = $1 WHERE email = $2", password_hash, email)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        return Ok(existing_user.id);
    }

    // Create new admin user
    let user_create = UserCreateDBRequest {
        username: email.to_string(),
        email: email.to_string(),
        display_name: None,
        avatar_url: None,
        is_admin: true,
        roles: vec![Role::PlatformManager],
        auth_source: "system".to_string(),
        password_hash,
        external_user_id: None,
    };

    let created_user = user_repo
        .create(&user_create)
        .await
        .map_err(|e| sqlx::Error::Protocol(format!("Failed to create admin user: {e}")))?;

    tx.commit().await?;
    Ok(created_user.id)
}

/// Seed the database with initial configuration (run only once).
///
/// This function initializes the database with inference endpoints from model sources
/// and generates a new system API key. It's idempotent - subsequent calls will skip
/// seeding to preserve manual changes.
///
/// The function checks the `endpoints_seeded` flag in `system_config` to determine
/// if seeding has already occurred. This prevents overwriting user modifications.
///
/// # Arguments
///
/// - `sources`: Slice of model source configurations to seed as inference endpoints
/// - `db`: PostgreSQL connection pool
///
/// # Returns
///
/// Returns `Ok(())` if seeding succeeds or was already completed.
///
/// # Errors
///
/// Returns an error if database operations fail.
#[instrument(skip_all)]
pub async fn seed_database(sources: &[config::ModelSource], db: &PgPool) -> Result<(), anyhow::Error> {
    // Use a transaction to ensure atomicity
    let mut tx = db.begin().await?;

    // Check if database has already been seeded to prevent overwriting manual changes
    let seeded = sqlx::query_scalar!("SELECT value FROM system_config WHERE key = 'endpoints_seeded'")
        .fetch_optional(&mut *tx)
        .await?;

    if let Some(true) = seeded {
        info!("Database already seeded, skipping seeding operations");
        tx.commit().await?;
        return Ok(());
    }

    info!("Seeding database with initial configuration");

    // Seed endpoints from model sources
    let system_user_id = Uuid::nil();
    for source in sources {
        // Insert endpoint if it doesn't already exist (first-time seeding only)
        if let Some(endpoint_id) = sqlx::query_scalar!(
            "INSERT INTO inference_endpoints (name, description, url, created_by)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (name) DO NOTHING
            RETURNING id",
            source.name,
            None::<String>, // System-created endpoints don't have descriptions
            source.url.as_str(),
            system_user_id,
        )
        .fetch_optional(&mut *tx)
        .await?
        {
            for model in source.default_models.as_deref().unwrap_or(&[]) {
                // Insert deployed model if it doesn't already exist
                let mut model_repo = Deployments::new(&mut tx);
                if let Ok(row) = model_repo
                    .create(&DeploymentCreateDBRequest::from_api_create(
                        Uuid::nil(),
                        DeployedModelCreate::Standard(StandardModelCreate {
                            model_name: model.name.clone(),
                            alias: Some(model.name.clone()),
                            hosted_on: endpoint_id,
                            description: None,
                            model_type: None,
                            capabilities: None,
                            requests_per_second: None,
                            burst_size: None,
                            capacity: None,
                            batch_capacity: None,
                            tariffs: None,
                            provider_pricing: None,
                        }),
                    ))
                    .await
                    && model.add_to_everyone_group
                {
                    let mut groups_repo = Groups::new(&mut tx);
                    if let Err(e) = groups_repo.add_deployment_to_group(row.id, Uuid::nil(), Uuid::nil()).await {
                        debug!(
                            "Failed to add deployed model {} to 'everyone' group during seeding: {}",
                            model.name, e
                        );
                    }
                }
            }
        }
    }

    // Update the system API key secret with a new secure value
    let system_api_key_id = Uuid::nil();
    let new_secret = crypto::generate_api_key();
    sqlx::query!("UPDATE api_keys SET secret = $1 WHERE id = $2", new_secret, system_api_key_id)
        .execute(&mut *tx)
        .await?;

    // Mark database as seeded to prevent future overwrites
    sqlx::query!(
        "UPDATE system_config SET value = true, updated_at = NOW()
         WHERE key = 'endpoints_seeded'"
    )
    .execute(&mut *tx)
    .await?;

    // Commit the transaction - either everything succeeds or nothing changes
    tx.commit().await?;

    debug!("Database seeded successfully");

    Ok(())
}

/// Setup database connections, run migrations, and initialize data
/// Returns: (embedded_db, main_pools, fusillade_pools, outlet_pools)
///
/// If `pool` is provided, it will be used directly instead of creating a new connection.
/// This is useful for tests where sqlx::test provides a pool.
async fn setup_database(
    config: &Config,
    pool: Option<PgPool>,
) -> anyhow::Result<(
    Option<db::embedded::EmbeddedDatabase>,
    db::DbPools,
    db::DbPools,
    Option<db::DbPools>,
)> {
    // If a pool is provided (e.g., from tests), use it directly
    let (embedded_db, pool) = if let Some(existing_pool) = pool {
        info!("Using provided database pool");
        (None, existing_pool)
    } else {
        // Database connection - handle both embedded and external
        let (_embedded_db, database_url) = match &config.database {
            config::DatabaseConfig::Embedded { .. } => {
                let persistent = config.database.embedded_persistent();
                info!("Starting with embedded database (persistent: {})", persistent);
                if !persistent {
                    info!("persistent=false: database will be ephemeral and data will be lost on shutdown");
                }
                #[cfg(feature = "embedded-db")]
                {
                    let data_dir = config.database.embedded_data_dir();
                    let embedded_db = db::embedded::EmbeddedDatabase::start(data_dir, persistent).await?;
                    let url = embedded_db.connection_string().to_string();
                    (Some(embedded_db), url)
                }
                #[cfg(not(feature = "embedded-db"))]
                {
                    anyhow::bail!(
                        "Embedded database is configured but the feature is not enabled. \
                         Rebuild with --features embedded-db to use embedded database."
                    );
                }
            }
            config::DatabaseConfig::External { url, .. } => {
                info!("Using external database");
                (None::<db::embedded::EmbeddedDatabase>, url.clone())
            }
        };

        let main_settings = config.database.main_pool_settings();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(main_settings.max_connections)
            .min_connections(main_settings.min_connections)
            .acquire_timeout(std::time::Duration::from_secs(main_settings.acquire_timeout_secs))
            .idle_timeout(if main_settings.idle_timeout_secs > 0 {
                Some(std::time::Duration::from_secs(main_settings.idle_timeout_secs))
            } else {
                None
            })
            .max_lifetime(if main_settings.max_lifetime_secs > 0 {
                Some(std::time::Duration::from_secs(main_settings.max_lifetime_secs))
            } else {
                None
            })
            .connect(&database_url)
            .await?;
        (_embedded_db, pool)
    };

    migrator().run(&pool).await?;

    // Create replica pool if configured
    let db_pools = if let Some(replica_url) = config.database.external_replica_url() {
        info!("Setting up read replica pool");
        let replica_settings = config.database.main_replica_pool_settings();
        let replica_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(replica_settings.max_connections)
            .min_connections(replica_settings.min_connections)
            .acquire_timeout(std::time::Duration::from_secs(replica_settings.acquire_timeout_secs))
            .idle_timeout(if replica_settings.idle_timeout_secs > 0 {
                Some(std::time::Duration::from_secs(replica_settings.idle_timeout_secs))
            } else {
                None
            })
            .max_lifetime(if replica_settings.max_lifetime_secs > 0 {
                Some(std::time::Duration::from_secs(replica_settings.max_lifetime_secs))
            } else {
                None
            })
            .connect(replica_url)
            .await?;
        db::DbPools::with_replica(pool, replica_pool)
    } else {
        db::DbPools::new(pool)
    };

    // Get connection options from the main pool to create schema-based child pools
    let main_connect_opts = db_pools.connect_options().as_ref().clone();

    // Helper to create a pool with schema-specific search_path
    // Reuses connection URLs from main pool (both primary and replica if configured)
    let create_schema_pool = |schema: String, opts: sqlx::postgres::PgConnectOptions, settings: &config::PoolSettings| {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(settings.max_connections)
            .min_connections(settings.min_connections)
            .acquire_timeout(std::time::Duration::from_secs(settings.acquire_timeout_secs))
            .idle_timeout(if settings.idle_timeout_secs > 0 {
                Some(std::time::Duration::from_secs(settings.idle_timeout_secs))
            } else {
                None
            })
            .max_lifetime(if settings.max_lifetime_secs > 0 {
                Some(std::time::Duration::from_secs(settings.max_lifetime_secs))
            } else {
                None
            })
            .after_connect(move |conn, _meta| {
                let s = schema.clone();
                Box::pin(async move {
                    conn.execute(&*format!("SET search_path = '{s}'")).await?;
                    Ok(())
                })
            })
            .connect_lazy_with(opts)
    };

    // Setup fusillade batch processing pool
    info!("Setting up fusillade batch processing pool");
    let fusillade_pools = match config.database.fusillade() {
        config::ComponentDb::Schema {
            name, pool: pool_settings, ..
        } => {
            // Create primary pool using main's connection, with schema-specific search_path
            let primary = create_schema_pool(name.clone(), main_connect_opts.clone(), pool_settings);
            primary.execute(&*format!("CREATE SCHEMA IF NOT EXISTS {name}")).await?;

            // Create replica pool if main has one configured (inherits main's replica connection)
            if let Some(replica_opts) = db_pools.replica_connect_options() {
                info!("Setting up fusillade read replica (schema mode)");
                let replica_pool_settings = config.database.fusillade().replica_pool_settings();
                let replica = create_schema_pool(name.clone(), replica_opts, replica_pool_settings);
                db::DbPools::with_replica(primary, replica)
            } else {
                db::DbPools::new(primary)
            }
        }
        config::ComponentDb::Dedicated {
            url,
            replica_url,
            pool: pool_settings,
            ..
        } => {
            info!("Using dedicated database for fusillade");
            let primary = sqlx::postgres::PgPoolOptions::new()
                .max_connections(pool_settings.max_connections)
                .min_connections(pool_settings.min_connections)
                .acquire_timeout(std::time::Duration::from_secs(pool_settings.acquire_timeout_secs))
                .idle_timeout(if pool_settings.idle_timeout_secs > 0 {
                    Some(std::time::Duration::from_secs(pool_settings.idle_timeout_secs))
                } else {
                    None
                })
                .max_lifetime(if pool_settings.max_lifetime_secs > 0 {
                    Some(std::time::Duration::from_secs(pool_settings.max_lifetime_secs))
                } else {
                    None
                })
                .connect(url)
                .await?;

            if let Some(replica_url) = replica_url {
                info!("Setting up fusillade read replica");
                let replica_pool_settings = config.database.fusillade().replica_pool_settings();
                let replica = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(replica_pool_settings.max_connections)
                    .min_connections(replica_pool_settings.min_connections)
                    .acquire_timeout(std::time::Duration::from_secs(replica_pool_settings.acquire_timeout_secs))
                    .idle_timeout(if replica_pool_settings.idle_timeout_secs > 0 {
                        Some(std::time::Duration::from_secs(replica_pool_settings.idle_timeout_secs))
                    } else {
                        None
                    })
                    .max_lifetime(if replica_pool_settings.max_lifetime_secs > 0 {
                        Some(std::time::Duration::from_secs(replica_pool_settings.max_lifetime_secs))
                    } else {
                        None
                    })
                    .connect(replica_url)
                    .await?;
                db::DbPools::with_replica(primary, replica)
            } else {
                db::DbPools::new(primary)
            }
        }
    };
    fusillade::migrator().run(&*fusillade_pools).await?;

    // Setup outlet schema and pool if request logging is enabled
    let outlet_pools = if config.enable_request_logging {
        info!("Setting up outlet request logging pool (logging enabled)");
        let pools = match config.database.outlet() {
            config::ComponentDb::Schema {
                name, pool: pool_settings, ..
            } => {
                // Create primary pool using main's connection, with schema-specific search_path
                let primary = create_schema_pool(name.clone(), main_connect_opts.clone(), pool_settings);
                primary.execute(&*format!("CREATE SCHEMA IF NOT EXISTS {name}")).await?;

                // Create replica pool if main has one configured (inherits main's replica connection)
                if let Some(replica_opts) = db_pools.replica_connect_options() {
                    info!("Setting up outlet read replica (schema mode)");
                    let replica_pool_settings = config.database.outlet().replica_pool_settings();
                    let replica = create_schema_pool(name.clone(), replica_opts, replica_pool_settings);
                    db::DbPools::with_replica(primary, replica)
                } else {
                    db::DbPools::new(primary)
                }
            }
            config::ComponentDb::Dedicated {
                url,
                replica_url,
                pool: pool_settings,
                ..
            } => {
                info!("Using dedicated database for outlet");
                let primary = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(pool_settings.max_connections)
                    .min_connections(pool_settings.min_connections)
                    .acquire_timeout(std::time::Duration::from_secs(pool_settings.acquire_timeout_secs))
                    .idle_timeout(if pool_settings.idle_timeout_secs > 0 {
                        Some(std::time::Duration::from_secs(pool_settings.idle_timeout_secs))
                    } else {
                        None
                    })
                    .max_lifetime(if pool_settings.max_lifetime_secs > 0 {
                        Some(std::time::Duration::from_secs(pool_settings.max_lifetime_secs))
                    } else {
                        None
                    })
                    .connect(url)
                    .await?;

                if let Some(replica_url) = replica_url {
                    info!("Setting up outlet read replica");
                    let replica_pool_settings = config.database.outlet().replica_pool_settings();
                    let replica = sqlx::postgres::PgPoolOptions::new()
                        .max_connections(replica_pool_settings.max_connections)
                        .min_connections(replica_pool_settings.min_connections)
                        .acquire_timeout(std::time::Duration::from_secs(replica_pool_settings.acquire_timeout_secs))
                        .idle_timeout(if replica_pool_settings.idle_timeout_secs > 0 {
                            Some(std::time::Duration::from_secs(replica_pool_settings.idle_timeout_secs))
                        } else {
                            None
                        })
                        .max_lifetime(if replica_pool_settings.max_lifetime_secs > 0 {
                            Some(std::time::Duration::from_secs(replica_pool_settings.max_lifetime_secs))
                        } else {
                            None
                        })
                        .connect(replica_url)
                        .await?;
                    db::DbPools::with_replica(primary, replica)
                } else {
                    db::DbPools::new(primary)
                }
            }
        };
        outlet_postgres::migrator().run(&*pools).await?;

        Some(pools)
    } else {
        info!("Skipping outlet pool setup (logging disabled)");
        None
    };

    // Create initial admin user if it doesn't exist (always use primary for writes)
    let argon2_params = password::Argon2Params {
        memory_kib: config.auth.native.password.argon2_memory_kib,
        iterations: config.auth.native.password.argon2_iterations,
        parallelism: config.auth.native.password.argon2_parallelism,
    };
    create_initial_admin_user(&config.admin_email, config.admin_password.as_deref(), argon2_params, &db_pools)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create initial admin user: {}", e))?;

    // Seed database with initial configuration (only runs once)
    seed_database(&config.model_sources, &db_pools).await?;

    Ok((embedded_db, db_pools, fusillade_pools, outlet_pools))
}

/// Create CORS layer from configuration
fn create_cors_layer(config: &Config) -> anyhow::Result<CorsLayer> {
    let mut origins = Vec::new();
    for origin in &config.auth.security.cors.allowed_origins {
        let header_value = match origin {
            CorsOrigin::Wildcard => "*".parse::<HeaderValue>()?,
            CorsOrigin::Url(url) => {
                // Strip trailing slash that Url::parse adds during normalization
                let url_str = url.as_str().trim_end_matches('/');
                url_str.parse::<HeaderValue>()?
            }
        };
        origins.push(header_value);
    }

    info!("Configuring CORS with allowed origins: {:?}", origins);

    // Parse exposed headers as HeaderName
    let exposed: Vec<http::HeaderName> = config
        .auth
        .security
        .cors
        .exposed_headers
        .iter()
        .filter_map(|h| h.parse().ok())
        .collect();

    let mut cors = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            http::Method::GET,
            http::Method::POST,
            http::Method::PUT,
            http::Method::DELETE,
            http::Method::PATCH,
            http::Method::OPTIONS,
        ])
        .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION, http::header::ACCEPT])
        .allow_credentials(config.auth.security.cors.allow_credentials)
        .expose_headers(exposed);

    if let Some(max_age) = config.auth.security.cors.max_age {
        cors = cors.max_age(std::time::Duration::from_secs(max_age));
    }

    Ok(cors)
}

/// Build the main application router with all endpoints and middleware.
///
/// This function constructs the complete Axum router with:
/// - Authentication routes (login, registration, password reset)
/// - Admin API routes (user/group/deployment management)
/// - AI proxy routes (OpenAI-compatible endpoints)
/// - Static asset serving and SPA fallback
/// - Optional request logging via outlet
/// - Optional Prometheus metrics
/// - CORS configuration
/// - Tracing middleware
///
/// # Arguments
///
/// - `state`: Mutable application state (metrics recorder may be initialized here)
/// - `onwards_router`: Pre-configured router for AI request proxying
///
/// # Returns
///
/// Returns the fully configured router ready to be served.
///
/// # Errors
///
/// Returns an error if CORS configuration is invalid or metrics initialization fails.
#[instrument(skip_all)]
pub async fn build_router(state: &mut AppState, onwards_router: Router) -> anyhow::Result<Router> {
    // Setup request logging if enabled
    let outlet_layer = if let Some(outlet_pool) = state.outlet_db.as_ref() {
        // Initialize GenAI metrics BEFORE creating analytics serializer if metrics enabled
        if state.config.enable_metrics {
            let gen_ai_registry = prometheus::Registry::new();
            let gen_ai_metrics =
                GenAiMetrics::new(&gen_ai_registry).map_err(|e| anyhow::anyhow!("Failed to create GenAI metrics: {}", e))?;
            state.metrics_recorder = Some(gen_ai_metrics);
        }

        let analytics_serializer = AnalyticsResponseSerializer::new(
            (*state.db).clone(),
            uuid::Uuid::new_v4(),
            state.config.clone(),
            state.metrics_recorder.clone(),
        );

        let postgres_handler = PostgresHandler::<ParsedAIRequest, AiResponse>::from_pool(outlet_pool.clone())
            .await
            .expect("Failed to create PostgresHandler for request logging")
            .with_request_serializer(parse_ai_request)
            .with_response_serializer(analytics_serializer.create_serializer());

        let outlet_config = RequestLoggerConfig {
            capture_request_body: true,
            capture_response_body: true,
            path_filter: None, // No path filter needed - applied directly to ai_router
        };

        Some(RequestLoggerLayer::new(outlet_config, postgres_handler))
    } else {
        None
    };
    // Authentication routes (at root level, can be masked when deployed behind SSO proxy)
    let auth_routes = Router::new()
        .route(
            "/authentication/register",
            get(api::handlers::auth::get_registration_info).post(api::handlers::auth::register),
        )
        .route(
            "/authentication/login",
            get(api::handlers::auth::get_login_info).post(api::handlers::auth::login),
        )
        .route("/authentication/logout", post(api::handlers::auth::logout))
        .route("/authentication/password-resets", post(api::handlers::auth::request_password_reset))
        .route(
            "/authentication/password-resets/{token_id}/confirm",
            post(api::handlers::auth::confirm_password_reset),
        )
        .route("/authentication/password-change", post(api::handlers::auth::change_password))
        .with_state(state.clone());

    // API routes
    let api_routes = Router::new()
        .route("/config", get(api::handlers::config::get_config))
        // User management (admin only for collection operations)
        .route("/users", get(api::handlers::users::list_users))
        .route("/users", post(api::handlers::users::create_user))
        .route("/users/{id}", get(api::handlers::users::get_user))
        .route("/users/{id}", patch(api::handlers::users::update_user))
        .route("/users/{id}", delete(api::handlers::users::delete_user))
        // API Keys as user sub-resources
        .route("/users/{user_id}/api-keys", get(api::handlers::api_keys::list_user_api_keys))
        .route("/users/{user_id}/api-keys", post(api::handlers::api_keys::create_user_api_key))
        .route("/users/{user_id}/api-keys/{id}", get(api::handlers::api_keys::get_user_api_key))
        .route(
            "/users/{user_id}/api-keys/{id}",
            delete(api::handlers::api_keys::delete_user_api_key),
        )
        // User-group relationships
        .route("/users/{user_id}/groups", get(api::handlers::groups::get_user_groups))
        .route("/users/{user_id}/groups/{group_id}", post(api::handlers::groups::add_group_to_user))
        .route(
            "/users/{user_id}/groups/{group_id}",
            delete(api::handlers::groups::remove_group_from_user),
        )
        // Transaction management (RESTful credit transactions)
        .route("/transactions", post(api::handlers::transactions::create_transaction))
        .route("/transactions/{transaction_id}", get(api::handlers::transactions::get_transaction))
        .route("/transactions", get(api::handlers::transactions::list_transactions))
        // Payment processing
        .route("/payments", post(api::handlers::payments::create_payment))
        .route("/payments/{id}", patch(api::handlers::payments::process_payment))
        .route("/billing-portal", post(api::handlers::payments::create_billing_portal_session))
        // Inference endpoints management (admin only for write operations)
        .route("/endpoints", get(api::handlers::inference_endpoints::list_inference_endpoints))
        .route("/endpoints", post(api::handlers::inference_endpoints::create_inference_endpoint))
        .route(
            "/endpoints/validate",
            post(api::handlers::inference_endpoints::validate_inference_endpoint),
        )
        .route("/endpoints/{id}", get(api::handlers::inference_endpoints::get_inference_endpoint))
        .route(
            "/endpoints/{id}",
            patch(api::handlers::inference_endpoints::update_inference_endpoint),
        )
        .route(
            "/endpoints/{id}",
            delete(api::handlers::inference_endpoints::delete_inference_endpoint),
        )
        .route(
            "/endpoints/{id}/synchronize",
            post(api::handlers::inference_endpoints::synchronize_endpoint),
        )
        // Models endpoints
        .route("/models", get(api::handlers::deployments::list_deployed_models))
        .route("/models", post(api::handlers::deployments::create_deployed_model))
        .route("/models/{id}", get(api::handlers::deployments::get_deployed_model))
        .route("/models/{id}", patch(api::handlers::deployments::update_deployed_model))
        .route("/models/{id}", delete(api::handlers::deployments::delete_deployed_model))
        // Composite model component management (for models where is_composite=true)
        .route("/models/{id}/components", get(api::handlers::deployments::get_model_components))
        .route(
            "/models/{id}/components/{component_id}",
            post(api::handlers::deployments::add_model_component),
        )
        .route(
            "/models/{id}/components/{component_id}",
            patch(api::handlers::deployments::update_model_component),
        )
        .route(
            "/models/{id}/components/{component_id}",
            delete(api::handlers::deployments::remove_model_component),
        )
        // Groups management
        .route("/groups", get(api::handlers::groups::list_groups))
        .route("/groups", post(api::handlers::groups::create_group))
        .route("/groups/{id}", get(api::handlers::groups::get_group))
        .route("/groups/{id}", patch(api::handlers::groups::update_group))
        .route("/groups/{id}", delete(api::handlers::groups::delete_group))
        // Group-user relationships
        .route("/groups/{group_id}/users", get(api::handlers::groups::get_group_users))
        .route("/groups/{group_id}/users/{user_id}", post(api::handlers::groups::add_user_to_group))
        .route(
            "/groups/{group_id}/users/{user_id}",
            delete(api::handlers::groups::remove_user_from_group),
        )
        // Group-model relationships
        .route("/groups/{group_id}/models", get(api::handlers::groups::get_group_deployments))
        .route(
            "/groups/{group_id}/models/{deployment_id}",
            post(api::handlers::groups::add_deployment_to_group),
        )
        .route(
            "/groups/{group_id}/models/{deployment_id}",
            delete(api::handlers::groups::remove_deployment_from_group),
        )
        .route("/models/{deployment_id}/groups", get(api::handlers::groups::get_deployment_groups))
        .route("/requests", get(api::handlers::requests::list_requests))
        .route("/requests/aggregate", get(api::handlers::requests::aggregate_requests))
        .route("/requests/aggregate-by-user", get(api::handlers::requests::aggregate_by_user))
        // Probes management
        .route("/probes", get(api::handlers::probes::list_probes))
        .route("/probes", post(api::handlers::probes::create_probe))
        .route("/probes/test/{deployment_id}", post(api::handlers::probes::test_probe))
        .route("/probes/{id}", get(api::handlers::probes::get_probe))
        .route("/probes/{id}", patch(api::handlers::probes::update_probe))
        .route("/probes/{id}", delete(api::handlers::probes::delete_probe))
        .route("/probes/{id}/activate", patch(api::handlers::probes::activate_probe))
        .route("/probes/{id}/deactivate", patch(api::handlers::probes::deactivate_probe))
        .route("/probes/{id}/execute", post(api::handlers::probes::execute_probe))
        .route("/probes/{id}/results", get(api::handlers::probes::get_probe_results))
        .route("/probes/{id}/statistics", get(api::handlers::probes::get_statistics));

    let api_routes_with_state = api_routes.with_state(state.clone());

    // Batches API routes (files + batches) - conditionally enabled under /ai/v1
    let batches_routes = if state.config.batches.enabled {
        // File upload route with custom body limit (other routes use default)
        let file_upload_limit = state.config.batches.files.max_file_size;
        let file_router = Router::new().route(
            "/files",
            post(api::handlers::files::upload_file).layer(DefaultBodyLimit::max(file_upload_limit as usize)),
        );

        Some(
            Router::new()
                // Files management - merge file upload route with custom body limit
                .merge(file_router)
                .route("/files", get(api::handlers::files::list_files))
                .route("/files/{file_id}", get(api::handlers::files::get_file))
                .route("/files/{file_id}", delete(api::handlers::files::delete_file))
                .route("/files/{file_id}/content", get(api::handlers::files::get_file_content))
                .route("/files/{file_id}/cost-estimate", get(api::handlers::files::get_file_cost_estimate))
                // Batches management
                .route("/batches", post(api::handlers::batches::create_batch))
                .route("/batches", get(api::handlers::batches::list_batches))
                .route("/batches/{batch_id}", get(api::handlers::batches::get_batch))
                .route("/batches/{batch_id}", delete(api::handlers::batches::delete_batch))
                .route("/batches/{batch_id}/analytics", get(api::handlers::batches::get_batch_analytics))
                .route("/batches/{batch_id}/results", get(api::handlers::batches::get_batch_results))
                .route("/batches/{batch_id}/cancel", post(api::handlers::batches::cancel_batch))
                .route(
                    "/batches/{batch_id}/retry",
                    post(api::handlers::batches::retry_failed_batch_requests),
                )
                .route(
                    "/batches/{batch_id}/retry-requests",
                    post(api::handlers::batches::retry_specific_requests),
                )
                // Daemon monitoring
                .route("/daemons", get(api::handlers::daemons::list_daemons))
                .with_state(state.clone()),
        )
    } else {
        None
    };

    // Serve embedded static assets, falling back to SPA for unmatched routes
    let fallback = get(api::handlers::static_assets::serve_embedded_asset).fallback(get(api::handlers::static_assets::spa_fallback));

    // Apply error enrichment middleware to onwards router (before outlet logging)
    let onwards_router = onwards_router.layer(middleware::from_fn_with_state(
        (*state.db).clone(),
        error_enrichment::error_enrichment_middleware,
    ));

    // Apply request logging layer only to onwards router
    let onwards_router = if let Some(outlet_layer) = outlet_layer.clone() {
        onwards_router.layer(outlet_layer)
    } else {
        onwards_router
    };

    // Build the app with admin API and onwards proxy nested. serve the (restricted) openai spec.
    // Batches routes are merged with onwards router under /ai/v1 (batches match first)
    let ai_router = if let Some(batches) = batches_routes {
        batches.merge(onwards_router)
    } else {
        onwards_router
    };

    let router = Router::new()
        .route("/healthz", get(|| async { "OK" }))
        // Webhook routes (external services, not part of client API docs)
        .route("/webhooks/payments", post(api::handlers::payments::webhook_handler))
        .with_state(state.clone())
        .merge(auth_routes)
        .nest("/ai/v1", ai_router)
        .nest("/admin/api/v1", api_routes_with_state)
        .route("/admin/openapi.json", get(|| async { axum::Json(AdminApiDoc::openapi()) }))
        .route("/ai/openapi.json", get(|| async { axum::Json(AiApiDoc::openapi()) }))
        .merge(Scalar::with_url("/admin/docs", AdminApiDoc::openapi()))
        .merge(Scalar::with_url("/ai/docs", AiApiDoc::openapi()))
        .fallback_service(fallback.with_state(state.clone()));

    // Create CORS layer from config
    let cors_layer = create_cors_layer(&state.config)?;

    // Apply CORS to main router (request logging already applied to onwards_router above)
    let mut router = router.layer(cors_layer);

    // Add Prometheus metrics if enabled
    if state.config.enable_metrics {
        let metric_handle = get_or_install_prometheus_handle();

        let prometheus_layer = PrometheusMetricLayerBuilder::new()
            .with_prefix("dwctl")
            .with_metrics_from_fn(move || metric_handle.clone())
            .build_pair()
            .0;

        // Get the GenAI registry from the metrics recorder (already initialized earlier)
        let gen_ai_registry = if let Some(ref recorder) = state.metrics_recorder {
            recorder.registry().clone()
        } else {
            // Fallback: create empty registry if somehow metrics recorder wasn't initialized
            prometheus::Registry::new()
        };

        // Get handle for the endpoint closure
        let endpoint_handle = get_or_install_prometheus_handle();

        // Add metrics endpoint that combines both axum-prometheus and GenAI metrics
        router = router
            .route(
                "/internal/metrics",
                get(|| async move {
                    use prometheus::{Encoder, TextEncoder};

                    // Get axum-prometheus metrics
                    let mut axum_metrics = endpoint_handle.render();

                    // Get GenAI metrics
                    let encoder = TextEncoder::new();
                    let gen_ai_families = gen_ai_registry.gather();
                    let mut gen_ai_buffer = vec![];
                    encoder.encode(&gen_ai_families, &mut gen_ai_buffer).unwrap();

                    // Combine both
                    axum_metrics.push_str(&String::from_utf8_lossy(&gen_ai_buffer));
                    axum_metrics
                }),
            )
            .layer(prometheus_layer);
    }

    // Add tracing layer
    let router = router.layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_request(DefaultOnRequest::new().level(Level::INFO))
            .on_response(DefaultOnResponse::new().level(Level::INFO)),
    );

    Ok(router)
}

/// Container for background services and their lifecycle management.
///
/// This struct encapsulates all background tasks that run alongside the HTTP server,
/// including:
/// - Fusillade batch request processing daemon
/// - Probe scheduler for health monitoring
/// - Onwards configuration synchronization
/// - Leader election coordination (in distributed mode)
///
/// # Graceful Shutdown
///
/// The struct provides a [`shutdown`](BackgroundServices::shutdown) method to gracefully
/// stop all background tasks. When dropped, the `drop_guard` will automatically cancel
/// the shutdown token, signaling all tasks to stop.
pub struct BackgroundServices {
    request_manager: Arc<fusillade::PostgresRequestManager<fusillade::ReqwestHttpClient>>,
    is_leader: bool,
    onwards_targets: onwards::target::Targets,
    #[cfg_attr(not(test), allow(dead_code))]
    onwards_sender: Option<tokio::sync::watch::Sender<onwards::target::Targets>>,
    // JoinSet is cancel-safe - can be polled in select! without losing tasks
    background_tasks: tokio::task::JoinSet<anyhow::Result<()>>,
    // Map task IDs to names for logging
    task_names: std::collections::HashMap<tokio::task::Id, &'static str>,
    shutdown_token: tokio_util::sync::CancellationToken,
    // Pub so that we can disarm it if we want to
    pub drop_guard: Option<tokio_util::sync::DropGuard>,
}

impl BackgroundServices {
    /// Wait for any background task to complete (indicating a failure)
    /// This method is cancel-safe - can be used in tokio::select! without losing tasks
    /// Returns an error with details about which task failed
    pub async fn wait_for_failure(&mut self) -> anyhow::Result<std::convert::Infallible> {
        match self.background_tasks.join_next_with_id().await {
            None => {
                // No background tasks - wait forever
                futures::future::pending::<()>().await;
                unreachable!()
            }
            Some(Ok((task_id, Ok(())))) => {
                let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                tracing::warn!(task = task_name, "Background task completed unexpectedly");
                anyhow::bail!("Background task '{}' completed early", task_name)
            }
            Some(Ok((task_id, Err(e)))) => {
                let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                tracing::error!(task = task_name, error = %e, "Background task failed");
                anyhow::bail!("Background task '{}' failed: {}", task_name, e)
            }
            Some(Err(e)) => {
                let task_id = e.id();
                let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                tracing::error!(task = task_name, error = %e, "Background task panicked");
                anyhow::bail!("Background task '{}' panicked: {}", task_name, e)
            }
        }
    }

    /// Gracefully shutdown all background tasks
    pub async fn shutdown(mut self) {
        // Signal all background tasks to shutdown
        self.shutdown_token.cancel();

        // Wait for all background tasks to complete and check for errors
        while let Some(result) = self.background_tasks.join_next_with_id().await {
            match result {
                Ok((task_id, Ok(()))) => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::debug!(task = task_name, "Background task completed successfully");
                }
                Ok((task_id, Err(e))) => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::error!(task = task_name, error = %e, "Background task failed");
                }
                Err(e) => {
                    let task_id = e.id();
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::error!(task = task_name, error = %e, "Background task panicked");
                }
            }
        }
    }

    /// Manually trigger a sync of onwards targets (for testing)
    /// This reloads the configuration from the database and updates the live routing
    /// Uses the same codepath as the automatic LISTEN/NOTIFY sync
    #[cfg(test)]
    pub async fn sync_onwards_config(&self, pool: &sqlx::PgPool) -> anyhow::Result<()> {
        let sender = self
            .onwards_sender
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Onwards sync not enabled"))?;

        // Use the same load function as the automatic sync
        let new_targets = crate::sync::onwards_config::load_targets_from_db(pool).await?;

        // Send through the watch channel (same as automatic sync)
        sender
            .send(new_targets)
            .map_err(|_| anyhow::anyhow!("Failed to send targets update"))?;

        Ok(())
    }
}

/// Helper for spawning named background tasks during setup
struct BackgroundTaskBuilder {
    tasks: tokio::task::JoinSet<anyhow::Result<()>>,
    names: std::collections::HashMap<tokio::task::Id, &'static str>,
}

impl BackgroundTaskBuilder {
    fn new() -> Self {
        Self {
            tasks: tokio::task::JoinSet::new(),
            names: std::collections::HashMap::new(),
        }
    }

    fn spawn<F>(&mut self, name: &'static str, future: F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let abort_handle = self.tasks.spawn(future);
        self.names.insert(abort_handle.id(), name);
    }

    fn into_parts(
        self,
    ) -> (
        tokio::task::JoinSet<anyhow::Result<()>>,
        std::collections::HashMap<tokio::task::Id, &'static str>,
    ) {
        (self.tasks, self.names)
    }
}

/// Setup background services (probe scheduler, batch daemon, leader election, onwards integration)
async fn setup_background_services(
    pool: PgPool,
    fusillade_pool: PgPool,
    outlet_pool: Option<PgPool>,
    config: Config,
    shutdown_token: tokio_util::sync::CancellationToken,
) -> anyhow::Result<BackgroundServices> {
    let drop_guard = shutdown_token.clone().drop_guard();
    // Track all background task handles for graceful shutdown
    let mut background_tasks = BackgroundTaskBuilder::new();

    // Create shared model capacity limits map for daemon coordination
    // This is populated by onwards config sync and read by fusillade daemon
    let model_capacity_limits = Arc::new(dashmap::DashMap::new());

    // Start onwards integration for proxying AI requests (if enabled)
    #[cfg_attr(not(test), allow(unused_variables))]
    let (initial_targets, onwards_sender) = if config.background_services.onwards_sync.enabled {
        let (onwards_config_sync, initial_targets, onwards_stream) =
            sync::onwards_config::OnwardsConfigSync::new_with_daemon_limits(pool.clone(), Some(model_capacity_limits.clone())).await?;

        // Clone the sender before moving onwards_config_sync into the spawn (for manual sync)
        let sender = onwards_config_sync.sender();

        // Start target updates - this spawns a background task internally and returns immediately
        initial_targets
            .receive_updates(onwards_stream)
            .await
            .map_err(anyhow::Error::from)
            .context("Onwards target updates failed")?;

        // Start the onwards configuration listener
        let onwards_shutdown = shutdown_token.clone();
        background_tasks.spawn("onwards-config-sync", async move {
            info!("Starting onwards configuration listener");
            onwards_config_sync
                .start(Default::default(), onwards_shutdown)
                .await
                .context("Onwards configuration listener failed")
        });

        (initial_targets, Some(sender))
    } else {
        info!("Onwards config sync disabled - AI proxy will not receive config updates");
        // Create empty targets when onwards sync is disabled
        let empty_config = onwards::target::ConfigFile {
            targets: std::collections::HashMap::new(),
            auth: None,
        };
        (onwards::target::Targets::from_config(empty_config)?, None)
    };
    // Leader election lock ID: 0x44574354_50524F42 (DWCT_PROB in hex for "dwctl probes")
    const LEADER_LOCK_ID: i64 = 0x4457_4354_5052_4F42_i64;

    let probe_scheduler = probes::ProbeScheduler::new(pool.clone(), config.clone());

    // Clone fusillade pool for metrics before moving into request manager
    let fusillade_pool_for_metrics = fusillade_pool.clone();

    // Initialize the fusillade request manager (for batch processing)
    let request_manager = Arc::new(
        fusillade::PostgresRequestManager::new(fusillade_pool)
            .with_config(
                config
                    .background_services
                    .batch_daemon
                    .to_fusillade_config_with_limits(Some(model_capacity_limits.clone())),
            )
            .with_download_buffer_size(config.batches.files.download_buffer_size)
            .with_batch_insert_strategy(
                config
                    .background_services
                    .batch_daemon
                    .batch_insert_strategy
                    .unwrap_or_default()  // Use fusillade's Default impl if None
            )
    );

    let is_leader: bool;

    if !config.background_services.leader_election.enabled {
        info!("Launching without leader election: running as leader");
        // Skip leader election - just become leader immediately
        is_leader = true;

        // Start probe scheduler if enabled
        if config.background_services.probe_scheduler.enabled {
            probe_scheduler.initialize(shutdown_token.clone()).await?;

            // Start the scheduler daemon in the background
            let daemon_scheduler = probe_scheduler.clone();
            let daemon_shutdown = shutdown_token.clone();
            background_tasks.spawn("probe-scheduler", async move {
                // Use LISTEN/NOTIFY in production, but disable in tests to avoid hangs
                let use_listen_notify = !cfg!(test);
                daemon_scheduler.run_daemon(daemon_shutdown, use_listen_notify, 300).await;
                // Probe scheduler runs until cancelled, then exits normally
                Ok(())
            });
        } else {
            info!("Probe scheduler disabled by configuration");
        }

        // Start the fusillade batch processing daemon based on config
        use crate::config::DaemonEnabled;
        use fusillade::DaemonExecutor;
        match config.background_services.batch_daemon.enabled {
            DaemonEnabled::Always | DaemonEnabled::Leader => {
                let daemon_handle = request_manager.clone().run(shutdown_token.clone())?;
                // Spawn task that propagates daemon errors
                background_tasks.spawn("fusillade-daemon", async move {
                    match daemon_handle.await {
                        Ok(Ok(())) => {
                            tracing::info!("Fusillade daemon exited normally");
                        }
                        Ok(Err(e)) => {
                            tracing::error!(error = %e, "Fusillade daemon failed");
                            anyhow::bail!("Fusillade daemon error: {}", e);
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Fusillade daemon task panicked");
                            anyhow::bail!("Fusillade daemon panic: {}", e);
                        }
                    }
                    Ok(())
                });
                info!("Skipping leader election - running as leader with probe scheduler and fusillade daemon");
            }
            DaemonEnabled::Never => {
                info!("Skipping leader election - running as leader with probe scheduler (fusillade daemon disabled)");
            }
        }
    } else {
        // Normal leader election
        is_leader = false;
        info!("Starting leader election - will attempt to acquire leadership");

        // If daemon is set to "Always", start it immediately regardless of leader election
        use crate::config::DaemonEnabled;
        if config.background_services.batch_daemon.enabled == DaemonEnabled::Always {
            use fusillade::DaemonExecutor;
            let daemon_handle = request_manager.clone().run(shutdown_token.clone())?;
            // Spawn task that propagates daemon errors
            background_tasks.spawn("fusillade-daemon", async move {
                match daemon_handle.await {
                    Ok(Ok(())) => {
                        tracing::info!("Fusillade daemon exited normally");
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "Fusillade daemon failed");
                        anyhow::bail!("Fusillade daemon error: {}", e);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Fusillade daemon task panicked");
                        anyhow::bail!("Fusillade daemon panic: {}", e);
                    }
                }
                Ok(())
            });
            info!("Fusillade batch daemon started (configured to always run)");
        }

        let is_leader_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Spawn leader election background task
        let leader_election_pool = pool.clone();
        let leader_election_scheduler_gain = probe_scheduler.clone();
        let leader_election_scheduler_lose = probe_scheduler.clone();
        let leader_election_request_manager_gain = request_manager.clone();
        let leader_election_config = config.clone();
        let leader_election_flag = is_leader_flag.clone();

        // Store daemon handle for cleanup on leadership loss
        let daemon_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<fusillade::Result<()>>>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let daemon_handle_gain = daemon_handle.clone();
        let daemon_handle_lose = daemon_handle.clone();

        // Store leadership session shutdown token for cleanup on leadership loss
        let leadership_shutdown: Arc<tokio::sync::Mutex<Option<tokio_util::sync::CancellationToken>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let leadership_shutdown_gain = leadership_shutdown.clone();
        let leadership_shutdown_lose = leadership_shutdown.clone();

        let leader_election_shutdown = shutdown_token.clone();
        background_tasks.spawn("leader-election", async move {
            leader_election::leader_election_task(
                leader_election_pool,
                leader_election_config,
                leader_election_flag,
                LEADER_LOCK_ID,
                leader_election_shutdown,
                move |_pool, config| {
                    // This closure is run when a replica becomes the leader
                    let scheduler = leader_election_scheduler_gain.clone();
                    let request_manager = leader_election_request_manager_gain.clone();
                    let daemon_handle = daemon_handle_gain.clone();
                    let leadership_shutdown = leadership_shutdown_gain.clone();
                    async move {
                        // Wait for the server to be fully up before starting probes
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                        // Create a new cancellation token for this leadership session
                        let session_token = tokio_util::sync::CancellationToken::new();
                        *leadership_shutdown.lock().await = Some(session_token.clone());

                        // Start probe scheduler if enabled
                        if config.background_services.probe_scheduler.enabled {
                            scheduler
                                .initialize(session_token.clone())
                                .await
                                .map_err(|e| anyhow::anyhow!("Failed to initialize probe scheduler: {}", e))?;

                            // Start the probe scheduler daemon in the background
                            let daemon_scheduler = scheduler.clone();
                            let daemon_session_token = session_token.clone();
                            tokio::spawn(async move {
                                let use_listen_notify = !cfg!(test);
                                daemon_scheduler.run_daemon(daemon_session_token, use_listen_notify, 300).await;
                            });
                        } else {
                            tracing::info!("Probe scheduler disabled by configuration");
                        }

                        // Start the fusillade batch processing daemon based on config
                        use crate::config::DaemonEnabled;
                        use fusillade::DaemonExecutor;
                        match config.background_services.batch_daemon.enabled {
                            DaemonEnabled::Leader => {
                                let handle = request_manager
                                    .run(session_token.clone())
                                    .map_err(|e| anyhow::anyhow!("Failed to start fusillade daemon: {}", e))?;

                                // Store the handle so we can abort it when losing leadership
                                *daemon_handle.lock().await = Some(handle);

                                tracing::info!("Fusillade batch daemon started on elected leader");
                            }
                            DaemonEnabled::Always => {
                                // Daemon already started earlier, nothing to do here
                            }
                            DaemonEnabled::Never => {
                                tracing::info!("Fusillade batch daemon disabled by configuration");
                            }
                        }

                        Ok(())
                    }
                },
                move |_pool, config| {
                    // This closure is run when a replica stops being the leader
                    let scheduler = leader_election_scheduler_lose.clone();
                    let daemon_handle = daemon_handle_lose.clone();
                    let leadership_shutdown = leadership_shutdown_lose.clone();
                    async move {
                        // Cancel the leadership session token first, which will stop all background tasks gracefully
                        if let Some(token) = leadership_shutdown.lock().await.take() {
                            token.cancel();
                        }

                        // Now stop all schedulers if probe scheduler was enabled
                        if config.background_services.probe_scheduler.enabled {
                            scheduler
                                .stop_all()
                                .await
                                .map_err(|e| anyhow::anyhow!("Failed to stop probe scheduler: {}", e))?;
                        }

                        // Stop the fusillade daemon
                        if let Some(handle) = daemon_handle.lock().await.take() {
                            handle.abort();
                            tracing::info!("Fusillade batch daemon stopped (lost leadership)");
                        }

                        Ok(())
                    }
                },
            )
            .await;
            // Leader election runs until cancelled, then exits normally
            Ok(())
        });
    }

    // Start pool metrics sampler if metrics are enabled
    if config.enable_metrics {
        let mut pools = vec![
            db::LabeledPool {
                name: "main",
                pool: pool.clone(),
            },
            db::LabeledPool {
                name: "fusillade",
                pool: fusillade_pool_for_metrics,
            },
        ];
        if let Some(outlet) = outlet_pool {
            pools.push(db::LabeledPool {
                name: "outlet",
                pool: outlet,
            });
        }
        let metrics_shutdown = shutdown_token.clone();
        let metrics_config = db::PoolMetricsConfig {
            sample_interval: config.background_services.pool_metrics.sample_interval,
        };
        background_tasks.spawn("pool-metrics-sampler", async move {
            db::run_pool_metrics_sampler(pools, metrics_config, metrics_shutdown).await
        });
    }

    let (background_tasks, task_names) = background_tasks.into_parts();

    Ok(BackgroundServices {
        request_manager,
        is_leader,
        onwards_targets: initial_targets,
        onwards_sender,
        background_tasks,
        task_names,
        shutdown_token,
        drop_guard: Some(drop_guard),
    })
}

/// Main application struct that owns all resources and lifecycle.
///
/// This is the top-level container for the entire application, managing:
/// - HTTP server and routing
/// - Database connections (main, fusillade, outlet, embedded)
/// - Application configuration
/// - Background services (probes, batches, leader election)
///
/// # Lifecycle
///
/// 1. **Create**: [`Application::new`] initializes all resources, runs migrations,
///    seeds the database, and starts background services
/// 2. **Serve**: [`Application::serve`] binds to a TCP port and starts handling requests
/// 3. **Shutdown**: When the shutdown signal is received, gracefully stops all services
/// ```
pub struct Application {
    router: Router,
    app_state: AppState,
    config: Config,
    db_pools: db::DbPools,
    _fusillade_pools: db::DbPools,
    _outlet_pools: Option<db::DbPools>,
    _embedded_db: Option<db::embedded::EmbeddedDatabase>,
    bg_services: BackgroundServices,
}

impl Application {
    /// Create a new application instance with all resources initialized
    ///
    /// If `pool` is provided, it will be used directly instead of creating a new connection.
    /// This is useful for tests where sqlx::test provides a pool.
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        Self::new_with_pool(config, None).await
    }

    /// Create a new application instance with an existing database pool
    ///
    /// This method is primarily for tests where sqlx::test provides a pool.
    /// For production use, prefer [`Application::new`] which will create its own pool.
    pub async fn new_with_pool(config: Config, pool: Option<PgPool>) -> anyhow::Result<Self> {
        debug!("Starting control layer with configuration: {:#?}", config);

        // Setup database connections, run migrations, and initialize data
        let (_embedded_db, db_pools, fusillade_pools, outlet_pools) = setup_database(&config, pool).await?;

        // Install Prometheus recorder BEFORE background services start
        // This ensures metrics set during background service initialization are captured
        if config.enable_metrics {
            get_or_install_prometheus_handle();
        }

        // Create a shutdown token for coordinating graceful shutdown of background tasks
        let shutdown_token = tokio_util::sync::CancellationToken::new();

        // Setup background services (onwards integration, probe scheduler, batch daemon, leader election)
        // Use primary pool for fusillade (via Deref)
        let bg_services = setup_background_services(
            (*db_pools).clone(),
            (*fusillade_pools).clone(),
            outlet_pools.as_ref().map(|p| (**p).clone()),
            config.clone(),
            shutdown_token.clone(),
        )
        .await?;

        // Build onwards router from targets with response sanitization enabled
        let onwards_app_state =
            onwards::AppState::new(bg_services.onwards_targets.clone()).with_response_transform(onwards::create_openai_sanitizer());
        let onwards_router = onwards::build_router(onwards_app_state);

        // Build app state and router
        // Extract primary pool for outlet_db (via Deref)
        let mut app_state = AppState::builder()
            .db(db_pools.clone())
            .config(config.clone())
            .is_leader(bg_services.is_leader)
            .request_manager(bg_services.request_manager.clone())
            .maybe_outlet_db(outlet_pools.as_ref().map(|p| (**p).clone()))
            .build();

        let router = build_router(&mut app_state, onwards_router).await?;

        Ok(Self {
            router,
            app_state,
            config,
            db_pools,
            _fusillade_pools: fusillade_pools,
            _outlet_pools: outlet_pools,
            _embedded_db,
            bg_services,
        })
    }

    /// Convert application into a test server
    ///
    /// This method is public to support integration tests but should only be used in test code.
    #[cfg(test)]
    pub fn into_test_server(self) -> (axum_test::TestServer, BackgroundServices) {
        // Apply middleware before path matching for tests
        let middleware = middleware::from_fn_with_state(self.app_state, admin_ai_proxy_middleware);
        let service = middleware.layer(self.router).into_make_service();
        let server = axum_test::TestServer::new(service).expect("Failed to create test server");
        (server, self.bg_services)
    }

    /// Start serving the application
    pub async fn serve<F>(mut self, shutdown: F) -> anyhow::Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let bind_addr = self.config.bind_address();
        let listener = TcpListener::bind(&bind_addr).await?;
        info!(
            "Control layer listening on http://{}, available at http://localhost:{}",
            bind_addr, self.config.port
        );

        // Apply middleware before path matching
        let middleware = middleware::from_fn_with_state(self.app_state, admin_ai_proxy_middleware);
        let service = middleware.layer(self.router);

        // Race the server against background task failures (fail-fast)
        let server_error: Option<anyhow::Error> = tokio::select! {
            result = axum::serve(listener, service.into_make_service()).with_graceful_shutdown(shutdown) => {
                result.err().map(Into::into) // None if server shut down cleanly
            }
            result = self.bg_services.wait_for_failure() => {
                // Background task failed - save error for fail-fast restart after cleanup
                match result {
                    Ok(_infallible) => unreachable!("wait_for_failure never returns Ok"),
                    Err(e) => Some(e),
                }
            }
        };

        // Graceful shutdown - even if we're failing fast, clean up properly
        info!("Shutting down background services...");
        self.bg_services.shutdown().await;

        // Close database connections
        info!("Closing database connections...");
        self.db_pools.close().await;

        // Shutdown telemetry
        info!("Shutting down telemetry...");
        telemetry::shutdown_telemetry();

        // Clean up embedded database if it exists
        if let Some(embedded_db) = self._embedded_db {
            info!("Shutting down embedded database...");
            embedded_db.stop().await?;
        }

        // If there was an error (either server or background task), propagate it after cleanup
        if let Some(e) = server_error {
            return Err(e);
        }

        Ok(())
    }
}
