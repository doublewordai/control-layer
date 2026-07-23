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
//!     let tracer_provider = dwctl::telemetry::init_telemetry(config.enable_otel_export)?;
//!
//!     // Create and start the application
//!     let app = Application::new(config, tracer_provider).await?;
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
/// Install the rustls crypto provider at process startup, before main() or any test runs.
/// This ensures every TLS client (reqwest, async-stripe, etc.) has a provider available.
#[ctor::ctor]
fn install_crypto_provider() {
    rustls::crypto::aws_lc_rs::default_provider().install_default().ok();
}

pub mod api;
pub mod auth;
pub mod config;
mod config_watcher;
pub mod connections;
mod crypto;
pub mod db;
mod email;
pub mod encryption;
mod error_enrichment;
pub mod errors;
pub mod image_normalizer;
pub mod inference;
pub mod keystore;
mod leader_election;
pub mod limits;
mod metrics;
mod notifications;
mod openapi;
mod payment_providers;
mod probes;
pub mod prompt_cache;
pub mod reasoning;
mod request_logging;
pub mod sample_files;
mod static_assets;
mod sync;
pub mod tasks;
pub mod telemetry;
mod types;
pub mod webhooks;

// Test modules
#[cfg(test)]
mod test;

use crate::metrics::errors::component::{ONWARDS_HEARTBEAT, SUPERVISOR};
use crate::{
    api::models::{
        deployments::{DeployedModelCreate, StandardModelCreate},
        users::Role,
    },
    auth::password,
    config::{CorsConfig, CorsOrigin},
    db::handlers::{Deployments, Groups, Repository, Users},
    db::models::{deployments::DeploymentCreateDBRequest, users::UserCreateDBRequest},
    metrics::GenAiMetrics,
    request_logging::serializers::{parse_ai_request, parse_ai_response},
};
use sqlx_pool_router::{DbPools, PoolProvider};

use anyhow::Context;
use auth::middleware::admin_ai_proxy_middleware;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::HeaderValue;
use axum::response::Response;
use axum::{
    Router, ServiceExt, http, middleware,
    routing::{delete, get, patch, post, put},
};
use axum_prometheus::PrometheusMetricLayerBuilder;
use bon::Builder;
pub use config::Config;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use opentelemetry::trace::TraceContextExt;
use outlet::{MultiHandler, RequestLoggerConfig, RequestLoggerLayer};
use outlet_postgres::PostgresHandler;
use request_logging::{AiResponse, ParsedAIRequest};
use sqlx::{ConnectOptions, Executor, PgPool, postgres::PgConnectOptions};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use tokio::net::TcpListener;
use tower::Layer;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};
use tracing::{debug, info, instrument, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

pub use types::{ApiKeyId, DeploymentId, GroupId, InferenceEndpointId, UserId};

#[derive(Clone)]
pub struct SharedConfig(Arc<arc_swap::ArcSwap<Config>>);

impl SharedConfig {
    pub fn new(config: Config) -> Self {
        Self(Arc::new(arc_swap::ArcSwap::from_pointee(config)))
    }

    pub fn snapshot(&self) -> Arc<Config> {
        self.0.load_full()
    }

    pub fn store(&self, config: Config) {
        self.0.store(Arc::new(config));
    }
}

impl From<Config> for SharedConfig {
    fn from(config: Config) -> Self {
        Self::new(config)
    }
}

/// Application state shared across all request handlers.
///
/// This struct contains all the shared resources needed by the API handlers,
/// including database connections, configuration, and background service managers.
///
/// # Fields
///
/// - `db`: Main PostgreSQL connection pool for application data
/// - `config`: Application configuration loaded from environment/files
/// - `outlet_db`: Optional connection pool for request logging (when enabled), uses same pool provider type as db
/// - `metrics_recorder`: Optional Prometheus metrics recorder (when enabled)
/// - `is_leader`: Whether this instance is the elected leader (for distributed deployments)
/// - `request_manager`: Fusillade batch request manager for async processing
/// - `limiters`: Resource limiters for protecting system capacity
///
/// # Example
///
/// ```ignore
/// let limiters = limits::Limiters::new(&config.limits);
/// let state = AppState::builder()
///     .db(db_pools)
///     .config(config.into())
///     .request_manager(request_manager)
///     .limiters(limiters)
///     .build();
/// ```
#[derive(Clone, Builder)]
pub struct AppState<P = DbPools>
where
    P: PoolProvider + Clone,
{
    /// Database pools (primary + optional replica).
    /// Use `.read()` for read-only queries, `.write()` for writes.
    pub db: P,
    pub config: SharedConfig,
    /// Outlet database pools for request logging. Always uses DbPools (production type).
    /// In tests, this uses DbPools without read-only enforcement (outlet is write-heavy).
    pub outlet_db: Option<DbPools>,
    pub metrics_recorder: Option<GenAiMetrics>,
    #[builder(default = false)]
    pub is_leader: bool,
    pub request_manager: Arc<fusillade_arsenal::PostgresRequestManager<P>>,
    /// Singleton commit-acknowledging response lifecycle writer.
    pub requests_writer: crate::inference::engine::writer::RequestsWriterHandle,
    /// Background task runner for enqueuing deferred work (batch population, etc.)
    pub task_runner: Arc<tasks::TaskRunner<P>>,
    /// Resource limiters for protecting system capacity.
    pub limiters: limits::Limiters,
    /// Encryption key for connection credentials, derived once at startup.
    /// `None` means connections encryption is unavailable.
    pub connections_encryption_key: Option<Vec<u8>>,
    /// Response store for Open Responses API lifecycle tracking.
    /// Reads/writes to fusillade's requests table.
    pub response_store: Arc<crate::inference::store::FusilladeResponseStore<P>>,
    /// Multi-step response_steps storage. Optional so deployments that
    /// don't use the multi-step Open Responses path can omit the
    /// wiring; the GET /v1/responses/{id} handler degrades to 404 in
    /// that case rather than panicking.
    pub response_step_manager: Option<Arc<fusillade_arsenal::PostgresResponseStepManager<P>>>,
    /// Singleton image normaliser used by the realtime middleware, the
    /// batch ingest path, the dispatcher's JIT-signing step, and the
    /// dashboard `/images/:sha256` endpoint. Built once at startup so
    /// the GCS client + ADC signer are not re-initialised per request.
    pub image_normalizer: Arc<dyn crate::image_normalizer::ImageNormalizer>,
    /// Encrypted key custody, built from `config.keystore`. `None` means it is
    /// not configured (ZDR flex disabled).
    pub keystore: Option<crate::keystore::Keystore>,
    /// Lock-free API-key metadata snapshot shared by response hot paths.
    pub api_key_cache: crate::sync::api_key_cache::ApiKeyMetadataCache,
    /// Shared cold-path resolver for hidden Flex batch keys.
    pub flex_batch_key_resolver: crate::sync::api_key_cache::FlexBatchKeyResolver,
}

impl<P> AppState<P>
where
    P: PoolProvider + Clone,
{
    pub fn current_config(&self) -> Arc<Config> {
        self.config.snapshot()
    }
}

/// Get the dwctl database migrator
pub fn migrator() -> sqlx::migrate::Migrator {
    sqlx::migrate!("./migrations")
}

/// Global Prometheus handle - ensures recorder is only installed once
static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
static AXUM_PROMETHEUS_PREFIX_SET: OnceLock<()> = OnceLock::new();

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

            // Custom histogram buckets for the cached-input-pricing layer latencies (1ms to 10s):
            // classify, tokenizer-svc call, commit, and index lookup (cache read).
            const CACHE_LATENCY_BUCKETS: &[f64] = &[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

            // Custom histogram buckets for fusillade retry attempts (0-10 retries)
            const RETRY_ATTEMPTS_BUCKETS: &[f64] = &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];

            // Custom histogram buckets for fusillade submission-epoch latencies
            // (pickup delay and submission TTFT): 60 is an exact edge because
            // the async-tier SLO ("starts within a minute") is read at it, and
            // compliance ratios are only exact at a bucket edge.
            const SUBMISSION_LATENCY_BUCKETS: &[f64] = &[1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 900.0, 1800.0, 3600.0];

            PrometheusBuilder::new()
                .set_buckets_for_metric(Matcher::Full("dwctl_analytics_lag_seconds".to_string()), ANALYTICS_LAG_BUCKETS)
                .expect("Failed to set custom buckets for dwctl_analytics_lag_seconds")
                .set_buckets_for_metric(Matcher::Full("dwctl_cache_sync_lag_seconds".to_string()), CACHE_SYNC_LAG_BUCKETS)
                .expect("Failed to set custom buckets for dwctl_cache_sync_lag_seconds")
                .set_buckets_for_metric(
                    Matcher::Full("dwctl_cache_classify_duration_seconds".to_string()),
                    CACHE_LATENCY_BUCKETS,
                )
                .expect("Failed to set custom buckets for dwctl_cache_classify_duration_seconds")
                .set_buckets_for_metric(
                    Matcher::Full("dwctl_cache_tokenizer_duration_seconds".to_string()),
                    CACHE_LATENCY_BUCKETS,
                )
                .expect("Failed to set custom buckets for dwctl_cache_tokenizer_duration_seconds")
                .set_buckets_for_metric(
                    Matcher::Full("dwctl_cache_commit_duration_seconds".to_string()),
                    CACHE_LATENCY_BUCKETS,
                )
                .expect("Failed to set custom buckets for dwctl_cache_commit_duration_seconds")
                .set_buckets_for_metric(
                    Matcher::Full("dwctl_cache_lookup_duration_seconds".to_string()),
                    CACHE_LATENCY_BUCKETS,
                )
                .expect("Failed to set custom buckets for dwctl_cache_lookup_duration_seconds")
                .set_buckets_for_metric(
                    Matcher::Full("fusillade_retry_attempts_on_success".to_string()),
                    RETRY_ATTEMPTS_BUCKETS,
                )
                .expect("Failed to set custom buckets for fusillade_retry_attempts_on_success")
                .set_buckets_for_metric(
                    Matcher::Full("fusillade_request_time_to_first_token_seconds".to_string()),
                    SUBMISSION_LATENCY_BUCKETS,
                )
                .expect("Failed to set custom buckets for fusillade_request_time_to_first_token_seconds")
                .set_buckets_for_metric(
                    Matcher::Full("fusillade_request_pickup_delay_seconds".to_string()),
                    SUBMISSION_LATENCY_BUCKETS,
                )
                .expect("Failed to set custom buckets for fusillade_request_pickup_delay_seconds")
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
            sqlx::query!("UPDATE users SET password_hash = $1 WHERE id = $2", password_hash, existing_user.id)
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
                            display_name: None,
                            hosted_on: endpoint_id,
                            description: None,
                            model_type: None,
                            capabilities: None,
                            requests_per_second: None,
                            burst_size: None,
                            capacity: None,
                            batch_capacity: None,
                            throughput: None,
                            tariffs: None,
                            provider_pricing: None,
                            sanitize_responses: None,
                            trusted: None,
                            open_responses_adapter: None,
                            reasoning_translation_overrides: None,
                            backoff_enabled: false,
                            backoff_initial_ms: 100,
                            backoff_max_ms: 5_000,
                            backoff_factor: 2.0,
                            backoff_jitter: Default::default(),
                            backoff_max_total_ms: None,
                            traffic_routing_rules: None,
                            allowed_batch_completion_windows: None,
                            metadata: None,
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

    // Update the system API key secret and ensure it has platform purpose
    // (required for admin API access used by internal services like scouter)
    let system_api_key_id = Uuid::nil();
    let new_secret = crypto::generate_api_key();
    sqlx::query!(
        "UPDATE api_keys SET secret = $1, purpose = 'platform' WHERE id = $2",
        new_secret,
        system_api_key_id
    )
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
) -> anyhow::Result<(Option<db::embedded::EmbeddedDatabase>, DbPools, DbPools, Option<DbPools>)> {
    let slow_threshold = std::time::Duration::from_millis(config.slow_statement_threshold_ms);

    // If a pool is provided (e.g., from tests), create a TestDbPools which will create a read-only replica
    let (embedded_db, pool, test_replica_pool) = if let Some(existing_pool) = pool {
        info!("Using provided database pool with TestDbPools for read/write separation");

        // Create TestDbPools which creates a read-only replica for testing
        let test_pools = sqlx_pool_router::TestDbPools::new(existing_pool.clone())
            .await
            .expect("Failed to create TestDbPools");

        // Extract the write and read pools to create a DbPools with proper read/write separation
        let replica_pool = test_pools.read().clone();
        (None, existing_pool, Some(replica_pool))
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
        let connect_opts = PgConnectOptions::from_str(&database_url)?.log_slow_statements(log::LevelFilter::Warn, slow_threshold);
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
            .connect_with(connect_opts)
            .await?;
        (_embedded_db, pool, None)
    };

    migrator().run(&pool).await?;

    // Create replica pool if configured (or use test replica if in test mode)
    let db_pools = if let Some(test_replica) = test_replica_pool {
        info!("Using test replica pool with read-only enforcement");
        DbPools::with_replica(pool, test_replica)
    } else if let Some(replica_url) = config.database.external_replica_url() {
        info!("Setting up read replica pool");
        let replica_settings = config.database.main_replica_pool_settings();
        let replica_opts = PgConnectOptions::from_str(replica_url)?.log_slow_statements(log::LevelFilter::Warn, slow_threshold);
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
            .connect_with(replica_opts)
            .await?;
        DbPools::with_replica(pool, replica_pool)
    } else {
        DbPools::new(pool)
    };

    // Get connection options from the main pool to create schema-based child pools
    let main_connect_opts = db_pools.connect_options().as_ref().clone();

    // Helper to create a pool with schema-specific search_path
    // Reuses connection URLs from main pool (both primary and replica if configured)
    // Sets search_path at the connection level (via PgConnectOptions) rather than using
    // after_connect hooks, ensuring it cannot be unset and works reliably with replicas
    // Uses eager connection (connect_with) to respect min_connections at startup
    async fn create_schema_pool(
        schema: String,
        opts: sqlx::postgres::PgConnectOptions,
        settings: &config::PoolSettings,
    ) -> Result<sqlx::PgPool, sqlx::Error> {
        // Set search_path directly in connection options so PostgreSQL enforces it
        // This is more reliable than after_connect hooks, especially with replicas
        // The options() method formats as: "-c key=value"
        let search_path_key = "search_path".to_string();
        let search_path_value = schema.clone();
        info!("Setting search_path={} via connection options for schema pool", schema);
        let opts_with_schema = opts.options([(search_path_key, search_path_value)]);

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
            .connect_with(opts_with_schema)
            .await
    }

    // Setup fusillade batch processing pool
    info!("Setting up fusillade batch processing pool");
    let fusillade_pools = match config.database.fusillade() {
        config::ComponentDb::Schema {
            name, pool: pool_settings, ..
        } => {
            // Create primary pool using main's connection, with schema-specific search_path
            let primary = create_schema_pool(name.clone(), main_connect_opts.clone(), pool_settings).await?;
            primary.execute(&*format!("CREATE SCHEMA IF NOT EXISTS {name}")).await?;

            // Create replica pool if main has one configured (inherits main's replica connection)
            if db_pools.has_replica() {
                info!("Setting up fusillade read replica (schema mode)");
                let replica_opts = db_pools.read().connect_options().as_ref().clone();
                let replica_pool_settings = config.database.fusillade().replica_pool_settings();
                let replica = create_schema_pool(name.clone(), replica_opts, replica_pool_settings).await?;
                DbPools::with_replica(primary, replica)
            } else {
                DbPools::new(primary)
            }
        }
        config::ComponentDb::Dedicated {
            url,
            replica_url,
            pool: pool_settings,
            ..
        } => {
            info!("Using dedicated database for fusillade");
            let connect_opts = PgConnectOptions::from_str(url)?.log_slow_statements(log::LevelFilter::Warn, slow_threshold);
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
                .connect_with(connect_opts)
                .await?;

            if let Some(replica_url) = replica_url {
                info!("Setting up fusillade read replica");
                let replica_pool_settings = config.database.fusillade().replica_pool_settings();
                let replica_opts = PgConnectOptions::from_str(replica_url)?.log_slow_statements(log::LevelFilter::Warn, slow_threshold);
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
                    .connect_with(replica_opts)
                    .await?;
                DbPools::with_replica(primary, replica)
            } else {
                DbPools::new(primary)
            }
        }
    };
    fusillade_arsenal::migrator().run(&*fusillade_pools).await?;

    // Run underway migrations (background task queue)
    underway::run_migrations(&*db_pools).await?;

    // Setup outlet schema and pool if request logging is enabled
    let outlet_pools = if config.enable_request_logging {
        info!("Setting up outlet request logging pool (logging enabled)");
        let pools = match config.database.outlet() {
            config::ComponentDb::Schema {
                name, pool: pool_settings, ..
            } => {
                // Create primary pool using main's connection, with schema-specific search_path
                let primary = create_schema_pool(name.clone(), main_connect_opts.clone(), pool_settings).await?;
                primary.execute(&*format!("CREATE SCHEMA IF NOT EXISTS {name}")).await?;

                // Create replica pool if main has one configured (inherits main's replica connection)
                if db_pools.has_replica() {
                    info!("Setting up outlet read replica (schema mode)");
                    let replica_opts = db_pools.read().connect_options().as_ref().clone();
                    let replica_pool_settings = config.database.outlet().replica_pool_settings();
                    let replica = create_schema_pool(name.clone(), replica_opts, replica_pool_settings).await?;
                    DbPools::with_replica(primary, replica)
                } else {
                    DbPools::new(primary)
                }
            }
            config::ComponentDb::Dedicated {
                url,
                replica_url,
                pool: pool_settings,
                ..
            } => {
                info!("Using dedicated database for outlet");
                let connect_opts = PgConnectOptions::from_str(url)?.log_slow_statements(log::LevelFilter::Warn, slow_threshold);
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
                    .connect_with(connect_opts)
                    .await?;

                if let Some(replica_url) = replica_url {
                    info!("Setting up outlet read replica");
                    let replica_pool_settings = config.database.outlet().replica_pool_settings();
                    let replica_opts = PgConnectOptions::from_str(replica_url)?.log_slow_statements(log::LevelFilter::Warn, slow_threshold);
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
                        .connect_with(replica_opts)
                        .await?;
                    DbPools::with_replica(primary, replica)
                } else {
                    DbPools::new(primary)
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

/// Build the base CORS layer (methods, headers, max-age, exposed headers) from
/// configuration. Origin/credential handling differs by mode — see [`apply_cors`].
fn create_cors_layer(cors: &CorsConfig) -> anyhow::Result<CorsLayer> {
    // Parse exposed headers as HeaderName
    let exposed: Vec<http::HeaderName> = cors.exposed_headers.iter().filter_map(|h| h.parse().ok()).collect();

    let mut cors_layer = CorsLayer::new()
        .allow_methods([
            http::Method::GET,
            http::Method::POST,
            http::Method::PUT,
            http::Method::DELETE,
            http::Method::PATCH,
            http::Method::OPTIONS,
        ])
        .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION, http::header::ACCEPT])
        .expose_headers(exposed);

    if cors.allow_any_origin_without_credentials {
        // Public mode: reflect ANY origin, credentials OFF at this layer. The CORS
        // spec forbids credentials alongside a wildcard, so we never grant them to
        // all origins here — first-party credentials are re-added per-origin by the
        // middleware in `apply_cors`.
        info!(
            "Configuring CORS: reflecting any origin without credentials (credentialed allowlist: {:?})",
            cors.allowed_origins
        );
        cors_layer = cors_layer.allow_origin(AllowOrigin::mirror_request()).allow_credentials(false);
    } else {
        // Allowlist mode (default): only configured origins; credentials governed
        // by `allow_credentials`.
        let mut origins = Vec::new();
        for origin in &cors.allowed_origins {
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
        cors_layer = cors_layer.allow_origin(origins).allow_credentials(cors.allow_credentials);
    }

    if let Some(max_age) = cors.max_age {
        cors_layer = cors_layer.max_age(std::time::Duration::from_secs(max_age));
    }

    Ok(cors_layer)
}

/// Apply CORS handling to the router.
///
/// When `allow_any_origin_without_credentials` is set, every origin is allowed
/// *without* credentials, and first-party `allowed_origins` additionally receive
/// `Access-Control-Allow-Credentials: true` via [`stamp_credentials_for_allowlisted_origins`].
/// Otherwise this is the plain allowlist layer.
fn apply_cors(router: Router, cors: &CorsConfig) -> anyhow::Result<Router> {
    if cors.allow_any_origin_without_credentials {
        warn!(
            "CORS: allow_any_origin_without_credentials is enabled — any origin may call the API without credentials; credentialed CORS stays limited to allowed_origins"
        );
    }

    let router = router.layer(create_cors_layer(cors)?);

    // In public mode the CORS layer left credentials off so arbitrary origins
    // never receive them. Re-grant `Access-Control-Allow-Credentials` to the
    // first-party allowlist via an outer middleware — it must wrap the CORS layer
    // so it also post-processes the preflight responses the layer short-circuits.
    if cors.allow_any_origin_without_credentials && cors.allow_credentials {
        let allowlist = Arc::new(origins_to_grant_credentials(cors)?);
        Ok(router.layer(middleware::from_fn_with_state(allowlist, stamp_credentials_for_allowlisted_origins)))
    } else {
        Ok(router)
    }
}

/// Exact-match `Origin` values that should be granted credentialed CORS, derived
/// from the configured `allowed_origins`.
///
/// Only `Url` entries are collected: a wildcard never grants credentials (the
/// Fetch spec requires an explicit origin for credentialed CORS), so any
/// `CorsOrigin::Wildcard` is intentionally skipped here.
fn origins_to_grant_credentials(cors: &CorsConfig) -> anyhow::Result<Vec<HeaderValue>> {
    let mut out = Vec::new();
    for origin in &cors.allowed_origins {
        if let CorsOrigin::Url(url) = origin {
            let url_str = url.as_str().trim_end_matches('/');
            out.push(url_str.parse::<HeaderValue>()?);
        }
    }
    Ok(out)
}

/// Stamp `Access-Control-Allow-Credentials: true` onto responses whose request
/// `Origin` is in the first-party allowlist. Used only in public CORS mode, where
/// the CORS layer reflects every origin but withholds credentials.
async fn stamp_credentials_for_allowlisted_origins(
    State(allowlist): State<Arc<Vec<HeaderValue>>>,
    request: Request,
    next: middleware::Next,
) -> Response {
    let origin = request.headers().get(http::header::ORIGIN).cloned();
    let allow = origin.as_ref().is_some_and(|o| allowlist.iter().any(|allowed| allowed == o));
    let mut response = next.run(request).await;
    if allow {
        response
            .headers_mut()
            .insert(http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
    }
    response
}

/// Build the (name, value) pairs for the configured security response headers.
///
/// Returns an empty list when the feature is disabled. Opt-in headers
/// (CSP, CSP-Report-Only, HSTS) are included only when their configured value
/// is non-empty. Invalid header values are rejected with an error so a
/// misconfiguration surfaces at startup rather than silently dropping a header.
fn security_header_pairs(cfg: &crate::config::SecurityHeadersConfig) -> anyhow::Result<Vec<(http::HeaderName, http::HeaderValue)>> {
    let mut pairs: Vec<(http::HeaderName, http::HeaderValue)> = Vec::new();
    if !cfg.enabled {
        return Ok(pairs);
    }

    let mut push = |name: &'static str, value: &str| -> anyhow::Result<()> {
        if value.is_empty() {
            return Ok(());
        }
        let header_value = http::HeaderValue::from_str(value).with_context(|| format!("invalid value for security header `{name}`"))?;
        pairs.push((http::HeaderName::from_static(name), header_value));
        Ok(())
    };

    // X-Content-Type-Options has a single meaningful value, so it is not
    // configurable — it is always `nosniff` while the feature is enabled.
    push("x-content-type-options", "nosniff")?;
    push("x-frame-options", &cfg.frame_options)?;
    push("referrer-policy", &cfg.referrer_policy)?;
    push("permissions-policy", &cfg.permissions_policy)?;
    push("content-security-policy", &cfg.content_security_policy)?;
    push("content-security-policy-report-only", &cfg.content_security_policy_report_only)?;
    push("strict-transport-security", &cfg.strict_transport_security)?;

    Ok(pairs)
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
/// - `analytics_sender`: Optional sender for analytics records (from background services)
/// - `metrics_recorder`: Optional GenAI metrics recorder (created before background services)
///
/// # Returns
///
/// Returns the fully configured router ready to be served.
///
/// # Errors
///
/// Returns an error if CORS configuration is invalid, a configured security
/// response header has an invalid value, or metrics initialization fails.
#[instrument(skip_all)]
pub async fn build_router(
    state: &mut AppState,
    onwards_router: Router,
    analytics_sender: Option<request_logging::batcher::AnalyticsSender>,
    metrics_recorder: Option<GenAiMetrics>,
    strict_mode: bool,
    inference_middleware_state: Option<crate::inference::middleware::InferenceMiddlewareState>,
) -> anyhow::Result<Router> {
    let config = state.current_config();

    // Setup request logging and/or analytics based on config flags
    //
    // These can be enabled independently:
    // - enable_request_logging: stores raw request/response bodies via outlet-postgres
    // - enable_analytics: stores analytics data, handles billing, records Prometheus metrics
    //
    // Both require the RequestLoggerLayer to capture request/response data, but use
    // different handlers to process that data.
    let request_logging_enabled = state.outlet_db.is_some() && config.enable_request_logging;
    let analytics_enabled = config.enable_analytics;

    let response_lifecycle_enabled = inference_middleware_state.is_some();
    let outlet_layer = if request_logging_enabled || analytics_enabled || response_lifecycle_enabled {
        // Store the metrics recorder in state (created earlier in Application::new)
        state.metrics_recorder = metrics_recorder;

        // Build handler chain based on config
        let mut multi_handler = MultiHandler::new();

        // Add PostgresHandler for request logging if enabled
        if request_logging_enabled {
            let outlet_pool = state.outlet_db.as_ref().expect("outlet_db checked above");
            let postgres_handler = PostgresHandler::<DbPools, ParsedAIRequest, AiResponse>::from_pool_provider(outlet_pool.clone())
                .await
                .expect("Failed to create PostgresHandler for request logging")
                .with_request_serializer(parse_ai_request)
                .with_response_serializer(parse_ai_response);
            // TRANSITIONAL (dwctl ZDR): guard the analytics logger so plaintext
            // ZDR bodies (decrypted for the upstream call, captured on the
            // loopback) never land in http_requests / http_responses. The marker
            // header rides on the dispatch; see ZdrBodyScrubber.
            let postgres_handler = crate::inference::engine::outlet_handler::ZdrBodyScrubber::new(postgres_handler);
            multi_handler = multi_handler.with(postgres_handler);
        }

        // Add AnalyticsHandler for analytics/billing if enabled
        // The batcher is spawned in setup_background_services and managed by BackgroundServices
        if let Some(sender) = analytics_sender {
            let analytics_handler = request_logging::AnalyticsHandler::new(sender, uuid::Uuid::new_v4(), config.as_ref().clone());
            multi_handler = multi_handler.with(analytics_handler);
        }

        // Add FusilladeOutletHandler so completed responses get written to
        // fusillade via the in-process RequestsWriter. We only attach when
        // the inference middleware is active. AppState owns the singleton
        // writer handle, avoiding a second router parameter that could point
        // at a different consumer.
        if let Some(rms) = inference_middleware_state.as_ref() {
            let fusillade_handler = crate::inference::engine::outlet_handler::FusilladeOutletHandler::new(
                state.requests_writer.clone(),
                rms.api_key_cache.clone(),
            );
            multi_handler = multi_handler.with(fusillade_handler);
        }

        // Only create layer if at least one handler is enabled (should always be true here)
        if multi_handler.is_empty() {
            None
        } else {
            let outlet_config = RequestLoggerConfig {
                capture_request_body: true,
                capture_response_body: true,
                path_filter: None, // No path filter needed - applied directly to ai_router
                ..Default::default()
            };
            Some(RequestLoggerLayer::new(outlet_config, multi_handler))
        }
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
        // CLI login endpoints — under /admin/api/v1/ so they route through the app,
        // not through oauth2-proxy (which intercepts all /authentication/* paths).
        .route("/auth/cli-callback", get(api::handlers::auth::cli_callback))
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
            patch(api::handlers::api_keys::update_user_api_key),
        )
        .route(
            "/users/{user_id}/api-keys/{id}",
            delete(api::handlers::api_keys::delete_user_api_key),
        )
        // Webhooks as user sub-resources
        .route("/users/{user_id}/webhooks", get(api::handlers::webhooks::list_webhooks))
        .route("/users/{user_id}/webhooks", post(api::handlers::webhooks::create_webhook))
        .route("/users/{user_id}/webhooks/{webhook_id}", get(api::handlers::webhooks::get_webhook))
        .route(
            "/users/{user_id}/webhooks/{webhook_id}",
            patch(api::handlers::webhooks::update_webhook),
        )
        .route(
            "/users/{user_id}/webhooks/{webhook_id}",
            delete(api::handlers::webhooks::delete_webhook),
        )
        .route(
            "/users/{user_id}/webhooks/{webhook_id}/rotate-secret",
            post(api::handlers::webhooks::rotate_secret),
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
        .route("/auto-topup/enable", post(api::handlers::payments::enable_auto_topup))
        .route("/auto-topup/disable", post(api::handlers::payments::disable_auto_topup))
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
        .route("/models/{id}/cache-pricing", get(api::handlers::cache_pricing::get_cache_pricing))
        .route(
            "/models/{id}/cache-pricing",
            put(api::handlers::cache_pricing::enable_cache_pricing),
        )
        .route(
            "/models/{id}/cache-pricing",
            delete(api::handlers::cache_pricing::disable_cache_pricing),
        )
        .route(
            "/provider-display-configs",
            get(api::handlers::provider_display_configs::list_provider_display_configs),
        )
        .route(
            "/provider-display-configs",
            post(api::handlers::provider_display_configs::create_provider_display_config),
        )
        .route(
            "/provider-display-configs/{provider_key}",
            get(api::handlers::provider_display_configs::get_provider_display_config),
        )
        .route(
            "/provider-display-configs/{provider_key}",
            patch(api::handlers::provider_display_configs::update_provider_display_config),
        )
        .route(
            "/provider-display-configs/{provider_key}",
            delete(api::handlers::provider_display_configs::delete_provider_display_config),
        )
        .route(
            "/provider-display-configs/{provider_key}/icon",
            get(api::handlers::provider_display_configs::get_provider_display_config_icon),
        )
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
        // Image content store — short-lived signed URL for normalised
        // image bytes the user has previously submitted. Authorisation
        // is per-user via the image_access table.
        .route("/images/{sha256}", get(api::handlers::images::get_image))
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
        // Organization management
        .route("/organizations", get(api::handlers::organizations::list_organizations))
        .route("/organizations", post(api::handlers::organizations::create_organization))
        .route("/organizations/{id}", get(api::handlers::organizations::get_organization))
        .route("/organizations/{id}", patch(api::handlers::organizations::update_organization))
        .route("/organizations/{id}", delete(api::handlers::organizations::delete_organization))
        // Organization membership
        .route("/organizations/{id}/members", get(api::handlers::organizations::list_members))
        .route("/organizations/{id}/members", post(api::handlers::organizations::add_member))
        .route(
            "/organizations/{id}/members/{user_id}",
            patch(api::handlers::organizations::update_member_role),
        )
        .route(
            "/organizations/{id}/members/{user_id}",
            delete(api::handlers::organizations::remove_member),
        )
        // Leave organization (self-removal)
        .route("/organizations/{id}/leave", post(api::handlers::organizations::leave_organization))
        // Organization invites
        .route("/organizations/{id}/invites", post(api::handlers::organizations::invite_member))
        .route(
            "/organizations/{id}/invites/{invite_id}",
            delete(api::handlers::organizations::cancel_invite),
        )
        .route(
            "/organizations/invites/{token}",
            get(api::handlers::organizations::get_invite_details),
        )
        .route(
            "/organizations/invites/{token}/accept",
            post(api::handlers::organizations::accept_invite),
        )
        .route(
            "/organizations/invites/{token}/decline",
            post(api::handlers::organizations::decline_invite),
        )
        // Email-change confirmation (GET so the link in the verification email works
        // when clicked from any mail client; no auth — secret token is the proof of
        // mailbox possession).
        .route(
            "/organizations/email-change/{token}/confirm",
            get(api::handlers::organizations::confirm_email_change),
        )
        // User's organizations (sub-resource on users)
        .route(
            "/users/{user_id}/organizations",
            get(api::handlers::organizations::list_user_organizations),
        )
        // Organization session context (validates membership, client stores org ID for X-Organization-Id header)
        .route("/session/organization", post(api::handlers::organizations::set_active_organization))
        // Support requests
        .route("/support/requests", post(api::handlers::support::submit_support_request))
        .route("/batches/requests", get(api::handlers::batch_requests::list_batch_requests))
        .route(
            "/batches/requests/{request_id}",
            get(api::handlers::batch_requests::get_batch_request),
        )
        .route(
            "/batches/requests/{request_id}",
            delete(api::handlers::batch_requests::delete_batch_request),
        )
        .route("/requests", get(api::handlers::requests::list_requests))
        .route("/requests/aggregate", get(api::handlers::requests::aggregate_requests))
        .route("/requests/aggregate-by-user", get(api::handlers::requests::aggregate_by_user))
        .route("/usage", get(api::handlers::requests::get_usage))
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
        .route("/probes/{id}/statistics", get(api::handlers::probes::get_statistics))
        // Queue monitoring
        .route(
            "/monitoring/pending-request-counts",
            get(api::handlers::queue::get_pending_request_counts),
        )
        // Tool sources CRUD
        .route("/tool-sources", get(api::handlers::tool_sources::list_tool_sources))
        .route("/tool-sources", post(api::handlers::tool_sources::create_tool_source))
        .route("/tool-sources/{id}", get(api::handlers::tool_sources::get_tool_source))
        .route("/tool-sources/{id}", patch(api::handlers::tool_sources::update_tool_source))
        .route("/tool-sources/{id}", delete(api::handlers::tool_sources::delete_tool_source))
        // Tool sources ↔ deployment attachment
        .route(
            "/deployments/{id}/tool-sources",
            get(api::handlers::tool_sources::list_deployment_tool_sources),
        )
        .route(
            "/deployments/{id}/tool-sources/{source_id}",
            axum::routing::put(api::handlers::tool_sources::attach_tool_source_to_deployment),
        )
        .route(
            "/deployments/{id}/tool-sources/{source_id}",
            delete(api::handlers::tool_sources::detach_tool_source_from_deployment),
        )
        // Tool sources ↔ group attachment
        .route(
            "/groups/{id}/tool-sources",
            get(api::handlers::tool_sources::list_group_tool_sources),
        )
        .route(
            "/groups/{id}/tool-sources/{source_id}",
            axum::routing::put(api::handlers::tool_sources::attach_tool_source_to_group),
        )
        .route(
            "/groups/{id}/tool-sources/{source_id}",
            delete(api::handlers::tool_sources::detach_tool_source_from_group),
        )
        // Connections (external data sources)
        .route("/connections", post(api::handlers::connections::create_connection))
        .route("/connections", get(api::handlers::connections::list_connections))
        .route("/connections/{connection_id}", get(api::handlers::connections::get_connection))
        .route(
            "/connections/{connection_id}",
            delete(api::handlers::connections::delete_connection),
        )
        .route(
            "/connections/{connection_id}/test",
            post(api::handlers::connections::test_connection),
        )
        .route(
            "/connections/{connection_id}/files",
            get(api::handlers::connections::list_connection_files),
        )
        .route(
            "/connections/{connection_id}/synced-keys",
            get(api::handlers::connections::list_synced_keys),
        )
        .route("/connections/{connection_id}/sync", post(api::handlers::connections::trigger_sync))
        .route("/connections/{connection_id}/syncs", get(api::handlers::connections::list_syncs))
        .route(
            "/connections/{connection_id}/syncs/{sync_id}",
            get(api::handlers::connections::get_sync),
        )
        .route(
            "/connections/{connection_id}/syncs/{sync_id}/entries",
            get(api::handlers::connections::list_sync_entries),
        );

    let api_routes_with_state = api_routes.with_state(state.clone());

    // Batches API routes (files + batches) - conditionally enabled under /ai/v1
    let batches_routes = if config.batches.enabled {
        // File upload route with custom body limit (other routes use default)
        // 0 = unlimited (disable body limit), otherwise set max size
        let file_upload_limit = config.limits.files.max_file_size;
        let body_limit_layer = if file_upload_limit == 0 {
            DefaultBodyLimit::disable()
        } else {
            // Add overhead for multipart encoding (headers, boundaries, etc.)
            let body_limit_u64 = file_upload_limit.saturating_add(limits::MULTIPART_OVERHEAD);
            // Clamp to usize::MAX to avoid truncation when converting to usize
            let body_limit = usize::try_from(body_limit_u64).unwrap_or(usize::MAX);
            DefaultBodyLimit::max(body_limit)
        };
        let file_router = Router::new().route("/files", post(api::handlers::files::upload_file).layer(body_limit_layer));

        Some(
            Router::new()
                // Files management - merge file upload route with custom body limit
                .merge(file_router)
                .route("/files", get(api::handlers::files::list_files))
                .route("/files/{file_id}", get(api::handlers::files::get_file))
                .route("/files/{file_id}", delete(api::handlers::files::delete_file))
                .route("/files/{file_id}/content", get(api::handlers::files::get_file_content))
                .route("/files/{file_id}/cost-estimate", get(api::handlers::files::get_file_cost_estimate))
                // Responses retrieval (Open Responses API)
                .route("/responses/{response_id}", get(crate::inference::handler::get_response))
                .route("/responses/{response_id}", delete(crate::inference::handler::delete_response))
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

    // ── onwards proxy middleware stack (order matters) ───────────────────────────────
    //
    // Tower applies layers inner-first, so the LAST `.layer()` call is the OUTERMOST
    // wrapper: on a request it runs first; on the response it runs last. The stack below,
    // outermost → innermost (i.e. reverse of the code order), is:
    //
    //   translation  →  responses_mw  →  outlet (logging/billing)
    //                →  cache  →  error_enrichment  →  image_normalizer
    //                →  tool_injection  →  models_route  →  onwards
    //
    // Why this order:
    //   • outlet outermost (of the body editors): it logs the request **as the customer
    //     sent it** (cache_control markers intact, original image URLs, pre tool-injection)
    //     and captures the response **after** cache injection, so billing sees cache_* usage.
    //   • cache inner to outlet, but OUTER to the body-mutating layers: it must hash the
    //     ORIGINAL request body. The image normaliser rewrites image URLs to per-request
    //     signed URLs (fresh expiry + V4 signature each call — NOT byte-stable), so hashing
    //     after it would make every image request a unique key → zero cache hits. Sitting
    //     before the reject-capable layers also means a request they 4xx (unfetchable/
    //     forbidden image) never gets a committed write — the success gate vetoes it.
    //   • image_normalizer before tool_injection: it fetches/sanitises external image URLs
    //     (and can reject the request) before tools are spliced in.
    //   • tool_injection innermost: the body onwards forwards upstream is fully resolved.
    //
    // Each block below adds one layer; the inline notes cover that layer's specifics.

    // Serve authenticated OpenAI-shaped model discovery from the control-layer
    // database using a real exact route. Other AI paths fall through to the
    // existing onwards router. Because this route is inserted before the shared
    // onwards middleware stack is layered on, request logging and protocol
    // translation still apply to `/models` just like other AI routes.
    let onwards_router = Router::new()
        .route("/models", get(api::handlers::ai_models::list_ai_models))
        .fallback_service(onwards_router);

    // Apply tool injection middleware to the onwards router so that per-request tool
    // schemas are resolved and injected into the request body before onwards processes it.
    let tool_injection_state = crate::inference::tools::ToolInjectionState {
        db: state.db.write().clone(),
    };
    let onwards_router = onwards_router.layer(middleware::from_fn_with_state(
        tool_injection_state,
        crate::inference::tools::tool_injection_middleware,
    ));

    // Apply the image-input normaliser middleware. This runs BEFORE
    // tool_injection in request flow (i.e. as an outer Tower layer added
    // after the inner one). For each `/chat/completions` and `/responses`
    // request, it walks the body for HTTP(S) `image_url` values, fetches +
    // stores them via `image_normalizer`, and substitutes signed URLs into
    // the body before the request reaches onwards.
    let onwards_router = {
        let cfg = state.current_config();
        // Re-use the AppState-bound singleton built once at startup.
        let normalizer = state.image_normalizer.clone();
        let realtime_ttl = cfg.image_normalizer.signing.realtime_ttl();
        let image_normalizer_state = crate::inference::image_normalizer_middleware::ImageNormalizerMiddlewareState {
            enabled: cfg.image_normalizer.enabled,
            normalizer,
            realtime_ttl,
            pool: Some(state.db.write().clone()),
        };
        onwards_router.layer(middleware::from_fn_with_state(
            image_normalizer_state,
            crate::inference::image_normalizer_middleware::image_normalizer_middleware,
        ))
    };

    // Apply error enrichment middleware to onwards router (before outlet logging)
    let onwards_router = onwards_router.layer(middleware::from_fn_with_state(
        state.db.write().clone(),
        error_enrichment::error_enrichment_middleware,
    ));

    // Apply the cached-input pricing layer (dwctl-owned). Placed inner
    // to outlet so the billing/analytics capture sees the injected `cache_*` usage
    // fields, but OUTER to the body-mutating layers (image normaliser, tool
    // injection) so the classifier hashes the original user body — stable across
    // requests, which per-request signed image URLs would otherwise break. Added
    // only when enabled; otherwise the stack is byte-identical to today.
    let onwards_router = {
        let cfg = state.current_config();
        if cfg.cache.enabled {
            let pool = state.db.write().clone();
            let classifier = crate::prompt_cache::Classifier::new(
                crate::prompt_cache::PrincipalResolver::new(pool.clone()),
                crate::prompt_cache::ModelConfigResolver::new(pool.clone()),
                crate::prompt_cache::TokenizerClient::new(cfg.cache.tokenizer_url.clone()),
                Arc::new(crate::prompt_cache::PostgresIndex::new(pool, cfg.cache.index_conn_retries)),
                crate::prompt_cache::TierPolicy::from_config(&cfg.cache.enabled_ttls, &cfg.cache.default_ttl),
                crate::prompt_cache::TelemetryPolicy::from_config(
                    cfg.cache.telemetry_blocks.strip_from_prompt,
                    &cfg.cache.telemetry_blocks.prefixes,
                ),
            );
            // Bound the cache layer's body buffer by the same limit onwards uses (0 =
            // unlimited), so it's never more restrictive than the entry point.
            let body_limit = match cfg.limits.requests.max_body_size {
                0 => usize::MAX,
                n => usize::try_from(n).unwrap_or(usize::MAX),
            };
            tracing::info!("Cached-input pricing enabled - wiring cache layer into onwards stack");
            onwards_router.layer(middleware::from_fn_with_state(
                crate::prompt_cache::CacheLayerState::new(
                    classifier,
                    body_limit,
                    std::time::Duration::from_secs(cfg.cache.classify_deadline_secs),
                ),
                crate::prompt_cache::cache_middleware,
            ))
        } else {
            onwards_router
        }
    };

    // Apply request logging layer only to onwards router
    let onwards_router = if let Some(outlet_layer) = outlet_layer.clone() {
        onwards_router.layer(outlet_layer)
    } else {
        onwards_router
    };

    // Apply inference middleware to create pending fusillade rows for inference requests.
    // This runs BEFORE outlet (outer layer executes first), so the X-Onwards-Response-Id
    // header is set before outlet captures the request and passes it to FusilladeOutletHandler.
    let onwards_router = if let Some(rms) = inference_middleware_state {
        onwards_router.layer(middleware::from_fn_with_state(
            rms,
            crate::inference::middleware::inference_middleware,
        ))
    } else {
        onwards_router
    };

    // Apply the generic edge protocol-translation middleware as the OUTERMOST
    // layer on the onwards router. On the request path it runs first, so any
    // foreign-protocol request (today: Anthropic `/v1/messages` and `/v1/models`)
    // is translated before model discovery, image_normalizer, tool_injection,
    // and onwards see it. On the response path it runs last, so only the final
    // client bytes are reframed back into the foreign protocol. Native OpenAI
    // requests match no translator and pass through untouched.
    let onwards_router = {
        // Bound the body the translation middleware buffers by the same cap as the
        // rest of the inference path (limits.requests.max_body_size, 0 = unlimited).
        let translation_body_limit = match config.limits.requests.max_body_size {
            0 => usize::MAX,
            n => usize::try_from(n).unwrap_or(usize::MAX),
        };
        let translators: Vec<std::sync::Arc<dyn crate::inference::translation::ProtocolTranslator>> = vec![
            // Pass cache.enabled so the translator only emits the top-level automatic-caching marker
            // when the cache middleware is present to consume + strip it (else it would leak upstream).
            std::sync::Arc::new(crate::inference::translation::anthropic::AnthropicMessages::new(
                config.cache.enabled,
            )),
            std::sync::Arc::new(crate::inference::translation::anthropic::models::AnthropicModels),
        ];
        let translation_registry =
            crate::inference::translation::TranslationRegistry::new(translators).with_max_body_size(translation_body_limit);
        onwards_router.layer(middleware::from_fn_with_state(
            translation_registry,
            crate::inference::translation::middleware::translation_middleware,
        ))
    };

    // Build the app with admin API and onwards proxy nested. serve the (restricted) openai spec.
    // Strict mode requires different nesting:
    // - Batches routes (no /v1 prefix) need to be at /ai/v1/files, /ai/v1/batches
    // - Onwards strict routes (with /v1 prefix) need to be at /ai so /ai/v1/chat/completions matches /v1/chat/completions
    // Non-strict mode:
    // - Both batches and onwards can be merged and nested at /ai/v1 (catchall handles everything)
    let mut router = Router::new()
        .route("/healthz", get(|| async { "OK" }))
        // Webhook routes (external services, not part of client API docs)
        .route("/webhooks/payments", post(api::handlers::payments::webhook_handler))
        .with_state(state.clone())
        .merge(auth_routes);

    // Add AI routes with appropriate nesting based on strict mode
    if strict_mode {
        // Strict mode: combine batches and onwards before nesting so the shared
        // `/models` route wrapper can fall through to onwards without competing
        // with a second `/ai/v1` fallback.
        let ai_router = if let Some(batches) = batches_routes {
            batches.merge(onwards_router)
        } else {
            onwards_router
        };
        router = router.nest("/ai/v1", ai_router);
    } else {
        // Non-strict mode: merge batches + onwards, nest at /ai/v1
        let ai_router = if let Some(batches) = batches_routes {
            batches.merge(onwards_router)
        } else {
            onwards_router
        };
        router = router.nest("/ai/v1", ai_router);
    }

    // OpenAPI spec routes. Both surfaces are gated by extractors in the
    // handlers (Admin → admin/PlatformManager; AI → any authenticated
    // identity) and can be disabled entirely via `config.openapi`. The
    // Admin surface is opt-in because the spec maps the full internal
    // management API. When disabled, we mount an explicit 404 stub so
    // probes can't tell the route exists (the SPA static fallback would
    // otherwise return the dashboard HTML).
    let not_found = || async { axum::http::StatusCode::NOT_FOUND };
    let openapi_router = Router::new()
        .route(
            "/admin/openapi.json",
            if config.openapi.admin_enabled {
                get(api::handlers::openapi_docs::admin_openapi_json)
            } else {
                get(not_found)
            },
        )
        .route(
            "/admin/docs",
            if config.openapi.admin_enabled {
                get(api::handlers::openapi_docs::admin_openapi_docs)
            } else {
                get(not_found)
            },
        )
        .route(
            "/ai/openapi.json",
            if config.openapi.ai_enabled {
                get(api::handlers::openapi_docs::ai_openapi_json)
            } else {
                get(not_found)
            },
        )
        .route(
            "/ai/docs",
            if config.openapi.ai_enabled {
                get(api::handlers::openapi_docs::ai_openapi_docs)
            } else {
                get(not_found)
            },
        );

    let router = router
        .nest("/admin/api/v1", api_routes_with_state)
        .merge(openapi_router.with_state(state.clone()))
        .fallback_service(fallback.with_state(state.clone()))
        .with_state(state.clone());

    // Apply CORS to the main router (request logging already applied to
    // onwards_router above). When configured, this also opens uncredentialed
    // CORS to any origin while keeping cookie-credentialed access first-party.
    let mut router = apply_cors(router, &config.auth.security.cors)?;

    // Apply browser security response headers. `if_not_present` means any
    // stricter per-route header (e.g. `Referrer-Policy: no-referrer` on
    // sensitive auth responses) is preserved rather than overwritten.
    for (name, value) in security_header_pairs(&config.auth.security.headers)? {
        router = router.layer(SetResponseHeaderLayer::if_not_present(name, value));
    }

    // Add Prometheus metrics if enabled
    if config.enable_metrics {
        let metric_handle = get_or_install_prometheus_handle();

        let prometheus_layer = if AXUM_PROMETHEUS_PREFIX_SET.set(()).is_ok() {
            PrometheusMetricLayerBuilder::new()
                .with_prefix("dwctl")
                .with_metrics_from_fn(move || metric_handle.clone())
                .build_pair()
                .0
        } else {
            PrometheusMetricLayerBuilder::new()
                .with_metrics_from_fn(move || metric_handle.clone())
                .build_pair()
                .0
        };

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

    // Add tracing layer with OTel-compatible span names and HTTP semantic conventions.
    // Only trace_id and otel.name are tracing span fields (visible in fmt log output).
    // All other attributes are set via OpenTelemetrySpanExt::set_attribute() so they're
    // exported to the trace backend but don't clutter log lines.
    // Reference: https://opentelemetry.io/docs/specs/semconv/http/http-spans/
    let router = router.layer(middleware::from_fn(inject_trace_id)).layer(
        TraceLayer::new_for_http()
            .make_span_with(|request: &http::Request<_>| {
                let path = request.uri().path();
                let route = request
                    .extensions()
                    .get::<axum::extract::MatchedPath>()
                    .map(|mp| mp.as_str().to_owned());
                let span_name = if let Some(ref route) = route {
                    format!("{} {}", request.method(), route)
                } else {
                    format!("{} {}", request.method(), path)
                };
                let api_type = if path.starts_with("/ai/") {
                    "ai_proxy"
                } else if path.starts_with("/admin/") {
                    "admin"
                } else {
                    "other"
                };
                let span = tracing::info_span!(
                    "request",
                    trace_id = tracing::field::Empty,
                    otel.name = %span_name,
                );

                // W3C Trace Context propagation (https://www.w3.org/TR/trace-context/)
                //
                // When an upstream caller (e.g. fusillade's batch daemon) sends a
                // request with a `traceparent` header, we parse it and set this
                // span's parent to the remote span context. This makes the dwctl
                // request span appear as a child of the caller's span in the trace
                // backend, producing one continuous trace across service boundaries.
                //
                // Without this, dwctl would start a new trace for every incoming
                // request, breaking the connection between fusillade's
                // process_request → execute and the dwctl request it dispatches.
                //
                // The traceparent header format is: {version}-{trace_id}-{span_id}-{flags}
                // e.g. "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                //
                // If parsing fails at any point we silently fall through and the
                // span starts a fresh trace — this is fine for requests that don't
                // carry trace context (e.g. direct API calls from users).
                if let Some(traceparent) = request.headers().get("traceparent")
                    && let Ok(tp) = traceparent.to_str()
                {
                    let parts: Vec<&str> = tp.split('-').collect();
                    if parts.len() == 4
                        && let (Ok(trace_id), Ok(span_id)) = (
                            opentelemetry::trace::TraceId::from_hex(parts[1]),
                            opentelemetry::trace::SpanId::from_hex(parts[2]),
                        )
                    {
                        let flags = u8::from_str_radix(parts[3], 16).unwrap_or(1);
                        let parent_ctx = opentelemetry::trace::SpanContext::new(
                            trace_id,
                            span_id,
                            opentelemetry::trace::TraceFlags::new(flags),
                            true, // remote: this span context came from another process
                            opentelemetry::trace::TraceState::default(),
                        );
                        let parent = opentelemetry::Context::new().with_remote_span_context(parent_ctx);
                        let _ = span.set_parent(parent);
                    }
                }

                span.set_attribute("otel.kind", "Server");
                span.set_attribute("api.type", api_type.to_string());
                span.set_attribute("http.request.method", request.method().to_string());
                span.set_attribute("http.route", route.unwrap_or_default());
                span.set_attribute("url.path", path.to_string());
                span.set_attribute("url.query", request.uri().query().unwrap_or("").to_string());
                span
            })
            .on_request(tower_http::trace::DefaultOnRequest::new().level(tracing::Level::TRACE))
            .on_response(|response: &http::Response<_>, latency: std::time::Duration, span: &tracing::Span| {
                let status = response.status().as_u16();
                span.set_attribute("http.response.status_code", i64::from(status));
                if status >= 500 {
                    span.set_attribute("otel.status_code", "ERROR");
                    span.set_attribute("error.type", status.to_string());
                } else if status >= 400 {
                    span.set_attribute("error.type", status.to_string());
                }
                tracing::info!(
                    http.response.status_code = status,
                    latency_ms = latency.as_millis() as u64,
                    "finished processing request"
                );
            })
            .on_failure(
                |error: tower_http::classify::ServerErrorsFailureClass, latency: std::time::Duration, span: &tracing::Span| {
                    span.set_attribute("otel.status_code", "ERROR");
                    span.set_attribute("error.type", error.to_string());
                    tracing::error!(
                        error = %error,
                        latency_ms = latency.as_millis() as u64,
                        "request failed"
                    );
                },
            ),
    );

    Ok(router)
}

/// Middleware that records the OpenTelemetry trace ID on the current span,
/// making it visible in fmt log output for Loki → Tempo correlation.
async fn inject_trace_id(request: axum::extract::Request, next: middleware::Next) -> axum::response::Response {
    let span = tracing::Span::current();
    let sc = span.context().span().span_context().clone();
    if sc.is_valid() {
        span.record("trace_id", tracing::field::display(sc.trace_id()));
    }
    next.run(request).await
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
    request_manager: Arc<fusillade_arsenal::PostgresRequestManager<DbPools>>,
    /// Step storage for multi-step responses, sharing the same fusillade
    /// pool as the request manager. Constructed in
    /// `setup_background_services` so the manager's processor (which
    /// dwctl wires later in `Application::new_with_pool`) can use it.
    step_manager: Arc<fusillade_arsenal::PostgresResponseStepManager<DbPools>>,
    /// The onwards-instance daemon id registered in the `daemons` table
    /// for realtime / inline-loop attribution. The graceful-shutdown
    /// drain (`shutdown()`) marks this row Dead and releases any
    /// in-progress rows it owns back to `pending` so the next pod picks
    /// them up immediately rather than waiting for stale-daemon
    /// detection (~30s).
    onwards_daemon_id: Option<Uuid>,
    /// Fusillade write pool retained for the SIGTERM drain queries.
    fusillade_write_pool: Option<sqlx::PgPool>,
    task_runner: Arc<tasks::TaskRunner>,
    is_leader: bool,
    onwards_targets: onwards::target::Targets,
    /// Per-key response hot-path metadata, initial-loaded and then refreshed by
    /// [`crate::sync::api_key_cache`].
    api_key_cache: crate::sync::api_key_cache::ApiKeyMetadataCache,
    /// Shared cold-path resolver for hidden Flex batch keys.
    flex_batch_key_resolver: crate::sync::api_key_cache::FlexBatchKeyResolver,
    #[cfg_attr(not(test), allow(dead_code))]
    onwards_sender: Option<tokio::sync::watch::Sender<onwards::target::Targets>>,
    #[allow(dead_code)] // Used in sync_onwards_config method
    strict_mode: bool,
    /// Sender for analytics records (if analytics is enabled)
    analytics_sender: Option<request_logging::batcher::AnalyticsSender>,
    /// Handle for durable creates and best-effort completion records consumed
    /// by the in-process `RequestsWriter`.
    requests_writer_handle: crate::inference::engine::writer::RequestsWriterHandle,
    // JoinSet is cancel-safe - can be polled in select! without losing tasks
    background_tasks: tokio::task::JoinSet<anyhow::Result<()>>,
    // Map task IDs to names for logging
    task_names: std::collections::HashMap<tokio::task::Id, &'static str>,
    shutdown_token: tokio_util::sync::CancellationToken,
    // Pub so that we can disarm it if we want to
    pub drop_guard: Option<tokio_util::sync::DropGuard>,
    /// Connections encryption key, derived once at startup.
    connections_encryption_key: Option<Vec<u8>>,
    /// Encrypted key custody, built once at startup. `None` when unconfigured.
    keystore: Option<crate::keystore::Keystore>,
}

impl BackgroundServices {
    fn spawn<F>(&mut self, name: &'static str, future: F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let abort_handle = self.background_tasks.spawn(future);
        self.task_names.insert(abort_handle.id(), name);
    }

    /// Wait for any background task to complete (indicating a failure)
    /// This method is cancel-safe - can be used in tokio::select! without losing tasks
    /// Returns an error with details about which task failed
    pub async fn wait_for_failure(&mut self) -> anyhow::Result<std::convert::Infallible> {
        loop {
            match self.background_tasks.join_next_with_id().await {
                None => {
                    // No background tasks - wait forever
                    futures::future::pending::<()>().await;
                    unreachable!()
                }
                Some(Ok((task_id, Ok(())))) if self.shutdown_token.is_cancelled() => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::debug!(task = task_name, "Background task completed during shutdown");
                }
                Some(Ok((task_id, Ok(())))) => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    crate::background_error!(
                        SUPERVISOR,
                        "task_exit_unexpected",
                        Error,
                        task = task_name,
                        "Background task completed unexpectedly"
                    );
                    anyhow::bail!("Background task '{}' completed early", task_name)
                }
                Some(Ok((task_id, Err(e)))) if self.shutdown_token.is_cancelled() => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::debug!(task = task_name, error = %e, "Background task exited with error during shutdown");
                }
                Some(Ok((task_id, Err(e)))) => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    crate::background_error!(SUPERVISOR, "task_failed", Error, task = task_name, error = %e, "Background task failed");
                    anyhow::bail!("Background task '{}' failed: {}", task_name, e)
                }
                Some(Err(e)) if self.shutdown_token.is_cancelled() => {
                    let task_id = e.id();
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::debug!(task = task_name, error = %e, "Background task panicked during shutdown");
                }
                Some(Err(e)) => {
                    let task_id = e.id();
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    crate::background_error!(SUPERVISOR, "task_panicked", Error, task = task_name, error = %e, "Background task panicked");
                    anyhow::bail!("Background task '{}' panicked: {}", task_name, e)
                }
            }
        }
    }

    /// Get a clone of the shutdown token for coordinating early cancellation
    pub fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown_token.clone()
    }

    /// Gracefully shutdown all background tasks.
    ///
    /// Implements the SIGTERM drain protocol from
    /// `fusillade/docs/plans/2026-04-28-multi-step-responses.md`:
    ///
    /// 1. Signal all in-process tasks to stop accepting new work
    ///    (`shutdown_token.cancel()`). The fusillade batch daemon stops
    ///    claiming and waits for in-flight workers to finish their
    ///    current loop iteration; it then marks its own daemon row Dead
    ///    via the `Running -> Dead` typestate transition.
    /// 2. Drain the **onwards-instance daemon** registration: this row
    ///    is created manually for realtime/inline-loop attribution and
    ///    is not managed by fusillade's daemon lifecycle, so we mark it
    ///    Dead explicitly + release any rows it owns back to pending.
    ///    Without this, the rows would wait for fusillade's stale-
    ///    daemon detection (default ~30s) before the next pod picks
    ///    them up.
    ///
    /// The drain is best-effort and logs errors rather than failing
    /// shutdown — a slow/missing drain falls back to time-based
    /// reclaim, which is correct just slower.
    pub async fn shutdown(mut self) {
        // Signal all background tasks to shutdown
        self.shutdown_token.cancel();

        // Drain the onwards-instance daemon registration before joining
        // tasks: this is the SIGTERM drain that gives the next pod
        // immediate ownership of any rows we still hold. We do this
        // BEFORE waiting for tasks because the unclaim is independent
        // of in-flight task completion — it just touches DB state.
        if let (Some(daemon_id), Some(pool)) = (self.onwards_daemon_id, self.fusillade_write_pool.as_ref()) {
            // Mark our daemon row Dead. fusillade's reclaim query
            // (`unclaim_stale_requests`) treats `daemons.status='dead'`
            // as the immediate-reclaim signal, so as soon as this
            // commits the next claim cycle on any other instance will
            // see our rows.
            //
            // The `dead_timestamp_check` constraint on `daemons`
            // requires `stopped_at IS NOT NULL` when status='dead', so
            // we set it explicitly here.
            let mark_dead = sqlx::query(
                "UPDATE daemons SET status = 'dead', stopped_at = NOW() \
                 WHERE id = $1",
            )
            .bind(daemon_id)
            .execute(pool)
            .await;
            if let Err(e) = mark_dead {
                tracing::warn!(error = %e, daemon_id = %daemon_id,
                    "SIGTERM drain: failed to mark onwards daemon Dead — \
                     rows will be reclaimed via stale-daemon detection");
            } else {
                tracing::info!(daemon_id = %daemon_id, "SIGTERM drain: marked onwards daemon Dead");
            }

            // Explicitly release any rows we own back to pending so the
            // next pod's claim cycle picks them up immediately rather
            // than waiting for the time-based fallback path. The
            // typestate constraints on `requests` require we clear
            // claimed_at/started_at when going back to pending.
            let unclaim = sqlx::query(
                "UPDATE requests \
                 SET state = 'pending', daemon_id = NULL, claimed_at = NULL, started_at = NULL \
                 WHERE daemon_id = $1 AND state IN ('claimed', 'processing')",
            )
            .bind(daemon_id)
            .execute(pool)
            .await;
            match unclaim {
                Ok(result) => {
                    if result.rows_affected() > 0 {
                        tracing::info!(
                            daemon_id = %daemon_id,
                            rows_released = result.rows_affected(),
                            "SIGTERM drain: released claimed/processing rows back to pending"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, daemon_id = %daemon_id,
                        "SIGTERM drain: failed to unclaim rows — \
                         rows will be reclaimed via stale-daemon detection");
                }
            }
        }

        // Wait for all background tasks to complete and check for errors
        while let Some(result) = self.background_tasks.join_next_with_id().await {
            match result {
                Ok((task_id, Ok(()))) => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::debug!(task = task_name, "Background task completed successfully");
                }
                Ok((task_id, Err(e))) => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    crate::background_error!(SUPERVISOR, "task_failed", Error, task = task_name, error = %e, "Background task failed");
                }
                Err(e) => {
                    let task_id = e.id();
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    crate::background_error!(SUPERVISOR, "task_panicked", Error, task = task_name, error = %e, "Background task panicked");
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
        // Note: escalation_models is empty for tests - individual tests can set up their own
        let new_targets =
            crate::sync::onwards_config::load_targets_from_db(pool, &[], self.strict_mode, &crate::config::RateLimitTiersConfig::default())
                .await?;

        // Send through the watch channel (same as automatic sync)
        sender
            .send(new_targets)
            .map_err(|_| anyhow::anyhow!("Failed to send targets update"))?;

        Ok(())
    }

    /// Manually refresh the per-key ZDR cache from the database (for testing).
    /// The cache handle is shared (same `ArcSwap`) with the inference
    /// middleware, so this immediately changes what `is_zdr_request` sees -
    /// letting a test flip an account to ZDR mid-run without spawning the
    /// LISTEN/NOTIFY loop.
    #[cfg(test)]
    pub async fn sync_api_key_cache(&self, pool: &sqlx::PgPool) -> anyhow::Result<()> {
        crate::sync::api_key_cache::refresh(pool, &self.api_key_cache).await?;
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
/// Wire the fusillade request manager, step manager, and (optionally)
/// the multi-step [`DwctlRequestProcessor`] into the daemon and start
/// the background-services stack.
///
/// The caller owns construction of `request_manager`, `step_manager`,
/// and `multi_step_processor` — and must build them in that order —
/// because the multi-step processor depends on a `FusilladeResponseStore`,
/// which itself depends on the request manager and step manager. That
/// ordering is enforced at the type level (you cannot construct the
/// processor without first constructing the others), which is why we
/// take them as parameters rather than constructing them here: it
/// guarantees that `set_processor` runs *before* any daemon spawn
/// inside this function, including the leader-election callback's
/// daemon spawn.
///
/// Pre-PR #1064 this function used to construct the request manager
/// itself and spawn the daemon *before* the caller had a chance to
/// build and wire the multi-step processor — which meant fusillade's
/// `OnceLock`-snapshot in `PostgresRequestManager::run` captured a
/// `None` processor and the daemon fell back to `DefaultRequestProcessor`
/// for every `/v1/responses + service_tier=flex` claim, looping the
/// request body back to ourselves and producing the
/// `{"choices":[],"usage":null}` terminal failure observed in prod.
///
/// `multi_step_processor` is `Option<...>` so the test path can pass
/// `None` to avoid forming the `request_manager → processor → response_store
/// → request_manager` Arc cycle that blocks `sqlx::test`'s `DROP DATABASE`
/// cleanup (the cycle's only effect in production — where the app lives
/// forever — is benign).
pub(crate) struct BackgroundServicesInput {
    /// Fusillade's durable DB store. The caller builds this so the
    /// multi-step processor can share the same storage instance as the
    /// daemon runtime.
    pub request_manager: Arc<fusillade_arsenal::PostgresRequestManager<DbPools>>,
    /// The singleton lifecycle writer consumer and its producer handle are
    /// constructed before the response store so descendants cannot observe a
    /// startup window without durable admission.
    pub requests_writer: crate::inference::engine::writer::RequestsWriter<DbPools>,
    pub requests_writer_handle: crate::inference::engine::writer::RequestsWriterHandle,
    /// Fusillade's scheduling daemon. Owns HTTP dispatch and runtime
    /// lifecycle; durable data operations live on `request_manager`.
    pub postgres_daemon: Arc<fusillade::PostgresDaemon<DbPools, fusillade::ReqwestHttpClient>>,
    /// Fusillade's response-step manager. Shares the same fusillade
    /// pool as the request manager.
    pub step_manager: Arc<fusillade_arsenal::PostgresResponseStepManager<DbPools>>,
    /// Multi-step processor to inject onto the request manager. `None`
    /// in tests to avoid forming the `request_manager → processor →
    /// response_store → request_manager` Arc cycle that blocks
    /// `sqlx::test`'s `DROP DATABASE` cleanup.
    pub multi_step_processor: Option<
        Arc<
            dyn fusillade::RequestProcessor<fusillade_arsenal::PostgresRequestManager<DbPools>, fusillade::ReqwestHttpClient> + Send + Sync,
        >,
    >,
    /// Shared map between the fusillade daemon's concurrency control
    /// and the onwards config-sync writer. Built once by the caller and
    /// passed in by-clone here.
    pub model_capacity_limits: Arc<dashmap::DashMap<String, usize>>,
    /// dwctl primary pool (used for probe scheduler, notification
    /// poller, and the inference middleware setup).
    pub pool: PgPool,
    /// Fusillade pool wrapper; kept around inside this function only
    /// to clone the write pool for the metrics sampler — fusillade's
    /// daemon already owns its own clone via `request_manager`.
    pub fusillade_pools: DbPools,
    /// Outlet (request-logging) pool. Optional because outlet is
    /// optional.
    pub outlet_pool: Option<PgPool>,
    pub config: Config,
    pub shared_config: SharedConfig,
    pub shutdown_token: tokio_util::sync::CancellationToken,
    pub metrics_recorder: Option<GenAiMetrics>,
    /// Shared ZDR keystore (built once by the caller). `None` = disabled.
    pub keystore: Option<crate::keystore::Keystore>,
}

async fn setup_background_services(input: BackgroundServicesInput) -> anyhow::Result<BackgroundServices> {
    let BackgroundServicesInput {
        request_manager,
        requests_writer,
        requests_writer_handle,
        postgres_daemon,
        step_manager,
        multi_step_processor,
        model_capacity_limits,
        pool,
        fusillade_pools,
        outlet_pool,
        config,
        shared_config,
        shutdown_token,
        metrics_recorder,
        keystore,
    } = input;

    // Wire the multi-step processor onto the daemon *before*
    // any daemon spawn below — this is the whole reason `setup_background_services`
    // accepts these as parameters rather than constructing them itself.
    // See the function-level doc comment.
    if let Some(processor) = multi_step_processor
        && let Err(e) = postgres_daemon.set_processor(processor)
    {
        tracing::warn!(error = e, "Multi-step processor was already set; skipping");
    }

    // `keystore` comes from the caller (built once and shared). Install the
    // TRANSITIONAL ZDR response transformer on the daemon before it
    // spawns below, so completed bodies are persisted encrypted.
    if let Some(ks) = keystore.clone()
        && let Err(e) = postgres_daemon.set_response_transformer(std::sync::Arc::new(crate::inference::zdr::ZdrResponseEncryptor::new(ks)))
    {
        tracing::warn!(error = e, "ZDR response transformer was already set; skipping");
    }

    let drop_guard = shutdown_token.clone().drop_guard();
    // Track all background task handles for graceful shutdown
    let mut background_tasks = BackgroundTaskBuilder::new();

    // `model_capacity_limits` (the shared map between the fusillade
    // daemon's concurrency control and the onwards config-sync writer)
    // is now owned by the caller — see the function-level doc.

    // Start onwards integration for proxying AI requests (if enabled)
    #[cfg_attr(not(test), allow(unused_variables))]
    let (initial_targets, onwards_sender) = if config.background_services.onwards_sync.enabled {
        // Extract escalation model names from batch daemon config
        // Batch API keys automatically get access to these models for completion window escalation
        let escalation_models: Vec<String> = config
            .background_services
            .batch_daemon
            .model_escalations
            .values()
            .map(|e| e.escalation_model.clone())
            .collect();

        let (onwards_config_sync, initial_targets, onwards_stream) = sync::onwards_config::OnwardsConfigSync::new_with_daemon_limits(
            pool.clone(),
            Some(model_capacity_limits.clone()),
            config.background_services.batch_daemon.default_model_concurrency,
            escalation_models,
            config.onwards.strict_mode,
            config.auth.rate_limits.clone(),
        )
        .await?;

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
        let fallback_interval = config.background_services.onwards_sync.fallback_interval_milliseconds;
        background_tasks.spawn("onwards-config-sync", async move {
            info!(
                "Starting onwards configuration listener (fallback sync every {}ms)",
                fallback_interval
            );
            let sync_config = sync::onwards_config::SyncConfig {
                status_tx: None,
                fallback_interval_milliseconds: fallback_interval,
            };
            onwards_config_sync
                .start(sync_config, onwards_shutdown)
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
            strict_mode: false,
            http_pool: None,
        };
        (onwards::target::Targets::from_config(empty_config)?, None)
    };

    // API-key response metadata: an initial synchronous load ALWAYS runs so
    // response hot paths never serve from a warming cache. Only the
    // LISTEN/NOTIFY refresh loop is gated on onwards config sync.
    let api_key_cache = crate::sync::api_key_cache::initial_cache(&pool).await?;
    let flex_batch_key_resolver = crate::sync::api_key_cache::FlexBatchKeyResolver::new(pool.clone(), api_key_cache.clone());
    if config.background_services.onwards_sync.enabled {
        let cache_pool = pool.clone();
        let cache = api_key_cache.clone();
        let cache_shutdown = shutdown_token.clone();
        let cache_fallback = config.background_services.onwards_sync.fallback_interval_milliseconds;
        background_tasks.spawn("api-key-metadata-sync", async move {
            crate::sync::api_key_cache::run(cache_pool, cache, cache_fallback, cache_shutdown)
                .await
                .context("API key metadata sync failed")
        });
    }

    // Leader election lock ID: 0x44574354_50524F42 (DWCT_PROB in hex for "dwctl probes")
    const LEADER_LOCK_ID: i64 = 0x4457_4354_5052_4F42_i64;

    let probe_scheduler = probes::ProbeScheduler::new(pool.clone(), config.clone());

    // Caller owns `request_manager` / `step_manager` construction —
    // see the function-level doc. We still need a pool clone here for
    // the metrics sampler; `fusillade_pools` is otherwise unused.
    let fusillade_pool_for_metrics = fusillade_pools.write().clone();
    drop(fusillade_pools);

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
        match config.background_services.batch_daemon.enabled {
            DaemonEnabled::Always | DaemonEnabled::Leader => {
                let daemon_handle = postgres_daemon.clone().run(shutdown_token.clone())?;
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

        // Always start the batch completion poller — it triggers lazy
        // finalization of terminal batches (setting completed_at / failed_at).
        // Notifications (emails, webhooks) are gated on config.enabled inside
        // the poller itself.
        {
            let daemon_config = config.clone();
            let daemon_request_manager = request_manager.clone();
            let daemon_pool = pool.clone();
            let daemon_shutdown = shutdown_token.clone();
            background_tasks.spawn("batch-completion", async move {
                notifications::run_notification_poller(
                    daemon_config.background_services.notifications.clone(),
                    daemon_config,
                    daemon_request_manager,
                    daemon_pool,
                    daemon_shutdown,
                )
                .await;
                Ok(())
            });
        }
    } else {
        // Normal leader election
        is_leader = false;
        info!("Starting leader election - will attempt to acquire leadership");

        // If daemon is set to "Always", start it immediately regardless of leader election
        use crate::config::DaemonEnabled;
        if config.background_services.batch_daemon.enabled == DaemonEnabled::Always {
            let daemon_handle = postgres_daemon.clone().run(shutdown_token.clone())?;
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
        let leader_election_postgres_daemon_gain = postgres_daemon.clone();
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
                move |pool, config| {
                    // This closure is run when a replica becomes the leader
                    let scheduler = leader_election_scheduler_gain.clone();
                    let request_manager = leader_election_request_manager_gain.clone();
                    let postgres_daemon = leader_election_postgres_daemon_gain.clone();
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

                        let notification_request_manager = request_manager.clone();

                        // Start the fusillade batch processing daemon based on config
                        use crate::config::DaemonEnabled;
                        match config.background_services.batch_daemon.enabled {
                            DaemonEnabled::Leader => {
                                let handle = postgres_daemon
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

                        // Always start the batch completion poller (see comment above)
                        {
                            let daemon_config = config.clone();
                            let daemon_session_token = session_token.clone();
                            tokio::spawn(async move {
                                notifications::run_notification_poller(
                                    daemon_config.background_services.notifications.clone(),
                                    daemon_config,
                                    notification_request_manager,
                                    pool,
                                    daemon_session_token,
                                )
                                .await;
                            });
                            tracing::info!("Batch completion poller started on elected leader");
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

    // Create a dedicated pool for the underway worker so its long-lived
    // PgListener connections don't compete with the main pool.
    let uw = config.database.underway_pool_settings();
    let underway_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(uw.max_connections)
        .min_connections(uw.min_connections)
        .acquire_timeout(std::time::Duration::from_secs(uw.acquire_timeout_secs))
        .idle_timeout(if uw.idle_timeout_secs > 0 {
            Some(std::time::Duration::from_secs(uw.idle_timeout_secs))
        } else {
            None
        })
        .max_lifetime(if uw.max_lifetime_secs > 0 {
            Some(std::time::Duration::from_secs(uw.max_lifetime_secs))
        } else {
            None
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await?;

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
            db::LabeledPool {
                name: "main_underway",
                pool: underway_pool.clone(),
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

    // Start the usage-refresh daemon: incrementally folds new http_analytics rows into
    // user_model_usage_daily. The analytics batcher (below) nudges it after every flush;
    // this shares an in-process Notify with it rather than round-tripping through Postgres.
    let usage_refresh_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    if config.enable_analytics && config.background_services.usage_refresh.enabled {
        let daemon_pool = pool.clone();
        let daemon_config = config.background_services.usage_refresh.clone();
        let daemon_notify = usage_refresh_notify.clone();
        let daemon_shutdown = shutdown_token.clone();
        background_tasks.spawn("usage-refresh", async move {
            sync::usage_refresh::run_usage_refresh_daemon(daemon_pool, daemon_config, daemon_notify, daemon_shutdown).await;
            Ok(())
        });
    }

    // Start analytics batcher if enabled
    let analytics_sender = if config.enable_analytics {
        let (batcher, sender) = request_logging::AnalyticsBatcher::new(pool.clone(), config.clone(), metrics_recorder);
        let batcher = batcher.with_usage_refresh_notify(usage_refresh_notify.clone());

        let batcher_shutdown = shutdown_token.clone();
        background_tasks.spawn("analytics-batcher", async move {
            batcher.run(batcher_shutdown).await;
            Ok(())
        });

        Some(sender)
    } else {
        None
    };

    // Start the responses writer. Replaces the underway create-response /
    // complete-response jobs. Outlet's FusilladeOutletHandler holds the
    // handle; this task drains the channel and flushes to fusillade.
    {
        let writer_shutdown = shutdown_token.clone();
        background_tasks.spawn("responses-writer", async move {
            requests_writer.run(writer_shutdown).await;
            Ok(())
        });
    }

    // Build the underway task runner for background jobs (batch population, sync pipeline, etc.)
    let encryption_key = match config.connections.encryption_key.as_deref().or(config.secret_key.as_deref()) {
        Some(secret) if !secret.trim().is_empty() => Some(encryption::derive_encryption_key(secret.trim())),
        Some(_) => {
            tracing::warn!("Encryption key is empty/whitespace — connection features will be unavailable");
            None
        }
        None => {
            tracing::info!("No encryption key configured for connections (set secret_key or connections.encryption_key)");
            None
        }
    };

    let task_state = tasks::TaskState {
        request_manager: request_manager.clone(),
        dwctl_pool: pool.clone(),
        config: shared_config.clone(),
        encryption_key: encryption_key.clone(),
        ingest_file_job: Arc::new(std::sync::OnceLock::new()),
        activate_batch_job: Arc::new(std::sync::OnceLock::new()),
        create_batch_job: Arc::new(std::sync::OnceLock::new()),
        cascade_batch_state_job: Arc::new(std::sync::OnceLock::new()),
    };
    let task_runner = Arc::new(tasks::TaskRunner::new(underway_pool, task_state, &config.background_services.task_workers).await?);
    for (name, handle) in task_runner.start(
        shutdown_token.clone(),
        &config.background_services.task_workers,
        &config.background_services.sync_workers,
    ) {
        background_tasks.spawn(name, async move { handle.await.map_err(|e| anyhow::anyhow!("{}", e)) });
    }

    let (background_tasks, task_names) = background_tasks.into_parts();

    Ok(BackgroundServices {
        request_manager,
        step_manager,
        task_runner,
        is_leader,
        onwards_targets: initial_targets,
        api_key_cache,
        flex_batch_key_resolver,
        onwards_sender,
        strict_mode: config.onwards.strict_mode,
        analytics_sender,
        requests_writer_handle,
        background_tasks,
        task_names,
        shutdown_token,
        drop_guard: Some(drop_guard),
        connections_encryption_key: encryption_key.clone(),
        keystore,
        // Application::new_with_pool wires these once the onwards-
        // instance daemon row is registered. Kept Optional here so
        // tests that bypass that wiring still construct cleanly.
        onwards_daemon_id: None,
        fusillade_write_pool: None,
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
    db_pools: DbPools,
    _fusillade_pools: DbPools,
    _outlet_pools: Option<DbPools>,
    _embedded_db: Option<db::embedded::EmbeddedDatabase>,
    _tracer_provider: Option<telemetry::SdkTracerProvider>,
    bg_services: BackgroundServices,
}

impl Application {
    /// Create a new application instance with all resources initialized
    ///
    /// If `pool` is provided, it will be used directly instead of creating a new connection.
    /// This is useful for tests where sqlx::test provides a pool.
    pub async fn new(config: Config, tracer_provider: Option<telemetry::SdkTracerProvider>) -> anyhow::Result<Self> {
        Self::new_with_pool_and_config_path(config, None, None, tracer_provider).await
    }

    pub async fn new_with_config_path(
        config: Config,
        config_path: Option<PathBuf>,
        tracer_provider: Option<telemetry::SdkTracerProvider>,
    ) -> anyhow::Result<Self> {
        Self::new_with_pool_and_config_path(config, config_path, None, tracer_provider).await
    }

    /// Create a new application instance with an existing database pool
    ///
    /// This method is primarily for tests where sqlx::test provides a pool.
    /// For production use, prefer [`Application::new`] which will create its own pool.
    pub async fn new_with_pool(
        config: Config,
        pool: Option<PgPool>,
        tracer_provider: Option<telemetry::SdkTracerProvider>,
    ) -> anyhow::Result<Self> {
        Self::new_with_pool_and_config_path(config, None, pool, tracer_provider).await
    }

    pub async fn new_with_pool_and_config_path(
        config: Config,
        config_path: Option<PathBuf>,
        pool: Option<PgPool>,
        tracer_provider: Option<telemetry::SdkTracerProvider>,
    ) -> anyhow::Result<Self> {
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

        // Create GenAI metrics recorder if both metrics and analytics are enabled
        // This is created here (before background services) so the analytics batcher can use it
        let metrics_recorder = if config.enable_metrics && config.enable_analytics {
            let gen_ai_registry = prometheus::Registry::new();
            Some(GenAiMetrics::new(&gen_ai_registry).map_err(|e| anyhow::anyhow!("Failed to create GenAI metrics: {}", e))?)
        } else {
            None
        };

        // Setup background services (onwards integration, probe scheduler, batch daemon, leader election)
        // Note: Must use primary pool (via Deref) because onwards sync uses LISTEN/NOTIFY
        // which requires direct database connection to primary (not through PgBouncer transaction pooling)
        let shared_config = SharedConfig::new(config.clone());

        // Build the fusillade request manager, step manager, response
        // store, and multi-step processor *before* spawning any daemons.
        // Order is enforced at the type level (processor depends on
        // response_store depends on (request_manager, step_manager)),
        // so by the time we hand all four to `setup_background_services`,
        // it can safely call `request_manager.set_processor(...)` before
        // any daemon spawn. Fusillade's daemon snapshots the processor
        // via `OnceLock::get()` at `run()` time, so anything spawned
        // afterward (synchronous fusillade daemon AND the leader-gained
        // closure) sees the multi-step processor — that's what fixes
        // the `/v1/responses + service_tier=flex` regression where the
        // daemon kept using `DefaultRequestProcessor` and looped the
        // request body back to ourselves.
        //
        // Shared `model_capacity_limits` map: the fusillade daemon's
        // per-model concurrency controller reads it; the onwards
        // config-sync writer (inside `setup_background_services`)
        // writes to it. Both must hold clones of the *same* Arc, so
        // we build it once here.
        let model_capacity_limits: Arc<dashmap::DashMap<String, usize>> = Arc::new(dashmap::DashMap::new());

        let fusillade_daemon_config = config
            .background_services
            .batch_daemon
            .to_fusillade_config_with_limits(Some(model_capacity_limits.clone()));

        let request_manager = Arc::new(
            fusillade_arsenal::PostgresRequestManager::new(
                fusillade_pools.clone(),
                fusillade_arsenal::PostgresStorageConfig::from(&fusillade_daemon_config),
            )
            .with_download_buffer_size(config.batches.files.download_buffer_size)
            .with_batch_insert_strategy(fusillade_arsenal::BatchInsertStrategy::Batched {
                batch_size: config.batches.files.batch_insert_size,
            }),
        );
        let (requests_writer, requests_writer_handle) = crate::inference::engine::writer::RequestsWriter::new(
            request_manager.clone(),
            config.background_services.task_workers.response_writer_batch_size,
            std::time::Duration::from_millis(config.background_services.task_workers.response_writer_max_linger_ms),
        );
        let postgres_daemon = Arc::new(fusillade::PostgresDaemon::from_store(
            request_manager.clone(),
            fusillade_daemon_config.clone(),
        ));
        let step_manager = Arc::new(fusillade_arsenal::PostgresResponseStepManager::new(fusillade_pools.clone()));
        // Build the ZDR keystore once and share it across the response store, the
        // daemon processor, and background services (which install the response
        // transformer). A misconfiguration is fatal.
        let keystore = match config.keystore.as_ref() {
            Some(c) => Some(crate::keystore::Keystore::from_config(c).map_err(|e| anyhow::anyhow!("failed to initialise keystore: {e}"))?),
            None => None,
        };
        let response_store = Arc::new(
            crate::inference::store::FusilladeResponseStore::new(request_manager.clone(), requests_writer_handle.clone())
                .with_step_manager(step_manager.clone())
                .with_keystore(keystore.clone()),
        );

        // Build the image normaliser ONCE — fail loud at startup if
        // `image_normalizer.enabled = true` but no backend is configured.
        // This single instance is threaded through the processor builder,
        // the realtime middleware, the batch ingest handler, and the
        // dashboard image-view handler via AppState — never reconstructed
        // per request.
        let image_normalizer =
            crate::image_normalizer::from_config(&config.image_normalizer).map_err(|e| anyhow::anyhow!("image normaliser config: {e}"))?;

        // Build the multi-step processor's dependencies. These also end
        // up wired into the inference middleware state below; cloning
        // them is cheap (Arc + reqwest::Client share their internal
        // connection pool / TLS root-cert cache across clones).
        let multi_step_reqwest_client = reqwest::Client::new();
        let multi_step_tool_executor_pool = Arc::new(db_pools.write().clone());
        let multi_step_tool_executor = Arc::new(crate::inference::tools::HttpToolExecutor::new(
            multi_step_reqwest_client.clone(),
            Some(multi_step_tool_executor_pool.clone()),
        ));
        // Same `ReqwestHttpClient` shape the batch daemon uses internally,
        // so per-step model fires inherit fusillade's header stamping
        // (`X-Fusillade-Request-Id` for analytics correlation) and
        // streamable-endpoint dispatch. Timeouts and the streamable list
        // come from the same config knobs the daemon respects, so warm
        // path and daemon path use identical streaming semantics.
        let multi_step_http_client: Arc<fusillade::ReqwestHttpClient> = Arc::new(fusillade::ReqwestHttpClient::new(
            std::time::Duration::from_millis(fusillade_daemon_config.first_chunk_timeout_ms),
            std::time::Duration::from_millis(fusillade_daemon_config.chunk_timeout_ms),
            std::time::Duration::from_millis(fusillade_daemon_config.body_timeout_ms),
            fusillade_daemon_config.streamable_endpoints.clone(),
        ));
        let multi_step_loop_config = onwards::LoopConfig {
            max_response_step_depth: config.responses.max_response_step_depth,
            max_response_iterations: config.responses.max_response_iterations,
        };

        // Build the processor itself only outside test mode: the
        // `request_manager → processor.OnceLock → response_store →
        // request_manager` Arc cycle is harmless in production (the app
        // lives forever) but in `#[sqlx::test]` it keeps each test's
        // pool clones alive past test teardown, blocking sqlx's
        // `DROP DATABASE` cleanup. Tests run with `DefaultRequestProcessor`
        // — their multi-step coverage lives in dedicated
        // `test/multi_step_*` modules that bypass the daemon path
        // anyway.
        let multi_step_processor_for_setup = if cfg!(test) {
            None
        } else {
            let tool_resolver: Arc<dyn crate::inference::engine::processor::DaemonToolResolver> =
                Arc::new(crate::inference::engine::processor::DbToolResolver {
                    pool: (*db_pools).write().clone(),
                });
            // Derive dispatch TTL from the batch daemon's processing timeout so
            // the signed URL is always valid for at least one full dispatch
            // attempt — never the cause of a batch failure on its own.
            let processing_timeout = std::time::Duration::from_millis(config.background_services.batch_daemon.processing_timeout_ms);
            let dispatch_ttl = config.image_normalizer.signing.dispatch_ttl(processing_timeout);
            let mut processor_builder = crate::inference::engine::processor::DwctlRequestProcessor::new(
                response_store.clone(),
                multi_step_tool_executor.clone(),
                multi_step_http_client.clone(),
                multi_step_loop_config,
            )
            .with_tool_resolver(tool_resolver);
            if config.image_normalizer.enabled {
                // Re-use the AppState-bound singleton built above; no second
                // GCS client / ADC signer init.
                processor_builder = processor_builder.with_image_normalizer(image_normalizer.clone(), dispatch_ttl);
            }
            processor_builder = processor_builder.with_keystore(keystore.clone());
            let processor = Arc::new(processor_builder);
            Some(
                processor
                    as Arc<
                        dyn fusillade::RequestProcessor<fusillade_arsenal::PostgresRequestManager<DbPools>, fusillade::ReqwestHttpClient>
                            + Send
                            + Sync,
                    >,
            )
        };

        let mut bg_services = setup_background_services(BackgroundServicesInput {
            request_manager: request_manager.clone(),
            requests_writer,
            requests_writer_handle: requests_writer_handle.clone(),
            postgres_daemon: postgres_daemon.clone(),
            step_manager: step_manager.clone(),
            multi_step_processor: multi_step_processor_for_setup,
            model_capacity_limits,
            pool: (*db_pools).clone(),
            fusillade_pools: fusillade_pools.clone(),
            outlet_pool: outlet_pools.as_ref().map(|p| (**p).clone()),
            config: config.clone(),
            shared_config: shared_config.clone(),
            shutdown_token: shutdown_token.clone(),
            metrics_recorder: metrics_recorder.clone(),
            keystore: keystore.clone(),
        })
        .await?;

        // Enforce `stream_options.include_usage` for streaming chat completions.
        //
        // For streaming requests, upstream providers only report token usage in the final
        // SSE chunk when `stream_options: { include_usage: true }` is set. Without it,
        // the response contains no usage data and the request logs record 0 tokens — meaning
        // the request can't be billed. The dashboard sets this automatically, but direct API
        // callers may not.
        //
        // This applies to /chat/completions and the legacy /completions endpoint (both
        // support `stream_options`). The Responses API (/responses) always includes usage
        // in its response object regardless of streaming, so no transform is needed there.
        // Embeddings don't support streaming.
        let body_transform: onwards::BodyTransformFn = Arc::new(request_logging::stream_usage::stream_usage_transform);

        // Create the HTTP tool executor used by the single-step
        // (non-multi-step) realtime tool-injection path. Re-uses the
        // same reqwest::Client and dwctl pool clones the multi-step
        // executor (built earlier, before `setup_background_services`)
        // does — cloning a reqwest::Client shares its connection pool /
        // TLS root cert cache, and the PgPool clone shares the
        // underlying connection pool. Building separate
        // clients/pools would double TLS init cost per test and add
        // unnecessary parallelism pressure.
        let tool_executor =
            crate::inference::tools::HttpToolExecutor::new(multi_step_reqwest_client.clone(), Some(multi_step_tool_executor_pool.clone()));

        // Register onwards as a fusillade daemon so realtime requests get a valid daemon_id.
        let onwards_daemon_id = uuid::Uuid::new_v4();
        let fusillade_write_pool = bg_services.request_manager.pool().clone();
        let daemon_insert_result = sqlx::query(
            "INSERT INTO daemons (id, hostname, pid, version, config_snapshot, status, started_at, last_heartbeat)
             VALUES ($1, $2, $3, $4, $5, 'running', NOW(), NOW())",
        )
        .bind(onwards_daemon_id)
        .bind(std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string()))
        .bind(std::process::id() as i32)
        .bind(env!("CARGO_PKG_VERSION"))
        .bind(serde_json::json!({"type": "onwards"}))
        .execute(&fusillade_write_pool)
        .await;

        let daemon_registered = match &daemon_insert_result {
            Ok(_) => {
                tracing::info!(daemon_id = %onwards_daemon_id, "Registered onwards as fusillade daemon");
                // Stash on bg_services so the SIGTERM drain in
                // BackgroundServices::shutdown can mark this row Dead
                // and release its claimed rows.
                bg_services.onwards_daemon_id = Some(onwards_daemon_id);
                bg_services.fusillade_write_pool = Some(fusillade_write_pool.clone());
                true
            }
            Err(e) => {
                crate::background_error!(ONWARDS_HEARTBEAT, "registration", Warning, error = %e, "Failed to register onwards daemon (table may not exist yet)");
                false
            }
        };

        // Spawn a background task to send periodic heartbeats for the onwards daemon.
        // Without this, fusillade's stale daemon detection would unclaim our processing
        // rows after stale_daemon_threshold_ms (default 30s).
        // Only spawn if the daemon was successfully registered.
        if daemon_registered {
            let heartbeat_pool = fusillade_write_pool.clone();
            let heartbeat_daemon_id = onwards_daemon_id;
            let heartbeat_shutdown = bg_services.shutdown_token();
            bg_services.spawn("onwards-daemon-heartbeat", async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let result = sqlx::query(
                                "UPDATE daemons SET last_heartbeat = NOW() WHERE id = $1",
                            )
                            .bind(heartbeat_daemon_id)
                            .execute(&heartbeat_pool)
                            .await;

                            if let Err(e) = result {
                                crate::background_error!(ONWARDS_HEARTBEAT, "heartbeat", Warning, error = %e, "Failed to send onwards daemon heartbeat");
                            }
                        }
                        _ = heartbeat_shutdown.cancelled() => {
                            // Mark daemon as dead on shutdown
                            let _ = sqlx::query(
                                "UPDATE daemons SET status = 'dead', stopped_at = NOW() WHERE id = $1",
                            )
                            .bind(heartbeat_daemon_id)
                            .execute(&heartbeat_pool)
                            .await;
                            tracing::info!(daemon_id = %heartbeat_daemon_id, "Onwards daemon marked as dead");
                            break;
                        }
                    }
                }
                Ok(())
            });
        } // daemon_registered

        // `response_store`, `multi_step_tool_executor`,
        // `multi_step_http_client`, `multi_step_loop_config` and the
        // multi-step processor were built upfront before
        // `setup_background_services` — `set_processor` ran inside
        // setup, before any daemon spawn (synchronous fusillade daemon
        // OR the leader-gained closure). All daemons see the
        // multi-step processor at claim time.

        // Inference middleware state. Non-background realtime no longer
        // does any DB work up front; the completion path goes through
        // FusilladeOutletHandler -> RequestsWriter.
        let inference_middleware_state = crate::inference::middleware::InferenceMiddlewareState {
            requests_writer: bg_services.requests_writer_handle.clone(),
            request_manager: bg_services.request_manager.clone(),
            daemon_id: crate::inference::store::OnwardsDaemonId(onwards_daemon_id),
            loopback_base_url: {
                let addr = config.bind_address();
                let addr = if addr.starts_with("0.0.0.0:") {
                    addr.replacen("0.0.0.0", "127.0.0.1", 1)
                } else {
                    addr
                };
                format!("http://{addr}/ai")
            },
            dwctl_pool: (*db_pools).write().clone(),
            response_store: response_store.clone(),
            multi_step_tool_executor,
            multi_step_http_client,
            loop_config: multi_step_loop_config,
            image_normalizer: image_normalizer.clone(),
            image_normalizer_enabled: config.image_normalizer.enabled,
            unverified_requests_per_completion_hour: config.batches.unverified_requests_per_completion_hour,
            flex_completion_window: config.batches.async_requests.completion_window.clone(),
            keystore: bg_services.keystore.clone(),
            api_key_cache: bg_services.api_key_cache.clone(),
            flex_batch_key_resolver: bg_services.flex_batch_key_resolver.clone(),
            onwards_targets: bg_services.onwards_targets.clone(),
        };

        // Build onwards router from targets with body transform, response sanitization, and tool executor.
        // Realtime request bodies share the same configurable cap as batch
        // file requests (limits.requests.max_body_size, 0 = unlimited);
        // without an explicit limit onwards' strict mode would fall back to
        // Axum's 2 MB default and 413 large payloads.
        let onwards_body_limit = match config.limits.requests.max_body_size {
            0 => usize::MAX,
            n => usize::try_from(n).unwrap_or(usize::MAX),
        };
        // onwards stays cache-agnostic: cached-input pricing now lives entirely in
        // the dwctl cache tower layer (wired in `build_router`, gated on `cache.enabled`).
        // No classifier is injected here.
        let onwards_app_state = onwards::AppState::with_transform(bg_services.onwards_targets.clone(), body_transform)
            .with_response_transform(onwards::create_openai_sanitizer())
            .with_streaming_header("x-fusillade-stream")
            .with_response_id_header("x-fusillade-request-id")
            .with_tool_executor(Arc::new(tool_executor))
            .with_response_store(response_store.clone() as Arc<dyn onwards::ResponseStore>)
            .with_body_limit(onwards_body_limit);

        let onwards_router = if bg_services.onwards_targets.strict_mode {
            tracing::info!("Strict mode enabled - using typed request validation");
            onwards::strict::build_strict_router(onwards_app_state)
        } else {
            onwards::build_router(onwards_app_state)
        };

        // Build resource limiters
        let limiters = limits::Limiters::new(&config.limits);

        // Build app state and router
        let mut app_state = AppState::builder()
            .db(db_pools.clone())
            .config(shared_config.clone())
            .is_leader(bg_services.is_leader)
            .request_manager(bg_services.request_manager.clone())
            .requests_writer(bg_services.requests_writer_handle.clone())
            .task_runner(bg_services.task_runner.clone())
            .maybe_outlet_db(outlet_pools.clone())
            .limiters(limiters)
            .maybe_connections_encryption_key(bg_services.connections_encryption_key.clone())
            .maybe_keystore(bg_services.keystore.clone())
            .api_key_cache(bg_services.api_key_cache.clone())
            .flex_batch_key_resolver(bg_services.flex_batch_key_resolver.clone())
            .response_store(response_store)
            .response_step_manager(bg_services.step_manager.clone())
            .image_normalizer(image_normalizer)
            .build();

        if let Some(config_path) = config_path {
            bg_services.spawn(
                "config-watcher",
                config_watcher::watch_config_file(config_path, shared_config, bg_services.shutdown_token()),
            );
        }

        let router = build_router(
            &mut app_state,
            onwards_router,
            bg_services.analytics_sender.clone(),
            metrics_recorder,
            bg_services.onwards_targets.strict_mode,
            Some(inference_middleware_state),
        )
        .await?;

        Ok(Self {
            router,
            app_state,
            config,
            db_pools,
            _fusillade_pools: fusillade_pools,
            _outlet_pools: outlet_pools,
            _embedded_db,
            _tracer_provider: tracer_provider,
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

        // Cancel shutdown token when SIGTERM arrives, BEFORE axum starts waiting
        // for in-flight connections to close. This lets background services (e.g.,
        // fusillade daemon) abort in-flight HTTP tasks immediately, allowing
        // proxy connections to close and axum's graceful shutdown to complete.
        let shutdown_token = self.bg_services.shutdown_token();
        let shutdown = async move {
            shutdown.await;
            shutdown_token.cancel();
        };

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

        // Flush pending spans without shutting down the processor.
        // We intentionally use force_flush() instead of shutdown() because the
        // tracing_opentelemetry layer (global subscriber) still holds a Tracer
        // referencing the same inner provider. Calling shutdown() marks the
        // BatchSpanProcessor as dead, but any tracing event emitted afterward
        // (during remaining cleanup, tokio runtime drop, etc.) still hits the
        // processor and generates an "AfterShutdown" warning per span. By only
        // flushing, the processor stays alive and silently accepts late spans.
        if let Some(ref provider) = self._tracer_provider {
            info!("Flushing telemetry...");
            if let Err(e) = provider.force_flush() {
                tracing::error!("Failed to flush tracer provider: {}", e);
            }
        }

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

#[cfg(test)]
mod security_header_tests {
    use super::security_header_pairs;
    use crate::config::SecurityHeadersConfig;

    fn names(cfg: &SecurityHeadersConfig) -> Vec<String> {
        security_header_pairs(cfg)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name.as_str().to_string())
            .collect()
    }

    #[test]
    fn defaults_emit_the_safe_hardening_headers() {
        let names = names(&SecurityHeadersConfig::default());
        assert!(names.contains(&"x-content-type-options".to_string()));
        assert!(names.contains(&"x-frame-options".to_string()));
        assert!(names.contains(&"referrer-policy".to_string()));
        assert!(names.contains(&"permissions-policy".to_string()));
        // Opt-in headers are not emitted unless explicitly configured.
        assert!(!names.contains(&"content-security-policy".to_string()));
        assert!(!names.contains(&"content-security-policy-report-only".to_string()));
        assert!(!names.contains(&"strict-transport-security".to_string()));
    }

    #[test]
    fn disabled_emits_nothing() {
        let cfg = SecurityHeadersConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(security_header_pairs(&cfg).unwrap().is_empty());
    }

    #[test]
    fn opt_in_headers_emitted_when_set() {
        let cfg = SecurityHeadersConfig {
            content_security_policy: "default-src 'self'".to_string(),
            strict_transport_security: "max-age=31536000; includeSubDomains".to_string(),
            ..Default::default()
        };
        let names = names(&cfg);
        assert!(names.contains(&"content-security-policy".to_string()));
        assert!(names.contains(&"strict-transport-security".to_string()));
    }

    #[test]
    fn empty_value_disables_individual_header() {
        let cfg = SecurityHeadersConfig {
            frame_options: String::new(),
            ..Default::default()
        };
        assert!(!names(&cfg).contains(&"x-frame-options".to_string()));
    }

    #[test]
    fn invalid_header_value_is_rejected() {
        let cfg = SecurityHeadersConfig {
            // A bare newline is not a valid header value.
            referrer_policy: "bad\nvalue".to_string(),
            ..Default::default()
        };
        assert!(security_header_pairs(&cfg).is_err());
    }
}

#[cfg(test)]
mod cors_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request};
    use tower::ServiceExt;
    use url::Url;

    fn cors_config(allow_any_origin_without_credentials: bool) -> CorsConfig {
        CorsConfig {
            allowed_origins: vec![CorsOrigin::Url(Url::parse("https://app.doubleword.ai").unwrap())],
            allow_credentials: true,
            max_age: Some(3600),
            exposed_headers: vec![],
            allow_any_origin_without_credentials,
        }
    }

    /// Drive a CORS preflight (OPTIONS) from `origin` through the real CORS
    /// stack and return the response headers.
    async fn preflight(cors: &CorsConfig, origin: &str) -> HeaderMap {
        let app = apply_cors(Router::new().route("/", get(|| async { "ok" })), cors).expect("apply_cors");
        let resp = app
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/")
                    .header("origin", origin)
                    .header("access-control-request-method", "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        resp.headers().clone()
    }

    fn acao(h: &HeaderMap) -> Option<String> {
        h.get("access-control-allow-origin").map(|v| v.to_str().unwrap().to_string())
    }

    fn acac(h: &HeaderMap) -> Option<String> {
        h.get("access-control-allow-credentials").map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn third_party_origin_allowed_without_credentials() {
        let h = preflight(&cors_config(true), "https://evil.example.com").await;
        assert_eq!(
            acao(&h).as_deref(),
            Some("https://evil.example.com"),
            "non-allowlisted origin should be reflected when the flag is set",
        );
        assert_eq!(
            acac(&h),
            None,
            "non-allowlisted origin must NOT receive Access-Control-Allow-Credentials",
        );
    }

    #[tokio::test]
    async fn first_party_origin_keeps_credentials() {
        let h = preflight(&cors_config(true), "https://app.doubleword.ai").await;
        assert_eq!(acao(&h).as_deref(), Some("https://app.doubleword.ai"));
        assert_eq!(acac(&h).as_deref(), Some("true"), "allowlisted origin must keep credentialed CORS",);
    }

    #[tokio::test]
    async fn flag_off_blocks_non_allowlisted_origin() {
        let h = preflight(&cors_config(false), "https://evil.example.com").await;
        assert_eq!(
            acao(&h),
            None,
            "with the flag off, non-allowlisted origins get no CORS (unchanged behavior)",
        );
    }

    #[tokio::test]
    async fn near_miss_origin_does_not_get_credentials() {
        // A look-alike of an allowlisted origin is reflected (public mode) but
        // MUST NOT be treated as first-party — exact match only, no suffix slip.
        let h = preflight(&cors_config(true), "https://app.doubleword.ai.evil.com").await;
        assert_eq!(acao(&h).as_deref(), Some("https://app.doubleword.ai.evil.com"),);
        assert_eq!(acac(&h), None, "a look-alike of an allowlisted origin must not receive credentials",);
    }

    #[tokio::test]
    async fn request_without_origin_gets_no_cors_headers() {
        let app = apply_cors(Router::new().route("/", get(|| async { "ok" })), &cors_config(true)).expect("apply_cors");
        let resp = app
            .oneshot(Request::builder().method("GET").uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let h = resp.headers();
        assert!(h.get("access-control-allow-origin").is_none());
        assert!(h.get("access-control-allow-credentials").is_none());
    }

    /// Drive an actual (non-preflight) GET from `origin` and return the headers.
    async fn actual_get(cors: &CorsConfig, origin: &str) -> HeaderMap {
        let app = apply_cors(Router::new().route("/", get(|| async { "ok" })), cors).expect("apply_cors");
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/")
                    .header("origin", origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        resp.headers().clone()
    }

    #[tokio::test]
    async fn third_party_actual_get_is_reflected_without_credentials() {
        let h = actual_get(&cors_config(true), "https://evil.example.com").await;
        assert_eq!(acao(&h).as_deref(), Some("https://evil.example.com"));
        assert_eq!(acac(&h), None);
    }

    #[tokio::test]
    async fn first_party_actual_get_keeps_credentials() {
        let h = actual_get(&cors_config(true), "https://app.doubleword.ai").await;
        assert_eq!(acao(&h).as_deref(), Some("https://app.doubleword.ai"));
        assert_eq!(acac(&h).as_deref(), Some("true"));
    }
}
