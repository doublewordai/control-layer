//! Per-alias model metadata cache for ingress request validation.
//!
//! Maintains a memory-local map from deployed model alias to the [`ModelInfo`]
//! the request rules need, so the inference hot path
//! ([`crate::inference::validation::evaluate`]) answers with a lock-free map
//! read and no DB round-trip.
//!
//! Unlike [`crate::sync::onwards_config`], which rebuilds the whole routing
//! table on every change, this runs one small single-table read and swaps a
//! flat map, so refreshing it on every `auth_config_changed` notification is
//! cheap. It reuses that channel (fired by the `deployed_models` trigger), a
//! 100ms debounce, and a periodic fallback reload.
//!
//! A cache that has not completed its first load reports
//! [`ModelLookup::NotLoaded`] for every alias, which the rules treat as "pass".
//! Failing open on a cold cache is load-bearing: metadata being unavailable must
//! never reject a request.

use std::{collections::HashMap, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use sqlx::{PgPool, postgres::PgListener};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config::ONWARDS_CONFIG_CHANGED_CHANNEL;
use crate::db::models::deployments::ModelType;
use crate::inference::validation::{ModelInfo, ModelInfoSource, ModelLookup};
use crate::metrics::errors::component;

/// Alias-to-metadata map held inside the cache.
type AliasMap = HashMap<String, Arc<ModelInfo>>;

/// Lock-free, cheap-to-clone handle to the shared alias-to-[`ModelInfo`] map.
///
/// `None` means "not loaded yet" (cold start, sync disabled, or a failed first
/// load) and reads as [`ModelLookup::NotLoaded`]. `Some(map)` is a completed
/// load, after which absence of an alias is a genuine [`ModelLookup::Unknown`].
#[derive(Clone)]
pub struct ModelMetadataCache {
    inner: Arc<ArcSwap<Option<AliasMap>>>,
}

impl ModelMetadataCache {
    /// An unloaded cache: every alias reads as [`ModelLookup::NotLoaded`]. Used
    /// before the first load, in tests, and when the sync is disabled.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(None)),
        }
    }

    /// Look up the facts for a deployed model alias. Hot-path safe.
    pub fn lookup(&self, alias: &str) -> ModelLookup {
        match self.inner.load().as_ref() {
            None => ModelLookup::NotLoaded,
            Some(map) => match map.get(alias) {
                Some(info) => ModelLookup::Known(Arc::clone(info)),
                None => ModelLookup::Unknown,
            },
        }
    }

    fn replace(&self, map: AliasMap) {
        self.inner.store(Arc::new(Some(map)));
    }

    /// Build a loaded cache from explicit `(alias, info)` pairs. Test-only.
    #[cfg(test)]
    fn from_infos<I: IntoIterator<Item = (String, ModelInfo)>>(infos: I) -> Self {
        let cache = Self::empty();
        cache.replace(infos.into_iter().map(|(alias, info)| (alias, Arc::new(info))).collect());
        cache
    }
}

impl ModelInfoSource for ModelMetadataCache {
    fn lookup(&self, alias: &str) -> ModelLookup {
        ModelMetadataCache::lookup(self, alias)
    }
}

/// Map the `deployed_models.type` column with the same values as
/// [`crate::db::handlers::deployments`]. An unrecognised or absent type reads as
/// unknown, which disables type-based rules for the model.
fn model_type_from_str(value: Option<&str>) -> Option<ModelType> {
    match value? {
        "CHAT" => Some(ModelType::Chat),
        "EMBEDDINGS" => Some(ModelType::Embeddings),
        "RERANKER" => Some(ModelType::Reranker),
        _ => None,
    }
}

/// Read a token-count field from catalog metadata. Only positive JSON integers
/// are accepted; missing, non-numeric, zero and negative values are all `None`,
/// which disables the rules that depend on the field (fail open).
fn token_count(metadata: &serde_json::Value, key: &str) -> Option<u64> {
    metadata.get(key)?.as_u64().filter(|value| *value > 0)
}

/// Read every deployed model onwards would route and collect the flat
/// alias-to-[`ModelInfo`] map. Deleted rows are excluded; composite models and
/// regular models with an endpoint are included, matching the routing query in
/// [`crate::sync::onwards_config`].
async fn load(pool: &PgPool) -> Result<AliasMap, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT dm.alias, dm.type AS model_type, dm.capabilities, dm.metadata
        FROM deployed_models dm
        WHERE dm.deleted = FALSE
          AND (dm.is_composite = TRUE OR dm.hosted_on IS NOT NULL)
        "#
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let info = ModelInfo {
                model_type: model_type_from_str(row.model_type.as_deref()),
                context_window: token_count(&row.metadata, "context_window"),
                max_output_tokens: token_count(&row.metadata, "max_output_tokens"),
                capabilities: row.capabilities,
            };
            (row.alias, Arc::new(info))
        })
        .collect())
}

/// Reload `cache` in place from the DB, returning the number of aliases loaded.
/// Shared by [`initial_cache`], the background listener, and the test-only
/// manual refresh so none of them can drift.
pub async fn refresh(pool: &PgPool, cache: &ModelMetadataCache) -> Result<usize, sqlx::Error> {
    let map = load(pool).await?;
    let n = map.len();
    cache.replace(map);
    Ok(n)
}

/// Load the map once, synchronously, returning a populated cache. Call at
/// startup before the server accepts traffic: until this succeeds every alias
/// reads as [`ModelLookup::NotLoaded`] and the rules pass.
pub async fn initial_cache(pool: &PgPool) -> Result<ModelMetadataCache, sqlx::Error> {
    let cache = ModelMetadataCache::empty();
    refresh(pool, &cache).await?;
    Ok(cache)
}

/// Initial delay before retrying listener setup after a failure.
const INITIAL_LISTENER_BACKOFF: Duration = Duration::from_secs(1);
/// Upper bound for the listener reconnect backoff.
const MAX_LISTENER_BACKOFF: Duration = Duration::from_secs(30);

/// Double `current`, capped at `max`. Split out so the retry schedule is
/// testable without a flaky database.
fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.saturating_mul(2).min(max)
}

/// Open the LISTEN session and subscribe to the change channel.
async fn connect_listener(listener_pools: &sqlx_pool_router::DynPools) -> Result<PgListener, sqlx::Error> {
    // LISTEN needs a session: direct connections, never the pooled endpoint.
    let mut listener = PgListener::connect_with(&listener_pools.write()).await?;
    listener.listen(ONWARDS_CONFIG_CHANGED_CHANNEL).await?;
    Ok(listener)
}

/// Background task: keep `cache` fresh. Listens on `auth_config_changed` and
/// reloads (debounced), with a periodic fallback reload to recover from any
/// missed notification. Returns `Ok` only when `shutdown` fires.
///
/// Notifications that arrive inside the debounce window are coalesced into a
/// single trailing reload (rather than dropped), so the final state after a
/// burst of changes still lands in the cache.
///
/// Model metadata is fail-open, so a database error must never end this task:
/// listener setup failures are retried forever with capped exponential backoff,
/// and reload failures are logged and retried on the next notification/tick.
pub async fn run(
    pools: impl sqlx_pool_router::PoolProvider,
    listener_pools: impl sqlx_pool_router::PoolProvider,
    cache: ModelMetadataCache,
    fallback_interval_ms: u64,
    shutdown: CancellationToken,
) -> Result<(), anyhow::Error> {
    run_with_backoff(
        pools,
        listener_pools,
        cache,
        fallback_interval_ms,
        INITIAL_LISTENER_BACKOFF,
        MAX_LISTENER_BACKOFF,
        shutdown,
    )
    .await
}

/// [`run`] with an explicit backoff schedule so tests can use millisecond
/// delays instead of waiting out the production 1s -> 30s ramp.
#[allow(clippy::too_many_arguments)]
async fn run_with_backoff(
    pools: impl sqlx_pool_router::PoolProvider,
    listener_pools: impl sqlx_pool_router::PoolProvider,
    cache: ModelMetadataCache,
    fallback_interval_ms: u64,
    initial_backoff: Duration,
    max_backoff: Duration,
    shutdown: CancellationToken,
) -> Result<(), anyhow::Error> {
    let pools = sqlx_pool_router::DynPools::new(pools);
    let listener_pools = sqlx_pool_router::DynPools::new(listener_pools);
    const MIN_RELOAD_INTERVAL: Duration = Duration::from_millis(100);
    let fallback = (fallback_interval_ms > 0).then(|| Duration::from_millis(fallback_interval_ms));

    'outer: loop {
        // Fail-open: never return on a DB error. Retry the listener setup with
        // capped exponential backoff, resetting the delay after each success,
        // and keep honouring shutdown while we wait.
        let mut backoff = initial_backoff;
        let mut listener = loop {
            match connect_listener(&listener_pools).await {
                Ok(listener) => break listener,
                Err(e) => {
                    crate::background_error!(
                        component::MODEL_METADATA_SYNC,
                        "listener_connect",
                        Error,
                        error = %e,
                        backoff_ms = backoff.as_millis(),
                        "Model metadata sync: listener setup failed, retrying"
                    );
                }
            }
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = next_backoff(backoff, max_backoff);
        };
        info!("Started model metadata sync listener");

        // A change committed before this subscription (or while the connection
        // was down) is never delivered over LISTEN, so reload once immediately
        // after every successful (re)connect. This keeps the cache correct even
        // when the periodic fallback reload is disabled.
        reload(&pools.write(), &cache).await;
        let mut last_reload = std::time::Instant::now();
        let mut pending_reload: Option<tokio::time::Instant> = None;
        let mut fallback_timer = fallback.map(|interval| {
            let mut timer = tokio::time::interval(interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            timer
        });

        loop {
            let tick = async {
                match fallback_timer.as_mut() {
                    Some(timer) => timer.tick().await,
                    None => std::future::pending().await,
                }
            };
            let coalesced = async {
                match pending_reload {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = shutdown.cancelled() => break 'outer,
                notif = listener.try_recv() => match notif {
                    Ok(Some(_)) => {
                        if last_reload.elapsed() < MIN_RELOAD_INTERVAL {
                            let delay = MIN_RELOAD_INTERVAL.saturating_sub(last_reload.elapsed());
                            pending_reload.get_or_insert_with(|| tokio::time::Instant::now() + delay);
                            continue;
                        }
                        last_reload = std::time::Instant::now();
                        pending_reload = None;
                        reload(&pools.write(), &cache).await;
                    }
                    Ok(None) => {
                        debug!("Model metadata sync: connection lost, reconnecting");
                        break;
                    }
                    Err(e) => {
                        crate::background_error!(component::MODEL_METADATA_SYNC, "listen", Error, error = %e, "Model metadata sync: listener error");
                        break;
                    }
                },
                _ = coalesced => {
                    pending_reload = None;
                    last_reload = std::time::Instant::now();
                    reload(&pools.write(), &cache).await;
                },
                _ = tick => {
                    if last_reload.elapsed() < MIN_RELOAD_INTERVAL { continue; }
                    last_reload = std::time::Instant::now();
                    reload(&pools.write(), &cache).await;
                }
            }
        }
    }
    Ok(())
}

async fn reload(pool: &PgPool, cache: &ModelMetadataCache) {
    match refresh(pool, cache).await {
        Ok(models) => debug!(models, "Model metadata sync: reloaded map"),
        Err(e) => {
            crate::background_error!(component::MODEL_METADATA_SYNC, "load", Error, error = %e, "Model metadata sync: failed to reload map");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(model_type: Option<ModelType>, context_window: Option<u64>, max_output_tokens: Option<u64>) -> ModelInfo {
        ModelInfo {
            model_type,
            context_window,
            max_output_tokens,
            capabilities: None,
        }
    }

    #[test]
    fn empty_cache_reports_not_loaded() {
        let cache = ModelMetadataCache::empty();
        assert!(matches!(cache.lookup("gpt-4o"), ModelLookup::NotLoaded));
    }

    #[test]
    fn loaded_cache_distinguishes_known_from_unknown() {
        let cache = ModelMetadataCache::from_infos([("gpt-4o".to_string(), info(Some(ModelType::Chat), Some(128_000), Some(16_384)))]);
        match cache.lookup("gpt-4o") {
            ModelLookup::Known(model) => {
                assert_eq!(model.model_type, Some(ModelType::Chat));
                assert_eq!(model.context_window, Some(128_000));
                assert_eq!(model.max_output_tokens, Some(16_384));
            }
            other => panic!("expected Known, got {other:?}"),
        }
        assert!(matches!(cache.lookup("missing"), ModelLookup::Unknown));
    }

    #[test]
    fn an_empty_loaded_cache_is_unknown_not_not_loaded() {
        // The first successful load of an empty table flips the cache from
        // NotLoaded to "loaded, nothing here": unknown aliases are caught then,
        // not mistaken for a cold start.
        let cache = ModelMetadataCache::from_infos(std::iter::empty::<(String, ModelInfo)>());
        assert!(matches!(cache.lookup("anything"), ModelLookup::Unknown));
    }

    #[test]
    fn clones_share_one_map_so_a_replace_is_visible_to_all_handles() {
        let cache = ModelMetadataCache::empty();
        let handle = cache.clone();
        assert!(matches!(handle.lookup("gpt-4o"), ModelLookup::NotLoaded));
        cache.replace(HashMap::from([("gpt-4o".to_string(), Arc::new(info(None, None, None)))]));
        assert!(matches!(handle.lookup("gpt-4o"), ModelLookup::Known(_)));
    }

    #[test]
    fn token_count_accepts_only_positive_integers() {
        let metadata = serde_json::json!({
            "context_window": 128_000,
            "max_output_tokens": 16_384,
        });
        assert_eq!(token_count(&metadata, "context_window"), Some(128_000));
        assert_eq!(token_count(&metadata, "max_output_tokens"), Some(16_384));
        for bad in [
            serde_json::json!({"context_window": 0}),
            serde_json::json!({"context_window": -1}),
            serde_json::json!({"context_window": 1.5}),
            serde_json::json!({"context_window": "128000"}),
            serde_json::json!({"context_window": null}),
            serde_json::json!({}),
        ] {
            assert_eq!(token_count(&bad, "context_window"), None, "{bad}");
        }
    }

    #[test]
    fn model_type_matches_the_handler_mapping() {
        assert_eq!(model_type_from_str(Some("CHAT")), Some(ModelType::Chat));
        assert_eq!(model_type_from_str(Some("EMBEDDINGS")), Some(ModelType::Embeddings));
        assert_eq!(model_type_from_str(Some("RERANKER")), Some(ModelType::Reranker));
        assert_eq!(model_type_from_str(Some("chat")), None);
        assert_eq!(model_type_from_str(None), None);
    }

    #[test]
    fn listener_backoff_doubles_then_caps() {
        let max = Duration::from_secs(30);
        assert_eq!(next_backoff(Duration::from_secs(1), max), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(16), max), Duration::from_secs(30));
        // Already at the cap: it must not keep growing (no overflow, no panic).
        assert_eq!(next_backoff(max, max), max);
    }

    // --- Integration tests: real DB, real trigger, real LISTEN/NOTIFY ---

    const SYSTEM_USER: &str = "00000000-0000-0000-0000-000000000000";
    const ENDPOINT_ID: &str = "30000000-0000-0000-0000-00000000e001";
    const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    async fn insert_endpoint(pool: &PgPool) {
        sqlx::query("INSERT INTO inference_endpoints (id, name, url, created_by) VALUES ($1::uuid, $2, $3, $4::uuid)")
            .bind(ENDPOINT_ID)
            .bind("model-metadata-test-endpoint")
            .bind("https://example.test/v1")
            .bind(SYSTEM_USER)
            .execute(pool)
            .await
            .unwrap();
    }

    /// Insert a deployment row. Composite models (which onwards routes without
    /// an endpoint) must have `hosted_on = NULL`; regular models need one.
    #[allow(clippy::too_many_arguments)]
    async fn insert_model(
        pool: &PgPool,
        id: &str,
        alias: &str,
        model_type: Option<&str>,
        capabilities: Option<Vec<String>>,
        metadata: serde_json::Value,
        is_composite: bool,
        deleted: bool,
    ) {
        let hosted_on: Option<&str> = (!is_composite).then_some(ENDPOINT_ID);
        sqlx::query(
            r#"
            INSERT INTO deployed_models (id, model_name, alias, type, capabilities, created_by, hosted_on, metadata, is_composite, deleted)
            VALUES ($1::uuid, $2, $3, $4, $5, $6::uuid, $7::uuid, $8::jsonb, $9, $10)
            "#,
        )
        .bind(id)
        .bind(alias)
        .bind(alias)
        .bind(model_type)
        .bind(capabilities)
        .bind(SYSTEM_USER)
        .bind(hosted_on)
        .bind(metadata)
        .bind(is_composite)
        .bind(deleted)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Poll until `cond` is true, failing the test after [`POLL_TIMEOUT`].
    /// What the cache should look like before and after the change is asserted
    /// by the caller, not here - this only waits for the async refresh to land.
    async fn wait_until(mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + POLL_TIMEOUT;
        loop {
            if cond() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "condition not met before timeout");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Barrier: wait until the background `run` task's `LISTEN` has completed,
    /// so a subsequent write cannot race the subscription and be missed.
    /// `pg_stat_activity` shows a statement from the moment it starts, and a
    /// NOTIFY committed before the LISTEN commits is never delivered, so the
    /// connection must also be back to `idle`. Scoped to this test database so
    /// parallel tests cannot satisfy it.
    async fn wait_for_listen_connection(pool: &PgPool) {
        let deadline = std::time::Instant::now() + POLL_TIMEOUT;
        loop {
            let listener: Option<i32> = sqlx::query_scalar(
                "SELECT pid FROM pg_stat_activity \
                 WHERE query LIKE 'LISTEN%auth_config_changed%' AND state = 'idle' \
                 AND datname = current_database() AND pid != pg_backend_pid() LIMIT 1",
            )
            .fetch_optional(pool)
            .await
            .unwrap();
            if listener.is_some() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "listener never connected");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[sqlx::test]
    async fn initial_cache_loads_routed_models_and_excludes_deleted(pool: PgPool) {
        insert_endpoint(&pool).await;
        insert_model(
            &pool,
            "40000000-0000-0000-0000-00000000e001",
            "regular-model",
            Some("CHAT"),
            Some(vec!["tools".to_string()]),
            serde_json::json!({"context_window": 128_000, "max_output_tokens": 16_384}),
            false,
            false,
        )
        .await;
        insert_model(
            &pool,
            "40000000-0000-0000-0000-00000000e002",
            "composite-model",
            Some("RERANKER"),
            None,
            serde_json::json!({}),
            true,
            false,
        )
        .await;
        insert_model(
            &pool,
            "40000000-0000-0000-0000-00000000e003",
            "deleted-model",
            Some("CHAT"),
            None,
            serde_json::json!({}),
            false,
            true,
        )
        .await;

        let cache = initial_cache(&pool).await.unwrap();

        match cache.lookup("regular-model") {
            ModelLookup::Known(model) => {
                assert_eq!(model.model_type, Some(ModelType::Chat));
                assert_eq!(model.context_window, Some(128_000));
                assert_eq!(model.max_output_tokens, Some(16_384));
                assert_eq!(model.capabilities.as_deref(), Some(["tools"].map(String::from).as_slice()));
            }
            other => panic!("regular-model should be Known, got {other:?}"),
        }
        assert!(matches!(cache.lookup("composite-model"), ModelLookup::Known(_)));
        assert!(matches!(cache.lookup("deleted-model"), ModelLookup::Unknown));
        assert!(matches!(cache.lookup("never-existed"), ModelLookup::Unknown));
    }

    #[sqlx::test]
    async fn notify_refreshes_cache_until_the_model_is_deleted(pool: PgPool) {
        insert_endpoint(&pool).await;
        insert_model(
            &pool,
            "40000000-0000-0000-0000-00000000e101",
            "live-model",
            Some("CHAT"),
            None,
            serde_json::json!({}),
            false,
            false,
        )
        .await;

        let cache = initial_cache(&pool).await.unwrap();
        // First state: the model is routed, but the row carries no token counts.
        match cache.lookup("live-model") {
            ModelLookup::Known(model) => {
                assert_eq!(model.context_window, None);
                assert_eq!(model.max_output_tokens, None);
            }
            other => panic!("live-model should be Known before the update, got {other:?}"),
        }

        let shutdown = CancellationToken::new();
        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            let pools = pool.clone();
            let listener_pools = pool.clone();
            let cache = cache.clone();
            async move { run(pools, listener_pools, cache, 0, shutdown).await }
        });
        wait_for_listen_connection(&pool).await;

        // An UPDATE to metadata must be picked up over LISTEN/NOTIFY.
        sqlx::query("UPDATE deployed_models SET metadata = $1 WHERE alias = 'live-model'")
            .bind(serde_json::json!({"context_window": 4096, "max_output_tokens": 512}))
            .execute(&pool)
            .await
            .unwrap();
        wait_until(|| matches!(cache.lookup("live-model"), ModelLookup::Known(m) if m.context_window == Some(4096) && m.max_output_tokens == Some(512))).await;

        // Soft-deleting removes the alias from the routed set.
        sqlx::query("UPDATE deployed_models SET deleted = TRUE WHERE alias = 'live-model'")
            .execute(&pool)
            .await
            .unwrap();
        wait_until(|| matches!(cache.lookup("live-model"), ModelLookup::Unknown)).await;

        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("run task should stop on shutdown")
            .expect("run task should not panic")
            .expect("run should return Ok after shutdown");
    }

    /// A [`sqlx_pool_router::PoolProvider`] whose first `write()` returns a pool
    /// that cannot connect, then serves the real pool. Proves `run` retries
    /// listener setup instead of returning an error and dying.
    #[derive(Clone)]
    struct FailFirstListenerPools {
        good: PgPool,
        bad: PgPool,
        writes: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl sqlx_pool_router::PoolProvider for FailFirstListenerPools {
        fn read(&self) -> sqlx_pool_router::PoolHandle {
            self.good.read()
        }

        fn write(&self) -> sqlx_pool_router::PoolHandle {
            if self.writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.bad.write()
            } else {
                self.good.write()
            }
        }
    }

    #[sqlx::test]
    async fn run_survives_a_failed_listener_connect_and_eventually_serves(pool: PgPool) {
        insert_endpoint(&pool).await;
        insert_model(
            &pool,
            "40000000-0000-0000-0000-00000000e201",
            "resilient-model",
            Some("CHAT"),
            None,
            serde_json::json!({}),
            false,
            false,
        )
        .await;

        // Cold cache: every alias reads NotLoaded until the listener task loads.
        let cache = ModelMetadataCache::empty();
        assert!(matches!(cache.lookup("resilient-model"), ModelLookup::NotLoaded));

        // A lazily-connected pool to a non-routable address fails only when
        // acquired, so the first listener-setup attempt fails and every later
        // one succeeds. The acquire timeout bounds the failure.
        let bad = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://postgres:password@192.0.2.1:1/nonexistent")
            .expect("lazy pool URL should parse");
        let listener_pools = FailFirstListenerPools {
            good: pool.clone(),
            bad,
            writes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };

        let shutdown = CancellationToken::new();
        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            let cache = cache.clone();
            let pools = pool.clone();
            let listener_pools = listener_pools.clone();
            async move {
                run_with_backoff(
                    pools,
                    listener_pools,
                    cache,
                    0,
                    Duration::from_millis(20),
                    Duration::from_millis(100),
                    shutdown,
                )
                .await
            }
        });

        // The task must not give up: despite the failed connect, the cache
        // eventually serves because the retry succeeded and reloaded.
        wait_until(|| matches!(cache.lookup("resilient-model"), ModelLookup::Known(_))).await;
        assert!(!handle.is_finished(), "run must keep running, not return after a DB error");
        assert!(
            listener_pools.writes.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the failed connect must have been retried"
        );

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run should stop on shutdown")
            .expect("run task should not panic")
            .expect("run should return Ok despite the earlier failure");
    }

    #[sqlx::test]
    async fn reload_after_listen_picks_up_changes_committed_before_subscribe(pool: PgPool) {
        insert_endpoint(&pool).await;
        // Commit the row BEFORE the listener subscribes. A NOTIFY committed with
        // no listener attached is never delivered, so without the reload that
        // runs immediately after LISTEN this row would stay missing forever.
        insert_model(
            &pool,
            "40000000-0000-0000-0000-00000000e202",
            "pre-subscribe-model",
            Some("CHAT"),
            None,
            serde_json::json!({}),
            false,
            false,
        )
        .await;

        let cache = ModelMetadataCache::empty();
        assert!(matches!(cache.lookup("pre-subscribe-model"), ModelLookup::NotLoaded));

        let shutdown = CancellationToken::new();
        // Fallback disabled (0): the reload-after-listen is the only path that
        // can ever load this pre-existing row.
        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            let cache = cache.clone();
            let pools = pool.clone();
            async move { run(pools.clone(), pools, cache, 0, shutdown).await }
        });

        wait_until(|| matches!(cache.lookup("pre-subscribe-model"), ModelLookup::Known(_))).await;

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run should stop on shutdown")
            .expect("run task should not panic")
            .expect("run should return Ok after shutdown");
    }
}
