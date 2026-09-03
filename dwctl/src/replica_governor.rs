//! Replica-group membership over Postgres, and the connection-budget governor
//! that divides `total_max_connections` across the live members.
//!
//! # Why
//!
//! With a fixed `max_connections` per pod, the replica count was effectively a
//! config constant: the database's connection limit, not CPU or memory, capped
//! how far a Deployment could be scaled. `PoolSettings::total_max_connections`
//! states the budget for a whole replica group instead. Every pod discovers how
//! many peers are alive through the `replica_registry` table and sizes its
//! pools at `total / live_replicas`, re-dividing at runtime as replicas come
//! and go — so an autoscaler can move the replica count freely and the
//! aggregate never exceeds the budget.
//!
//! # How
//!
//! Postgres is the consensus substrate, exactly like the advisory-lock leader
//! election: a registry row is alive iff its heartbeat falls inside
//! [`LIVENESS_WINDOW`], so all members of a group converge on the same count
//! within one [`HEARTBEAT_INTERVAL`]. sqlx pools cannot be resized in place,
//! so applying a new share means build-new / [`DbPools::replace`] / drain-old;
//! every component holds a live provider (`DbPools` / `DynPools`) rather than a
//! pinned `PgPool`, so the swap reaches them on their next query.
//!
//! Fully dormant unless some pool declares a total
//! (`DatabaseConfig::connection_budget_enabled`): no registry rows, no
//! heartbeats, pools sized from `max_connections` as before.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx_pool_router::{DbPools, PoolProvider};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::PoolSettings;

/// How often a member refreshes its registry row and re-counts the group.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// A member whose heartbeat is older than this is dead (3 missed beats).
pub const LIVENESS_WINDOW: Duration = Duration::from_secs(30);
/// How long a new count must hold before pools are rebuilt. A rolling update
/// surges a pod up and back down within about a minute; re-dividing on every
/// blip would churn pools for nothing.
pub const RESIZE_SETTLE: Duration = Duration::from_secs(20);
/// Rows silent for this long are swept (crashed pods never deregister).
const REGISTRY_GC_AFTER: Duration = Duration::from_secs(600);

/// Equal division of `total` across `live_replicas`, floor 1.
pub fn share(total: u32, live_replicas: u32) -> u32 {
    (total / live_replicas.max(1)).max(1)
}

/// The pool size this pod should run: its share of the total when one is
/// declared, otherwise the plain per-pod `max_connections`.
pub fn effective_max(settings: &PoolSettings, live_replicas: u32) -> u32 {
    match settings.total_max_connections {
        Some(total) => share(total, live_replicas),
        None => settings.max_connections,
    }
}

/// `PgPoolOptions` for `settings` at an explicit `max_connections` (the one
/// place the timeout/lifetime knobs are translated). `min_connections` is
/// clamped to the max so a small share cannot ask for more warm connections
/// than it may hold.
pub fn pool_options(settings: &PoolSettings, max_connections: u32) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .min_connections(settings.min_connections.min(max_connections))
        .acquire_timeout(Duration::from_secs(settings.acquire_timeout_secs))
        .idle_timeout((settings.idle_timeout_secs > 0).then(|| Duration::from_secs(settings.idle_timeout_secs)))
        .max_lifetime((settings.max_lifetime_secs > 0).then(|| Duration::from_secs(settings.max_lifetime_secs)))
}

/// Upsert this instance's registry row and return the number of live members
/// of `group`, including itself (never below 1).
pub async fn register_and_count(pool: &PgPool, instance_id: Uuid, group: &str, liveness: Duration) -> Result<u32, sqlx::Error> {
    sqlx::query(
        "INSERT INTO replica_registry (instance_id, replica_group)
         VALUES ($1, $2)
         ON CONFLICT (instance_id) DO UPDATE SET last_heartbeat = now()",
    )
    .bind(instance_id)
    .bind(group)
    .execute(pool)
    .await?;
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM replica_registry
         WHERE replica_group = $1
           AND last_heartbeat > now() - make_interval(secs => $2)",
    )
    .bind(group)
    .bind(liveness.as_secs_f64())
    .fetch_one(pool)
    .await?;
    Ok(u32::try_from(live).unwrap_or(u32::MAX).max(1))
}

/// Remove this instance's registry row (graceful shutdown), so peers re-divide
/// on their next heartbeat instead of waiting out the liveness window.
pub async fn deregister(pool: &PgPool, instance_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM replica_registry WHERE instance_id = $1")
        .bind(instance_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Timing knobs for [`run_membership`]; production uses the defaults, tests
/// shrink them so convergence is observable in milliseconds.
#[derive(Debug, Clone)]
pub struct MembershipConfig {
    pub heartbeat_interval: Duration,
    pub liveness_window: Duration,
}

impl Default for MembershipConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: HEARTBEAT_INTERVAL,
            liveness_window: LIVENESS_WINDOW,
        }
    }
}

/// Heartbeat loop: keeps this instance's row fresh, publishes the live member
/// count of `group` on `count_tx` whenever it changes, and deregisters on
/// shutdown. Runs until `shutdown` fires.
pub async fn run_membership(
    pools: impl PoolProvider,
    instance_id: Uuid,
    group: String,
    count_tx: watch::Sender<u32>,
    cfg: MembershipConfig,
    shutdown: CancellationToken,
) {
    let mut interval = tokio::time::interval(cfg.heartbeat_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                if let Err(e) = deregister(&pools.write(), instance_id).await {
                    warn!(error = %e, "replica_registry deregistration failed; peers will time the row out");
                }
                break;
            }
            _ = interval.tick() => {}
        }
        let pool = pools.write();
        match register_and_count(&pool, instance_id, &group, cfg.liveness_window).await {
            Ok(n) => {
                metrics::gauge!("dwctl_replica_group_size", "group" => group.clone()).set(n as f64);
                count_tx.send_if_modified(|current| {
                    if *current != n {
                        debug!(group, from = *current, to = n, "replica group size changed");
                        *current = n;
                        true
                    } else {
                        false
                    }
                });
            }
            // Keep the last known count on error: shrinking pools because the
            // database blipped would amplify the blip.
            Err(e) => warn!(error = %e, "replica_registry heartbeat failed; keeping last known count"),
        }
        if let Err(e) = sqlx::query("DELETE FROM replica_registry WHERE last_heartbeat < now() - make_interval(secs => $1)")
            .bind(REGISTRY_GC_AFTER.as_secs_f64())
            .execute(&pool)
            .await
        {
            debug!(error = %e, "replica_registry sweep failed");
        }
    }
}

/// Identity this pod joined the group with at boot, and the count it saw.
#[derive(Debug, Clone)]
pub struct GovernanceSeed {
    pub instance_id: Uuid,
    pub group: String,
    pub initial_live: u32,
}

/// One `DbPools` the governor resizes, with everything needed to rebuild it.
pub struct GovernedPool {
    pub name: &'static str,
    pub primary: (PoolSettings, PgConnectOptions),
    pub replica: Option<(PoolSettings, PgConnectOptions)>,
    pub target: DbPools,
}

impl GovernedPool {
    fn current_primary_max(&self) -> u32 {
        self.target.write().options().get_max_connections()
    }

    fn current_replica_max(&self) -> Option<u32> {
        self.target.has_replica().then(|| self.target.read().options().get_max_connections())
    }
}

/// Everything `setup_database` hands to the background services to keep the
/// budget honoured after boot.
pub struct PoolGovernance {
    pub seed: GovernanceSeed,
    pub pools: Vec<GovernedPool>,
}

fn record_share(name: &'static str, max: u32) {
    metrics::gauge!("dwctl_db_pool_share", "pool" => name).set(max as f64);
}

/// Rebuild `gp` at the share for `live` replicas if that differs from what it
/// runs now. The old pools are drained in the background: `close()` shuts idle
/// connections immediately and checked-out ones as they are returned, so
/// in-flight work completes untouched.
fn apply_share(gp: &GovernedPool, live: u32) {
    let new_primary_max = effective_max(&gp.primary.0, live);
    let new_replica_max = gp.replica.as_ref().map(|(settings, _)| effective_max(settings, live));
    let current_primary = gp.current_primary_max();
    if new_primary_max == current_primary && new_replica_max == gp.current_replica_max() {
        return;
    }
    let new_primary = pool_options(&gp.primary.0, new_primary_max).connect_lazy_with(gp.primary.1.clone());
    let new_replica = gp
        .replica
        .as_ref()
        .zip(new_replica_max)
        .map(|((settings, opts), max)| pool_options(settings, max).connect_lazy_with(opts.clone()));
    let (old_primary, old_replica) = gp.target.replace(new_primary, new_replica);
    record_share(gp.name, new_primary_max);
    info!(
        pool = gp.name,
        from = current_primary,
        to = new_primary_max,
        replicas = live,
        "re-divided connection budget"
    );
    tokio::spawn(async move {
        old_primary.close().await;
        if let Some(replica) = old_replica {
            replica.close().await;
        }
    });
}

/// Resize loop: whenever the live count published by [`run_membership`]
/// changes and then holds for `settle`, rebuild every governed pool at its new
/// share. Runs until `shutdown` fires or the count sender is dropped.
pub async fn run_governor(pools: Vec<GovernedPool>, mut count_rx: watch::Receiver<u32>, settle: Duration, shutdown: CancellationToken) {
    for gp in &pools {
        record_share(gp.name, gp.current_primary_max());
    }
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            changed = count_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
        let live = *count_rx.borrow_and_update();
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(settle) => {}
        }
        if *count_rx.borrow() != live {
            // Moved again during the settle window; `changed()` fires next loop.
            continue;
        }
        for gp in &pools {
            apply_share(gp, live);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(total: Option<u32>, max: u32) -> PoolSettings {
        PoolSettings {
            max_connections: max,
            total_max_connections: total,
            min_connections: 0,
            ..PoolSettings::default()
        }
    }

    #[test]
    fn share_divides_equally_with_floor_one() {
        assert_eq!(share(800, 2), 400);
        assert_eq!(share(1000, 3), 333);
        assert_eq!(share(10, 0), 10, "zero live replicas is treated as one");
        assert_eq!(share(1, 4), 1, "never below one connection");
    }

    #[test]
    fn effective_max_prefers_total_over_per_pod() {
        assert_eq!(effective_max(&settings(Some(800), 400), 2), 400);
        assert_eq!(effective_max(&settings(Some(800), 400), 4), 200);
        assert_eq!(effective_max(&settings(None, 400), 4), 400, "no total: per-pod value, replica count irrelevant");
    }

    #[test]
    fn pool_options_clamps_min_to_max() {
        let mut s = settings(Some(100), 100);
        s.min_connections = 50;
        let opts = pool_options(&s, 10);
        assert_eq!(opts.get_max_connections(), 10);
        assert_eq!(opts.get_min_connections(), 10);
    }

    #[sqlx::test]
    async fn register_and_count_sees_only_live_members_of_the_group(pool: PgPool) {
        let liveness = Duration::from_secs(30);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(register_and_count(&pool, a, "api", liveness).await.unwrap(), 1);
        assert_eq!(register_and_count(&pool, b, "api", liveness).await.unwrap(), 2);
        // Other groups do not count.
        let c = Uuid::new_v4();
        assert_eq!(register_and_count(&pool, c, "fusillade-batch", liveness).await.unwrap(), 1);
        // A stale heartbeat drops out of the count.
        sqlx::query("UPDATE replica_registry SET last_heartbeat = now() - interval '1 hour' WHERE instance_id = $1")
            .bind(b)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(register_and_count(&pool, a, "api", liveness).await.unwrap(), 1);
        // Deregistration removes the row.
        deregister(&pool, a).await.unwrap();
        assert_eq!(register_and_count(&pool, c, "api", liveness).await.unwrap(), 1, "only c remains in api");
    }

    #[sqlx::test]
    async fn two_members_converge_on_count_two_and_shrink_on_deregister(pool: PgPool) {
        let cfg = MembershipConfig {
            heartbeat_interval: Duration::from_millis(50),
            liveness_window: Duration::from_secs(5),
        };
        let shutdown_a = CancellationToken::new();
        let shutdown_b = CancellationToken::new();
        let (tx_a, mut rx_a) = watch::channel(1u32);
        let (tx_b, mut rx_b) = watch::channel(1u32);
        let handle_a = tokio::spawn(run_membership(pool.clone(), Uuid::new_v4(), "api".into(), tx_a, cfg.clone(), shutdown_a.clone()));
        let _handle_b = tokio::spawn(run_membership(pool.clone(), Uuid::new_v4(), "api".into(), tx_b, cfg.clone(), shutdown_b.clone()));
        // A member of another group must not be counted.
        let (tx_c, _rx_c) = watch::channel(1u32);
        let shutdown_c = CancellationToken::new();
        let _handle_c = tokio::spawn(run_membership(pool.clone(), Uuid::new_v4(), "batch".into(), tx_c, cfg.clone(), shutdown_c.clone()));

        tokio::time::timeout(Duration::from_secs(5), async {
            while !(*rx_a.borrow() == 2 && *rx_b.borrow() == 2) {
                tokio::select! {
                    r = rx_a.changed() => r.unwrap(),
                    r = rx_b.changed() => r.unwrap(),
                }
            }
        })
        .await
        .expect("both api members should observe count=2");

        // Graceful shutdown of A deregisters it, so B sees 1 well before the
        // liveness window would have expired.
        shutdown_a.cancel();
        handle_a.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while *rx_b.borrow() != 1 {
                rx_b.changed().await.unwrap();
            }
        })
        .await
        .expect("remaining member should observe count=1 after deregistration");
        shutdown_b.cancel();
        shutdown_c.cancel();
    }

    #[sqlx::test]
    async fn governor_rebuilds_pools_at_the_new_share(pool: PgPool) {
        let opts = pool.connect_options().as_ref().clone();
        let s = settings(Some(10), 10);
        let target = DbPools::new(pool_options(&s, 10).connect_lazy_with(opts.clone()));
        let held = target.clone();
        let governed = vec![GovernedPool {
            name: "test",
            primary: (s.clone(), opts),
            replica: None,
            target: target.clone(),
        }];
        let (count_tx, count_rx) = watch::channel(1u32);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(run_governor(governed, count_rx, Duration::from_millis(20), shutdown.clone()));

        let max_of = |p: &DbPools| p.write().options().get_max_connections();
        async fn wait_for(p: &DbPools, want: u32) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while p.write().options().get_max_connections() != want {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("pool never reached max {want}"));
        }

        count_tx.send(2).unwrap();
        wait_for(&held, 5).await;
        assert_eq!(max_of(&held), 5, "clone held elsewhere sees the swap");
        // The swapped-in pool is usable.
        let one: (i32,) = sqlx::query_as("SELECT 1").fetch_one(held.write()).await.unwrap();
        assert_eq!(one.0, 1);

        // A blip that reverts inside the settle window changes nothing.
        count_tx.send(4).unwrap();
        count_tx.send(2).unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(max_of(&held), 5);

        count_tx.send(1).unwrap();
        wait_for(&held, 10).await;

        shutdown.cancel();
        handle.await.unwrap();
    }
}
