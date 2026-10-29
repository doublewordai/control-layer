//! Aggregate-only batch alert queries. Connections use the caller's component schema.
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection};

#[derive(Debug, Clone, FromRow, PartialEq)]
pub struct BatchSlaRow {
    pub policy: String,
    pub completion_window: String,
    pub model: String,
    pub affected_batches: i64,
    pub max_lateness_seconds: f64,
    pub min_time_remaining_seconds: f64,
}

/// Resolve public model names in the main database, separately from Fusillade.
/// A private alias must never become a metric label.
pub async fn public_models(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT names.name FROM deployed_models dm \
         JOIN deployment_groups dg ON dg.deployment_id = dm.id \
         JOIN groups g ON g.id = dg.group_id \
         CROSS JOIN LATERAL (VALUES (dm.alias), (dm.model_name)) names(name) \
         WHERE g.name = 'Everyone' AND dm.deleted = false AND names.name IS NOT NULL",
    )
    .fetch_all(conn)
    .await
}

pub async fn snapshot(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    public_models: &[String],
    excluded_standard_models: &[String],
) -> Result<Vec<BatchSlaRow>, sqlx::Error> {
    sqlx::query_as(include_str!("batch_sla.sql"))
        .bind(now)
        .bind(public_models)
        .bind(excluded_standard_models)
        .fetch_all(conn)
        .await
}
