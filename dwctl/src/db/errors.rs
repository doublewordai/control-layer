use crate::types::Operation;
use metrics::{counter, describe_counter};
use thiserror::Error;

/// Register a zero sample before the first error so rate-based alerts can see it.
pub(crate) fn initialize_database_error_metrics() {
    describe_counter!(
        "dwctl_db_cached_plan_errors_total",
        "Database operations rejected because a cached prepared statement changed result type"
    );
    counter!("dwctl_db_cached_plan_errors_total").increment(0);
}

/// Unified error type for database operations that application code can handle
#[derive(Error, Debug)]
pub enum DbError {
    /// Entity not found by the given identifier
    #[error("Entity not found")]
    NotFound,

    /// Unique constraint violation
    #[error("Unique constraint violation")]
    UniqueViolation {
        constraint: Option<String>,
        table: Option<String>,
        message: String,
        /// The conflicting value that caused the violation (if extractable)
        conflicting_value: Option<String>,
    },

    /// Foreign key constraint violation
    #[error("Foreign key constraint violation")]
    ForeignKeyViolation {
        constraint: Option<String>,
        table: Option<String>,
        message: String,
    },

    /// Check constraint violation
    #[error("Check constraint violation")]
    CheckViolation {
        constraint: Option<String>,
        table: Option<String>,
        message: String,
    },

    /// Entity cannot be modified or deleted due to protection rules
    /// NOTE: use this only for DB-level protection rules, not user roles etc. - that's handled at
    /// the API layer.
    #[error("{operation:?} cannot be applied to entity of type {entity_type}: {reason}")]
    ProtectedEntity {
        operation: Operation,      // "deleted", "updated", "modified"
        reason: String,            // "system entity", "has active dependencies", etc.
        entity_type: String,       // "user", "role", "configuration", etc.
        entity_id: Option<String>, // ID for logging/debugging
    },

    /// Invalid or empty model name/alias field
    #[error("Invalid model field: {field} must not be empty or whitespace")]
    InvalidModelField { field: &'static str },

    /// Connection pool exhausted - all connections are in use and acquire timed out
    #[error("Database connection pool exhausted")]
    PoolExhausted,

    /// Catch-all for non-recoverable errors
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convert from sqlx::Error using proper sqlx error categorization
impl From<sqlx::Error> for DbError {
    fn from(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::RowNotFound => DbError::NotFound,
            sqlx::Error::PoolTimedOut => {
                // Record metric for pool exhaustion - this is a key indicator of capacity issues
                counter!("dwctl_db_pool_acquire_timeouts_total").increment(1);
                DbError::PoolExhausted
            }
            sqlx::Error::Database(db_err) => {
                // SQLSTATE 0A000 covers other unsupported features too. Only count
                // result-descriptor invalidation, preserving the original error.
                // Do not replay the operation: it may be a write or an aborted transaction.
                if db_err.code().as_deref() == Some("0A000") && db_err.message().contains("cached plan must not change result type") {
                    counter!("dwctl_db_cached_plan_errors_total").increment(1);
                }
                if db_err.is_unique_violation() {
                    let constraint = db_err.constraint().map(|s| s.to_string());

                    // Extract conflicting value only for alias conflicts
                    let conflicting_value = if let Some(pg_err) = db_err.try_downcast_ref::<sqlx::postgres::PgDatabaseError>() {
                        if let Some(detail_msg) = pg_err.detail() {
                            extract_conflicting_alias(detail_msg, constraint.as_deref())
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    DbError::UniqueViolation {
                        constraint,
                        table: db_err.table().map(|s| s.to_string()),
                        message: db_err.message().to_string(),
                        conflicting_value,
                    }
                } else if db_err.is_foreign_key_violation() {
                    DbError::ForeignKeyViolation {
                        constraint: db_err.constraint().map(|s| s.to_string()),
                        table: db_err.table().map(|s| s.to_string()),
                        message: db_err.message().to_string(),
                    }
                } else if db_err.is_check_violation() {
                    DbError::CheckViolation {
                        constraint: db_err.constraint().map(|s| s.to_string()),
                        table: db_err.table().map(|s| s.to_string()),
                        message: db_err.message().to_string(),
                    }
                } else {
                    // All other database errors are non-recoverable - convert to anyhow
                    DbError::Other(anyhow::Error::from(err))
                }
            }
            // All other sqlx errors are non-recoverable - convert to anyhow with context
            _ => DbError::Other(anyhow::Error::from(err)),
        }
    }
}

/// Extract the conflicting alias from PostgreSQL error detail message
/// Only extracts for deployment alias constraints to avoid affecting other flows
fn extract_conflicting_alias(detail: &str, constraint: Option<&str>) -> Option<String> {
    // Only extract for deployment alias unique constraint
    if constraint == Some("deployed_models_alias_unique") {
        // PostgreSQL unique violation details typically look like:
        // "Key (alias)=(my-alias) already exists."
        if let Some(start) = detail.find("=(")
            && let Some(end) = detail[start + 2..].find(')')
        {
            return Some(detail[start + 2..start + 2 + end].to_string());
        }
    }
    None
}

/// Type alias for database operation results
pub type Result<T> = std::result::Result<T, DbError>;

#[cfg(test)]
mod tests {
    use super::{DbError, initialize_database_error_metrics};
    use metrics::with_local_recorder;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn cached_result_shape_failure_is_counted_without_retry(pool: PgPool) {
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query("CREATE TABLE cached_shape (id integer)")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("SELECT * FROM cached_shape").fetch_all(&mut *connection).await.unwrap();
        sqlx::query("ALTER TABLE cached_shape ADD COLUMN added text")
            .execute(&pool)
            .await
            .unwrap();
        let error = sqlx::query("SELECT * FROM cached_shape")
            .fetch_all(&mut *connection)
            .await
            .unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("0A000"));
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        with_local_recorder(&recorder, || {
            initialize_database_error_metrics();
            assert!(handle.render().contains("dwctl_db_cached_plan_errors_total 0"));
            assert!(matches!(DbError::from(error), DbError::Other(_)));
        });
        assert!(handle.render().contains("dwctl_db_cached_plan_errors_total 1"));
        // SQLx checks its existing statement cache even when persistent(false)
        // is requested. This alone is not a safe stale-plan recovery mechanism.
        let repeated = sqlx::query("SELECT * FROM cached_shape")
            .persistent(false)
            .fetch_all(&mut *connection)
            .await
            .unwrap_err();
        assert!(repeated.to_string().contains("cached plan must not change result type"));
    }

    #[sqlx::test]
    async fn unrelated_unsupported_feature_does_not_count_as_cached_plan(pool: PgPool) {
        let error = sqlx::query("SELECT count(*) FROM pg_class FOR UPDATE")
            .fetch_all(&pool)
            .await
            .unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("0A000"));
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        with_local_recorder(&recorder, || {
            initialize_database_error_metrics();
            assert!(matches!(DbError::from(error), DbError::Other(_)));
        });
        assert!(handle.render().contains("dwctl_db_cached_plan_errors_total 0"));
    }
}
