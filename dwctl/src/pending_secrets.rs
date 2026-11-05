//! Keystore-wrapped secrets of issued API keys awaiting their one-off reveal.
//!
//! When a manager issues a key to another user, the holder may fetch its
//! secret once. Only the key's hash is stored, so that secret is held in
//! `api_keys.pending_secret`, wrapped with the keystore's wrap keyring, and
//! cleared by the reveal. A pending key without a wrap of its current secret
//! cannot be revealed; its issuer can rotate it, which wraps the new secret.
//!
//! [`run`] reconciles the wraps on every instance, at startup and then
//! periodically: it re-wraps values sealed with a non-current wrap key (so an
//! old wrap key can be retired), clears wraps left on keys that can no longer
//! be revealed, and reports pending keys whose reveal cannot succeed in
//! `dwctl_api_key_pending_reveals_unrecoverable`.

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
/// only touches the few keys with a pending reveal, so this is cheap.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Unwrap a pending secret and check it is the key's current secret.
/// `None` if it doesn't unwrap or belongs to a secret since rotated away.
fn unwrap_current(keystore: &Keystore, wrapped: &[u8], secret_hash: &[u8]) -> Option<String> {
    let plaintext = keystore.unwrap(wrapped).ok()?;
    let secret = String::from_utf8(plaintext).ok()?;
    (hash_api_key(&secret)[..] == *secret_hash).then_some(secret)
}

/// The secret a consumed reveal hands to the holder: the wrapped pending
/// secret, when it unwraps to the key's current secret. `None` otherwise.
pub fn resolve_reveal(keystore: Option<&Keystore>, reveal: &PendingReveal) -> Option<String> {
    let (Some(keystore), Some(wrapped), Some(hash)) = (keystore, reveal.pending_secret.as_deref(), reveal.secret_hash.as_deref()) else {
        return None;
    };
    unwrap_current(keystore, wrapped, hash)
}

/// Outcome of one reconcile pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Pending keys re-wrapped from a non-current wrap key.
    pub rewrapped: u64,
    /// Wraps cleared from keys that can no longer be revealed.
    pub cleared: u64,
    /// Pending keys whose reveal cannot succeed, counted by re-reading them
    /// after re-wrapping: no wrap of the current secret, or no keystore.
    pub unrecoverable: u64,
}

/// What a pending key's reveal needs, judged from one read of its row.
enum PendingState {
    /// Wrapped with the current wrap key, and the wrap is of the key's current
    /// secret: the reveal needs nothing else.
    Verified,
    /// A valid wrap sealed with an old wrap key: re-wrap this secret.
    Rewrap { secret: String },
    /// No keystore, or no wrap of the key's current secret.
    Unrecoverable,
}

fn pending_state(keystore: Option<&Keystore>, row: &PendingReveal) -> PendingState {
    let (Some(keystore), Some(secret_hash), Some(wrapped)) = (keystore, row.secret_hash.as_deref(), row.pending_secret.as_deref()) else {
        return PendingState::Unrecoverable;
    };
    match unwrap_current(keystore, wrapped, secret_hash) {
        Some(_) if keystore.is_current(wrapped) => PendingState::Verified,
        Some(secret) => PendingState::Rewrap { secret },
        None => PendingState::Unrecoverable,
    }
}

/// Re-wrap every row sealed with an old wrap key. A guarded write that
/// matches nothing (the key was revealed, deleted or rotated since it was
/// read) is left to the verification pass, which re-reads it.
async fn rewrap_rows(
    repo: &mut ApiKeys<'_>,
    keystore: Option<&Keystore>,
    rows: Vec<PendingReveal>,
    report: &mut ReconcileReport,
) -> anyhow::Result<()> {
    let Some(keystore) = keystore else { return Ok(()) };
    for row in rows {
        let PendingState::Rewrap { secret } = pending_state(Some(keystore), &row) else {
            continue;
        };
        let Some(secret_hash) = row.secret_hash.as_deref() else { continue };
        let wrapped = keystore.wrap(secret.as_bytes())?;
        if repo.set_pending_secret(row.id, &wrapped, secret_hash).await? {
            report.rewrapped += 1;
        }
    }
    Ok(())
}

/// Re-read every pending key and count those whose reveal is not verified to
/// work from its wrap. Only this count feeds the gauge, so a write lost to a
/// concurrent rotation shows up instead of being assumed settled.
async fn count_unrecoverable(repo: &mut ApiKeys<'_>, keystore: Option<&Keystore>) -> anyhow::Result<u64> {
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
    rewrap_rows(&mut repo, keystore, rows, &mut report).await?;
    report.unrecoverable = count_unrecoverable(&mut repo, keystore).await?;
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
                gauge!("dwctl_api_key_pending_reveals_unrecoverable").set(report.unrecoverable as f64);
                if report.rewrapped + report.cleared > 0 {
                    info!(
                        rewrapped = report.rewrapped,
                        cleared = report.cleared,
                        "reconciled wrapped pending API key secrets"
                    );
                }
                if report.unrecoverable > 0 {
                    crate::background_error!(
                        component::PENDING_SECRETS,
                        "unrecoverable",
                        Warning,
                        count = report.unrecoverable,
                        keystore_configured = keystore.is_some(),
                        "issued API keys awaiting reveal have no usable wrapped secret; their reveal fails until the key is rotated"
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
        }
    }

    #[test]
    fn resolve_returns_a_wrap_of_the_current_secret() {
        let ks = test_keystore("k1", &[("k1", "wrap-one")]);
        let wrapped = ks.wrap(b"sk-current").unwrap();
        assert_eq!(
            resolve_reveal(Some(&ks), &reveal(Some(wrapped), "sk-current")),
            Some("sk-current".to_string())
        );
    }

    #[test]
    fn resolve_fails_for_missing_stale_or_unreadable_wraps() {
        let ks = test_keystore("k1", &[("k1", "wrap-one")]);
        // Never wrapped.
        assert_eq!(resolve_reveal(Some(&ks), &reveal(None, "sk-current")), None);
        // Wrap of a secret since rotated away.
        let stale = reveal(Some(ks.wrap(b"sk-old").unwrap()), "sk-current");
        assert_eq!(resolve_reveal(Some(&ks), &stale), None);
        // Sealed by a wrap key this keystore doesn't hold.
        let other = test_keystore("k9", &[("k9", "wrap-nine")]);
        let foreign = reveal(Some(other.wrap(b"sk-current").unwrap()), "sk-current");
        assert_eq!(resolve_reveal(Some(&ks), &foreign), None);
        // No keystore configured.
        let wrapped = reveal(Some(ks.wrap(b"sk-current").unwrap()), "sk-current");
        assert_eq!(resolve_reveal(None, &wrapped), None);
    }

    /// A key awaiting its reveal, with no wrap.
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
        sqlx::query!("UPDATE api_keys SET secret_revealed_at = NULL WHERE id = $1", key.id)
            .execute(pool)
            .await
            .unwrap();
        (key.id, key.secret.unwrap())
    }

    async fn stored_wrap(pool: &PgPool, id: ApiKeyId) -> Option<Vec<u8>> {
        sqlx::query_scalar!("SELECT pending_secret FROM api_keys WHERE id = $1", id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn set_wrap(pool: &PgPool, id: ApiKeyId, wrapped: Vec<u8>) {
        sqlx::query!("UPDATE api_keys SET pending_secret = $2 WHERE id = $1", id, wrapped)
            .execute(pool)
            .await
            .unwrap();
    }

    #[dwctl_test_macros::test]
    async fn reconcile_rewraps_clears_and_reports_unrecoverable(pool: PgPool) {
        let db = TestDbPools::new(pool.clone()).await.unwrap();
        let (unwrapped, _) = pending_key(&pool).await;
        let (old_wrap, old_wrap_secret) = pending_key(&pool).await;
        let (current_wrap, current_wrap_secret) = pending_key(&pool).await;
        let (revealed, _) = pending_key(&pool).await;

        // Sealed with k1, then k2 became current with k1 still held.
        let k1 = test_keystore("k1", &[("k1", "wrap-one")]);
        let ks = test_keystore("k2", &[("k1", "wrap-one"), ("k2", "wrap-two")]);
        set_wrap(&pool, old_wrap, k1.wrap(old_wrap_secret.as_bytes()).unwrap()).await;
        set_wrap(&pool, current_wrap, ks.wrap(current_wrap_secret.as_bytes()).unwrap()).await;
        // Revealed by a release that didn't clear the wrap.
        sqlx::query!(
            "UPDATE api_keys SET secret_revealed_at = NOW(), pending_secret = '\\x00'::bytea WHERE id = $1",
            revealed
        )
        .execute(&pool)
        .await
        .unwrap();

        // Without a keystore no pending key can be revealed.
        let report = reconcile(&db, None).await.unwrap();
        assert_eq!(
            report,
            ReconcileReport {
                rewrapped: 0,
                cleared: 1,
                unrecoverable: 3
            }
        );
        assert_eq!(stored_wrap(&pool, revealed).await, None);

        // The key with no wrap stays unrecoverable: nothing holds its secret.
        let report = reconcile(&db, Some(&ks)).await.unwrap();
        assert_eq!(
            report,
            ReconcileReport {
                rewrapped: 1,
                cleared: 0,
                unrecoverable: 1
            }
        );
        assert_eq!(stored_wrap(&pool, unwrapped).await, None);
        let rewrapped = stored_wrap(&pool, old_wrap).await.unwrap();
        assert!(ks.is_current(&rewrapped));
        assert_eq!(ks.unwrap(&rewrapped).unwrap(), old_wrap_secret.as_bytes());

        // Idempotent.
        assert_eq!(
            reconcile(&db, Some(&ks)).await.unwrap(),
            ReconcileReport {
                unrecoverable: 1,
                ..Default::default()
            }
        );
    }

    #[dwctl_test_macros::test]
    async fn rotation_between_read_and_rewrap_is_not_reported_settled(pool: PgPool) {
        let k1 = test_keystore("k1", &[("k1", "wrap-one")]);
        let ks = test_keystore("k2", &[("k1", "wrap-one"), ("k2", "wrap-two")]);
        let (id, secret) = pending_key(&pool).await;
        set_wrap(&pool, id, k1.wrap(secret.as_bytes()).unwrap()).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = ApiKeys::new(&mut conn);
        let rows = repo.list_pending_reveals().await.unwrap();
        assert_eq!(rows.len(), 1);

        // A release that doesn't maintain wraps rotates the key after the read.
        sqlx::query!(
            "UPDATE api_keys SET secret_hash = $2 WHERE id = $1",
            id,
            &hash_api_key("sk-rotated-by-previous-release")[..]
        )
        .execute(&pool)
        .await
        .unwrap();

        // The guarded write misses, so the stale wrap is kept and the key is
        // reported unrecoverable.
        let mut report = ReconcileReport::default();
        rewrap_rows(&mut repo, Some(&ks), rows, &mut report).await.unwrap();
        assert_eq!(report.rewrapped, 0);
        assert!(!ks.is_current(&stored_wrap(&pool, id).await.unwrap()));
        assert_eq!(count_unrecoverable(&mut repo, Some(&ks)).await.unwrap(), 1);
    }
}
