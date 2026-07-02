//! Lightweight per-key zero-data-retention (ZDR) sync.
//!
//! Maintains a memory-local map from api key secret to the owning account's
//! `users.zero_data_retention` flag, so the request hot path
//! ([`crate::inference::zdr::is_zdr_request`]) answers per-key ZDR policy with a
//! lock-free map read and no DB round-trip.
//!
//! Unlike [`crate::sync::onwards_config`], which rebuilds the whole routing
//! table on every change, this runs one small two-table join and swaps a flat
//! map, so refreshing it on every `auth_config_changed` notification is cheap.
//! It reuses that channel (fired by both the `api_keys` trigger and the
//! `users` ZDR trigger), a 100ms debounce, and a periodic fallback reload.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::{PgPool, postgres::PgListener};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config::ONWARDS_CONFIG_CHANGED_CHANNEL;
use crate::metrics::errors::component;

/// Lock-free, cheap-to-clone handle to the shared secret-to-ZDR-flag map.
///
/// A secret absent from the map is a deleted or invalid key (auth rejects it
/// before any body is stored), so absence safely reads as "not ZDR".
#[derive(Clone)]
pub struct ZdrKeyCache {
    inner: Arc<ArcSwap<HashMap<String, bool>>>,
}

impl ZdrKeyCache {
    /// An empty cache: every key reads as non-ZDR. Used before the first load,
    /// in tests, and when the sync is disabled.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    /// Whether the api key `secret` belongs to a ZDR account. Hot-path safe.
    pub fn is_zdr(&self, secret: &str) -> bool {
        self.inner.load().get(secret).copied().unwrap_or(false)
    }

    fn replace(&self, map: HashMap<String, bool>) {
        self.inner.store(Arc::new(map));
    }

    /// Build a cache from explicit (secret, zdr) pairs. Test-only.
    #[cfg(test)]
    pub fn from_pairs<I: IntoIterator<Item = (String, bool)>>(pairs: I) -> Self {
        let cache = Self::empty();
        cache.replace(pairs.into_iter().collect());
        cache
    }
}

/// Run the join and collect the flat secret-to-zdr map. ZDR lives on `users`
/// (account-wide); a key inherits its owner's flag via `api_keys.user_id`.
async fn load(pool: &PgPool) -> Result<HashMap<String, bool>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT ak.secret, u.zero_data_retention
        FROM api_keys ak
        JOIN users u ON u.id = ak.user_id
        WHERE NOT ak.is_deleted
        "#
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| (r.secret, r.zero_data_retention)).collect())
}

/// Reload `cache` in place from the DB, returning the number of keys loaded.
/// Shared by [`initial_cache`], the background listener, and the test-only
/// manual refresh so none of them can drift.
pub async fn refresh(pool: &PgPool, cache: &ZdrKeyCache) -> Result<usize, sqlx::Error> {
    let map = load(pool).await?;
    let n = map.len();
    cache.replace(map);
    Ok(n)
}

/// Load the map once, synchronously, returning a populated cache. Call at
/// startup before the server accepts traffic so the map is never empty under
/// live traffic (an empty map reads every key as non-ZDR and would leak a ZDR
/// account's body during warm-up).
pub async fn initial_cache(pool: &PgPool) -> Result<ZdrKeyCache, sqlx::Error> {
    let cache = ZdrKeyCache::empty();
    refresh(pool, &cache).await?;
    Ok(cache)
}

/// Background task: keep `cache` fresh. Listens on `auth_config_changed` and
/// reloads (debounced), with a periodic fallback reload to recover from any
/// missed notification. Returns when `shutdown` fires.
pub async fn run(pool: PgPool, cache: ZdrKeyCache, fallback_interval_ms: u64, shutdown: CancellationToken) -> Result<(), anyhow::Error> {
    const MIN_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
    let fallback = (fallback_interval_ms > 0).then(|| std::time::Duration::from_millis(fallback_interval_ms));

    'outer: loop {
        let mut listener = PgListener::connect_with(&pool).await?;
        listener.listen(ONWARDS_CONFIG_CHANGED_CHANNEL).await?;
        info!("Started ZDR key sync listener");

        let mut last_reload = std::time::Instant::now();
        let mut fallback_timer = fallback.map(|iv| {
            let mut t = tokio::time::interval(iv);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        });

        loop {
            let tick = async {
                match fallback_timer.as_mut() {
                    Some(t) => t.tick().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = shutdown.cancelled() => break 'outer,
                notif = listener.try_recv() => match notif {
                    Ok(Some(_)) => {
                        if last_reload.elapsed() < MIN_RELOAD_INTERVAL { continue; }
                        last_reload = std::time::Instant::now();
                        reload(&pool, &cache).await;
                    }
                    Ok(None) => {
                        debug!("ZDR key sync: connection lost, reconnecting");
                        break;
                    }
                    Err(e) => {
                        crate::background_error!(component::ZDR_KEY_SYNC, "listen", Error, error = %e, "ZDR key sync: listener error");
                        break;
                    }
                },
                _ = tick => {
                    if last_reload.elapsed() < MIN_RELOAD_INTERVAL { continue; }
                    last_reload = std::time::Instant::now();
                    reload(&pool, &cache).await;
                }
            }
        }
    }
    Ok(())
}

async fn reload(pool: &PgPool, cache: &ZdrKeyCache) {
    match refresh(pool, cache).await {
        Ok(keys) => debug!(keys, "ZDR key sync: reloaded map"),
        Err(e) => {
            crate::background_error!(component::ZDR_KEY_SYNC, "load", Error, error = %e, "ZDR key sync: failed to reload map");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cache_reads_every_key_as_non_zdr() {
        let cache = ZdrKeyCache::empty();
        assert!(!cache.is_zdr("sk-anything"));
    }

    #[test]
    fn is_zdr_reflects_the_maps_flag() {
        let cache = ZdrKeyCache::from_pairs([("sk-on".to_string(), true), ("sk-off".to_string(), false)]);
        assert!(cache.is_zdr("sk-on"));
        assert!(!cache.is_zdr("sk-off"));
        // Absent key (deleted/invalid, auth rejects it anyway) is non-ZDR.
        assert!(!cache.is_zdr("sk-missing"));
    }

    #[test]
    fn clones_share_one_map_so_a_replace_is_visible_to_all_handles() {
        // The inference middleware holds a clone of the sync's cache; a reload
        // through one handle must be visible through the other (same ArcSwap).
        // The integration tests rely on exactly this.
        let cache = ZdrKeyCache::empty();
        let handle = cache.clone();
        assert!(!handle.is_zdr("sk-on"));
        cache.replace([("sk-on".to_string(), true)].into_iter().collect());
        assert!(handle.is_zdr("sk-on"));
    }
}
