use crate::types::Operation;
use thiserror::Error;

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

    /// Catch-all for non-recoverable errors
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convert from sqlx::Error using proper sqlx error categorization
impl From<sqlx::Error> for DbError {
    fn from(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::RowNotFound => DbError::NotFound,
            sqlx::Error::Database(db_err) => {
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
        if let Some(start) = detail.find("=(") {
            if let Some(end) = detail[start + 2..].find(')') {
                return Some(detail[start + 2..start + 2 + end].to_string());
            }
        }
    }
    None
}

/// Type alias for database operation results
pub type Result<T> = std::result::Result<T, DbError>;
