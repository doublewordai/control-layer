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

/// Resolves cached hidden batch identities and serializes cold creation by owner and creator.
#[derive(Clone)]
pub struct FlexBatchKeyResolver {
    pool: PgPool,
    cache: ApiKeyMetadataCache,
    locks: Arc<ResolverLocks>,
}

type ResolverLocks = DashMap<(Uuid, Uuid), Arc<Mutex<()>>>;

impl FlexBatchKeyResolver {
    /// Bind a resolver to the shared metadata snapshot and primary database pool.
    pub fn new(pool: PgPool, cache: ApiKeyMetadataCache) -> Self {
        Self {
            pool,
            cache,
            locks: Arc::new(DashMap::new()),
        }
    }

    /// Resolve a presented API key to its owner-scoped hidden batch key.
    pub async fn resolve_hidden_batch_key(&self, presented_secret: &str) -> Result<Option<ResolvedFlexKey>, DbError> {
        let Some(metadata) = self.cache.get(presented_secret) else {
            return Ok(None);
        };
        if let Some(secret) = metadata.hidden_batch_key.clone() {
            return Ok(Some(resolved_flex_key(secret, &metadata)));
        }

        let lock_key = (metadata.owner_id, metadata.created_by);
        let lock = self.locks.entry(lock_key).or_insert_with(|| Arc::new(Mutex::new(()))).clone();
        let _guard = lock.lock().await;

        let Some(metadata) = self.cache.get(presented_secret) else {
            return Ok(None);
        };
        if let Some(secret) = metadata.hidden_batch_key.clone() {
            return Ok(Some(resolved_flex_key(secret, &metadata)));
        }

        let secret = {
            let mut conn = self.pool.acquire().await?;
            ApiKeys::new(&mut conn)
                .get_or_create_hidden_key(metadata.owner_id, ApiKeyPurpose::Batch, metadata.created_by)
                .await?
        };
        refresh(&self.pool, &self.cache).await?;

        let Some(refreshed_metadata) = self.cache.get(presented_secret) else {
            return Ok(None);
        };
        Ok(Some(resolved_flex_key(secret, &refreshed_metadata)))
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
            batch.secret AS hidden_batch_key
        FROM api_keys ak
        JOIN users u ON u.id = ak.user_id
        LEFT JOIN api_keys batch
          ON batch.user_id = ak.user_id
         AND batch.created_by = ak.created_by
         AND batch.purpose = 'batch'
         AND batch.hidden = true
         AND batch.is_deleted = false
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
                            reload(&pool, &cache).await;
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
                    reload(&pool, &cache).await;
                }
                _ = tick => {
                    if last_reload.elapsed() < MIN_RELOAD_INTERVAL { continue; }
                    trailing_reload = None;
                    last_reload = tokio::time::Instant::now();
                    reload(&pool, &cache).await;
                }
            }
        }
    }
    Ok(())
}

async fn reload(pool: &PgPool, cache: &ApiKeyMetadataCache) {
    match refresh(pool, cache).await {
        Ok(keys) => debug!(keys, "API key metadata sync: reloaded map"),
        Err(error) => {
            crate::background_error!(component::API_KEY_CACHE_SYNC, "load", Error, error = %error, "API key metadata sync: failed to reload map");
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
