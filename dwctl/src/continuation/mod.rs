//! Mid-stream continuation: resume generations that die mid-stream.
//!
//! When an upstream kills a stream part-way through a generation, the resume
//! middleware (wired inner to outlet/cache, outer to error enrichment) rebuilds
//! the exact prompt + partial output as a token-id vector via tokenizer-svc
//! `/v1/render` and re-enters the onwards stack as a `/v1/completions` request
//! on the model's continuation composite (dynamo first, provider fallback).
//! The client keeps one uninterrupted stream; outlet/billing see one logical
//! request with a merged usage frame.
//!
//! This module currently owns the **global continuation key**: a single hidden
//! `continuation`-purpose API key that authenticates resume legs into onwards
//! and carries the purpose label the `model_traffic_rules` redirect fires on.
//! It is deliberately global (not per-user):
//! - resume legs must keep working even when the requesting user's own keys
//!   have been pulled mid-stream (e.g. credit exhaustion) — once we have
//!   accepted and partially streamed a response, we finish it; the user is
//!   still billed normally via the merged frame on the original request;
//! - key cardinality stays constant as users grow (onwards config sync cost
//!   scales with key count);
//! - resumes are model/provider faults, so throttling belongs per-model in the
//!   middleware, not per-user on the key.
//!
//! The key is owned by the initial admin user and provisioned at startup so it
//! has synced into the onwards key cache before the first resume attempt.

pub mod accumulate;
pub mod detect;
pub mod layer;
pub mod metrics;
pub mod render;
pub mod resume;
pub mod rewrap;

#[cfg(test)]
mod resume_tests;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use dashmap::DashMap;
use moka::future::Cache;
use sqlx::PgPool;

use crate::UserId;
use crate::db::handlers::api_keys::ApiKeys;
use crate::db::handlers::users::Users;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::metrics::errors::component;

pub use layer::{ContinuationState, continuation_middleware};

/// How often the per-model route cache is refreshed from `model_traffic_rules`.
/// v1 is a poll: attaching a continuation route is an admin action measured in
/// days, so a 30s window to take effect is irrelevant, and LISTEN/NOTIFY (which
/// the onwards config sync already runs) can replace it later without changing
/// the read side.
const ROUTE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Get or create the global hidden continuation key, returning its secret.
///
/// Idempotent (`ON CONFLICT` upsert keyed on owner + purpose): the startup call
/// guarantees existence/sync, and the resume middleware calls it again to
/// obtain the secret without caring which call created the row.
pub async fn provision_global_key(pool: &PgPool, admin_user_id: UserId) -> anyhow::Result<String> {
    let mut tx = pool.begin().await?;
    let secret = ApiKeys::new(&mut tx)
        .get_or_create_hidden_key(admin_user_id, ApiKeyPurpose::Continuation, admin_user_id)
        .await?;
    tx.commit().await?;
    Ok(secret)
}

/// Provision (or fetch) the global continuation key by the configured admin
/// email. `build_router` has the config but not the admin's id — startup created
/// that user from the same email, and the provisioning call is idempotent, so
/// resolving by email here re-uses the very row the startup call made.
pub async fn provision_global_key_for_admin(pool: &PgPool, admin_email: &str) -> anyhow::Result<String> {
    let admin_id = {
        let mut conn = pool.acquire().await?;
        Users::new(&mut conn)
            .get_user_by_email(admin_email)
            .await?
            .ok_or_else(|| anyhow::anyhow!("admin user {admin_email} not found"))?
            .id
    };
    provision_global_key(pool, admin_id).await
}

/// The set of model aliases that have a continuation route attached, refreshed
/// by a background poll.
///
/// "Attached" = a `model_traffic_rules` row with `api_key_purpose = 'continuation'`
/// (M3 wires one by SQL for the canary model). Reads are lock-cheap and happen on
/// every streaming chat request, so the set is swapped wholesale rather than
/// mutated. An empty set — including the moment before the first refresh lands —
/// means no model is resumable, which is the safe direction.
pub struct ContinuationRoutes {
    enabled: RwLock<Arc<HashSet<String>>>,
}

impl Default for ContinuationRoutes {
    fn default() -> Self {
        Self::new()
    }
}

impl ContinuationRoutes {
    pub fn new() -> Self {
        Self {
            enabled: RwLock::new(Arc::new(HashSet::new())),
        }
    }

    /// Test/seed constructor.
    pub fn with_models<I: IntoIterator<Item = String>>(models: I) -> Self {
        Self {
            enabled: RwLock::new(Arc::new(models.into_iter().collect())),
        }
    }

    pub fn is_enabled(&self, model: &str) -> bool {
        self.enabled.read().map(|s| s.contains(model)).unwrap_or(false)
    }

    pub fn len(&self) -> usize {
        self.enabled.read().map(|s| s.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn store(&self, models: HashSet<String>) {
        if let Ok(mut guard) = self.enabled.write() {
            *guard = Arc::new(models);
        }
    }

    /// One refresh pass. Public so tests can drive it deterministically instead
    /// of waiting on the poller's interval.
    pub async fn refresh(&self, pool: &PgPool) -> anyhow::Result<()> {
        // Any rule row for the purpose counts, per spec: attachment is a
        // redirect rule, and a hypothetical `deny` row would simply make the
        // resume leg fail at onwards (wasted work, never wrong output).
        let aliases = sqlx::query_scalar!(
            r#"
            SELECT DISTINCT dm.alias
            FROM model_traffic_rules mtr
            JOIN deployed_models dm ON dm.id = mtr.deployed_model_id
            WHERE mtr.api_key_purpose = 'continuation'
              AND dm.deleted = false
            "#
        )
        .fetch_all(pool)
        .await?;
        self.store(aliases.into_iter().collect());
        Ok(())
    }

    /// Spawn the refresh poller. A failed refresh keeps the previous set (a DB
    /// blip must not silently disable resume) and is reported through
    /// `background_error!` so it lands on the unified error metric.
    pub fn spawn_poller(self: Arc<Self>, pool: PgPool) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(ROUTE_REFRESH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if let Err(e) = self.refresh(&pool).await {
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
    pool: PgPool,
    l1: Cache<String, Option<ApiKeyPurpose>>,
}

impl PurposeResolver {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
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
        let mut conn = self.pool.acquire().await.ok()?;
        let purpose = ApiKeys::new(&mut conn)
            .get_user_info_by_secret(token)
            .await
            .ok()
            .flatten()
            .map(|(_, _, purpose)| purpose);
        drop(conn);
        self.l1.insert(token.to_string(), purpose.clone()).await;
        purpose
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::{create_test_endpoint, create_test_model, create_test_user};

    /// Provisioning twice returns the same secret (idempotent upsert), and the
    /// row is a hidden continuation-purpose key owned by the given user.
    #[sqlx::test]
    async fn provision_global_key_is_idempotent_and_hidden(pool: PgPool) {
        let admin = create_test_user(&pool, Role::PlatformManager).await;

        let first = provision_global_key(&pool, admin.id).await.unwrap();
        let second = provision_global_key(&pool, admin.id).await.unwrap();
        assert_eq!(first, second);

        let row = sqlx::query!(r#"SELECT user_id, purpose, hidden FROM api_keys WHERE secret = $1"#, first)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.user_id, admin.id);
        assert_eq!(row.purpose, "continuation");
        assert!(row.hidden);
    }

    #[sqlx::test]
    async fn provisioning_by_admin_email_reuses_the_startup_key(pool: PgPool) {
        let admin = create_test_user(&pool, Role::PlatformManager).await;
        let from_startup = provision_global_key(&pool, admin.id).await.unwrap();
        let from_router = provision_global_key_for_admin(&pool, &admin.email).await.unwrap();
        assert_eq!(from_startup, from_router);
    }

    #[sqlx::test]
    async fn routes_only_include_models_with_a_continuation_rule(pool: PgPool) {
        let user = create_test_user(&pool, Role::PlatformManager).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let resumable = create_test_model(&pool, "m1", "alias-resumable", endpoint, user.id).await;
        let _plain = create_test_model(&pool, "m2", "alias-plain", endpoint, user.id).await;
        let batch_ruled = create_test_model(&pool, "m3", "alias-batch-denied", endpoint, user.id).await;
        sqlx::query!(
            "INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action) VALUES ($1, 'batch', 'deny')",
            batch_ruled
        )
        .execute(&pool)
        .await
        .unwrap();

        let routes = ContinuationRoutes::new();
        // Before any rule exists the set is empty — the safe direction.
        routes.refresh(&pool).await.unwrap();
        assert!(routes.is_empty());
        assert!(!routes.is_enabled("alias-resumable"));

        sqlx::query!(
            r#"INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action, redirect_target_id)
               VALUES ($1, 'continuation', 'redirect', $1)"#,
            resumable
        )
        .execute(&pool)
        .await
        .unwrap();
        routes.refresh(&pool).await.unwrap();

        assert!(routes.is_enabled("alias-resumable"));
        assert!(!routes.is_enabled("alias-plain"), "a model with no rule is never resumable");
        assert!(
            !routes.is_enabled("alias-batch-denied"),
            "a rule for a different purpose must not enable resume"
        );
        assert_eq!(routes.len(), 1);
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
