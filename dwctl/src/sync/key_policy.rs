//! Lightweight per-key account policy sync.
//!
//! Maintains a memory-local map from api key secret to the owning account's
//! request policy: the `users.zero_data_retention` flag (read by
//! [`crate::inference::zdr::is_zdr_request`]) and the account's
//! `users.disabled_modalities` (read by the inference middleware's modality
//! gate). Both answer on the request hot path with a lock-free map read and no
//! DB round-trip.
//!
//! Unlike [`crate::sync::onwards_config`], which rebuilds the whole routing
//! table on every change, this runs one small two-table join and swaps a flat
//! map, so refreshing it on every `auth_config_changed` notification is cheap.
//! It reuses that channel (fired by the `api_keys` trigger and the `users`
//! ZDR and disabled-modalities triggers), a 100ms debounce, and a periodic
//! fallback reload.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::{PgPool, postgres::PgListener};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config::ONWARDS_CONFIG_CHANGED_CHANNEL;
use crate::metrics::errors::component;
use crate::modalities::ModalitySet;

/// The account-level policy a key inherits from its owner (`api_keys.user_id`,
/// the organization for an org key). `Copy` so a map read hands back a value
/// and the caller never holds the `ArcSwap` guard across an await.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyPolicy {
    /// `users.zero_data_retention` of the owning account.
    pub zdr: bool,
    /// `users.disabled_modalities` of the owning account.
    pub disabled_modalities: ModalitySet,
}

/// Lock-free, cheap-to-clone handle to the shared secret-to-policy map.
///
/// A secret absent from the map is a deleted or invalid key (auth rejects it
/// before any body is stored or any request is served), so absence safely
/// reads as the permissive default: not ZDR, nothing disabled.
#[derive(Clone)]
pub struct KeyPolicyCache {
    inner: Arc<ArcSwap<HashMap<String, KeyPolicy>>>,
}

impl KeyPolicyCache {
    /// An empty cache: every key reads as the default policy. Used before the
    /// first load, in tests, and when the sync is disabled.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    /// The policy for the api key `secret`, or the default for an unknown key.
    /// Hot-path safe.
    pub fn policy(&self, secret: &str) -> KeyPolicy {
        self.inner.load().get(secret).copied().unwrap_or_default()
    }

    /// Whether the api key `secret` belongs to a ZDR account. Hot-path safe.
    pub fn is_zdr(&self, secret: &str) -> bool {
        self.policy(secret).zdr
    }

    /// The modalities the owning account has disabled for `secret`.
    pub fn disabled_modalities(&self, secret: &str) -> ModalitySet {
        self.policy(secret).disabled_modalities
    }

    fn replace(&self, map: HashMap<String, KeyPolicy>) {
        self.inner.store(Arc::new(map));
    }

    /// Build a cache from explicit (secret, zdr) pairs. Test-only.
    #[cfg(test)]
    pub fn from_pairs<I: IntoIterator<Item = (String, bool)>>(pairs: I) -> Self {
        let cache = Self::empty();
        cache.replace(
            pairs
                .into_iter()
                .map(|(secret, zdr)| (secret, KeyPolicy { zdr, ..Default::default() }))
                .collect(),
        );
        cache
    }

    /// Build a cache from explicit (secret, policy) pairs. Test-only.
    #[cfg(test)]
    pub fn from_policies<I: IntoIterator<Item = (String, KeyPolicy)>>(pairs: I) -> Self {
        let cache = Self::empty();
        cache.replace(pairs.into_iter().collect());
        cache
    }
}

/// Run the join and collect the flat secret-to-policy map. Both settings live
/// on `users` (account-wide); a key inherits its owner's via `api_keys.user_id`.
async fn load(pool: &PgPool) -> Result<HashMap<String, KeyPolicy>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT ak.secret, u.zero_data_retention, u.disabled_modalities
        FROM api_keys ak
        JOIN users u ON u.id = ak.user_id
        WHERE NOT ak.is_deleted
        "#
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.secret,
                KeyPolicy {
                    zdr: r.zero_data_retention,
                    disabled_modalities: ModalitySet::from_db(&r.disabled_modalities),
                },
            )
        })
        .collect())
}

/// Reload `cache` in place from the DB, returning the number of keys loaded.
/// Shared by [`initial_cache`], the background listener, and the test-only
/// manual refresh so none of them can drift.
pub async fn refresh(pool: &PgPool, cache: &KeyPolicyCache) -> Result<usize, sqlx::Error> {
    let map = load(pool).await?;
    let n = map.len();
    cache.replace(map);
    Ok(n)
}

/// Load the map once, synchronously, returning a populated cache. Call at
/// startup before the server accepts traffic so the map is never empty under
/// live traffic (an empty map reads every key as non-ZDR and would leak a ZDR
/// account's body during warm-up).
pub async fn initial_cache(pool: &PgPool) -> Result<KeyPolicyCache, sqlx::Error> {
    let cache = KeyPolicyCache::empty();
    refresh(pool, &cache).await?;
    Ok(cache)
}

/// Background task: keep `cache` fresh. Listens on `auth_config_changed` and
/// reloads (debounced), with a periodic fallback reload to recover from any
/// missed notification. Returns when `shutdown` fires.
pub async fn run(
    pools: impl sqlx_pool_router::PoolProvider,
    listener_pools: impl sqlx_pool_router::PoolProvider,
    cache: KeyPolicyCache,
    fallback_interval_ms: u64,
    shutdown: CancellationToken,
) -> Result<(), anyhow::Error> {
    let pools = sqlx_pool_router::DynPools::new(pools);
    // LISTEN needs a session: direct connections, never the pooled endpoint.
    let listener_pools = sqlx_pool_router::DynPools::new(listener_pools);
    const MIN_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
    let fallback = (fallback_interval_ms > 0).then(|| std::time::Duration::from_millis(fallback_interval_ms));

    'outer: loop {
        let mut listener = PgListener::connect_with(&listener_pools.write()).await?;
        listener.listen(ONWARDS_CONFIG_CHANGED_CHANNEL).await?;
        info!("Started key policy sync listener");

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
                        reload(&pools.write(), &cache).await;
                    }
                    Ok(None) => {
                        debug!("key policy sync: connection lost, reconnecting");
                        break;
                    }
                    Err(e) => {
                        crate::background_error!(component::ZDR_KEY_SYNC, "listen", Error, error = %e, "key policy sync: listener error");
                        break;
                    }
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

async fn reload(pool: &PgPool, cache: &KeyPolicyCache) {
    match refresh(pool, cache).await {
        Ok(keys) => debug!(keys, "key policy sync: reloaded map"),
        Err(e) => {
            crate::background_error!(component::ZDR_KEY_SYNC, "load", Error, error = %e, "key policy sync: failed to reload map");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cache_reads_every_key_as_non_zdr() {
        let cache = KeyPolicyCache::empty();
        assert!(!cache.is_zdr("sk-anything"));
    }

    #[test]
    fn is_zdr_reflects_the_maps_flag() {
        let cache = KeyPolicyCache::from_pairs([("sk-on".to_string(), true), ("sk-off".to_string(), false)]);
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
        let cache = KeyPolicyCache::empty();
        let handle = cache.clone();
        assert!(!handle.is_zdr("sk-on"));
        cache.replace(
            [(
                "sk-on".to_string(),
                KeyPolicy {
                    zdr: true,
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        );
        assert!(handle.is_zdr("sk-on"));
    }

    #[test]
    fn disabled_modalities_default_to_empty_and_follow_the_policy() {
        use crate::modalities::Modality;
        let cache = KeyPolicyCache::from_policies([(
            "sk-no-batch".to_string(),
            KeyPolicy {
                zdr: false,
                disabled_modalities: [Modality::Batch].into_iter().collect(),
            },
        )]);
        assert!(cache.disabled_modalities("sk-no-batch").contains(Modality::Batch));
        assert!(!cache.disabled_modalities("sk-no-batch").contains(Modality::Realtime));
        assert!(cache.disabled_modalities("sk-missing").is_empty());
    }
}
