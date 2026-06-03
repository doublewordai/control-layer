//! Periodic balance drift check (read-only safety net for `user_balances`).
//!
//! `user_balances` is maintained incrementally by a trigger on
//! `credits_transactions` (migration 102), and the ledger is append-only
//! (migration 050 forbids deletes and amount/transaction_type updates), so the
//! materialized balance should always equal the ledger-derived balance. This
//! service periodically recomputes the derived balance and compares it, emitting
//! the `dwctl_balance_drift_users` gauge and logging loudly if they ever disagree.
//!
//! It is intentionally **read-only** and does not auto-correct: drift would mean a
//! bug to investigate, and blindly overwriting `user_balances` could race the live
//! maintenance trigger and *introduce* drift.

use std::time::Duration;

use metrics::gauge;
use sqlx::PgPool;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::BalanceReconcileConfig;

/// Number of users whose materialized `user_balances.balance` disagrees with the
/// balance derived from `user_balance_checkpoints` + `credits_transactions`.
///
/// This is a single statement, so it runs under one MVCC snapshot: because a
/// ledger insert and its trigger-driven `user_balances` update commit in the same
/// transaction, a concurrent write is either fully visible or not at all to this
/// query — it cannot produce a transient false positive.
async fn count_balance_drift(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "drift_count!"
        FROM (
            SELECT u.id AS user_id,
                   COALESCE(c.balance, 0) + COALESCE(
                       (SELECT SUM(CASE WHEN ct.transaction_type IN ('purchase', 'admin_grant')
                                        THEN ct.amount ELSE -ct.amount END)
                        FROM credits_transactions ct
                        WHERE ct.user_id = u.id AND ct.seq > COALESCE(c.checkpoint_seq, 0)), 0) AS derived
            FROM users u
            LEFT JOIN user_balance_checkpoints c ON c.user_id = u.id
        ) d
        LEFT JOIN user_balances ub ON ub.user_id = d.user_id
        WHERE d.derived IS DISTINCT FROM COALESCE(ub.balance, 0)
        "#
    )
    .fetch_one(pool)
    .await
}

/// Run the periodic drift check until `shutdown` is cancelled.
pub async fn run_balance_reconcile(pool: PgPool, config: BalanceReconcileConfig, shutdown: CancellationToken) {
    if !config.enabled {
        info!("Balance reconcile drift check disabled by configuration");
        return;
    }

    let period = Duration::from_millis(config.interval_milliseconds.max(1));
    info!("Starting balance reconcile drift check (every {:?})", period);

    let mut ticker = interval(period);
    // Don't fire a burst of catch-up ticks after a runtime stall.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("Balance reconcile drift check shutting down");
                break;
            }
            _ = ticker.tick() => {
                match count_balance_drift(&pool).await {
                    Ok(0) => {
                        debug!("Balance reconcile: no drift");
                        gauge!("dwctl_balance_drift_users").set(0.0);
                    }
                    Ok(n) => {
                        warn!(
                            drift_users = n,
                            "Balance reconcile detected user_balances disagreeing with the credits ledger -- investigate balance maintenance"
                        );
                        gauge!("dwctl_balance_drift_users").set(n as f64);
                    }
                    Err(e) => {
                        error!("Balance reconcile drift check failed: {}", e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Role;

    #[sqlx::test]
    async fn test_count_balance_drift_detects_mismatch(pool: sqlx::PgPool) {
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // The trigger maintains user_balances correctly for a real transaction.
        sqlx::query(
            "INSERT INTO credits_transactions (id, user_id, transaction_type, amount, source_id, description) \
             VALUES (gen_random_uuid(), $1, 'purchase', 100, gen_random_uuid()::text, 'p')",
        )
        .bind(user.id)
        .execute(&pool)
        .await
        .unwrap();

        let baseline = count_balance_drift(&pool).await.unwrap();
        assert_eq!(baseline, 0, "a correctly-maintained balance must show no drift");

        // Corrupt the materialized value directly to simulate a maintenance bug.
        sqlx::query("UPDATE user_balances SET balance = balance + 5 WHERE user_id = $1")
            .bind(user.id)
            .execute(&pool)
            .await
            .unwrap();

        let after = count_balance_drift(&pool).await.unwrap();
        assert_eq!(after, baseline + 1, "drift in user_balances must be detected");
    }
}
