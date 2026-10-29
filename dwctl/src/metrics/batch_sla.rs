//! Aggregate batch health. A scrape never executes SQL or accepts query parameters.
//! The complete snapshot is swapped atomically, including its freshness state.
use std::{fmt::Write, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, PgConnection};
use sqlx_pool_router::{DbPools, PoolProvider};
use tokio_util::sync::CancellationToken;

use crate::db::handlers::batch_sla::{self, BatchSlaRow};
use crate::metrics::errors::component;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BatchSlaMetricsConfig {
    /// Opt in after validating query cost and the metrics-only alert rules.
    pub enabled: bool,
    #[serde(with = "humantime_serde")]
    pub sample_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub query_timeout: Duration,
    /// Deployment policy, never published in metric labels. Mixed batches remain eligible.
    pub excluded_standard_models: Vec<String>,
}

impl Default for BatchSlaMetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_interval: Duration::from_secs(30),
            query_timeout: Duration::from_secs(10),
            excluded_standard_models: Vec::new(),
        }
    }
}

impl BatchSlaMetricsConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.sample_interval < Duration::from_secs(5) || self.query_timeout.is_zero() || self.query_timeout >= self.sample_interval {
            return Err("batch_sla_metrics requires sample_interval >= 5s and 0 < query_timeout < sample_interval".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub enabled: bool,
    pub active: bool,
    pub success: bool,
    pub last_success: i64,
    pub rows: Vec<BatchSlaRow>,
}

impl Snapshot {
    fn failed(&self) -> Self {
        // Keep old values explicitly stale, rather than reporting a healthy zero.
        Self {
            active: false,
            success: false,
            ..self.clone()
        }
    }

    pub fn render(&self) -> String {
        if !self.enabled {
            return String::new();
        }
        let mut output = String::new();
        for (name, value) in [
            ("dwctl_batch_sla_collector_active", i64::from(self.active)),
            ("dwctl_batch_sla_collection_success", i64::from(self.success)),
            ("dwctl_batch_sla_last_success_timestamp_seconds", self.last_success),
        ] {
            writeln!(output, "# TYPE {name} gauge\n{name} {value}").unwrap();
        }
        for name in ["affected_batches", "max_lateness_seconds", "min_time_remaining_seconds"] {
            writeln!(output, "# TYPE dwctl_batch_sla_{name} gauge").unwrap();
        }
        // Empty policies render zeros; consumers must check health and freshness.
        // Replacement drops old model series.
        for (policy, window) in [("flex", "1h"), ("standard", "24h"), ("near", "_all")] {
            let rows: Vec<_> = self.rows.iter().filter(|row| row.policy == policy).collect();
            if rows.is_empty() {
                render_row(
                    &mut output,
                    &BatchSlaRow {
                        policy: policy.into(),
                        completion_window: window.into(),
                        model: "_none".into(),
                        affected_batches: 0,
                        max_lateness_seconds: 0.0,
                        min_time_remaining_seconds: 0.0,
                    },
                );
            } else {
                for row in rows {
                    render_row(&mut output, row);
                }
            }
        }
        output
    }
}

fn label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n").replace('"', "\\\"")
}

fn render_row(output: &mut String, row: &BatchSlaRow) {
    for (name, value) in [
        ("affected_batches", row.affected_batches as f64),
        ("max_lateness_seconds", row.max_lateness_seconds),
        ("min_time_remaining_seconds", row.min_time_remaining_seconds),
    ] {
        writeln!(
            output,
            "dwctl_batch_sla_{name}{{policy=\"{}\",completion_window=\"{}\",model=\"{}\"}} {value}",
            label(&row.policy),
            label(&row.completion_window),
            label(&row.model)
        )
        .unwrap();
    }
}

/// The request manager's transaction helper preserves shared/scoped and
/// transaction-pooled schema routing. Main and Fusillade may be different DBs.
async fn collect<M: PoolProvider, F: PoolProvider + Clone>(
    main: &M,
    manager: &fusillade_arsenal::PostgresRequestManager<F>,
    config: &BatchSlaMetricsConfig,
) -> anyhow::Result<Snapshot> {
    let timeout_ms = config.query_timeout.as_millis().max(1).to_string();
    let mut catalog = main.write().begin().await?;
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(&timeout_ms)
        .execute(&mut *catalog)
        .await?;
    let public_models = batch_sla::public_models(&mut catalog).await?;
    catalog.commit().await?;

    let mut tx = manager.begin_write().await?;
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(&timeout_ms)
        .execute(&mut *tx)
        .await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()").fetch_one(&mut *tx).await?;
    let rows = batch_sla::snapshot(&mut tx, now, &public_models, &config.excluded_standard_models).await?;
    tx.commit().await?;
    Ok(Snapshot {
        enabled: true,
        active: true,
        success: true,
        last_success: now.timestamp(),
        rows,
    })
}

/// Keep one physical direct connection for a schema-specific advisory lock.
/// PgConnection (not a pooled lease) closes on drop, including cancellation.
async fn claim_leadership(connection: &mut Option<PgConnection>, direct: &DbPools) -> anyhow::Result<bool> {
    if let Some(connection) = connection.as_mut() {
        connection.ping().await?;
        return Ok(true);
    }
    let mut candidate = PgConnection::connect_with(direct.write().connect_options().as_ref()).await?;
    let claimed: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(1146570316, hashtext(current_schema()))")
        .fetch_one(&mut candidate)
        .await?;
    if claimed {
        *connection = Some(candidate);
    }
    Ok(claimed)
}

pub async fn run(
    main: DbPools,
    manager: Arc<fusillade_arsenal::PostgresRequestManager<DbPools>>,
    direct: DbPools,
    config: BatchSlaMetricsConfig,
    published: Arc<ArcSwap<Snapshot>>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    config.validate().map_err(anyhow::Error::msg)?;
    published.store(Arc::new(Snapshot {
        enabled: true,
        ..Default::default()
    }));
    let mut connection = None;
    let mut interval = tokio::time::interval(config.sample_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
        }
        let attempt = async {
            if !claim_leadership(&mut connection, &direct).await? {
                return Ok(Snapshot {
                    enabled: true,
                    ..Default::default()
                });
            }
            let snapshot = collect(&main, &manager, &config).await?;
            // Do not publish if the lock-holding connection died during the query.
            connection.as_mut().expect("leader connection").ping().await?;
            Ok::<_, anyhow::Error>(snapshot)
        };
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            result = tokio::time::timeout(config.query_timeout, attempt) => result,
        };
        match result {
            Ok(Ok(snapshot)) => published.store(Arc::new(snapshot)),
            _ => {
                connection = None;
                published.store(Arc::new(published.load().failed()));
                // Never include database errors, queries or customer values in logs.
                crate::background_error!(
                    component::BATCH_SLA_METRICS,
                    "collection_failed",
                    Warning,
                    "Batch SLA metrics collection failed or timed out"
                );
            }
        }
    }
    drop(connection);
    published.store(Arc::new(published.load().failed()));
    Ok(())
}

#[cfg(test)]
mod tests;
