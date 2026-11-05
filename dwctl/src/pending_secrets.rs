//! Keystore-wrapped secrets of issued API keys awaiting their one-off reveal.
//!
//! When a manager issues a key to another user, the holder may fetch its
//! secret once. That secret is held in `api_keys.pending_secret`, wrapped with
//! the keystore's wrap keyring, and cleared by the reveal, so the reveal never
//! needs the plaintext `api_keys.secret` column.
//!
//! Keys that became pending before this column existed, or were issued or
//! rotated by a pod of the previous release during the rollout, have no wrap
//! (or a stale one). [`run`] reconciles them on every instance, at startup and
//! then periodically: it wraps the plaintext of pending keys that lack a valid
//! wrap, re-wraps values sealed with a non-current wrap key (so an old wrap key
//! can be retired), and clears wraps left on keys that can no longer be
//! revealed. Until the plaintext column is gone, a reveal whose wrap is missing
//! or stale falls back to the plaintext.
//!
//! The reconcile has to have wrapped every pending key before the release that
//! stops storing plaintext: `dwctl_api_key_pending_reveals_needing_plaintext`
//! must read 0 on every instance first.

use std::time::Duration;

use metrics::gauge;
use sqlx_pool_router::PoolProvider;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::crypto::hash_api_key;
use crate::db::handlers::api_keys::{ApiKeys, PendingReveal};
use crate::keystore::Keystore;
use crate::metrics::errors::component;

/// How often each instance re-runs the reconcile after its startup pass. It
/// only touches the few keys with a pending reveal, so this is cheap; it is
/// periodic so keys issued by previous-release pods during a rollout are
/// wrapped without a restart.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Where a revealed secret came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealSource {
    /// Unwrapped from `pending_secret`.
    Wrapped,
    /// Read from the plaintext `secret` column (keys not yet wrapped).
    Plaintext,
}

impl RevealSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RevealSource::Wrapped => "wrapped",
            RevealSource::Plaintext => "plaintext",
        }
    }
}

/// Unwrap a pending secret and check it is the key's current secret.
/// `None` if it doesn't unwrap or belongs to a secret since rotated away.
fn unwrap_current(keystore: &Keystore, wrapped: &[u8], secret_hash: &[u8]) -> Option<String> {
    let plaintext = keystore.unwrap(wrapped).ok()?;
    let secret = String::from_utf8(plaintext).ok()?;
    (hash_api_key(&secret)[..] == *secret_hash).then_some(secret)
}

/// The secret a consumed reveal hands to the holder: the wrapped pending
/// secret when it unwraps to the key's current secret, otherwise the plaintext
/// column while it still exists. `None` when neither is available.
pub fn resolve_reveal(keystore: Option<&Keystore>, reveal: &PendingReveal) -> Option<(String, RevealSource)> {
    if let (Some(keystore), Some(wrapped), Some(hash)) = (keystore, reveal.pending_secret.as_deref(), reveal.secret_hash.as_deref()) {
        if let Some(secret) = unwrap_current(keystore, wrapped, hash) {
            return Some((secret, RevealSource::Wrapped));
        }
        tracing::warn!(api_key_id = %reveal.id, "pending secret did not unwrap to the key's current secret; using plaintext");
    }
    reveal.secret.clone().map(|secret| (secret, RevealSource::Plaintext))
}

/// Outcome of one reconcile pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Pending keys whose plaintext was wrapped for the first time (or whose
    /// stale wrap was replaced).
    pub wrapped: u64,
    /// Pending keys re-wrapped from a non-current wrap key.
    pub rewrapped: u64,
    /// Wraps cleared from keys that can no longer be revealed.
    pub cleared: u64,
    /// Pending keys whose reveal is not verified to work from its wrap alone,
    /// counted by re-reading them after wrapping: every pending key when no
    /// keystore is configured.
    pub needing_plaintext: u64,
}

/// What a pending key's reveal needs, judged from one read of its row.
enum PendingState {
    /// Wrapped with the current wrap key, and the wrap is of the key's current
    /// secret: the reveal needs nothing else.
    Verified,
    /// Wrap this secret (`rewrap`: replacing a valid wrap by an old wrap key).
    Wrap { secret: String, rewrap: bool },
    /// No keystore, or neither the wrap nor the plaintext yields the secret.
    Unresolved,
}

fn pending_state(keystore: Option<&Keystore>, row: &PendingReveal) -> PendingState {
    let (Some(keystore), Some(secret_hash)) = (keystore, row.secret_hash.as_deref()) else {
        return PendingState::Unresolved;
    };
    if let Some(wrapped) = row.pending_secret.as_deref()
        && let Some(secret) = unwrap_current(keystore, wrapped, secret_hash)
    {
        return if keystore.is_current(wrapped) {
            PendingState::Verified
        } else {
            PendingState::Wrap { secret, rewrap: true }
        };
    }
    match &row.secret {
        Some(secret) if hash_api_key(secret)[..] == *secret_hash => PendingState::Wrap {
            secret: secret.clone(),
            rewrap: false,
        },
        _ => PendingState::Unresolved,
    }
}

/// Wrap every row that needs it. A guarded write that matches nothing (the
/// key was revealed, deleted or rotated since it was read) is left to the
/// verification pass, which re-reads it.
async fn wrap_rows(
    repo: &mut ApiKeys<'_>,
    keystore: Option<&Keystore>,
    rows: Vec<PendingReveal>,
    report: &mut ReconcileReport,
) -> anyhow::Result<()> {
    for row in rows {
        let (Some(keystore), PendingState::Wrap { secret, rewrap }) = (keystore, pending_state(keystore, &row)) else {
            continue;
        };
        let Some(secret_hash) = row.secret_hash.as_deref() else { continue };
        let wrapped = keystore.wrap(secret.as_bytes())?;
        if repo.set_pending_secret(row.id, &wrapped, secret_hash).await? {
            if rewrap {
                report.rewrapped += 1;
            } else {
                report.wrapped += 1;
            }
        }
    }
    Ok(())
}

/// Re-read every pending key and count those whose reveal is not verified to
/// work from its wrap alone. Only this count feeds the readiness gauge, so a
/// write lost to a concurrent rotation by a release that doesn't maintain
/// wraps shows up as unresolved instead of being assumed settled.
async fn count_unverified(repo: &mut ApiKeys<'_>, keystore: Option<&Keystore>) -> anyhow::Result<u64> {
    Ok(repo
        .list_pending_reveals()
        .await?
        .iter()
        .filter(|row| !matches!(pending_state(keystore, row), PendingState::Verified))
        .count() as u64)
}

/// One reconcile pass over every key with a pending reveal.
pub async fn reconcile<P: PoolProvider>(db: &P, keystore: Option<&Keystore>) -> anyhow::Result<ReconcileReport> {
    let mut conn = db.write().acquire().await?;
    let mut repo = ApiKeys::new(&mut conn);
    let mut report = ReconcileReport {
        cleared: repo.clear_orphaned_pending_secrets().await?,
        ..Default::default()
    };
    let rows = repo.list_pending_reveals().await?;
    wrap_rows(&mut repo, keystore, rows, &mut report).await?;
    report.needing_plaintext = count_unverified(&mut repo, keystore).await?;
    Ok(report)
}

/// Run [`reconcile`] at startup and every [`RECONCILE_INTERVAL`] until shutdown.
pub async fn run<P: PoolProvider>(db: P, keystore: Option<Keystore>, shutdown: CancellationToken) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        match reconcile(&db, keystore.as_ref()).await {
            Ok(report) => {
                gauge!("dwctl_api_key_pending_reveals_needing_plaintext").set(report.needing_plaintext as f64);
                if report.wrapped + report.rewrapped + report.cleared > 0 {
                    info!(
                        wrapped = report.wrapped,
                        rewrapped = report.rewrapped,
                        cleared = report.cleared,
                        "reconciled wrapped pending API key secrets"
                    );
                }
                if report.needing_plaintext > 0 {
                    crate::background_error!(
                        component::PENDING_SECRETS,
                        "needs_plaintext",
                        Warning,
                        count = report.needing_plaintext,
                        keystore_configured = keystore.is_some(),
                        "issued API keys awaiting reveal have no wrapped secret; their reveal still reads the plaintext column"
                    );
                }
            }
            Err(e) => {
                crate::background_error!(
                    component::PENDING_SECRETS,
                    "reconcile_failed",
                    Error,
                    error = %e,
                    "failed to reconcile wrapped pending API key secrets"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::handlers::Repository;
    use crate::db::models::api_keys::ApiKeyCreateDBRequest;
    use crate::keystore::test_keystore;
    use crate::test::utils::create_test_user;
    use crate::{api::models::users::Role, types::ApiKeyId};
    use sqlx::PgPool;
    use sqlx_pool_router::TestDbPools;

    fn reveal(pending_secret: Option<Vec<u8>>, secret: &str) -> PendingReveal {
        PendingReveal {
            id: uuid::Uuid::new_v4(),
            pending_secret,
            secret_hash: Some(hash_api_key(secret).to_vec()),
            secret: Some(secret.to_string()),
        }
    }

    #[test]
    fn resolve_prefers_a_wrap_of_the_current_secret() {
        let ks = test_keystore("k1", &[("k1", "wrap-one")]);
        let wrapped = ks.wrap(b"sk-current").unwrap();
        let mut row = reveal(Some(wrapped), "sk-current");
        row.secret = None;
        assert_eq!(
            resolve_reveal(Some(&ks), &row),
            Some(("sk-current".to_string(), RevealSource::Wrapped))
        );
    }

    #[test]
    fn resolve_falls_back_to_plaintext_for_missing_stale_or_unreadable_wraps() {
        let ks = test_keystore("k1", &[("k1", "wrap-one")]);
        let plaintext = Some(("sk-current".to_string(), RevealSource::Plaintext));
        // Never wrapped.
        assert_eq!(resolve_reveal(Some(&ks), &reveal(None, "sk-current")), plaintext);
        // Wrap of a secret since rotated away.
        let stale = reveal(Some(ks.wrap(b"sk-old").unwrap()), "sk-current");
        assert_eq!(resolve_reveal(Some(&ks), &stale), plaintext);
        // Sealed by a wrap key this keystore doesn't hold.
        let other = test_keystore("k9", &[("k9", "wrap-nine")]);
        let foreign = reveal(Some(other.wrap(b"sk-current").unwrap()), "sk-current");
        assert_eq!(resolve_reveal(Some(&ks), &foreign), plaintext);
        // No keystore configured.
        let wrapped = reveal(Some(ks.wrap(b"sk-current").unwrap()), "sk-current");
        assert_eq!(resolve_reveal(None, &wrapped), plaintext);
        // Nothing to reveal without the plaintext column.
        let mut gone = reveal(None, "sk-current");
        gone.secret = None;
        assert_eq!(resolve_reveal(Some(&ks), &gone), None);
    }

    async fn pending_key(pool: &PgPool) -> (ApiKeyId, String) {
        let user = create_test_user(pool, Role::StandardUser).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = ApiKeys::new(&mut conn);
        let request = ApiKeyCreateDBRequest {
            user_id: user.id,
            created_by: user.id,
            name: "issued".to_string(),
            description: None,
            purpose: crate::db::models::api_keys::ApiKeyPurpose::Realtime,
            spend_limit: None,
            spend_limit_interval: None,
        };
        let key = repo.create(&request).await.unwrap();
        // As a pre-wrap release left it: pending, plaintext only.
        sqlx::query!("UPDATE api_keys SET secret_revealed_at = NULL WHERE id = $1", key.id)
            .execute(pool)
            .await
            .unwrap();
        (key.id, key.secret)
    }

    async fn stored_wrap(pool: &PgPool, id: ApiKeyId) -> Option<Vec<u8>> {
        sqlx::query_scalar!("SELECT pending_secret FROM api_keys WHERE id = $1", id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[dwctl_test_macros::test]
    async fn reconcile_wraps_rewraps_and_clears(pool: PgPool) {
        let db = TestDbPools::new(pool.clone()).await.unwrap();
        let (legacy, legacy_secret) = pending_key(&pool).await;
        let (old_wrap, old_wrap_secret) = pending_key(&pool).await;
        let (revealed, _) = pending_key(&pool).await;

        // Sealed with k1, then k2 became current with k1 still held.
        let k1 = test_keystore("k1", &[("k1", "wrap-one")]);
        let ks = test_keystore("k2", &[("k1", "wrap-one"), ("k2", "wrap-two")]);
        sqlx::query!(
            "UPDATE api_keys SET pending_secret = $2 WHERE id = $1",
            old_wrap,
            k1.wrap(old_wrap_secret.as_bytes()).unwrap()
        )
        .execute(&pool)
        .await
        .unwrap();
        // Revealed by a release that didn't clear the wrap.
        sqlx::query!(
            "UPDATE api_keys SET secret_revealed_at = NOW(), pending_secret = '\\x00'::bytea WHERE id = $1",
            revealed
        )
        .execute(&pool)
        .await
        .unwrap();

        // Without a keystore nothing is wrapped; both pending keys still need plaintext.
        let report = reconcile(&db, None).await.unwrap();
        assert_eq!(report.needing_plaintext, 2);
        assert_eq!(report.cleared, 1);
        assert_eq!(stored_wrap(&pool, legacy).await, None);

        let report = reconcile(&db, Some(&ks)).await.unwrap();
        assert_eq!(
            report,
            ReconcileReport {
                wrapped: 1,
                rewrapped: 1,
                cleared: 0,
                needing_plaintext: 0
            }
        );
        let legacy_wrap = stored_wrap(&pool, legacy).await.unwrap();
        assert_eq!(ks.unwrap(&legacy_wrap).unwrap(), legacy_secret.as_bytes());
        let rewrapped = stored_wrap(&pool, old_wrap).await.unwrap();
        assert!(ks.is_current(&rewrapped));
        assert_eq!(ks.unwrap(&rewrapped).unwrap(), old_wrap_secret.as_bytes());
        assert_eq!(stored_wrap(&pool, revealed).await, None);

        // Idempotent.
        assert_eq!(reconcile(&db, Some(&ks)).await.unwrap(), ReconcileReport::default());
    }

    #[dwctl_test_macros::test]
    async fn rotation_between_read_and_wrap_is_not_reported_settled(pool: PgPool) {
        let ks = test_keystore("k1", &[("k1", "wrap-one")]);
        let (id, _) = pending_key(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = ApiKeys::new(&mut conn);
        let rows = repo.list_pending_reveals().await.unwrap();
        assert_eq!(rows.len(), 1);

        // A release that doesn't maintain wraps rotates the key after the read.
        let rotated = "sk-rotated-by-previous-release";
        sqlx::query!(
            "UPDATE api_keys SET secret = $2, secret_hash = $3 WHERE id = $1",
            id,
            rotated,
            &hash_api_key(rotated)[..]
        )
        .execute(&pool)
        .await
        .unwrap();

        // The guarded write misses, so nothing is wrapped and the key still
        // needs plaintext.
        let mut report = ReconcileReport::default();
        wrap_rows(&mut repo, Some(&ks), rows, &mut report).await.unwrap();
        assert_eq!(report.wrapped, 0);
        assert_eq!(stored_wrap(&pool, id).await, None);
        assert_eq!(count_unverified(&mut repo, Some(&ks)).await.unwrap(), 1);

        // The next pass wraps the rotated secret and reports it verified.
        let db = TestDbPools::new(pool.clone()).await.unwrap();
        let report = reconcile(&db, Some(&ks)).await.unwrap();
        assert_eq!((report.wrapped, report.needing_plaintext), (1, 0));
        assert_eq!(ks.unwrap(&stored_wrap(&pool, id).await.unwrap()).unwrap(), rotated.as_bytes());
    }
}
