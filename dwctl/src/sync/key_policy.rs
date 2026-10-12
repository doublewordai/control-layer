//! Lightweight per-key account policy sync.
//!
//! Maintains a memory-local map from api key secret to the owning account's
//! request policy: the `users.zero_data_retention` flag (read by
//! [`crate::inference::zdr::is_zdr_request`]), the account's
//! `users.disabled_modalities` (read by the inference middleware's modality
//! gate), and any `users.toleration_preset` (read by the inference middleware
//! and the batch ingest path to affix a preset's toleration list to every
//! request from the account). All answer on the request hot path with a
//! lock-free map read and no DB round-trip.
//!
//! Unlike [`crate::sync::onwards_config`], which rebuilds the whole routing
//! table on every change, this runs one small two-table join and swaps a flat
//! map, so refreshing it on every `auth_config_changed` notification is cheap.
//! It reuses that channel (fired by the `api_keys` trigger and the `users`
//! ZDR, disabled-modalities and toleration-preset triggers), a 100ms debounce,
//! and a periodic fallback reload.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::{PgPool, postgres::PgListener};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config::ONWARDS_CONFIG_CHANGED_CHANNEL;
use crate::metrics::errors::component;
use crate::modalities::ModalitySet;
use crate::scheduling::TolerationPresets;

/// The account-level policy a key inherits from its owner (`api_keys.user_id`,
/// the organization for an org key).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyPolicy {
    /// `users.zero_data_retention` of the owning account.
    pub zdr: bool,
    /// `users.disabled_modalities` of the owning account.
    pub disabled_modalities: ModalitySet,
    /// `users.toleration_preset` of the owning account: the id of the config
    /// preset whose toleration list is written verbatim to
    /// `nvext.routing_constraints.tolerations` on every request from this
    /// account. `None` means the account is on "Default" (nothing pinned).
    ///
    /// An `Arc<str>` so a per-request [`KeyPolicyCache::policy`] clone (which
    /// every ZDR and modality read already does) copies a refcount rather than
    /// allocating a fresh `String`.
    pub toleration_preset: Option<Arc<str>>,
}

/// Lock-free, cheap-to-clone handle to the shared secret-to-policy map, plus
/// the deployment's resolved toleration presets the ids are looked up in.
///
/// A secret absent from the map is a deleted or invalid key (auth rejects it
/// before any body is stored or any request is served), so absence safely
/// reads as the permissive default: not ZDR, nothing disabled, Default preset.
#[derive(Clone)]
pub struct KeyPolicyCache {
    inner: Arc<ArcSwap<HashMap<String, KeyPolicy>>>,
    /// The config presets a stored `toleration_preset` id resolves against.
    /// Shared by `Arc` (the map inside is immutable after construction), so a
    /// clone is cheap and a hot-path resolve holds no lock.
    presets: TolerationPresets,
}

impl KeyPolicyCache {
    /// An empty cache: every key reads as the default policy. Used before the
    /// first load, in tests, and when the sync is disabled.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            presets: TolerationPresets::default(),
        }
    }

    /// An empty map that resolves toleration-preset ids against `presets`.
    /// Callers pass the deployment's configured presets; without this, every id
    /// would fail closed (see `TolerationPresets::resolve`).
    pub fn with_presets(presets: TolerationPresets) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            presets,
        }
    }

    /// The policy for the api key `secret`, or the default for an unknown key.
    /// Hot-path safe.
    pub fn policy(&self, secret: &str) -> KeyPolicy {
        self.inner.load().get(secret).cloned().unwrap_or_default()
    }

    /// Whether the api key `secret` belongs to a ZDR account. Hot-path safe.
    pub fn is_zdr(&self, secret: &str) -> bool {
        self.policy(secret).zdr
    }

    /// The modalities the owning account has disabled for `secret`.
    pub fn disabled_modalities(&self, secret: &str) -> ModalitySet {
        self.policy(secret).disabled_modalities
    }

    /// The toleration list the account owning `secret` is pinned to, resolved
    /// from its stored preset id, or `None` when the account is on "Default".
    /// An id the config no longer defines fails closed to the empty list. Hot
    /// path safe.
    pub fn tolerations(&self, secret: &str) -> Option<Arc<serde_json::Value>> {
        self.presets.resolve(self.preset_id(secret).as_deref())
    }

    /// The stored preset id for `secret`, cheaply cloned (an `Arc<str>`), so a
    /// caller that only needs the id does not allocate a whole `KeyPolicy`.
    fn preset_id(&self, secret: &str) -> Option<Arc<str>> {
        self.inner.load().get(secret).and_then(|p| p.toleration_preset.clone())
    }

    /// The presets a stored id resolves against, for a caller that read an
    /// account's policy directly rather than through this map.
    pub fn presets(&self) -> &TolerationPresets {
        &self.presets
    }

    /// Replace the resolved preset map, so a config reload can change or drop
    /// presets without a restart. The account-id map is left alone; the next
    /// `refresh` reloads it.
    pub fn replace_presets(&self, presets: TolerationPresets) {
        self.presets.store(presets);
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

    /// Build a cache from explicit policies that resolve preset ids against
    /// `presets`. Test-only.
    #[cfg(test)]
    pub fn from_policies_with_presets<I: IntoIterator<Item = (String, KeyPolicy)>>(presets: TolerationPresets, pairs: I) -> Self {
        let cache = Self::with_presets(presets);
        cache.replace(pairs.into_iter().collect());
        cache
    }
}

/// Run the join and collect the flat secret-to-policy map. The settings live
/// on `users` (account-wide); a key inherits its owner's via `api_keys.user_id`.
async fn load(pool: &PgPool) -> Result<HashMap<String, KeyPolicy>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT ak.secret, u.zero_data_retention, u.disabled_modalities, u.toleration_preset
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
                    toleration_preset: r.toleration_preset.map(Arc::from),
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

/// The account policy for one owner, read directly from the DB. The batch
/// ingest path uses this for an account whose hidden key was just created and
/// cannot yet be in the map (see `upload_file`). `Ok(default)` for an unknown
/// owner, matching the map's permissive read for an absent key.
pub async fn load_policy(conn: &mut sqlx::PgConnection, user_id: crate::types::UserId) -> Result<KeyPolicy, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT zero_data_retention, disabled_modalities, toleration_preset
        FROM users
        WHERE id = $1
        "#,
        user_id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row
        .map(|r| KeyPolicy {
            zdr: r.zero_data_retention,
            disabled_modalities: ModalitySet::from_db(&r.disabled_modalities),
            toleration_preset: r.toleration_preset.map(Arc::from),
        })
        .unwrap_or_default())
}

/// Load the map once, synchronously, returning a populated cache. Call at
/// startup before the server accepts traffic so the map is never empty under
/// live traffic (an empty map reads every key as non-ZDR and would leak a ZDR
/// account's body during warm-up).
///
/// Resolves toleration-preset ids against the deployment's configured
/// `presets`. The serving process calls this with
/// [`TolerationPresets::new`]`(&config.scheduling)`; without it, every stored
/// preset id would fail closed to the empty list.
pub async fn initial_cache(pool: &PgPool, presets: TolerationPresets) -> Result<KeyPolicyCache, sqlx::Error> {
    let cache = KeyPolicyCache::with_presets(presets);
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
                ..Default::default()
            },
        )]);
        assert!(cache.disabled_modalities("sk-no-batch").contains(Modality::Batch));
        assert!(!cache.disabled_modalities("sk-no-batch").contains(Modality::Realtime));
        assert!(cache.disabled_modalities("sk-missing").is_empty());
    }

    #[test]
    fn tolerations_resolve_the_preset_id_and_default_to_none() {
        use crate::config::{SchedulingConfig, TolerationPreset};
        use crate::scheduling::{SchedulingOperator, SchedulingToleration};

        let presets = TolerationPresets::new(&SchedulingConfig {
            toleration_presets: vec![
                SchedulingConfig::default_preset(),
                TolerationPreset {
                    id: "gpu".to_string(),
                    label: "GPU".to_string(),
                    description: "GPU capacity only.".to_string(),
                    tolerations: vec![SchedulingToleration {
                        key: "pool".to_string(),
                        operator: Some(SchedulingOperator::Equal),
                        value: Some("dedicated".to_string()),
                        effect: None,
                    }],
                },
            ],
        });
        let cache = KeyPolicyCache::from_policies_with_presets(
            presets,
            [
                (
                    "sk-preset".to_string(),
                    KeyPolicy {
                        toleration_preset: Some("gpu".into()),
                        ..Default::default()
                    },
                ),
                (
                    "sk-empty".to_string(),
                    KeyPolicy {
                        toleration_preset: Some("no-overflow".into()),
                        ..Default::default()
                    },
                ),
                (
                    "sk-default".to_string(),
                    KeyPolicy {
                        toleration_preset: None,
                        ..Default::default()
                    },
                ),
            ],
        );
        assert_eq!(
            cache.tolerations("sk-preset").as_deref(),
            Some(&serde_json::json!([{"key": "pool", "operator": "Equal", "value": "dedicated"}]))
        );
        assert_eq!(cache.tolerations("sk-empty").as_deref(), Some(&serde_json::json!([])));
        // The Default setting and an unknown key are both unpinned.
        assert!(cache.tolerations("sk-default").is_none());
        assert!(cache.tolerations("sk-missing").is_none());
    }

    #[test]
    fn an_unconfigured_preset_fails_closed_to_the_empty_list() {
        let cache = KeyPolicyCache::from_policies_with_presets(
            TolerationPresets::default(),
            [(
                "sk-retired".to_string(),
                KeyPolicy {
                    toleration_preset: Some("retired".into()),
                    ..Default::default()
                },
            )],
        );
        // A preset an operator has dropped from config resolves to the most
        // restrictive option rather than leaving the account unpinned.
        assert_eq!(cache.tolerations("sk-retired").as_deref(), Some(&serde_json::json!([])));
    }
}
