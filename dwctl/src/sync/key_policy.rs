//! Lightweight per-key account policy sync.
//!
//! Maintains a memory-local map from api key secret to the owning account's
//! request policy: the `users.zero_data_retention` flag (read by
//! [`crate::inference::zdr::is_zdr_request`]), the account's
//! `users.disabled_modalities` (read by the inference middleware's modality
//! gate), any `users.toleration_preset` (read by the inference middleware
//! and the batch ingest path to affix a preset's toleration list to every
//! request from the account), and the account and purpose the key bills
//! (read with the [`PriceBook`] to price a request before it is sent). All
//! answer on the request hot path with a lock-free map read and no DB
//! round-trip.
//!
//! Unlike [`crate::sync::onwards_config`], which rebuilds the whole routing
//! table on every change, this runs one small two-table join plus the tariff
//! lookups and swaps flat maps, so refreshing it on every
//! `auth_config_changed` notification is cheap. It reuses that channel (fired
//! by the `api_keys` and `model_tariffs` triggers and the `users` ZDR,
//! disabled-modalities and toleration-preset triggers), a 100ms debounce, and
//! a periodic fallback reload, which is also what picks up a cache-tariff
//! change.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::{ArcSwap, ArcSwapOption};
use sqlx::{PgPool, postgres::PgListener};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config::ONWARDS_CONFIG_CHANGED_CHANNEL;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::metrics::errors::component;
use crate::modalities::ModalitySet;
use crate::pricing::{PriceBook, PriceQuery, UnitPrices};
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
    /// Per-model preset ids from the account catalog (`account_model_policies`),
    /// keyed by model alias, each overriding `toleration_preset` for requests to
    /// that model. A class policy has already been folded over its model's.
    /// Shared by every key of the account.
    pub model_toleration_presets: Option<Arc<HashMap<String, Arc<str>>>>,
    /// The account the key's usage is charged to and the purpose that selects
    /// its tariff. Set for every key in the map; `None` for a policy read per
    /// account rather than per key.
    pub billing: Option<KeyBilling>,
}

/// Who a key's usage is charged to (`api_keys.user_id`) and the purpose its
/// tariff is selected by (`api_keys.purpose`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBilling {
    pub account: crate::types::UserId,
    pub purpose: ApiKeyPurpose,
}

impl KeyPolicy {
    /// The preset id that applies to a request for `model`: the model's (or
    /// its class's) from the account catalog, else the account's own.
    pub fn toleration_preset_for(&self, model: &str) -> Option<&str> {
        self.model_toleration_presets
            .as_ref()
            .and_then(|presets| presets.get(model))
            .or(self.toleration_preset.as_ref())
            .map(|preset| &**preset)
    }
}

/// One account's toleration presets, resolvable per model: what a batch
/// upload stamps on each line, by the line's model.
#[derive(Clone, Default)]
pub struct AccountTolerations {
    policy: KeyPolicy,
    presets: TolerationPresets,
}

impl AccountTolerations {
    /// The toleration list for a request to `model`, or `None` when the
    /// account is on "Default" for it.
    pub fn for_model(&self, model: &str) -> Option<Arc<serde_json::Value>> {
        self.presets.resolve(self.policy.toleration_preset_for(model))
    }
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
    /// Every model's prices, general and for the accounts in the map,
    /// reloaded with it. `None` until a load succeeds; a failed reload keeps
    /// the previous book, which prices only the accounts and models it was
    /// loaded with.
    prices: Arc<ArcSwapOption<PriceBook>>,
}

impl KeyPolicyCache {
    /// An empty cache: every key reads as the default policy. Used before the
    /// first load, in tests, and when the sync is disabled.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            presets: TolerationPresets::default(),
            prices: Arc::new(ArcSwapOption::empty()),
        }
    }

    /// An empty map that resolves toleration-preset ids against `presets`.
    /// Callers pass the deployment's configured presets; without this, every id
    /// would fail closed (see `TolerationPresets::resolve`).
    pub fn with_presets(presets: TolerationPresets) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            presets,
            prices: Arc::new(ArcSwapOption::empty()),
        }
    }

    /// The policy for the api key `secret`, or the default for an unknown key.
    /// Hot-path safe.
    pub fn policy(&self, secret: &str) -> KeyPolicy {
        self.inner.load().get(secret).cloned().unwrap_or_default()
    }

    /// The policy for `secret` if the key is present in the map, or `None` when
    /// it is absent (a key created since the last refresh, or one auth would
    /// reject). Unlike [`Self::policy`], this distinguishes a cache miss from a
    /// cached account on the permissive default, so a caller can decide whether
    /// a DB read is warranted. Hot-path safe.
    pub fn policy_if_known(&self, secret: &str) -> Option<KeyPolicy> {
        self.inner.load().get(secret).cloned()
    }

    /// Whether the api key `secret` belongs to a ZDR account. Hot-path safe.
    pub fn is_zdr(&self, secret: &str) -> bool {
        self.policy(secret).zdr
    }

    /// The modalities the owning account has disabled for `secret`.
    pub fn disabled_modalities(&self, secret: &str) -> ModalitySet {
        self.policy(secret).disabled_modalities
    }

    /// The toleration list the account owning `secret` is pinned to for a
    /// request to `model`: the account catalog's preset for that model, else
    /// the account's own, or `None` when neither is set ("Default"). An id the
    /// config no longer defines fails closed to the empty list. Hot path safe.
    pub fn tolerations(&self, secret: &str, model: &str) -> Option<Arc<serde_json::Value>> {
        let map = self.inner.load();
        self.presets
            .resolve(map.get(secret).and_then(|policy| policy.toleration_preset_for(model)))
    }

    /// Everything needed to resolve the tolerations of the account owning
    /// `secret`, for any model, when the key is present in the map. `None`
    /// means the key is absent (a key created since the last refresh), which
    /// the caller may resolve from the DB with [`load_policy`]. Hot path safe.
    pub fn account_tolerations_if_known(&self, secret: &str) -> Option<AccountTolerations> {
        self.policy_if_known(secret).map(|policy| self.account_tolerations(policy))
    }

    /// Resolve tolerations for an account policy read directly from the DB.
    pub fn account_tolerations(&self, policy: KeyPolicy) -> AccountTolerations {
        AccountTolerations {
            policy,
            presets: self.presets.clone(),
        }
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

    /// The per-token prices a request made with `secret` is charged at, or
    /// `None` when the key is not in the map, no prices have loaded yet, or
    /// the loaded prices predate the key's account or the model (a price
    /// reload failed since they were added). Hot-path safe.
    pub(crate) fn unit_prices(&self, secret: &str, query: &PriceQuery<'_>) -> Option<UnitPrices> {
        let map = self.inner.load();
        let billing = map.get(secret)?.billing.as_ref()?;
        let prices = self.prices.load();
        prices.as_ref()?.unit_prices(billing.account, &billing.purpose, query)
    }

    fn replace(&self, map: HashMap<String, KeyPolicy>) {
        self.inner.store(Arc::new(map));
    }

    /// Replace the price book. Test-only; [`refresh`] reloads it with the map.
    #[cfg(test)]
    pub(crate) fn replace_prices(&self, prices: PriceBook) {
        self.prices.store(Some(Arc::new(prices)));
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
        SELECT ak.secret, ak.user_id, ak.purpose, u.zero_data_retention, u.disabled_modalities, u.toleration_preset
        FROM api_keys ak
        JOIN users u ON u.id = ak.user_id
        WHERE NOT ak.is_deleted
        "#
    )
    .fetch_all(pool)
    .await?;
    let model_presets = load_model_presets(pool, None).await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.secret,
                KeyPolicy {
                    zdr: r.zero_data_retention,
                    disabled_modalities: ModalitySet::from_db(&r.disabled_modalities),
                    toleration_preset: r.toleration_preset.map(Arc::from),
                    model_toleration_presets: model_presets.get(&r.user_id).cloned(),
                    billing: Some(KeyBilling {
                        account: r.user_id,
                        purpose: crate::pricing::parse_api_key_purpose(&r.purpose),
                    }),
                },
            )
        })
        .collect())
}

/// Per-model preset ids from the account catalog, for every account or for
/// one, each account's map shared by `Arc` across its keys. A class row
/// (`standard`, the only class a request resolves to today) overrides its
/// model row.
async fn load_model_presets<'c>(
    conn: impl sqlx::PgExecutor<'c>,
    account: Option<crate::types::UserId>,
) -> Result<HashMap<crate::types::UserId, Arc<HashMap<String, Arc<str>>>>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT p.user_id, dm.alias, p.toleration_preset AS "toleration_preset!"
        FROM account_model_policies p
        JOIN deployed_models dm ON dm.id = p.deployed_model_id
        WHERE p.toleration_preset IS NOT NULL
          AND dm.deleted = FALSE
          AND (p.serving_class IS NULL OR p.serving_class = 'standard')
          AND ($1::uuid IS NULL OR p.user_id = $1)
        ORDER BY p.serving_class NULLS FIRST
        "#,
        account
    )
    .fetch_all(conn)
    .await?;

    let mut presets: HashMap<crate::types::UserId, HashMap<String, Arc<str>>> = HashMap::new();
    for row in rows {
        presets
            .entry(row.user_id)
            .or_default()
            .insert(row.alias, Arc::from(row.toleration_preset));
    }
    Ok(presets.into_iter().map(|(account, presets)| (account, Arc::new(presets))).collect())
}

/// Reload `cache` in place from the DB, returning the number of keys loaded.
/// Shared by [`initial_cache`], the background listener, and the test-only
/// manual refresh so none of them can drift.
///
/// Only the policy map's load is an error. Prices are optional: when they fail
/// to load, the error is logged, the map is still replaced, and the previous
/// price book is kept (none at boot, so requests carry no prices). A key of an
/// account that book was not loaded for, or a request for a model it does not
/// hold, carries no prices until a load succeeds.
pub async fn refresh(pool: &PgPool, cache: &KeyPolicyCache) -> Result<usize, sqlx::Error> {
    let map = load(pool).await?;
    let accounts: Vec<crate::types::UserId> = map
        .values()
        .filter_map(|policy| policy.billing.as_ref().map(|billing| billing.account))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let prices = PriceBook::load(pool, &accounts).await;
    let n = map.len();
    cache.replace(map);
    match prices {
        Ok(prices) => cache.prices.store(Some(Arc::new(prices))),
        Err(e) => {
            crate::background_error!(component::ZDR_KEY_SYNC, "load_prices", Error, error = %e, "key policy sync: failed to load prices; keeping the previous prices");
        }
    }
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
    let Some(row) = row else {
        return Ok(KeyPolicy::default());
    };

    let model_toleration_presets = load_model_presets(&mut *conn, Some(user_id)).await?.remove(&user_id);
    Ok(KeyPolicy {
        zdr: row.zero_data_retention,
        disabled_modalities: ModalitySet::from_db(&row.disabled_modalities),
        toleration_preset: row.toleration_preset.map(Arc::from),
        model_toleration_presets,
        billing: None,
    })
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
            cache.tolerations("sk-preset", "any/model").as_deref(),
            Some(&serde_json::json!([{"key": "pool", "operator": "Equal", "value": "dedicated"}]))
        );
        assert_eq!(cache.tolerations("sk-empty", "any/model").as_deref(), Some(&serde_json::json!([])));
        // The Default setting and an unknown key are both unpinned.
        assert!(cache.tolerations("sk-default", "any/model").is_none());
        assert!(cache.tolerations("sk-missing", "any/model").is_none());
        // `account_tolerations_if_known` distinguishes a cached Default from a
        // key absent from the map (`None`), so the batch upload only pays for
        // a DB read on a genuine miss.
        assert!(
            cache
                .account_tolerations_if_known("sk-default")
                .unwrap()
                .for_model("any/model")
                .is_none()
        );
        assert!(cache.account_tolerations_if_known("sk-missing").is_none());
        assert_eq!(
            cache
                .account_tolerations_if_known("sk-preset")
                .unwrap()
                .for_model("any/model")
                .as_deref(),
            Some(&serde_json::json!([{"key": "pool", "operator": "Equal", "value": "dedicated"}]))
        );
    }

    #[test]
    fn a_model_preset_overrides_the_account_preset_for_that_model_only() {
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
        let models = Arc::new(HashMap::from([("org/pinned".to_string(), Arc::<str>::from("no-overflow"))]));
        let cache = KeyPolicyCache::from_policies_with_presets(
            presets,
            [
                (
                    "sk-both".to_string(),
                    KeyPolicy {
                        toleration_preset: Some("gpu".into()),
                        model_toleration_presets: Some(models.clone()),
                        ..Default::default()
                    },
                ),
                (
                    "sk-model-only".to_string(),
                    KeyPolicy {
                        model_toleration_presets: Some(models),
                        ..Default::default()
                    },
                ),
            ],
        );
        let gpu = serde_json::json!([{"key": "pool", "operator": "Equal", "value": "dedicated"}]);
        assert_eq!(cache.tolerations("sk-both", "org/pinned").as_deref(), Some(&serde_json::json!([])));
        assert_eq!(cache.tolerations("sk-both", "org/other").as_deref(), Some(&gpu));
        assert_eq!(
            cache.tolerations("sk-model-only", "org/pinned").as_deref(),
            Some(&serde_json::json!([]))
        );
        // "No preset" stays distinct from the empty list on other models.
        assert!(cache.tolerations("sk-model-only", "org/other").is_none());
        let account = cache.account_tolerations_if_known("sk-both").unwrap();
        assert_eq!(account.for_model("org/pinned").as_deref(), Some(&serde_json::json!([])));
        assert_eq!(account.for_model("org/other").as_deref(), Some(&gpu));
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
        assert_eq!(
            cache.tolerations("sk-retired", "any/model").as_deref(),
            Some(&serde_json::json!([]))
        );
    }

    /// Deploy `alias` on an endpoint of its own, returning the model's id.
    async fn deploy(pool: &sqlx::PgPool, by: crate::types::UserId, alias: &str) -> uuid::Uuid {
        sqlx::query_scalar(
            "WITH e AS (INSERT INTO inference_endpoints (name,url,created_by) VALUES ($1,'http://localhost:1',$2) RETURNING id)
             INSERT INTO deployed_models (model_name,alias,hosted_on,created_by,type) SELECT $1,$1,e.id,$2,'CHAT' FROM e RETURNING id",
        )
        .bind(alias)
        .bind(by)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Price `model` for realtime at `input` and `output` millionths of a
    /// credit per token, for `account` alone or, with `None`, for everyone.
    async fn add_tariff(pool: &sqlx::PgPool, model: uuid::Uuid, account: Option<crate::types::UserId>, input: i64, output: i64) {
        sqlx::query(
            "INSERT INTO model_tariffs (deployed_model_id,user_id,name,input_price_per_token,output_price_per_token,api_key_purpose,valid_from) VALUES ($1,$2,'test',$3,$4,'realtime',$5)",
        )
        .bind(model)
        .bind(account)
        .bind(rust_decimal::Decimal::new(input, 6))
        .bind(rust_decimal::Decimal::new(output, 6))
        .bind(chrono::Utc::now() - chrono::Duration::minutes(1))
        .execute(pool)
        .await
        .unwrap();
    }

    /// Prices are optional: a price load that fails still refreshes the
    /// policy map and keeps the prices already loaded, and at boot it leaves
    /// requests unpriced rather than failing startup. The kept prices are
    /// used only for what they were loaded with: an account or a model added
    /// since gets no prices rather than general or free ones.
    #[dwctl_test_macros::test]
    async fn a_failed_price_load_still_refreshes_the_map_and_keeps_the_old_prices(pool: sqlx::PgPool) {
        use crate::api::models::users::Role;
        use crate::test::utils::{create_test_api_key_for_user, create_test_user};
        use rust_decimal::Decimal;

        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await.secret;
        let model = deploy(&pool, user.id, "priced-model").await;
        add_tariff(&pool, model, None, 1, 3).await;
        let query = |model: &'static str| PriceQuery {
            model,
            completion_window: None,
            batch_created_at: None,
            now: chrono::Utc::now(),
            serving_class: "standard",
            cache_reads: None,
        };
        let priced = Some(UnitPrices {
            prompt: Decimal::new(1, 6),
            cached: Decimal::new(1, 6),
            output: Decimal::new(3, 6),
        });

        let cache = initial_cache(&pool, TolerationPresets::default()).await.unwrap();
        assert_eq!(cache.unit_prices(&key, &query("priced-model")), priced);
        assert!(!cache.is_zdr(&key));

        // Change the policy, add an account with its own price and a newly
        // priced model, then make the tariff lookup fail.
        sqlx::query("UPDATE users SET zero_data_retention = true WHERE id = $1")
            .bind(user.id)
            .execute(&pool)
            .await
            .unwrap();
        let deal = create_test_user(&pool, Role::StandardUser).await;
        let deal_key = create_test_api_key_for_user(&pool, deal.id).await.secret;
        add_tariff(&pool, model, Some(deal.id), 2, 6).await;
        let later = deploy(&pool, user.id, "later-model").await;
        add_tariff(&pool, later, None, 5, 15).await;
        sqlx::query("ALTER TABLE model_tariffs RENAME TO model_tariffs_unavailable")
            .execute(&pool)
            .await
            .unwrap();

        refresh(&pool, &cache).await.expect("a price load failure is not a refresh failure");
        assert!(cache.is_zdr(&key), "the policy map was replaced");
        assert_eq!(
            cache.unit_prices(&key, &query("priced-model")),
            priced,
            "the previous prices are kept"
        );
        assert!(cache.policy_if_known(&deal_key).is_some(), "the new account's key is in the map");
        assert_eq!(
            cache.unit_prices(&deal_key, &query("priced-model")),
            None,
            "the kept prices hold nothing of the new account's own, so its key is not priced"
        );
        assert_eq!(
            cache.unit_prices(&key, &query("later-model")),
            None,
            "the kept prices predate the model, so it is not priced"
        );

        let booted = initial_cache(&pool, TolerationPresets::default())
            .await
            .expect("a price load failure does not fail startup");
        assert!(booted.is_zdr(&key));
        assert_eq!(
            booted.unit_prices(&key, &query("priced-model")),
            None,
            "no prices have loaded, so none are sent"
        );
    }
}
