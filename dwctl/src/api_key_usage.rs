//! Records when each API key was last used (`api_keys.last_used`).
//!
//! The request path only notes the key in memory ([`ApiKeyUsage::record`]): no
//! query and no await. [`run_api_key_usage_flusher`] writes the latest use of
//! every key seen since the previous flush in one `UPDATE … FROM UNNEST`, once
//! per [`FLUSH_INTERVAL`], so the write load grows with the number of active
//! keys, not with request volume.
//!
//! A key counts as used when it authenticates a request, whatever the request
//! then does:
//! - realtime inference through onwards (the response carries
//!   [`onwards::AuthenticatedApiKeyId`]),
//! - flex and background submissions, which the inference middleware accepts
//!   without passing through onwards,
//! - every route that authenticates with the `CurrentUser` extractor: files,
//!   batches and the management API.
//!
//! Batch and flex work that the daemon dispatches later runs on the owner's
//! hidden batch key, so it marks that key, not the key the client presented.
//! The client's key was already marked when it uploaded the file, created the
//! batch or submitted the flex request.
//!
//! # Precision
//!
//! `last_used` can be up to two minutes behind the latest use: a use waits up
//! to [`FLUSH_INTERVAL`] in memory, and a flush skips a key whose stored value
//! is less than [`FLUSH_INTERVAL`] older than the new one, so several replicas
//! do not all rewrite the same row each minute. Uses noted after shutdown
//! begins, and uses whose flush fails at shutdown, are lost. Keys not used
//! since this feature shipped stay `NULL`.
//!
//! Updates that touch only `last_used` do not reload the onwards key cache:
//! `notify_api_keys_config_change()` ignores that column.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use chrono::{DateTime, Utc};
use onwards::AuthenticatedApiKeyId;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument};
use uuid::Uuid;

use crate::metrics::errors::component::API_KEY_USAGE;

/// How often noted uses are written to `api_keys.last_used`.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// In-memory record of the latest use of each key since the last flush.
/// Cloning shares the record.
#[derive(Clone, Default)]
pub struct ApiKeyUsage {
    pending: Arc<Mutex<HashMap<Uuid, DateTime<Utc>>>>,
}

impl ApiKeyUsage {
    /// Note that `api_key_id` authenticated a request now.
    pub fn record(&self, api_key_id: Uuid) {
        self.record_at(api_key_id, Utc::now());
    }

    fn record_at(&self, api_key_id: Uuid, at: DateTime<Utc>) {
        let mut pending = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.entry(api_key_id).and_modify(|seen| *seen = (*seen).max(at)).or_insert(at);
    }

    fn take(&self) -> HashMap<Uuid, DateTime<Utc>> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    /// Write every noted use to `api_keys.last_used` and return the number of
    /// rows updated. On failure the uses are kept for the next flush.
    #[instrument(skip_all, fields(keys))]
    pub async fn flush(&self, pool: &PgPool) -> Result<u64, sqlx::Error> {
        let seen = self.take();
        if seen.is_empty() {
            return Ok(0);
        }
        tracing::Span::current().record("keys", seen.len());
        let (ids, timestamps): (Vec<Uuid>, Vec<DateTime<Utc>>) = seen.iter().map(|(id, at)| (*id, *at)).unzip();

        // Rows are locked in id order so concurrent flushes from other
        // replicas cannot deadlock on each other.
        let result = sqlx::query!(
            r#"
            WITH due AS (
                SELECT ak.id, seen.last_used
                FROM UNNEST($1::uuid[], $2::timestamptz[]) AS seen(id, last_used)
                JOIN api_keys ak ON ak.id = seen.id
                WHERE ak.last_used IS NULL OR ak.last_used < seen.last_used - make_interval(secs => $3)
                ORDER BY ak.id
                FOR UPDATE OF ak
            )
            UPDATE api_keys ak
            SET last_used = due.last_used
            FROM due
            WHERE ak.id = due.id
            "#,
            &ids,
            &timestamps,
            FLUSH_INTERVAL.as_secs_f64(),
        )
        .execute(pool)
        .await;

        match result {
            Ok(result) => Ok(result.rows_affected()),
            Err(e) => {
                for (id, at) in seen {
                    self.record_at(id, at);
                }
                Err(e)
            }
        }
    }
}

/// AI proxy middleware: notes the key named by the response's
/// [`AuthenticatedApiKeyId`], which onwards sets for realtime requests and the
/// inference middleware sets for queued submissions.
pub async fn record_proxy_key_use(State(usage): State<ApiKeyUsage>, request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    if let Some(AuthenticatedApiKeyId(api_key_id)) = response.extensions().get() {
        usage.record(*api_key_id);
    }
    response
}

/// Flush noted key uses every [`FLUSH_INTERVAL`] until `shutdown` is
/// cancelled, then flush once more. Every replica runs its own flusher.
#[instrument(skip_all)]
pub async fn run_api_key_usage_flusher(pools: impl sqlx_pool_router::PoolProvider, usage: ApiKeyUsage, shutdown: CancellationToken) {
    let pools = sqlx_pool_router::DynPools::new(pools);
    info!(interval_s = FLUSH_INTERVAL.as_secs(), "API key usage flusher started");

    let mut tick = tokio::time::interval(FLUSH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; there is nothing to flush yet.
    tick.tick().await;

    loop {
        let stopping = tokio::select! {
            biased;
            _ = shutdown.cancelled() => true,
            _ = tick.tick() => false,
        };
        match usage.flush(&pools.write()).await {
            Ok(updated) => debug!(updated, "Flushed API key usage"),
            Err(e) => {
                crate::background_error!(API_KEY_USAGE, "flush_failed", Warning, error = %e, "Failed to flush API key last-used times");
            }
        }
        if stopping {
            info!("API key usage flusher shutting down");
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::{create_test_api_key_for_user, create_test_user};

    async fn last_used(pool: &PgPool, id: Uuid) -> Option<DateTime<Utc>> {
        sqlx::query_scalar("SELECT last_used FROM api_keys WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn new_key(pool: &PgPool) -> Uuid {
        let user = create_test_user(pool, Role::StandardUser).await;
        create_test_api_key_for_user(pool, user.id).await.id
    }

    #[dwctl_test_macros::test]
    async fn flush_writes_the_latest_use_of_each_key(pool: PgPool) {
        let (a, b) = (new_key(&pool).await, new_key(&pool).await);
        assert_eq!(last_used(&pool, a).await, None);

        let usage = ApiKeyUsage::default();
        let t0 = Utc::now() - chrono::Duration::minutes(10);
        usage.record_at(a, t0);
        usage.record_at(a, t0 + chrono::Duration::minutes(5));
        usage.record_at(a, t0 + chrono::Duration::minutes(1));
        usage.record_at(b, t0);
        // A key deleted from the table meanwhile is ignored.
        usage.record_at(Uuid::new_v4(), t0);

        assert_eq!(usage.flush(&pool).await.unwrap(), 2);
        let stored = last_used(&pool, a).await.unwrap();
        assert_eq!(stored.timestamp_micros(), (t0 + chrono::Duration::minutes(5)).timestamp_micros());
        assert_eq!(last_used(&pool, b).await.unwrap().timestamp_micros(), t0.timestamp_micros());

        // Everything was taken: a second flush has nothing to write.
        assert_eq!(usage.flush(&pool).await.unwrap(), 0);
    }

    #[dwctl_test_macros::test]
    async fn flush_never_moves_last_used_back_or_rewrites_it_within_the_interval(pool: PgPool) {
        let key = new_key(&pool).await;
        let usage = ApiKeyUsage::default();
        let t0 = Utc::now() - chrono::Duration::minutes(10);
        usage.record_at(key, t0);
        usage.flush(&pool).await.unwrap();

        // Older (another replica flushing late) and too recent to matter.
        usage.record_at(key, t0 - chrono::Duration::minutes(1));
        assert_eq!(usage.flush(&pool).await.unwrap(), 0);
        usage.record_at(key, t0 + chrono::Duration::seconds(30));
        assert_eq!(usage.flush(&pool).await.unwrap(), 0);
        assert_eq!(last_used(&pool, key).await.unwrap().timestamp_micros(), t0.timestamp_micros());

        let later = t0 + FLUSH_INTERVAL * 2;
        usage.record_at(key, later);
        assert_eq!(usage.flush(&pool).await.unwrap(), 1);
        assert_eq!(last_used(&pool, key).await.unwrap().timestamp_micros(), later.timestamp_micros());
    }

    #[dwctl_test_macros::test]
    async fn failed_flush_keeps_uses_for_the_next_one(pool: PgPool) {
        let key = new_key(&pool).await;
        let usage = ApiKeyUsage::default();
        usage.record(key);

        let closed = PgPool::connect_lazy_with(pool.connect_options().as_ref().clone());
        closed.close().await;
        assert!(usage.flush(&closed).await.is_err());
        assert_eq!(last_used(&pool, key).await, None);

        assert_eq!(usage.flush(&pool).await.unwrap(), 1);
        assert!(last_used(&pool, key).await.is_some());
    }

    #[dwctl_test_macros::test]
    async fn last_used_updates_do_not_reload_the_onwards_key_cache(pool: PgPool) {
        let key = new_key(&pool).await;
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await.unwrap();
        listener.listen("auth_config_changed").await.unwrap();

        let usage = ApiKeyUsage::default();
        usage.record(key);
        assert_eq!(usage.flush(&pool).await.unwrap(), 1);

        // A marker sent after the flush arrives first only if the flush sent nothing.
        sqlx::query("SELECT pg_notify('auth_config_changed', 'marker')")
            .execute(&pool)
            .await
            .unwrap();
        let notification = tokio::time::timeout(Duration::from_secs(5), listener.recv())
            .await
            .expect("notification")
            .unwrap();
        assert_eq!(notification.payload(), "marker");
    }
}
