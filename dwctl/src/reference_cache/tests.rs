use std::time::Duration;

use chrono::{DateTime, Utc};

use super::*;
use crate::api::models::users::Role;
use crate::crypto::hash_api_key;
use crate::test::utils::{create_test_api_key_for_user, create_test_user};

const MODEL: &str = "acme/chat-model";

fn policies() -> (TierPolicy, TelemetryPolicy) {
    (
        TierPolicy::from_config(&["5m".into(), "1h".into(), "24h".into()], "5m"),
        TelemetryPolicy::from_config(false, &[]),
    )
}

fn body(messages: &[(&str, &str)]) -> Bytes {
    let msgs: Vec<serde_json::Value> = messages.iter().map(|(r, c)| serde_json::json!({"role": r, "content": c})).collect();
    Bytes::from(serde_json::to_vec(&serde_json::json!({"model": MODEL, "messages": msgs})).unwrap())
}

fn bounds(b: &Bytes) -> Boundaries {
    let (tier, telemetry) = policies();
    boundaries(b, &tier, &telemetry).unwrap()
}

const SYSTEM: (&str, &str) = ("system", "You are a careful assistant for a fictional hardware store.");
const USER_1: (&str, &str) = ("user", "Which of the listed drills is the lightest?");
const ASSISTANT_1: (&str, &str) = ("assistant", "The compact model, at 1.1 kg.");
const USER_2: (&str, &str) = ("user", "And the cheapest?");
const USER_2_ALT: (&str, &str) = ("user", "Does it ship with a spare battery?");

// ===== The estimate on its own =====

fn entry(b: &Boundaries, i: usize, tpb: Option<f32>, exact: Option<i32>, visible_at: DateTime<Utc>) -> LiveEntry {
    LiveEntry {
        prefix_hash: b.hashes[i].clone(),
        cumulative_bytes: b.cumulative_bytes[i],
        tokens_per_byte: tpb,
        exact_tokens: exact,
        visible_at,
    }
}

#[test]
fn boundaries_accumulate_text_bytes() {
    let b = bounds(&body(&[SYSTEM, USER_1]));
    assert_eq!(b.hashes.len(), 2);
    assert_eq!(
        b.cumulative_bytes,
        vec![SYSTEM.1.len() as i64, (SYSTEM.1.len() + USER_1.1.len()) as i64]
    );
}

#[test]
fn a_final_boundary_match_uses_the_exact_count_less_the_allowance() {
    let b = bounds(&body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2]));
    let past = Utc::now() - chrono::Duration::seconds(10);
    let live = [entry(&b, 1, Some(0.3), Some(1_000), past)];
    assert_eq!(estimate(&b, &live, Utc::now(), 1_500, 0), 1_000);
    assert_eq!(estimate(&b, &live, Utc::now(), 1_500, 7), 993);
}

#[test]
fn an_intermediate_match_is_bytes_times_the_writers_ratio() {
    let b = bounds(&body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2]));
    let past = Utc::now() - chrono::Duration::seconds(10);
    let live = [entry(&b, 0, Some(0.5), None, past), entry(&b, 2, Some(0.5), None, past)];
    let expected = (b.cumulative_bytes[2] as f64 * 0.5).round() as u64;
    assert_eq!(estimate(&b, &live, Utc::now(), 10_000, 0), expected, "the deepest match wins");
}

#[test]
fn a_provisional_match_borrows_the_readers_ratio() {
    let b = bounds(&body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2]));
    let past = Utc::now() - chrono::Duration::seconds(1);
    let live = [entry(&b, 1, None, None, past)];
    let own = 2_000f32 / b.total_bytes() as f32;
    assert_eq!(
        estimate(&b, &live, Utc::now(), 2_000, 0),
        (b.cumulative_bytes[1] as f64 * f64::from(own)).round() as u64
    );
}

#[test]
fn entries_visible_only_after_arrival_do_not_count() {
    let b = bounds(&body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2]));
    let arrival = Utc::now();
    let live = [
        entry(&b, 0, Some(1.0), None, arrival - chrono::Duration::seconds(5)),
        entry(&b, 3, Some(1.0), Some(900), arrival + chrono::Duration::milliseconds(1)),
    ];
    assert_eq!(estimate(&b, &live, arrival, 1_000, 0), b.cumulative_bytes[0] as u64);
    assert_eq!(estimate(&b, &live[1..], arrival, 1_000, 0), 0);
}

#[test]
fn the_estimate_is_capped_at_prompt_tokens() {
    let b = bounds(&body(&[SYSTEM, USER_1]));
    let past = Utc::now() - chrono::Duration::seconds(1);
    assert_eq!(estimate(&b, &[entry(&b, 1, Some(0.3), Some(1_000), past)], Utc::now(), 950, 0), 950);
    assert_eq!(estimate(&b, &[entry(&b, 0, Some(50.0), None, past)], Utc::now(), 100, 0), 100);
}

#[test]
fn no_match_is_zero() {
    let b = bounds(&body(&[SYSTEM, USER_1]));
    assert_eq!(estimate(&b, &[], Utc::now(), 1_000, 0), 0);
}

#[test]
fn only_new_or_incomplete_boundaries_are_rewritten() {
    let b = bounds(&body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2]));
    let t = Utc::now();
    assert_eq!(needs_write(&b, &[]), vec![0, 1, 2, 3]);
    let live = [
        entry(&b, 0, Some(0.3), None, t),
        entry(&b, 1, Some(0.3), Some(10), t),
        entry(&b, 2, None, None, t),
        entry(&b, 3, Some(0.3), None, t),
    ];
    assert_eq!(
        needs_write(&b, &live),
        vec![2, 3],
        "a provisional row and a final row without its count"
    );
}

// ===== Config =====

#[test]
fn config_validation() {
    assert!(
        ReferenceCacheConfig::default().validate(false, false).is_ok(),
        "disabled ignores everything"
    );
    let enabled = ReferenceCacheConfig {
        enabled: true,
        orgs: vec![Uuid::new_v4()],
        ..Default::default()
    };
    assert!(enabled.validate(true, true).is_ok());
    assert!(enabled.validate(false, true).unwrap_err().contains("enable_analytics"));
    assert!(
        enabled.validate(true, false).unwrap_err().contains("cache.enabled"),
        "it runs in the cache layer"
    );
    let no_orgs = ReferenceCacheConfig {
        orgs: vec![],
        ..enabled.clone()
    };
    assert!(
        no_orgs.validate(true, true).is_ok(),
        "assurance terms cover their organisations without the list"
    );
    let no_join = ReferenceCacheConfig {
        join_deadline_ms: 0,
        ..enabled.clone()
    };
    assert!(no_join.validate(true, true).unwrap_err().contains("join_deadline_ms"));
    let no_models = ReferenceCacheConfig {
        models: Some(vec![]),
        ..enabled.clone()
    };
    assert!(no_models.validate(true, true).unwrap_err().contains("models"));
    let no_ttl = ReferenceCacheConfig {
        ttl_secs: 0,
        ..enabled.clone()
    };
    assert!(no_ttl.validate(true, true).unwrap_err().contains("ttl_secs"));
    let no_timeout = ReferenceCacheConfig {
        estimate_timeout_ms: 0,
        ..enabled.clone()
    };
    assert!(no_timeout.validate(true, true).unwrap_err().contains("estimate_timeout_ms"));
    let no_sweep = ReferenceCacheConfig {
        sweep_interval_secs: 0,
        ..enabled.clone()
    };
    assert!(no_sweep.validate(true, true).unwrap_err().contains("sweep_interval_secs"));
    let no_connections = ReferenceCacheConfig {
        max_connections: 0,
        ..enabled
    };
    assert!(no_connections.validate(true, true).unwrap_err().contains("max_connections"));
}

#[test]
fn orgs_deserialize_from_a_list_or_a_comma_string() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let from_yaml: ReferenceCacheConfig = serde_json::from_value(serde_json::json!({"orgs": [a, b]})).unwrap();
    assert_eq!(from_yaml.orgs, vec![a, b]);
    let from_env: ReferenceCacheConfig = serde_json::from_value(serde_json::json!({"orgs": format!("{a}, {b}")})).unwrap();
    assert_eq!(from_env.orgs, vec![a, b]);
    assert!(serde_json::from_value::<ReferenceCacheConfig>(serde_json::json!({"orgs": "not-a-uuid"})).is_err());
    let defaults: ReferenceCacheConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!((defaults.ttl_secs, defaults.orgs.len()), (300, 0));
}

// ===== Against the database =====

pub(super) struct Fixture {
    pub cache: Arc<ReferenceCache>,
    pub secret: String,
    pub principal: UserId,
    pub pool: sqlx::PgPool,
}

/// A reference cache covering a fresh user's key: through the `orgs` list when
/// `allowlisted`, and through `terms` on [`MODEL`] when given.
pub(super) async fn fixture_with(pool: sqlx::PgPool, allowlisted: bool, allowance: u32, terms: Option<CacheAssurance>) -> Fixture {
    let user = create_test_user(&pool, Role::StandardUser).await;
    let key = create_test_api_key_for_user(&pool, user.id).await;
    let config = ReferenceCacheConfig {
        enabled: true,
        orgs: vec![if allowlisted { user.id } else { Uuid::new_v4() }],
        template_allowance_tokens: HashMap::from([(MODEL.to_string(), allowance)]),
        ..Default::default()
    };
    let (_, telemetry) = policies();
    let book = AssuranceBook::fixed(terms.map(|t| ((user.id, MODEL.to_string()), t)).into_iter().collect());
    Fixture {
        cache: Arc::new(ReferenceCache::new(&config, pool.clone(), telemetry, Arc::new(book))),
        secret: key.secret.clone(),
        principal: user.id,
        pool,
    }
}

async fn fixture(pool: sqlx::PgPool, allowlisted: bool, allowance: u32) -> Fixture {
    fixture_with(pool, allowlisted, allowance, None).await
}

impl Fixture {
    /// One successful request, start to finish: the lookup at `arrival`, the estimate
    /// for `prompt_tokens`, and the completion write.
    async fn complete(&self, b: &Bytes, prompt_tokens: u64, arrival: DateTime<Utc>) -> Option<u64> {
        let mut pending = self.cache.begin(hash_api_key(&self.secret), MODEL, MODEL, b.clone(), arrival);
        let estimate = pending.join(Duration::from_secs(5)).await.estimate(prompt_tokens);
        pending.finish(Some(prompt_tokens), true);
        estimate
    }

    async fn joined(&self, b: &Bytes) -> Joined {
        let mut pending = self.cache.begin(hash_api_key(&self.secret), MODEL, MODEL, b.clone(), Utc::now());
        pending.join(Duration::from_secs(5)).await.clone()
    }

    /// Poll until the spawned write for `b` has landed: its final boundary is live with
    /// an exact count (`exact`) or provisionally (`!exact`). Then wait until this
    /// process's clock has passed the entry's `visible_at`, which the database clock
    /// stamps: a request arriving next (stamped with this clock) must see it even when
    /// the database clock runs slightly ahead.
    pub(super) async fn wait_for(&self, b: &Bytes, exact: bool) {
        let last = bounds(b).hashes.pop().unwrap();
        for _ in 0..500 {
            let row = sqlx::query!(
                "SELECT exact_tokens, visible_at FROM reference_cache_entries WHERE principal_id = $1 AND prefix_hash = $2 AND expires_at > now()",
                self.principal,
                last
            )
            .fetch_optional(&self.pool)
            .await
            .unwrap();
            if let Some(row) = row.filter(|r| r.exact_tokens.is_some() == exact) {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while Utc::now() <= row.visible_at {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "database clock is far ahead of this process's"
                    );
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("write never landed");
    }

    pub(super) async fn rows(&self) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM reference_cache_entries WHERE principal_id = $1"#,
            self.principal
        )
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

/// Turn two continues turn one: it gets turn one's exact `prompt_tokens`, less the
/// configured template allowance.
#[dwctl_test_macros::test]
async fn linear_continuation_gets_the_exact_count(pool: sqlx::PgPool) {
    let f = fixture(pool, true, 4).await;
    let turn_1 = body(&[SYSTEM, USER_1]);
    assert_eq!(
        f.complete(&turn_1, 1_200, Utc::now()).await,
        Some(0),
        "a cold cache is a miss, not NULL"
    );
    f.wait_for(&turn_1, true).await;

    let turn_2 = body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2]);
    assert_eq!(f.complete(&turn_2, 1_500, Utc::now()).await, Some(1_196));
}

/// A request that diverges after an earlier boundary is credited the shared bytes at
/// the writer's tokens per byte.
#[dwctl_test_macros::test]
async fn a_fork_at_an_intermediate_boundary_is_a_byte_estimate(pool: sqlx::PgPool) {
    let f = fixture(pool, true, 0).await;
    let original = body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2]);
    f.complete(&original, 2_000, Utc::now()).await;
    f.wait_for(&original, true).await;

    let fork = body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2_ALT]);
    let o = bounds(&original);
    let tpb = 2_000.0f32 / o.total_bytes() as f32;
    let expected = (o.cumulative_bytes[2] as f64 * f64::from(tpb)).round() as u64;
    assert_eq!(f.complete(&fork, 2_100, Utc::now()).await, Some(expected));
}

/// Past the window, the prefix is gone.
#[dwctl_test_macros::test]
async fn entries_expire_after_the_ttl(pool: sqlx::PgPool) {
    let f = fixture(pool, true, 0).await;
    let turn_1 = body(&[SYSTEM, USER_1]);
    f.complete(&turn_1, 1_200, Utc::now()).await;
    f.wait_for(&turn_1, true).await;

    // Age every entry past its window, as five idle minutes would.
    sqlx::query!(
        "UPDATE reference_cache_entries SET expires_at = now() - interval '1 second' WHERE principal_id = $1",
        f.principal
    )
    .execute(&f.pool)
    .await
    .unwrap();
    let turn_2 = body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2]);
    assert_eq!(f.complete(&turn_2, 1_500, Utc::now()).await, Some(0));
}

/// A streaming request is visible from its first token: a request arriving while it is
/// still generating hits it, one that arrived before its first token does not, and the
/// writer does not match its own entries.
#[dwctl_test_macros::test]
async fn a_stream_is_visible_from_its_first_token(pool: sqlx::PgPool) {
    let f = fixture(pool, true, 0).await;
    let a = body(&[SYSTEM, USER_1]);
    let a_arrival = Utc::now();

    assert_eq!(f.rows().await, 0);
    f.cache
        .observe_first_token(hash_api_key(&f.secret), MODEL.into(), MODEL.into(), a.clone());
    f.wait_for(&a, false).await;

    // In flight: A has produced its first token but not completed.
    let b = body(&[SYSTEM, USER_1, ASSISTANT_1, USER_2]);
    let bb = bounds(&b);
    let own_tpb = 1_500.0f32 / bb.total_bytes() as f32;
    let expected = (bb.cumulative_bytes[1] as f64 * f64::from(own_tpb)).round() as u64;
    assert_eq!(
        f.complete(&b, 1_500, Utc::now()).await,
        Some(expected),
        "provisional: bytes at B's own ratio"
    );
    assert_eq!(f.complete(&b, 1_500, a_arrival).await, Some(0), "arrived before A was visible");

    // A completes: its own first-token write does not count for it, and its exact count
    // is filled in for later readers.
    assert_eq!(f.complete(&a, 1_200, a_arrival).await, Some(0));
    f.wait_for(&a, true).await;
    // A later continuation of A (diverging from B after A's last block) gets A's exact count.
    let d = body(&[SYSTEM, USER_1, ("assistant", "Probably the cordless one."), USER_2]);
    assert_eq!(f.complete(&d, 1_500, Utc::now()).await, Some(1_200));
}

#[dwctl_test_macros::test]
async fn the_estimate_never_exceeds_prompt_tokens(pool: sqlx::PgPool) {
    let f = fixture(pool, true, 0).await;
    let a = body(&[SYSTEM, USER_1]);
    f.complete(&a, 1_200, Utc::now()).await;
    f.wait_for(&a, true).await;
    // An identical resend the engine counted lower.
    assert_eq!(f.complete(&a, 1_100, Utc::now()).await, Some(1_100));
}

/// Outside the allowlist nothing is estimated or written; covered requests that cannot
/// be estimated, or that fail, record NULL and write nothing.
#[dwctl_test_macros::test]
async fn uncovered_or_unusable_requests_record_null(pool: sqlx::PgPool) {
    let f = fixture(pool.clone(), false, 0).await;
    let a = body(&[SYSTEM, USER_1]);
    f.cache
        .observe_first_token(hash_api_key(&f.secret), MODEL.into(), MODEL.into(), a.clone());
    assert!(matches!(f.joined(&a).await, Joined::Uncovered));
    assert_eq!(f.complete(&a, 1_200, Utc::now()).await, None);
    assert_eq!(f.rows().await, 0);

    let f = fixture(pool, true, 0).await;
    let not_json = f.joined(&Bytes::from_static(b"not json")).await;
    assert!(
        matches!(
            not_json,
            Joined::Unavailable {
                outcome: "parse_error",
                coverage: Some(_)
            }
        ),
        "{not_json:?}"
    );
    assert_eq!(not_json.estimate(10), None);

    for (prompt_tokens, success, what) in [(Some(1_200), false, "failed request"), (None, true, "no usage")] {
        let mut pending = f.cache.begin(hash_api_key(&f.secret), MODEL, MODEL, a.clone(), Utc::now());
        assert_eq!(pending.join(Duration::from_secs(5)).await.estimate(1_200), Some(0), "{what}");
        pending.finish(prompt_tokens, success);
    }
    // A request dropped before it finishes (a client disconnect) writes nothing either.
    drop(f.cache.begin(hash_api_key(&f.secret), MODEL, MODEL, a.clone(), Utc::now()));
    // Give a write that should not happen the chance to land.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.rows().await, 0, "none of them wrote entries");
}

/// Terms cover their principal on their model even when it is not in `orgs`, and the
/// join hands them on with the estimate.
#[dwctl_test_macros::test]
async fn assurance_terms_cover_their_principal(pool: sqlx::PgPool) {
    let f = fixture_with(pool, false, 0, Some(CacheAssurance::GapShare(0.4))).await;
    let a = body(&[SYSTEM, USER_1]);
    let joined = f.joined(&a).await;
    assert_eq!(joined.terms(), Some(CacheAssurance::GapShare(0.4)));
    assert_eq!(joined.coverage().map(|c| c.principal), Some(f.principal));
    assert_eq!(joined.estimate(1_000), Some(0));

    let mut other = f
        .cache
        .begin(hash_api_key(&f.secret), "other-model", "other-model", a.clone(), Utc::now());
    assert!(
        matches!(other.join(Duration::from_secs(5)).await, Joined::Uncovered),
        "terms name their model"
    );
}

/// A lookup that misses the join deadline bills nothing from the reference, but carries
/// on: the request's entries are still written once it finishes.
#[dwctl_test_macros::test]
async fn a_late_lookup_records_null_but_still_writes(pool: sqlx::PgPool) {
    let f = fixture_with(pool, false, 0, Some(CacheAssurance::GapShare(0.4))).await;
    let a = body(&[SYSTEM, USER_1]);
    // Hold the table so the lookup (which also slides expiries) cannot run; plain
    // reads still can.
    let mut blocker = f.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE reference_cache_entries IN EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let mut pending = f.cache.begin(hash_api_key(&f.secret), MODEL, MODEL, a.clone(), Utc::now());
    let joined = pending.join(Duration::from_millis(50)).await.clone();
    match &joined {
        Joined::Unavailable { coverage, outcome } => {
            assert_eq!(*outcome, "deadline_exceeded");
            assert_eq!(
                coverage.as_ref().and_then(|c| c.terms),
                Some(CacheAssurance::GapShare(0.4)),
                "the key resolved before the table wait, so the fallback is attributed"
            );
        }
        other => panic!("expected a deadline miss, got {other:?}"),
    }
    assert_eq!(joined.estimate(1_200), None);
    pending.finish(Some(1_200), true);
    blocker.rollback().await.unwrap();
    f.wait_for(&a, true).await;
}
