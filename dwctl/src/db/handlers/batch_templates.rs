//! Read access to stored batch request templates.

use std::collections::HashMap;
use std::pin::Pin;

use futures::{Stream, TryStreamExt};
use sqlx::{FromRow, PgConnection, Row};
use uuid::Uuid;

use crate::db::errors::{DbError, Result};

/// Fields needed to revalidate reasoning controls before batch creation.
#[derive(Debug, FromRow)]
pub struct BatchTemplateReasoningRequest {
    pub custom_id: Option<String>,
    pub path: String,
    pub body: String,
    pub model: String,
    pub line_number: i32,
}

pub struct BatchTemplates<'c> {
    db: &'c mut PgConnection,
}

impl<'c> BatchTemplates<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    pub async fn get_model_counts(&mut self, file_id: Uuid) -> Result<HashMap<String, i64>> {
        let rows = sqlx::query(
            r#"
            SELECT model, COUNT(*) AS request_count
            FROM request_templates
            WHERE file_id = $1
            GROUP BY model
            "#,
        )
        .bind(file_id)
        .fetch_all(&mut *self.db)
        .await?;

        rows.into_iter()
            .map(|row| Ok((row.try_get("model")?, row.try_get("request_count")?)))
            .collect::<std::result::Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// Stream templates in their original JSONL order from the primary pool.
    pub fn stream_reasoning_requests<'a>(
        &'a mut self,
        file_id: Uuid,
    ) -> Pin<Box<dyn Stream<Item = Result<BatchTemplateReasoningRequest>> + Send + 'a>> {
        Box::pin(
            sqlx::query_as::<_, BatchTemplateReasoningRequest>(
                r#"
                SELECT custom_id, path, body, model, line_number
                FROM request_templates
                WHERE file_id = $1
                ORDER BY line_number ASC, id ASC
                "#,
            )
            .bind(file_id)
            .fetch(&mut *self.db)
            .map_err(DbError::from),
        )
    }
}
