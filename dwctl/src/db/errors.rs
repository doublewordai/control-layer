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
                    DbError::UniqueViolation {
                        constraint: db_err.constraint().map(|s| s.to_string()),
                        table: db_err.table().map(|s| s.to_string()),
                        message: db_err.message().to_string(),
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

/// Type alias for database operation results
pub type Result<T> = std::result::Result<T, DbError>;
