//! Mid-stream continuation: resume generations that die mid-stream.
//!
//! When an upstream kills a stream part-way through a generation, the resume
//! middleware (wired inner to outlet/cache, outer to error enrichment) rebuilds
//! the exact prompt + partial output as a token-id vector via tokenizer-svc
//! `/v1/render` and re-enters the onwards stack as a `/v1/completions` request
//! on the SAME model alias. Routing is onwards' business: the model's composite
//! carries a `completions` POOL (dynamo first, the validated continuation
//! target behind it), and onwards resolves a completions-class request to that
//! pool before it selects a provider — so a resume leg can never land on a chat
//! failover that was never validated to continue a token-id prefix. The client
//! keeps one uninterrupted stream; outlet/billing see one logical request with
//! a merged usage frame.
//!
//! This module currently owns the **global continuation key**: a single hidden
//! `continuation`-purpose API key that authenticates resume legs into onwards.
//! Its purpose label is no longer a routing input (the request path is), but it
//! is what permits the leg to carry a scheduling `priority` — onwards strips
//! that field from every other caller. It is deliberately global (not
//! per-user):
//! - resume legs must keep working even when the requesting user's own keys
//!   have been pulled mid-stream (e.g. credit exhaustion) — once we have
//!   accepted and partially streamed a response, we finish it; the user is
//!   still billed normally via the merged frame on the original request;
//! - key cardinality stays constant as users grow (onwards config sync cost
//!   scales with key count);
//! - resumes are model/provider faults, so throttling belongs per-model in the
//!   middleware, not per-user on the key.
//!
//! The key is owned by the nil-UUID SYSTEM user and provisioned at startup so
//! it has synced into the onwards key cache before the first resume attempt.
//! System ownership is load-bearing, not cosmetic: onwards' per-model keysets
//! admit a key only when its owner is in one of the model's groups (or the
//! model is public) AND the owner carries positive balance (or the model is
//! free) — gates a prod-shaped (group-restricted, priced) composite would fail
//! for any ordinary owner, silently 403ing every resume leg. The system user is
//! exempt from all of those gates in the keyset queries.

pub mod accumulate;
pub mod detect;
pub mod dsv4;
pub mod layer;
pub mod metrics;
pub mod render;
pub mod resume;
pub mod rewrap;

#[cfg(test)]
mod resume_tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use dashmap::DashMap;
use moka::future::Cache;
use sqlx::PgPool;

use crate::UserId;
use crate::db::handlers::api_keys::ApiKeys;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::metrics::errors::component;

pub use layer::{ContinuationState, continuation_middleware};

/// How often the per-model route cache is refreshed from the composite's
/// components.
/// v1 is a poll: attaching a continuation route is an admin action measured in
/// days, so a 30s window to take effect is irrelevant, and LISTEN/NOTIFY (which
/// the onwards config sync already runs) can replace it later without changing
/// the read side.
const ROUTE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// The global continuation key's owner: the nil-UUID system user, which the
/// onwards keyset queries exempt from the group-access, balance and purpose
/// gates. Owned by anyone else, the key would be silently absent from the
/// keysets of group-restricted or priced composites and every resume leg on
/// them would 403.
pub fn global_key_owner() -> UserId {
    uuid::Uuid::nil()
}

/// Get or create the global hidden continuation key, returning its secret.
///
/// Idempotent (`ON CONFLICT` upsert keyed on owner + purpose): the startup call
/// guarantees existence/sync, and the resume middleware calls it again to
/// obtain the secret without caring which call created the row.
pub async fn provision_global_key(pool: &PgPool) -> anyhow::Result<String> {
    let owner = global_key_owner();
    let mut tx = pool.begin().await?;
    let secret = ApiKeys::new(&mut tx)
        .get_or_create_hidden_key(owner, ApiKeyPurpose::Continuation, owner)
        .await?;
    tx.commit().await?;
    Ok(secret)
}

/// Per-model continuation route configuration, read from the composite's
/// `completions` pool.
///
/// The same rows are what make the model resumable at all (onwards resolves
/// `/v1/completions` to that pool) and how we must render for it, so "is this
/// model resumable" and "how do we build its prefix" stop being configured in
/// two places.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteInfo {
    /// `chat_template_kwargs` for tokenizer-svc `/v1/render`. Also the source of
    /// truth for the serving mode the reconstructor must match: a route serving
    /// DeepSeek in chat mode (`{"thinking_mode": "chat"}`) must not have a
    /// `</think>` spliced into its resume prefix, while tokenizer-svc renders
    /// that family in thinking mode by default. `None` = the model's template
    /// default.
    pub render_kwargs: Option<serde_json::Value>,
    /// This provider prepends its own BOS (Fireworks does on most models), so
    /// our leading BOS has to come off or the exact prefix shifts by one token.
    ///
    /// **Carried, not yet applied — deliberately.** BOS-prepending is a property
    /// of the MEMBER that ends up serving the leg, and the middleware builds one
    /// body before onwards picks one: a composite tries its on-prem member first,
    /// which does not prepend, so a pre-stripped prompt would reach dynamo a
    /// token short. The strip belongs in onwards' per-member request forwarding,
    /// next to `onwards_model` rewriting, and should be wired there when a
    /// provider that needs it is onboarded. Today's only validated route
    /// (Fireworks / DeepSeek-V4-Flash) is `strip_leading_bos = false`, so
    /// nothing is lost by carrying the value and acting on it later.
    pub strip_leading_bos: bool,
}

impl RouteInfo {
    /// Whether the leg this route serves generates in thinking mode.
    ///
    /// Read from `render_kwargs`, because that is literally what the prompt was
    /// rendered with: `thinking_mode` (DeepSeek's spelling, `"thinking"` /
    /// `"chat"`) or a boolean `thinking`. Absent ⇒ true, matching
    /// tokenizer-svc's own default for the families that have one.
    pub fn thinking(&self) -> bool {
        let Some(kwargs) = self.render_kwargs.as_ref() else {
            return true;
        };
        if let Some(mode) = kwargs.get("thinking_mode").and_then(|v| v.as_str()) {
            return !mode.eq_ignore_ascii_case("chat");
        }
        kwargs.get("thinking").and_then(|v| v.as_bool()).unwrap_or(true)
    }

    /// Merge this route's render kwargs with the ones the client sent.
    ///
    /// The route describes how the model is served (its default mode); the
    /// request's own `chat_template_kwargs` are what leg 1 was actually
    /// templated with downstream, so they win key-by-key. Reproducing leg 1's
    /// prompt is the whole objective.
    pub fn merged_render_kwargs(&self, request_kwargs: Option<&serde_json::Value>) -> Option<serde_json::Value> {
        match (self.render_kwargs.as_ref(), request_kwargs) {
            (None, other) => other.cloned(),
            (Some(route), None) => Some(route.clone()),
            (Some(serde_json::Value::Object(route)), Some(serde_json::Value::Object(request))) => {
                let mut merged = route.clone();
                for (k, v) in request {
                    merged.insert(k.clone(), v.clone());
                }
                Some(serde_json::Value::Object(merged))
            }
            // A non-object on either side is not mergeable; the request's own
            // value is the more specific one.
            (Some(route), Some(request)) => Some(if request.is_null() { route.clone() } else { request.clone() }),
        }
    }
}

/// The models that have a continuation route attached, with that route's
/// per-model config, refreshed by a background poll.
///
/// "Attached" = the model's composite has a `completions` pool with at least one
/// enabled member — the pool onwards resolves `/v1/completions` traffic to.
/// Reads are lock-cheap and happen on every
/// streaming chat request, so the map is swapped wholesale rather than mutated.
/// An empty map — including the moment before the first refresh lands — means no
/// model is resumable, which is the safe direction.
pub struct ContinuationRoutes {
    routes: RwLock<Arc<HashMap<String, RouteInfo>>>,
}

impl Default for ContinuationRoutes {
    fn default() -> Self {
        Self::new()
    }
}

impl ContinuationRoutes {
    pub fn new() -> Self {
        Self {
            routes: RwLock::new(Arc::new(HashMap::new())),
        }
    }

    /// Test/seed constructor: models with default route config.
    pub fn with_models<I: IntoIterator<Item = String>>(models: I) -> Self {
        Self::with_routes(models.into_iter().map(|m| (m, RouteInfo::default())))
    }

    /// Test/seed constructor carrying per-model config.
    pub fn with_routes<I: IntoIterator<Item = (String, RouteInfo)>>(routes: I) -> Self {
        Self {
            routes: RwLock::new(Arc::new(routes.into_iter().collect())),
        }
    }

    pub fn is_enabled(&self, model: &str) -> bool {
        self.get(model).is_some()
    }

    /// This model's route config, or `None` when it has no continuation route.
    pub fn get(&self, model: &str) -> Option<RouteInfo> {
        self.routes.read().ok()?.get(model).cloned()
    }

    pub fn len(&self) -> usize {
        self.routes.read().map(|s| s.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn store(&self, routes: HashMap<String, RouteInfo>) {
        if let Ok(mut guard) = self.routes.write() {
            *guard = Arc::new(routes);
        }
    }

    /// One refresh pass. Public so tests can drive it deterministically instead
    /// of waiting on the poller's interval.
    pub async fn refresh(&self, pool: &PgPool) -> anyhow::Result<()> {
        // A disabled component receives no traffic from onwards either, so it is
        // not a route. The composite's alias is the key because that is the model
        // name the client asked for and the resume leg re-sends.
        //
        // One row per composite: a completions pool is a failover list, and the
        // middleware builds ONE body before onwards picks a member of it. The
        // representative is the pool's first member that is NOT also in the
        // default pool — the validated continuation target. (A member shared
        // with the default pool is the free first hop, typically dynamo, and
        // carries no continuation config of its own.) Falling back to plain
        // pool order keeps a single-member completions pool working.
        //
        // `render_kwargs` is the one field NOT taken from that representative
        // row. **Selection rule: the first NON-NULL `render_kwargs` across the
        // pool's members, in `sort_order`.** It describes how THE MODEL is
        // served — the serving mode the prefix must be rendered in — not a
        // property of whichever member answers, so any member stating it states
        // it for the pool, and a member that is silent (NULL) must not shout
        // down one that is not. First-member-wins got this wrong in production:
        // a NULL at sort 0 (the blackhole/dynamo primary) beat the validated
        // target's `{"thinking_mode": "chat"}` at sort 1, the resume prefix
        // rendered in thinking mode, and a real model emitted a literal
        // `</think>` into a client's content stream. Its eventual home is a
        // pool-level (or composite-level) column, at which point this
        // aggregate-over-members collapses into reading that column; the rule
        // here is chosen to be what that column would say.
        //
        // `strip_leading_bos` keeps the representative row's value, because
        // BOS-prepending genuinely IS per-member (see
        // [`RouteInfo::strip_leading_bos`]): the pool can hold one member that
        // prepends and one that does not, so no single pool-wide value is
        // correct. It stays approximate until the strip moves into onwards'
        // per-member request forwarding, where each member can be adjusted
        // outbound.
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT ON (cm.alias)
                cm.alias,
                first_value(dmc.render_kwargs) OVER (
                    PARTITION BY dmc.composite_model_id
                    ORDER BY (dmc.render_kwargs IS NULL), dmc.sort_order ASC
                ) AS render_kwargs,
                dmc.strip_leading_bos
            FROM deployed_model_components dmc
            JOIN deployed_models cm ON cm.id = dmc.composite_model_id
            JOIN deployed_models dm ON dm.id = dmc.deployed_model_id
            WHERE dmc.pool = 'completions'
              AND dmc.enabled = true
              AND cm.deleted = false
              AND dm.deleted = false
            ORDER BY
                cm.alias,
                EXISTS (
                    SELECT 1 FROM deployed_model_components shared
                    WHERE shared.composite_model_id = dmc.composite_model_id
                      AND shared.deployed_model_id = dmc.deployed_model_id
                      AND shared.pool = 'default'
                ),
                dmc.sort_order ASC
            "#
        )
        .fetch_all(pool)
        .await?;
        self.store(
            rows.into_iter()
                .map(|row| {
                    (
                        row.alias,
                        RouteInfo {
                            render_kwargs: row.render_kwargs,
                            strip_leading_bos: row.strip_leading_bos,
                        },
                    )
                })
                .collect(),
        );
        Ok(())
    }

    /// Spawn the refresh poller. A failed refresh keeps the previous set (a DB
    /// blip must not silently disable resume) and is reported through
    /// `background_error!` so it lands on the unified error metric.
    pub fn spawn_poller(self: Arc<Self>, pools: sqlx_pool_router::DynPools) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(ROUTE_REFRESH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if let Err(e) = self.refresh(&pools.write()).await {
                    crate::background_error!(
                        component::CONTINUATION,
                        "route_refresh",
                        Warning,
                        error = %e,
                        "Failed to refresh continuation routes; keeping the previous set"
                    );
                }
            }
        });
    }
}

/// Per-model cap on concurrently in-flight resume legs.
///
/// Mid-stream deaths are incident-bursty: a flapping model can produce hundreds
/// of deaths a minute, and every one of them would otherwise fire a full-prompt
/// prefill at the continuation provider. The counter is released by
/// [`InflightGuard`] on every exit path, including a client disconnect mid-resume
/// (the whole chain lives inside the response body stream, which is dropped).
pub struct InflightLimiter {
    max: u32,
    counts: DashMap<String, AtomicU32>,
}

impl InflightLimiter {
    pub fn new(max: u32) -> Self {
        Self {
            max,
            counts: DashMap::new(),
        }
    }

    /// Reserve a slot for `model`, or `None` when the model is at its cap.
    pub fn try_acquire(self: &Arc<Self>, model: &str) -> Option<InflightGuard> {
        let entry = self.counts.entry(model.to_string()).or_default();
        // fetch_update gives a compare-and-swap; a plain load+store could let two
        // concurrent deaths on the same model both see `max - 1`.
        let acquired = entry
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < self.max).then_some(n + 1))
            .is_ok();
        drop(entry);
        acquired.then(|| InflightGuard {
            limiter: Arc::clone(self),
            model: model.to_string(),
        })
    }

    pub fn in_flight(&self, model: &str) -> u32 {
        self.counts.get(model).map(|c| c.load(Ordering::SeqCst)).unwrap_or(0)
    }
}

/// Releases the per-model resume slot on drop.
pub struct InflightGuard {
    limiter: Arc<InflightLimiter>,
    model: String,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(entry) = self.limiter.counts.get(&self.model) {
            // Saturating: a released-twice bug must not wrap to u32::MAX and
            // permanently wedge the model's resume path.
            let _ = entry.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(1)));
        }
    }
}

/// Resolves a bearer token to its API-key `purpose`, which is how the origin gate
/// (realtime / batch / playground) is derived — the same input analytics uses for
/// `request_origin`.
///
/// Read-through with an in-process memo, exactly like
/// [`crate::prompt_cache::principal::PrincipalResolver`]: a key's purpose is
/// immutable, so the memo needs no invalidation and the TTL is hygiene only.
/// This layer sits BELOW auth (onwards validates the key), so the token here is
/// not yet known-good — memoising a miss is harmless (it grants nothing) and
/// bounds the cost of key probing.
#[derive(Clone)]
pub struct PurposeResolver {
    /// Live provider (not a pinned pool): survives runtime pool swaps.
    pools: sqlx_pool_router::DynPools,
    l1: Cache<String, Option<ApiKeyPurpose>>,
}

impl PurposeResolver {
    pub fn new(pools: impl sqlx_pool_router::PoolProvider) -> Self {
        Self {
            pools: sqlx_pool_router::DynPools::new(pools),
            l1: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(Duration::from_secs(3600))
                .build(),
        }
    }

    pub async fn resolve(&self, token: &str) -> Option<ApiKeyPurpose> {
        if let Some(cached) = self.l1.get(token).await {
            return cached;
        }
        // Only a SUCCESSFUL lookup is cached. A transient DB failure must not
        // pin `None` (= treated as realtime) for the TTL — a batch key would
        // sail past a disabled batch gate for an hour. The failed request
        // still resolves to None (one-off misclassification, fail-open on the
        // origin gate only); the next request retries the lookup.
        let Ok(mut conn) = self.pools.write().acquire().await else {
            return None;
        };
        let looked_up = ApiKeys::new(&mut conn).get_user_info_by_secret(token).await;
        drop(conn);
        match looked_up {
            Ok(found) => {
                let purpose = found.map(|(_, _, purpose)| purpose);
                self.l1.insert(token.to_string(), purpose.clone()).await;
                purpose
            }
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::{create_test_endpoint, create_test_model, create_test_user};

    /// Provisioning twice returns the same secret (idempotent upsert), and the
    /// row is a hidden continuation-purpose key owned by the SYSTEM user — the
    /// ownership the onwards keyset queries exempt from the group-access and
    /// balance gates, without which resume legs 403 on any group-restricted or
    /// priced composite.
    #[sqlx::test]
    async fn provision_global_key_is_idempotent_hidden_and_system_owned(pool: PgPool) {
        let first = provision_global_key(&pool).await.unwrap();
        let second = provision_global_key(&pool).await.unwrap();
        assert_eq!(first, second);

        let row = sqlx::query!(r#"SELECT user_id, purpose, hidden FROM api_keys WHERE secret = $1"#, first)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.user_id, global_key_owner());
        assert_eq!(row.purpose, "continuation");
        assert!(row.hidden);
    }

    /// Link `component` into one of `composite`'s pools.
    async fn add_component(pool: &PgPool, composite: uuid::Uuid, component: uuid::Uuid, component_pool: &str) {
        add_component_at(pool, composite, component, component_pool, 0).await
    }

    /// Link `component` into a pool at an explicit position within that pool.
    async fn add_component_at(pool: &PgPool, composite: uuid::Uuid, component: uuid::Uuid, component_pool: &str, sort_order: i32) {
        sqlx::query!(
            "INSERT INTO deployed_model_components (composite_model_id, deployed_model_id, weight, enabled, sort_order, pool)
             VALUES ($1, $2, 1, true, $4, $3)",
            composite,
            component,
            component_pool,
            sort_order
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn create_composite(pool: &PgPool, alias: &str, created_by: UserId) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO deployed_models (id, model_name, alias, created_by, deleted, is_composite)
             VALUES ($1, $2, $3, $4, false, true)",
            id,
            alias,
            alias,
            created_by
        )
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[sqlx::test]
    async fn routes_only_include_composites_with_a_completions_pool(pool: PgPool) {
        let user = create_test_user(&pool, Role::PlatformManager).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;

        let resumable = create_composite(&pool, "alias-resumable", user.id).await;
        let plain = create_composite(&pool, "alias-plain", user.id).await;
        let dynamo = create_test_model(&pool, "m-dynamo", "dynamo", endpoint, user.id).await;
        let openrouter = create_test_model(&pool, "m-or", "openrouter", endpoint, user.id).await;
        let fireworks = create_test_model(&pool, "m-fw", "fireworks", endpoint, user.id).await;

        // Both composites start with a default pool only.
        add_component(&pool, resumable, dynamo, "default").await;
        add_component(&pool, plain, openrouter, "default").await;

        let routes = ContinuationRoutes::new();
        routes.refresh(&pool).await.unwrap();
        assert!(routes.is_empty(), "a composite with only a default pool is not resumable");
        assert!(!routes.is_enabled("alias-resumable"));

        // Give it a completions pool: dynamo first, the validated target behind.
        add_component_at(&pool, resumable, dynamo, "completions", 0).await;
        add_component_at(&pool, resumable, fireworks, "completions", 1).await;
        routes.refresh(&pool).await.unwrap();

        assert!(routes.is_enabled("alias-resumable"));
        assert!(
            !routes.is_enabled("alias-plain"),
            "a composite with no completions pool is never resumable"
        );
        assert_eq!(routes.len(), 1);
        // Default per-route config until someone sets it.
        let route = routes.get("alias-resumable").unwrap();
        assert_eq!(route.render_kwargs, None);
        assert!(!route.strip_leading_bos);

        // A disabled member receives no traffic from onwards, so it is not part
        // of the pool; disabling every member of the completions pool removes
        // the route.
        sqlx::query!(
            "UPDATE deployed_model_components SET enabled = false WHERE composite_model_id = $1 AND pool = 'completions'",
            resumable
        )
        .execute(&pool)
        .await
        .unwrap();
        routes.refresh(&pool).await.unwrap();
        assert!(routes.is_empty());
    }

    #[sqlx::test]
    async fn per_route_config_reaches_the_cache(pool: PgPool) {
        let user = create_test_user(&pool, Role::PlatformManager).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let composite = create_composite(&pool, "dsv4-flash", user.id).await;
        let fireworks = create_test_model(&pool, "m-fw", "fireworks", endpoint, user.id).await;
        add_component(&pool, composite, fireworks, "completions").await;
        sqlx::query!(
            r#"UPDATE deployed_model_components
               SET render_kwargs = '{"thinking_mode": "chat"}'::jsonb,
                   strip_leading_bos = true,
                   continuation_validated_at = NOW()
               WHERE composite_model_id = $1"#,
            composite
        )
        .execute(&pool)
        .await
        .unwrap();

        let routes = ContinuationRoutes::new();
        routes.refresh(&pool).await.unwrap();
        let route = routes.get("dsv4-flash").expect("the canary route");
        assert_eq!(route.render_kwargs, Some(serde_json::json!({"thinking_mode": "chat"})));
        assert!(route.strip_leading_bos);
        assert!(!route.thinking(), "a chat-mode route must not close a think tag");
    }

    /// A completions pool is a failover list, and the middleware builds ONE body
    /// before onwards picks a member of it. The route's config therefore comes
    /// from the member that exists only to serve completions — the validated
    /// target — not from the free first hop it shares with the default pool.
    #[sqlx::test]
    async fn the_route_config_comes_from_the_completions_only_member(pool: PgPool) {
        let user = create_test_user(&pool, Role::PlatformManager).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let composite = create_composite(&pool, "dsv4-flash", user.id).await;
        let dynamo = create_test_model(&pool, "m-dynamo", "dynamo", endpoint, user.id).await;
        let fireworks = create_test_model(&pool, "m-fw", "fireworks", endpoint, user.id).await;

        // dynamo is in both pools (position 0 of each); fireworks only in
        // completions, behind it.
        add_component(&pool, composite, dynamo, "default").await;
        add_component_at(&pool, composite, dynamo, "completions", 0).await;
        add_component_at(&pool, composite, fireworks, "completions", 1).await;
        sqlx::query!(
            r#"UPDATE deployed_model_components
               SET render_kwargs = '{"thinking_mode": "chat"}'::jsonb, strip_leading_bos = true
               WHERE composite_model_id = $1 AND deployed_model_id = $2 AND pool = 'completions'"#,
            composite,
            fireworks
        )
        .execute(&pool)
        .await
        .unwrap();

        let routes = ContinuationRoutes::new();
        routes.refresh(&pool).await.unwrap();
        let route = routes.get("dsv4-flash").expect("the canary route");
        assert_eq!(
            route.render_kwargs,
            Some(serde_json::json!({"thinking_mode": "chat"})),
            "the validated target's config wins over the shared first hop's defaults"
        );
        assert!(route.strip_leading_bos);
    }

    /// `render_kwargs` describes the MODEL's serving mode, so the first member
    /// to state one states it for the pool. A silent (NULL) member ahead of it
    /// must not win by position: that is exactly how a chat-mode route rendered
    /// its resume prefix in thinking mode and leaked a `</think>` to a client.
    #[sqlx::test]
    async fn the_first_non_null_render_kwargs_in_the_pool_wins(pool: PgPool) {
        let user = create_test_user(&pool, Role::PlatformManager).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let composite = create_composite(&pool, "dsv4-flash", user.id).await;
        let blackhole = create_test_model(&pool, "m-dynamo", "dynamo", endpoint, user.id).await;
        let fireworks = create_test_model(&pool, "m-fw", "fireworks", endpoint, user.id).await;

        // Both members are completions-only (neither is in the default pool, so
        // the shared-first-hop tie-break cannot save us here); the primary at
        // sort 0 carries no render config at all.
        add_component_at(&pool, composite, blackhole, "completions", 0).await;
        add_component_at(&pool, composite, fireworks, "completions", 1).await;
        sqlx::query!(
            r#"UPDATE deployed_model_components
               SET render_kwargs = '{"thinking_mode": "chat"}'::jsonb
               WHERE composite_model_id = $1 AND deployed_model_id = $2 AND pool = 'completions'"#,
            composite,
            fireworks
        )
        .execute(&pool)
        .await
        .unwrap();

        let routes = ContinuationRoutes::new();
        routes.refresh(&pool).await.unwrap();
        let route = routes.get("dsv4-flash").expect("the canary route");
        assert_eq!(
            route.render_kwargs,
            Some(serde_json::json!({"thinking_mode": "chat"})),
            "a NULL at sort 0 must not override the mode a later member states"
        );
        assert!(!route.thinking(), "a chat-mode route must not close a think tag");
    }

    #[test]
    fn thinking_is_read_from_the_route_kwargs() {
        let route = |kwargs: Option<serde_json::Value>| RouteInfo {
            render_kwargs: kwargs,
            strip_leading_bos: false,
        };
        // tokenizer-svc renders the reasoning families in thinking mode by
        // default, so an unconfigured route is a thinking route.
        assert!(route(None).thinking());
        assert!(route(Some(serde_json::json!({}))).thinking());
        assert!(route(Some(serde_json::json!({"thinking_mode": "thinking"}))).thinking());
        assert!(!route(Some(serde_json::json!({"thinking_mode": "chat"}))).thinking());
        assert!(!route(Some(serde_json::json!({"thinking_mode": "CHAT"}))).thinking());
        // The boolean spelling some templates use.
        assert!(!route(Some(serde_json::json!({"thinking": false}))).thinking());
        assert!(route(Some(serde_json::json!({"thinking": true}))).thinking());
    }

    #[test]
    fn request_kwargs_override_the_route_key_by_key() {
        let route = RouteInfo {
            render_kwargs: Some(serde_json::json!({"thinking_mode": "chat", "tool_style": "dsml"})),
            strip_leading_bos: false,
        };
        // Nothing from the client: the route's own kwargs.
        assert_eq!(route.merged_render_kwargs(None), route.render_kwargs);
        // The client templated leg 1 with something else; that is what we must
        // reproduce, key by key.
        assert_eq!(
            route.merged_render_kwargs(Some(&serde_json::json!({"thinking_mode": "thinking"}))),
            Some(serde_json::json!({"thinking_mode": "thinking", "tool_style": "dsml"}))
        );
        // No route config: the client's kwargs pass through untouched.
        let bare = RouteInfo::default();
        assert_eq!(
            bare.merged_render_kwargs(Some(&serde_json::json!({"thinking": true}))),
            Some(serde_json::json!({"thinking": true}))
        );
        assert_eq!(bare.merged_render_kwargs(None), None);
    }

    #[sqlx::test]
    async fn purpose_resolver_reads_through_and_memoises(pool: PgPool) {
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = crate::test::utils::create_test_api_key_for_user(&pool, user.id).await;

        let resolver = PurposeResolver::new(pool);
        assert_eq!(resolver.resolve(&key.secret).await, Some(ApiKeyPurpose::Realtime));
        // Second call is served from the memo — still correct.
        assert_eq!(resolver.resolve(&key.secret).await, Some(ApiKeyPurpose::Realtime));
        assert_eq!(resolver.resolve("not-a-real-secret").await, None);
    }

    #[test]
    fn inflight_limiter_caps_per_model_and_releases_on_drop() {
        let limiter = Arc::new(InflightLimiter::new(2));
        let a1 = limiter.try_acquire("m").expect("first slot");
        let _a2 = limiter.try_acquire("m").expect("second slot");
        assert_eq!(limiter.in_flight("m"), 2);
        assert!(limiter.try_acquire("m").is_none(), "the cap is per model");
        // A different model has its own budget.
        assert!(limiter.try_acquire("other").is_some());

        drop(a1);
        assert_eq!(limiter.in_flight("m"), 1);
        assert!(limiter.try_acquire("m").is_some(), "a released slot is reusable");
    }
}
