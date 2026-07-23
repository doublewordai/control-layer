//! Lock-free API-key metadata cache for response hot paths.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use sqlx::{FromRow, PgPool, postgres::PgListener};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use uuid::Uuid;

use crate::config::ONWARDS_CONFIG_CHANGED_CHANNEL;
use crate::db::errors::DbError;
use crate::db::handlers::api_keys::ApiKeys;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::metrics::errors::component;

/// Response hot-path metadata for one non-deleted API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyMetadata {
    pub owner_id: Uuid,
    pub created_by: Uuid,
    pub purpose: ApiKeyPurpose,
    pub verified: bool,
    pub zero_data_retention: bool,
    pub hidden_batch_key: Option<String>,
    /// Whether `hidden_batch_key` was joined through this key's cap-scope
    /// child. A shared fallback is not durable authority because a child may
    /// be created while the in-memory snapshot still names the shared key.
    pub hidden_batch_key_is_child: bool,
}

/// Cheap-to-clone handle to an atomically replaced API-key metadata snapshot.
#[derive(Clone)]
pub struct ApiKeyMetadataCache {
    inner: Arc<ArcSwap<HashMap<String, ApiKeyMetadata>>>,
    refresh_lock: Arc<Mutex<()>>,
}

impl ApiKeyMetadataCache {
    /// Construct an empty snapshot.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            refresh_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Return a cloned metadata entry, or `None` for deleted and unknown keys.
    pub fn get(&self, secret: &str) -> Option<ApiKeyMetadata> {
        self.inner.load().get(secret).cloned()
    }

    /// Read an owning account's ZDR flag without cloning the metadata entry.
    pub fn is_zdr(&self, secret: &str) -> bool {
        self.inner.load().get(secret).is_some_and(|metadata| metadata.zero_data_retention)
    }

    pub(crate) fn replace(&self, map: HashMap<String, ApiKeyMetadata>) {
        self.inner.store(Arc::new(map));
    }
}

/// Hidden Flex key identity resolved from a presented API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFlexKey {
    pub secret: String,
    pub owner_id: Uuid,
    pub verified: bool,
}

/// Resolves hidden batch identities and serializes cold creation by owner and creator.
///
/// Snapshot-proven and positively checked spend-cap children stay on the
/// memory-only hot path. A shared fallback is rechecked against PostgreSQL on
/// every Flex resolution because a cap child can appear between snapshots.
#[derive(Clone)]
pub struct FlexBatchKeyResolver {
    pool: PgPool,
    cache: ApiKeyMetadataCache,
    locks: Arc<ResolverLocks>,
    resolutions: Arc<DashMap<String, CachedFlexResolution>>,
}

type ResolverLocks = DashMap<(Uuid, Uuid), Arc<Mutex<()>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedFlexResolution {
    /// Execution key visible in the metadata snapshot when this result was
    /// checked against PostgreSQL.
    snapshot_secret: Option<String>,
    /// Authoritative child key. Negative/shared resolutions are never cached.
    resolved_secret: String,
}

impl FlexBatchKeyResolver {
    /// Bind a resolver to the shared metadata snapshot and primary database pool.
    pub fn new(pool: PgPool, cache: ApiKeyMetadataCache) -> Self {
        Self {
            pool,
            cache,
            locks: Arc::new(DashMap::new()),
            resolutions: Arc::new(DashMap::new()),
        }
    }

    /// Resolve a presented API key to its owner-scoped hidden batch key.
    pub async fn resolve_hidden_batch_key(&self, presented_secret: &str) -> Result<Option<ResolvedFlexKey>, DbError> {
        let Some(metadata) = self.cache.get(presented_secret) else {
            return Ok(None);
        };
        if metadata.hidden_batch_key_is_child {
            let secret = metadata
                .hidden_batch_key
                .clone()
                .expect("child provenance requires a hidden batch key");
            return Ok(Some(resolved_flex_key(secret, &metadata)));
        }
        if let Some(resolved) = self.cached_resolution(presented_secret, &metadata) {
            return Ok(Some(resolved));
        }

        let lock_key = (metadata.owner_id, metadata.created_by);
        let lock = self.locks.entry(lock_key).or_insert_with(|| Arc::new(Mutex::new(()))).clone();
        let _guard = lock.lock().await;

        let Some(metadata) = self.cache.get(presented_secret) else {
            return Ok(None);
        };
        if metadata.hidden_batch_key_is_child {
            let secret = metadata
                .hidden_batch_key
                .clone()
                .expect("child provenance requires a hidden batch key");
            return Ok(Some(resolved_flex_key(secret, &metadata)));
        }
        if let Some(resolved) = self.cached_resolution(presented_secret, &metadata) {
            return Ok(Some(resolved));
        }

        // A cap-scope child can be created after this process loaded its
        // snapshot. Check PostgreSQL before every shared fallback; caching a
        // negative result would bypass per-key spend attribution until the
        // independently refreshed metadata snapshot caught up.
        let child_secret = sqlx::query_scalar::<_, String>(
            r#"
            SELECT child.secret
            FROM api_keys parent
            JOIN api_keys child
              ON child.parent_api_key_id = parent.id
             AND child.purpose = 'batch'
             AND child.hidden = true
             AND child.is_deleted = false
            WHERE parent.secret = $1
              AND parent.is_deleted = false
            LIMIT 1
            "#,
        )
        .bind(presented_secret)
        .fetch_optional(&self.pool)
        .await?;

        let (secret, resolved_metadata, cache_positive_child) = match child_secret {
            Some(secret) => (secret, metadata, true),
            None if metadata.hidden_batch_key.is_some() => {
                let secret = metadata.hidden_batch_key.clone().expect("guard above requires a cached shared key");
                (secret, metadata, false)
            }
            None => {
                let mut conn = self.pool.acquire().await?;
                let shared_secret = ApiKeys::new(&mut conn)
                    .get_or_create_hidden_key(metadata.owner_id, ApiKeyPurpose::Batch, metadata.created_by)
                    .await?;
                refresh(&self.pool, &self.cache).await?;
                let Some(refreshed_metadata) = self.cache.get(presented_secret) else {
                    return Ok(None);
                };
                if refreshed_metadata.hidden_batch_key_is_child {
                    let child_secret = refreshed_metadata
                        .hidden_batch_key
                        .clone()
                        .expect("child provenance requires a hidden batch key");
                    (child_secret, refreshed_metadata, true)
                } else {
                    (shared_secret, refreshed_metadata, false)
                }
            }
        };
        if cache_positive_child {
            self.resolutions.insert(
                presented_secret.to_string(),
                CachedFlexResolution {
                    snapshot_secret: resolved_metadata.hidden_batch_key.clone(),
                    resolved_secret: secret.clone(),
                },
            );
        } else {
            // A negative lookup is never durable authority: a cap child may
            // appear without changing the current metadata snapshot. Recheck
            // PostgreSQL on the next Flex request.
            self.resolutions.remove(presented_secret);
        }
        Ok(Some(resolved_flex_key(secret, &resolved_metadata)))
    }

    fn cached_resolution(&self, presented_secret: &str, metadata: &ApiKeyMetadata) -> Option<ResolvedFlexKey> {
        self.resolutions
            .get(presented_secret)
            .filter(|resolution| resolution.snapshot_secret == metadata.hidden_batch_key)
            .map(|resolution| resolved_flex_key(resolution.resolved_secret.clone(), metadata))
    }
}

fn resolved_flex_key(secret: String, metadata: &ApiKeyMetadata) -> ResolvedFlexKey {
    ResolvedFlexKey {
        secret,
        owner_id: metadata.owner_id,
        verified: metadata.verified,
    }
}

#[derive(FromRow)]
struct ApiKeyMetadataRow {
    secret: String,
    owner_id: Uuid,
    created_by: Uuid,
    purpose: ApiKeyPurpose,
    verified: bool,
    zero_data_retention: bool,
    hidden_batch_key: Option<String>,
    hidden_batch_key_is_child: bool,
}

async fn load(pool: &PgPool) -> Result<HashMap<String, ApiKeyMetadata>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ApiKeyMetadataRow>(
        r#"
        SELECT
            ak.secret,
            ak.user_id AS owner_id,
            ak.created_by,
            ak.purpose,
            u.verified,
            u.zero_data_retention,
            COALESCE(child.secret, batch.secret) AS hidden_batch_key,
            child.secret IS NOT NULL AS hidden_batch_key_is_child
        FROM api_keys ak
        JOIN users u ON u.id = ak.user_id
        LEFT JOIN api_keys child
          ON child.parent_api_key_id = ak.id
         AND child.purpose = 'batch'
         AND child.hidden = true
         AND child.is_deleted = false
        LEFT JOIN api_keys batch
          ON batch.user_id = ak.user_id
         AND batch.created_by = ak.created_by
         AND batch.purpose = 'batch'
         AND batch.hidden = true
         AND batch.is_deleted = false
         AND batch.parent_api_key_id IS NULL
        WHERE ak.is_deleted = false
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.secret,
                ApiKeyMetadata {
                    owner_id: row.owner_id,
                    created_by: row.created_by,
                    purpose: row.purpose,
                    verified: row.verified,
                    zero_data_retention: row.zero_data_retention,
                    hidden_batch_key: row.hidden_batch_key,
                    hidden_batch_key_is_child: row.hidden_batch_key_is_child,
                },
            )
        })
        .collect())
}

/// Atomically replace the cache from the database and return its entry count.
pub async fn refresh(pool: &PgPool, cache: &ApiKeyMetadataCache) -> Result<usize, sqlx::Error> {
    refresh_with_loader(cache, || load(pool)).await
}

async fn refresh_with_loader<F, Fut>(cache: &ApiKeyMetadataCache, loader: F) -> Result<usize, sqlx::Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<HashMap<String, ApiKeyMetadata>, sqlx::Error>>,
{
    let _refresh_guard = cache.refresh_lock.lock().await;
    let map = loader().await?;
    let count = map.len();
    cache.replace(map);
    Ok(count)
}

/// Synchronously load the initial snapshot before traffic is served.
pub async fn initial_cache(pool: &PgPool) -> Result<ApiKeyMetadataCache, sqlx::Error> {
    let cache = ApiKeyMetadataCache::empty();
    refresh(pool, &cache).await?;
    Ok(cache)
}

/// Refresh the snapshot on auth notifications and the configured fallback interval.
pub async fn run(
    pool: PgPool,
    cache: ApiKeyMetadataCache,
    fallback_interval_ms: u64,
    shutdown: CancellationToken,
) -> Result<(), anyhow::Error> {
    const MIN_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
    let fallback = (fallback_interval_ms > 0).then(|| std::time::Duration::from_millis(fallback_interval_ms));

    'outer: loop {
        let mut listener = PgListener::connect_with(&pool).await?;
        listener.listen(ONWARDS_CONFIG_CHANGED_CHANNEL).await?;
        // LISTEN has no replay. Refresh after subscribing so a change committed
        // while the previous connection was unavailable cannot remain hidden
        // until the long fallback interval; a concurrent change is either in
        // this snapshot or queued as a notification on the new connection.
        loop {
            let refreshed = tokio::select! {
                refreshed = reload(&pool, &cache) => refreshed,
                _ = shutdown.cancelled() => break 'outer,
            };
            if refreshed {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(MIN_RELOAD_INTERVAL) => {},
                _ = shutdown.cancelled() => break 'outer,
            }
        }
        info!("Started API key metadata sync listener");

        let mut last_reload = tokio::time::Instant::now();
        let mut trailing_reload = None;
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
            let trailing_deadline = trailing_reload;
            let trailing = async move {
                match trailing_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = shutdown.cancelled() => break 'outer,
                notification = listener.try_recv() => match notification {
                    Ok(Some(_)) => {
                        let elapsed = last_reload.elapsed();
                        if elapsed < MIN_RELOAD_INTERVAL {
                            trailing_reload = Some(last_reload + MIN_RELOAD_INTERVAL);
                        } else {
                            trailing_reload = None;
                            last_reload = tokio::time::Instant::now();
                            if !reload(&pool, &cache).await {
                                trailing_reload =
                                    Some(tokio::time::Instant::now() + MIN_RELOAD_INTERVAL);
                            }
                        }
                    }
                    Ok(None) => {
                        debug!("API key metadata sync: connection lost, reconnecting");
                        break;
                    }
                    Err(error) => {
                        crate::background_error!(component::API_KEY_CACHE_SYNC, "listen", Error, error = %error, "API key metadata sync: listener error");
                        break;
                    }
                },
                _ = trailing => {
                    trailing_reload = None;
                    last_reload = tokio::time::Instant::now();
                    if !reload(&pool, &cache).await {
                        trailing_reload =
                            Some(tokio::time::Instant::now() + MIN_RELOAD_INTERVAL);
                    }
                }
                _ = tick => {
                    if last_reload.elapsed() < MIN_RELOAD_INTERVAL { continue; }
                    trailing_reload = None;
                    last_reload = tokio::time::Instant::now();
                    if !reload(&pool, &cache).await {
                        trailing_reload =
                            Some(tokio::time::Instant::now() + MIN_RELOAD_INTERVAL);
                    }
                }
            }
        }
    }
    Ok(())
}

async fn reload(pool: &PgPool, cache: &ApiKeyMetadataCache) -> bool {
    match refresh(pool, cache).await {
        Ok(keys) => {
            debug!(keys, "API key metadata sync: reloaded map");
            true
        }
        Err(error) => {
            crate::background_error!(component::API_KEY_CACHE_SYNC, "load", Error, error = %error, "API key metadata sync: failed to reload map");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use uuid::Uuid;

    use super::*;
    use crate::api::models::api_keys::ApiKeyCreate;
    use crate::api::models::users::Role;
    use crate::db::handlers::{Repository, api_keys::ApiKeys};
    use crate::db::models::api_keys::ApiKeyCreateDBRequest;

    async fn create_realtime_key(pool: &sqlx::PgPool, owner_id: Uuid, created_by: Uuid, name: &str) -> String {
        let mut conn = pool.acquire().await.expect("acquire API-key connection");
        ApiKeys::new(&mut conn)
            .create(&ApiKeyCreateDBRequest::new(
                owner_id,
                created_by,
                ApiKeyCreate {
                    name: name.to_string(),
                    description: None,
                    purpose: ApiKeyPurpose::Realtime,
                    requests_per_second: None,
                    burst_size: None,
                    member_id: None,
                },
            ))
            .await
            .expect("create realtime API key")
            .secret
    }
    #[test]
    fn metadata_lookup_supplies_owner_zdr_and_batch_identity() {
        let owner_id = Uuid::new_v4();
        let created_by = Uuid::new_v4();
        let metadata = ApiKeyMetadata {
            owner_id,
            created_by,
            purpose: ApiKeyPurpose::Realtime,
            verified: true,
            zero_data_retention: true,
            hidden_batch_key: Some("sk-batch".to_string()),
            hidden_batch_key_is_child: true,
        };
        let cache = ApiKeyMetadataCache::empty();
        cache.replace(HashMap::from([("sk-presented".to_string(), metadata.clone())]));

        assert_eq!(cache.get("sk-presented"), Some(metadata));
        assert!(cache.is_zdr("sk-presented"));
    }

    #[test]
    fn absent_keys_return_none() {
        let cache = ApiKeyMetadataCache::empty();

        assert_eq!(cache.get("sk-deleted-or-invalid"), None);
        assert!(!cache.is_zdr("sk-deleted-or-invalid"));
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_refreshes_cannot_roll_snapshot_backward() {
        fn snapshot(zero_data_retention: bool) -> HashMap<String, ApiKeyMetadata> {
            HashMap::from([(
                "sk-overlap".to_string(),
                ApiKeyMetadata {
                    owner_id: Uuid::nil(),
                    created_by: Uuid::nil(),
                    purpose: ApiKeyPurpose::Realtime,
                    verified: false,
                    zero_data_retention,
                    hidden_batch_key: None,
                    hidden_batch_key_is_child: false,
                },
            )])
        }

        let cache = ApiKeyMetadataCache::empty();
        let (old_started_tx, old_started_rx) = tokio::sync::oneshot::channel();
        let (release_old_tx, release_old_rx) = tokio::sync::oneshot::channel();
        let old_cache = cache.clone();
        let old_refresh = tokio::spawn(async move {
            refresh_with_loader(&old_cache, || async move {
                old_started_tx.send(()).expect("signal old load started");
                release_old_rx.await.expect("release old load");
                Ok::<_, sqlx::Error>(snapshot(false))
            })
            .await
        });
        old_started_rx.await.expect("old load should start");

        let (new_started_tx, mut new_started_rx) = tokio::sync::oneshot::channel();
        let new_cache = cache.clone();
        let new_refresh = tokio::spawn(async move {
            refresh_with_loader(&new_cache, || async move {
                new_started_tx.send(()).expect("signal new load started");
                Ok::<_, sqlx::Error>(snapshot(true))
            })
            .await
        });

        let new_started_before_old_finished = tokio::time::timeout(std::time::Duration::from_millis(50), &mut new_started_rx)
            .await
            .is_ok();
        if new_started_before_old_finished {
            new_refresh.await.expect("join new refresh").expect("new refresh succeeds");
            release_old_tx.send(()).expect("release old refresh");
            old_refresh.await.expect("join old refresh").expect("old refresh succeeds");
        } else {
            release_old_tx.send(()).expect("release old refresh");
            old_refresh.await.expect("join old refresh").expect("old refresh succeeds");
            new_refresh.await.expect("join new refresh").expect("new refresh succeeds");
        }

        assert!(cache.is_zdr("sk-overlap"), "the older snapshot must not replace the newer one");
    }

    #[sqlx::test]
    async fn deleted_keys_disappear_after_refresh(pool: sqlx::PgPool) {
        let owner = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let secret = create_realtime_key(&pool, owner.id, owner.id, "refresh deletion").await;
        let cache = initial_cache(&pool).await.expect("initial cache load");
        assert!(cache.get(&secret).is_some());

        sqlx::query("UPDATE api_keys SET is_deleted = true WHERE secret = $1")
            .bind(&secret)
            .execute(&pool)
            .await
            .expect("soft-delete API key");
        refresh(&pool, &cache).await.expect("refresh after deletion");

        assert_eq!(cache.get(&secret), None);
    }

    #[sqlx::test]
    async fn owner_policy_changes_appear_after_refresh(pool: sqlx::PgPool) {
        let owner = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let secret = create_realtime_key(&pool, owner.id, owner.id, "refresh owner policy").await;
        let cache = initial_cache(&pool).await.expect("initial cache load");

        sqlx::query("UPDATE users SET verified = true, zero_data_retention = true WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .expect("change owner policy");
        refresh(&pool, &cache).await.expect("refresh after owner policy change");

        let metadata = cache.get(&secret).expect("presented key metadata");
        assert!(metadata.verified);
        assert!(metadata.zero_data_retention);
    }

    #[sqlx::test]
    async fn notification_inside_debounce_gets_trailing_refresh(pool: sqlx::PgPool) {
        let owner = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let secret = create_realtime_key(&pool, owner.id, owner.id, "debounced owner policy").await;
        let cache = initial_cache(&pool).await.expect("initial cache load");
        let shutdown = CancellationToken::new();
        let sync_task = tokio::spawn(run(pool.clone(), cache.clone(), 0, shutdown.clone()));

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let listeners: i64 = sqlx::query_scalar(
                    r#"
                    SELECT COUNT(*)
                    FROM pg_stat_activity
                    WHERE datname = current_database()
                      AND query LIKE 'LISTEN%auth_config_changed%'
                    "#,
                )
                .fetch_one(&pool)
                .await
                .expect("inspect listener activity");
                if listeners > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("API-key cache listener should subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(110)).await;

        sqlx::query("UPDATE users SET zero_data_retention = true WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .expect("enable ZDR");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !cache.is_zdr(&secret) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first notification should refresh immediately");

        sqlx::query("UPDATE users SET zero_data_retention = false WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .expect("disable ZDR inside debounce window");
        tokio::time::timeout(std::time::Duration::from_millis(500), async {
            while cache.is_zdr(&secret) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("debounced notification should receive a trailing refresh without fallback");

        shutdown.cancel();
        sync_task
            .await
            .expect("join cache sync task")
            .expect("cache sync should shut down cleanly");
    }

    #[sqlx::test]
    async fn listener_start_refreshes_changes_missed_before_subscription(pool: sqlx::PgPool) {
        let owner = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let secret = create_realtime_key(&pool, owner.id, owner.id, "pre-listen policy change").await;
        let cache = initial_cache(&pool).await.expect("initial cache load");
        assert!(!cache.is_zdr(&secret));

        // This notification has no subscriber. Starting the listener must
        // still close that gap by refreshing after LISTEN succeeds.
        sqlx::query("UPDATE users SET zero_data_retention = true WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .expect("change policy before listener starts");

        let shutdown = CancellationToken::new();
        let sync_task = tokio::spawn(run(pool.clone(), cache.clone(), 0, shutdown.clone()));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !cache.is_zdr(&secret) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-LISTEN refresh should observe the missed change");

        shutdown.cancel();
        sync_task
            .await
            .expect("join cache sync task")
            .expect("cache sync should shut down cleanly");
    }

    #[sqlx::test]
    async fn listener_start_retries_a_failed_post_subscription_refresh(pool: sqlx::PgPool) {
        let owner = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let secret = create_realtime_key(&pool, owner.id, owner.id, "post-listen retry").await;
        let cache = initial_cache(&pool).await.expect("initial cache load");
        assert!(!cache.is_zdr(&secret));

        // Commit a change with no listener, then make the first post-LISTEN
        // snapshot load fail. Restoring the table emits no auth notification,
        // so fallback=0 can recover only if startup retries the failed load.
        sqlx::query("UPDATE users SET zero_data_retention = true WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .expect("change policy before listener starts");
        sqlx::query("ALTER TABLE api_keys RENAME TO api_keys_temporarily_unavailable")
            .execute(&pool)
            .await
            .expect("make first post-LISTEN load fail");

        let shutdown = CancellationToken::new();
        let sync_task = tokio::spawn(run(pool.clone(), cache.clone(), 0, shutdown.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        sqlx::query("ALTER TABLE api_keys_temporarily_unavailable RENAME TO api_keys")
            .execute(&pool)
            .await
            .expect("restore API-key table without a notification");

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !cache.is_zdr(&secret) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-LISTEN refresh failure should be retried with fallback disabled");

        shutdown.cancel();
        sync_task
            .await
            .expect("join cache sync task")
            .expect("cache sync should shut down cleanly");
    }

    #[sqlx::test]
    async fn listener_retries_a_failed_notification_refresh(pool: sqlx::PgPool) {
        let owner = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let secret = create_realtime_key(&pool, owner.id, owner.id, "notification retry").await;
        let cache = initial_cache(&pool).await.expect("initial cache load");
        assert!(!cache.is_zdr(&secret));

        let shutdown = CancellationToken::new();
        let sync_task = tokio::spawn(run(pool.clone(), cache.clone(), 0, shutdown.clone()));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let listeners: i64 = sqlx::query_scalar(
                    r#"
                    SELECT COUNT(*)
                    FROM pg_stat_activity
                    WHERE datname = current_database()
                      AND query LIKE 'LISTEN%auth_config_changed%'
                    "#,
                )
                .fetch_one(&pool)
                .await
                .expect("inspect listener activity");
                if listeners > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("API-key cache listener should subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(110)).await;

        sqlx::query("ALTER TABLE api_keys RENAME TO api_keys_temporarily_unavailable")
            .execute(&pool)
            .await
            .expect("make notification refresh fail");
        sqlx::query("UPDATE users SET zero_data_retention = true WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .expect("emit policy-change notification");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(!cache.is_zdr(&secret), "failed loads must not replace the snapshot");

        sqlx::query("ALTER TABLE api_keys_temporarily_unavailable RENAME TO api_keys")
            .execute(&pool)
            .await
            .expect("restore API-key table without a second notification");

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !cache.is_zdr(&secret) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed notification refresh should retry with fallback disabled");

        shutdown.cancel();
        sync_task
            .await
            .expect("join cache sync task")
            .expect("cache sync should shut down cleanly");
    }

    #[sqlx::test]
    async fn org_scoped_resolution_uses_matching_creator_hidden_key(pool: sqlx::PgPool) {
        let member = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let other_member = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, member.id).await;
        let presented_secret = create_realtime_key(&pool, org.id, member.id, "member org realtime").await;
        let (expected_secret, other_secret) = {
            let mut conn = pool.acquire().await.expect("acquire hidden-key connection");
            let mut api_keys = ApiKeys::new(&mut conn);
            let expected = api_keys
                .get_or_create_hidden_key(org.id, ApiKeyPurpose::Batch, member.id)
                .await
                .expect("create member hidden key");
            let other = api_keys
                .get_or_create_hidden_key(org.id, ApiKeyPurpose::Batch, other_member.id)
                .await
                .expect("create other member hidden key");
            (expected, other)
        };
        let cache = initial_cache(&pool).await.expect("initial cache load");
        let resolver = FlexBatchKeyResolver::new(pool.clone(), cache);

        let resolved = resolver
            .resolve_hidden_batch_key(&presented_secret)
            .await
            .expect("resolve member batch key")
            .expect("known presented key");

        assert_eq!(resolved.secret, expected_secret);
        assert_ne!(resolved.secret, other_secret);
        assert_eq!(resolved.owner_id, org.id);
    }

    #[sqlx::test]
    async fn cap_scoped_child_is_preferred_without_leaking_to_sibling_keys(pool: sqlx::PgPool) {
        let member = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, member.id).await;
        let capped_secret = create_realtime_key(&pool, org.id, member.id, "capped execution key").await;
        let uncapped_secret = create_realtime_key(&pool, org.id, member.id, "uncapped execution key").await;
        let capped_id: Uuid = sqlx::query_scalar("SELECT id FROM api_keys WHERE secret = $1")
            .bind(&capped_secret)
            .fetch_one(&pool)
            .await
            .expect("load capped key id");
        let (shared_secret, child_secret) = {
            let mut conn = pool.acquire().await.expect("acquire hidden-key connection");
            let mut api_keys = ApiKeys::new(&mut conn);
            let shared = api_keys
                .get_or_create_hidden_key(org.id, ApiKeyPurpose::Batch, member.id)
                .await
                .expect("create shared hidden key");
            let (child, _) = api_keys
                .get_or_create_child_hidden_key(capped_id)
                .await
                .expect("create cap-scoped child");
            (shared, child)
        };
        let cache = initial_cache(&pool).await.expect("initial cache load");
        let resolver = FlexBatchKeyResolver::new(pool.clone(), cache);

        let capped = resolver
            .resolve_hidden_batch_key(&capped_secret)
            .await
            .expect("resolve capped key")
            .expect("known capped key");
        let uncapped = resolver
            .resolve_hidden_batch_key(&uncapped_secret)
            .await
            .expect("resolve uncapped key")
            .expect("known uncapped key");

        assert_eq!(capped.secret, child_secret);
        assert_eq!(uncapped.secret, shared_secret);
        assert_ne!(capped.secret, uncapped.secret);
    }

    #[sqlx::test]
    async fn shared_snapshot_checks_for_a_child_created_after_the_snapshot(pool: sqlx::PgPool) {
        let member = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, member.id).await;
        let presented_secret = create_realtime_key(&pool, org.id, member.id, "shared then cap child").await;
        let parent_id: Uuid = sqlx::query_scalar("SELECT id FROM api_keys WHERE secret = $1")
            .bind(&presented_secret)
            .fetch_one(&pool)
            .await
            .expect("load parent key id");
        let shared_secret = {
            let mut conn = pool.acquire().await.expect("acquire shared-key connection");
            ApiKeys::new(&mut conn)
                .get_or_create_hidden_key(org.id, ApiKeyPurpose::Batch, member.id)
                .await
                .expect("create shared hidden key before snapshot")
        };
        let cache = initial_cache(&pool).await.expect("load snapshot with shared key");
        assert_eq!(
            cache.get(&presented_secret).and_then(|metadata| metadata.hidden_batch_key),
            Some(shared_secret)
        );
        let child_secret = {
            let mut conn = pool.acquire().await.expect("acquire child-key connection");
            ApiKeys::new(&mut conn)
                .get_or_create_child_hidden_key(parent_id)
                .await
                .expect("create cap child after shared snapshot")
                .0
        };
        let resolver = FlexBatchKeyResolver::new(pool.clone(), cache);

        let resolved = resolver
            .resolve_hidden_batch_key(&presented_secret)
            .await
            .expect("resolve shared snapshot after child creation")
            .expect("known presented key");

        assert_eq!(resolved.secret, child_secret);
    }

    #[sqlx::test]
    async fn shared_resolution_rechecks_for_a_child_created_after_negative_lookup(pool: sqlx::PgPool) {
        let member = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, member.id).await;
        let presented_secret = create_realtime_key(&pool, org.id, member.id, "shared resolve then cap child").await;
        let parent_id: Uuid = sqlx::query_scalar("SELECT id FROM api_keys WHERE secret = $1")
            .bind(&presented_secret)
            .fetch_one(&pool)
            .await
            .expect("load parent key id");
        let shared_secret = {
            let mut conn = pool.acquire().await.expect("acquire shared-key connection");
            ApiKeys::new(&mut conn)
                .get_or_create_hidden_key(org.id, ApiKeyPurpose::Batch, member.id)
                .await
                .expect("create shared hidden key before snapshot")
        };
        let cache = initial_cache(&pool).await.expect("load snapshot with shared key");
        let resolver = FlexBatchKeyResolver::new(pool.clone(), cache);

        let before_child = resolver
            .resolve_hidden_batch_key(&presented_secret)
            .await
            .expect("resolve shared key before child creation")
            .expect("known presented key");
        assert_eq!(before_child.secret, shared_secret);

        let child_secret = {
            let mut conn = pool.acquire().await.expect("acquire child-key connection");
            ApiKeys::new(&mut conn)
                .get_or_create_child_hidden_key(parent_id)
                .await
                .expect("create cap child after negative lookup")
                .0
        };

        let after_child = resolver
            .resolve_hidden_batch_key(&presented_secret)
            .await
            .expect("recheck shared resolution after child creation")
            .expect("known presented key");

        assert_eq!(after_child.secret, child_secret);
    }

    #[sqlx::test]
    async fn cold_snapshot_observes_cap_child_before_shared_fallback(pool: sqlx::PgPool) {
        let member = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, member.id).await;
        let presented_secret = create_realtime_key(&pool, org.id, member.id, "late cap child").await;
        let parent_id: Uuid = sqlx::query_scalar("SELECT id FROM api_keys WHERE secret = $1")
            .bind(&presented_secret)
            .fetch_one(&pool)
            .await
            .expect("load parent key id");
        let cache = initial_cache(&pool).await.expect("load snapshot before child creation");
        assert!(
            cache
                .get(&presented_secret)
                .is_some_and(|metadata| metadata.hidden_batch_key.is_none()),
            "fixture must enter the cold resolver path"
        );
        let child_secret = {
            let mut conn = pool.acquire().await.expect("acquire child-key connection");
            ApiKeys::new(&mut conn)
                .get_or_create_child_hidden_key(parent_id)
                .await
                .expect("create cap child after snapshot")
                .0
        };
        let resolver = FlexBatchKeyResolver::new(pool.clone(), cache);

        let resolved = resolver
            .resolve_hidden_batch_key(&presented_secret)
            .await
            .expect("resolve stale cold snapshot")
            .expect("known presented key");

        assert_eq!(resolved.secret, child_secret);
        let shared_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM api_keys
            WHERE user_id = $1
              AND created_by = $2
              AND purpose = 'batch'
              AND hidden = true
              AND is_deleted = false
              AND parent_api_key_id IS NULL
            "#,
        )
        .bind(org.id)
        .bind(member.id)
        .fetch_one(&pool)
        .await
        .expect("count shared fallback keys");
        assert_eq!(shared_count, 0, "a stale snapshot must not bypass the cap-scoped child");
    }

    #[sqlx::test]
    async fn concurrent_cold_resolutions_create_one_hidden_key(pool: sqlx::PgPool) {
        let member = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let owner = crate::test::utils::create_test_org(&pool, member.id).await;
        let presented_secret = create_realtime_key(&pool, owner.id, member.id, "concurrent cold resolution").await;
        let cache = initial_cache(&pool).await.expect("initial cache load");
        let resolver = FlexBatchKeyResolver::new(pool.clone(), cache);

        let results = futures::future::join_all((0..20).map(|_| resolver.resolve_hidden_batch_key(&presented_secret))).await;
        let resolved = results
            .into_iter()
            .map(|result| result.expect("cold resolution succeeds").expect("known presented key"))
            .collect::<Vec<_>>();
        let expected_secret = resolved.first().expect("at least one result").secret.clone();

        assert!(resolved.iter().all(|key| key.secret == expected_secret));
        let hidden_key_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM api_keys
            WHERE user_id = $1
              AND created_by = $2
              AND purpose = 'batch'
              AND hidden = true
              AND is_deleted = false
            "#,
        )
        .bind(owner.id)
        .bind(member.id)
        .fetch_one(&pool)
        .await
        .expect("count hidden batch keys");
        assert_eq!(hidden_key_count, 1);
    }

    #[sqlx::test]
    async fn independent_resolvers_racing_on_cold_key_both_succeed(pool: sqlx::PgPool) {
        const INSERT_GATE: i64 = 550_003;

        let member = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;
        let owner = crate::test::utils::create_test_org(&pool, member.id).await;
        let presented_secret = create_realtime_key(&pool, owner.id, member.id, "cross-resolver cold race").await;
        let first_cache = initial_cache(&pool).await.expect("first initial cache load");
        let second_cache = initial_cache(&pool).await.expect("second initial cache load");
        let first_resolver = FlexBatchKeyResolver::new(pool.clone(), first_cache);
        let second_resolver = FlexBatchKeyResolver::new(pool.clone(), second_cache);

        sqlx::query(
            r#"
            CREATE FUNCTION gate_hidden_batch_insert_for_test() RETURNS trigger AS $$
            BEGIN
                IF NEW.hidden = true AND NEW.purpose = 'batch' THEN
                    PERFORM pg_advisory_xact_lock(550003);
                END IF;
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .execute(&pool)
        .await
        .expect("create deterministic insert-gate function");
        sqlx::query(
            r#"
            CREATE TRIGGER gate_hidden_batch_insert_for_test
                BEFORE INSERT ON api_keys
                FOR EACH ROW EXECUTE FUNCTION gate_hidden_batch_insert_for_test()
            "#,
        )
        .execute(&pool)
        .await
        .expect("install deterministic insert gate");

        let mut gate_connection = pool.acquire().await.expect("acquire insert-gate connection");
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(INSERT_GATE)
            .execute(&mut *gate_connection)
            .await
            .expect("close insert gate");

        let start = Arc::new(tokio::sync::Barrier::new(3));
        let first_start = start.clone();
        let first_presented_secret = presented_secret.clone();
        let first = tokio::spawn(async move {
            first_start.wait().await;
            first_resolver.resolve_hidden_batch_key(&first_presented_secret).await
        });
        let second_start = start.clone();
        let second_presented_secret = presented_secret.clone();
        let second = tokio::spawn(async move {
            second_start.wait().await;
            second_resolver.resolve_hidden_batch_key(&second_presented_secret).await
        });
        start.wait().await;

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let blocked_inserts: i64 = sqlx::query_scalar(
                    r#"
                    SELECT COUNT(*)
                    FROM pg_stat_activity
                    WHERE datname = current_database()
                      AND wait_event_type = 'Lock'
                      AND wait_event = 'advisory'
                      AND query LIKE '%INSERT INTO api_keys%'
                    "#,
                )
                .fetch_one(&pool)
                .await
                .expect("inspect blocked hidden-key inserts");
                if blocked_inserts == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both resolver inserts should reach the gate");

        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(INSERT_GATE)
            .execute(&mut *gate_connection)
            .await
            .expect("open insert gate");

        let first = first
            .await
            .expect("join first resolver")
            .expect("first resolver succeeds")
            .expect("known first key");
        let second = second
            .await
            .expect("join second resolver")
            .expect("second resolver succeeds")
            .expect("known second key");
        assert_eq!(first.secret, second.secret);

        let hidden_key_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM api_keys
            WHERE user_id = $1
              AND created_by = $2
              AND purpose = 'batch'
              AND hidden = true
              AND is_deleted = false
            "#,
        )
        .bind(owner.id)
        .bind(member.id)
        .fetch_one(&pool)
        .await
        .expect("count cross-resolver hidden keys");
        assert_eq!(hidden_key_count, 1);
    }
}
