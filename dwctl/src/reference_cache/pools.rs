//! A separate connection budget for optional reference-cache work, including principal
//! resolution. A caller dropping its query future must not return capacity to the main
//! application pool while PostgreSQL is still executing that query.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Transaction};
use sqlx_pool_router::{DynPools, PoolHandle, PoolProvider};

// A database-wide budget, not one multiplied by the number of application replicas.
// Transaction locks release on completion, rollback, disconnect and server timeout.
const QUERY_BUDGET_NAMESPACE: i32 = 0x52434643;
const GLOBAL_QUERY_SLOTS: i32 = 4;

struct CurrentPool {
    source: Arc<PgConnectOptions>,
    pool: PgPool,
}

#[derive(Clone)]
pub(super) struct CachePools {
    source: DynPools,
    current: Arc<Mutex<Option<CurrentPool>>>,
    max_connections: u32,
}

impl CachePools {
    pub fn new(source: impl PoolProvider, max_connections: usize) -> Self {
        Self {
            source: DynPools::new(source),
            current: Arc::new(Mutex::new(None)),
            max_connections: u32::try_from(max_connections.max(1)).unwrap_or(u32::MAX),
        }
    }

    pub async fn begin(&self) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
        let mut tx = self.write().begin().await?;
        // SET during connection setup is not safe through transaction-mode poolers:
        // the next statement may use a different backend. Keep deadlines local.
        let slot = sqlx::query(
            "SELECT slot, set_config('statement_timeout', '1s', true), set_config('lock_timeout', '250ms', true) \
             FROM generate_series(0, $2 - 1) AS slots(slot) \
             WHERE pg_try_advisory_xact_lock($1, slot) LIMIT 1",
        )
        .bind(QUERY_BUDGET_NAMESPACE)
        .bind(GLOBAL_QUERY_SLOTS)
        .fetch_optional(&mut *tx)
        .await?;
        if slot.is_none() {
            tx.rollback().await?;
            return Err(sqlx::Error::PoolTimedOut);
        }
        Ok(tx)
    }
}

impl PoolProvider for CachePools {
    fn read(&self) -> PoolHandle {
        // The reference table is unlogged; even lookups must use the primary.
        self.write()
    }

    fn write(&self) -> PoolHandle {
        let source = self.source.write().connect_options();
        let mut current = self.current.lock().expect("reference-cache pool mutex poisoned");
        if let Some(active) = current.as_ref()
            && Arc::ptr_eq(&active.source, &source)
        {
            return active.pool.clone().into();
        }
        let options = (*source).clone().application_name("dwctl-reference-cache");
        let pool = PgPoolOptions::new()
            .max_connections(self.max_connections)
            .acquire_timeout(Duration::from_millis(500))
            .idle_timeout(Duration::from_secs(30))
            .connect_lazy_with(options);
        if let Some(old) = current.replace(CurrentPool {
            source,
            pool: pool.clone(),
        }) {
            // Follow runtime primary/pool replacements, but retire old connections so
            // successive configuration changes cannot accumulate extra cache pools.
            tokio::spawn(async move { old.pool.close().await });
        }
        pool.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[dwctl_test_macros::test]
    async fn query_budget_is_shared_across_independent_replicas(pool: PgPool) {
        let one = CachePools::new(pool.clone(), GLOBAL_QUERY_SLOTS as usize);
        let two = CachePools::new(pool.clone(), GLOBAL_QUERY_SLOTS as usize);
        let mut held = Vec::new();
        for _ in 0..GLOBAL_QUERY_SLOTS {
            held.push(one.begin().await.unwrap());
        }
        assert!(matches!(two.begin().await, Err(sqlx::Error::PoolTimedOut)));
        assert_eq!(sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&pool).await.unwrap(), 1);
        held.pop().unwrap().rollback().await.unwrap();
        two.begin().await.unwrap().commit().await.unwrap();
        for tx in held {
            tx.rollback().await.unwrap();
        }
        one.write().close().await;
        two.write().close().await;
    }

    #[dwctl_test_macros::test]
    async fn server_deadlines_end_with_the_transaction(pool: PgPool) {
        let cache = CachePools::new(pool, 1);
        let before: String = sqlx::query_scalar("SHOW statement_timeout").fetch_one(cache.write()).await.unwrap();
        let mut tx = cache.begin().await.unwrap();
        let during: String = sqlx::query_scalar("SHOW statement_timeout").fetch_one(&mut *tx).await.unwrap();
        let lock: String = sqlx::query_scalar("SHOW lock_timeout").fetch_one(&mut *tx).await.unwrap();
        assert_eq!(during, "1s");
        assert_eq!(lock, "250ms");
        tx.commit().await.unwrap();
        let after: String = sqlx::query_scalar("SHOW statement_timeout").fetch_one(cache.write()).await.unwrap();
        assert_eq!(after, before, "transaction-mode poolers must not inherit cache deadlines");
        cache.write().close().await;
    }

    #[dwctl_test_macros::test]
    async fn cache_capacity_is_separate_from_the_application_pool(pool: PgPool) {
        let main = PgPoolOptions::new()
            .max_connections(1)
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        let cache = CachePools::new(main.clone(), 1);
        let app_connection = main.acquire().await.unwrap();
        let cache_connection = cache.write().acquire().await.unwrap();
        drop(app_connection);

        assert!(matches!(cache.write().acquire().await, Err(sqlx::Error::PoolTimedOut)));
        assert_eq!(sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&main).await.unwrap(), 1);
        drop(cache_connection);
        cache.write().close().await;
        main.close().await;
    }

    #[dwctl_test_macros::test]
    async fn abandoned_queries_stay_bounded_and_are_stopped_by_postgres(pool: PgPool) {
        let cache = CachePools::new(pool.clone(), 1);
        sqlx::query("SELECT 1").execute(cache.write()).await.unwrap();
        // A client deadline abandons the future while its SQL may still run. Repeated
        // timeouts must neither grow the cache's backend count nor borrow main slots.
        for _ in 0..5 {
            let _ = tokio::time::timeout(Duration::from_millis(30), async {
                let mut tx = cache.begin().await?;
                sqlx::query("SELECT pg_sleep(30)").execute(&mut *tx).await
            })
            .await;
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
             AND application_name = 'dwctl-reference-cache' AND state = 'active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(active <= 1, "abandoned SQL exceeded the dedicated pool budget: {active}");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match sqlx::query("SELECT 1").execute(cache.write()).await {
                    Ok(_) => break,
                    Err(sqlx::Error::PoolTimedOut) => tokio::task::yield_now().await,
                    Err(e) => panic!("cache failed to recover after server deadline: {e}"),
                }
            }
        })
        .await
        .expect("Postgres must stop the abandoned pg_sleep instead of running for 30s");
        let peer = CachePools::new(pool, GLOBAL_QUERY_SLOTS as usize);
        let mut slots = Vec::new();
        for _ in 0..GLOBAL_QUERY_SLOTS {
            slots.push(peer.begin().await.expect("abandoned work must release its database-wide slot"));
        }
        for tx in slots {
            tx.rollback().await.unwrap();
        }
        peer.write().close().await;
        cache.write().close().await;
    }

    #[dwctl_test_macros::test]
    async fn dedicated_pool_follows_primary_replacement_and_closes_old_pool(pool: PgPool) {
        let source = sqlx_pool_router::DbPools::new(pool.clone());
        let cache = CachePools::new(source.clone(), 1);
        let old = cache.write();
        assert!(Arc::ptr_eq(&old.connect_options(), &cache.clone().write().connect_options()));
        let replacement = PgPoolOptions::new().connect_lazy_with((*pool.connect_options()).clone());
        source.replace(replacement, None);
        let new = cache.write();
        assert!(!Arc::ptr_eq(&old.connect_options(), &new.connect_options()));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !old.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        sqlx::query("SELECT 1").execute(new.clone()).await.unwrap();
        new.close().await;
        source.write().close().await;
    }
}
