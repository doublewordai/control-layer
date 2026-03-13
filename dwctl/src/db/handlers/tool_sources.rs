//! Database repository for tool sources and their junction table relationships.

use crate::db::{
    errors::{DbError, Result},
    models::tool_sources::{ToolSourceCreateDBRequest, ToolSourceDBResponse, ToolSourceUpdateDBRequest},
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgConnection};
use tracing::instrument;
use uuid::Uuid;

/// Internal row struct that maps exactly to the `tool_sources` table.
#[derive(Debug, Clone, FromRow)]
struct ToolSourceRow {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
    pub url: String,
    pub api_key: Option<String>,
    pub timeout_secs: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<ToolSourceRow> for ToolSourceDBResponse {
    fn from(row: ToolSourceRow) -> Self {
        Self {
            id: row.id,
            kind: row.kind,
            name: row.name,
            description: row.description,
            parameters: row.parameters,
            url: row.url,
            api_key: row.api_key,
            timeout_secs: row.timeout_secs,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

pub struct ToolSources<'c> {
    db: &'c mut PgConnection,
}

impl<'c> ToolSources<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    #[instrument(skip(self, request), fields(name = %request.name), err)]
    pub async fn create(&mut self, request: &ToolSourceCreateDBRequest) -> Result<ToolSourceDBResponse> {
        let row = sqlx::query_as!(
            ToolSourceRow,
            r#"
            INSERT INTO tool_sources (kind, name, description, parameters, url, api_key, timeout_secs)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING *
            "#,
            request.kind,
            request.name,
            request.description,
            request.parameters,
            request.url,
            request.api_key,
            request.timeout_secs,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(ToolSourceDBResponse::from(row))
    }

    #[instrument(skip(self), fields(id = %id), err)]
    pub async fn get_by_id(&mut self, id: Uuid) -> Result<Option<ToolSourceDBResponse>> {
        let row = sqlx::query_as!(ToolSourceRow, "SELECT * FROM tool_sources WHERE id = $1", id)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(row.map(ToolSourceDBResponse::from))
    }

    #[instrument(skip(self), err)]
    pub async fn list(&mut self) -> Result<Vec<ToolSourceDBResponse>> {
        let rows = sqlx::query_as!(ToolSourceRow, "SELECT * FROM tool_sources ORDER BY name")
            .fetch_all(&mut *self.db)
            .await?;

        Ok(rows.into_iter().map(ToolSourceDBResponse::from).collect())
    }

    #[instrument(skip(self, request), fields(id = %id), err)]
    pub async fn update(&mut self, id: Uuid, request: &ToolSourceUpdateDBRequest) -> Result<ToolSourceDBResponse> {
        let row = sqlx::query_as!(
            ToolSourceRow,
            r#"
            UPDATE tool_sources SET
                name          = COALESCE($2, name),
                description   = CASE WHEN $3 THEN $4 ELSE description END,
                parameters    = CASE WHEN $5 THEN $6 ELSE parameters END,
                url           = COALESCE($7, url),
                api_key       = CASE WHEN $8 THEN $9 ELSE api_key END,
                timeout_secs  = COALESCE($10, timeout_secs)
            WHERE id = $1
            RETURNING *
            "#,
            id,
            request.name,
            request.description.is_some(),
            request.description.as_ref().and_then(|d| d.as_deref()).map(|s| s.to_string()),
            request.parameters.is_some(),
            request.parameters.as_ref().and_then(|p| p.as_ref()),
            request.url,
            request.api_key.is_some(),
            request.api_key.as_ref().and_then(|k| k.as_deref()).map(|s| s.to_string()),
            request.timeout_secs,
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or_else(|| DbError::NotFound)?;

        Ok(ToolSourceDBResponse::from(row))
    }

    #[instrument(skip(self), fields(id = %id), err)]
    pub async fn delete(&mut self, id: Uuid) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM tool_sources WHERE id = $1", id)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    // --- Deployment attachments ---

    #[instrument(skip(self), fields(deployment_id = %deployment_id), err)]
    pub async fn list_for_deployment(&mut self, deployment_id: Uuid) -> Result<Vec<ToolSourceDBResponse>> {
        let rows = sqlx::query_as!(
            ToolSourceRow,
            r#"
            SELECT ts.*
            FROM tool_sources ts
            INNER JOIN deployment_tool_sources dts ON dts.tool_source_id = ts.id
            WHERE dts.deployment_id = $1
            ORDER BY ts.name
            "#,
            deployment_id,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(ToolSourceDBResponse::from).collect())
    }

    #[instrument(skip(self), fields(deployment_id = %deployment_id, tool_source_id = %tool_source_id), err)]
    pub async fn attach_to_deployment(&mut self, deployment_id: Uuid, tool_source_id: Uuid) -> Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO deployment_tool_sources (deployment_id, tool_source_id)
            VALUES ($1, $2)
            ON CONFLICT DO NOTHING
            "#,
            deployment_id,
            tool_source_id,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }

    #[instrument(skip(self), fields(deployment_id = %deployment_id, tool_source_id = %tool_source_id), err)]
    pub async fn detach_from_deployment(&mut self, deployment_id: Uuid, tool_source_id: Uuid) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM deployment_tool_sources WHERE deployment_id = $1 AND tool_source_id = $2",
            deployment_id,
            tool_source_id,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    // --- Group attachments ---

    #[instrument(skip(self), fields(group_id = %group_id), err)]
    pub async fn list_for_group(&mut self, group_id: Uuid) -> Result<Vec<ToolSourceDBResponse>> {
        let rows = sqlx::query_as!(
            ToolSourceRow,
            r#"
            SELECT ts.*
            FROM tool_sources ts
            INNER JOIN group_tool_sources gts ON gts.tool_source_id = ts.id
            WHERE gts.group_id = $1
            ORDER BY ts.name
            "#,
            group_id,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(ToolSourceDBResponse::from).collect())
    }

    #[instrument(skip(self), fields(group_id = %group_id, tool_source_id = %tool_source_id), err)]
    pub async fn attach_to_group(&mut self, group_id: Uuid, tool_source_id: Uuid) -> Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO group_tool_sources (group_id, tool_source_id)
            VALUES ($1, $2)
            ON CONFLICT DO NOTHING
            "#,
            group_id,
            tool_source_id,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }

    #[instrument(skip(self), fields(group_id = %group_id, tool_source_id = %tool_source_id), err)]
    pub async fn detach_from_group(&mut self, group_id: Uuid, tool_source_id: Uuid) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM group_tool_sources WHERE group_id = $1 AND tool_source_id = $2",
            group_id,
            tool_source_id,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Resolve the effective tool set for a request: intersection of deployment tools and group tools.
    ///
    /// Returns tool sources that appear in BOTH the deployment's tool set AND the group's tool set.
    /// If the deployment has no tools attached the result is empty (no tools authorised).
    #[instrument(skip(self), fields(deployment_id = %deployment_id, group_id = %group_id), err)]
    pub async fn resolve_effective_tools(
        &mut self,
        deployment_id: Uuid,
        group_id: Uuid,
    ) -> Result<Vec<ToolSourceDBResponse>> {
        let rows = sqlx::query_as!(
            ToolSourceRow,
            r#"
            SELECT ts.*
            FROM tool_sources ts
            INNER JOIN deployment_tool_sources dts ON dts.tool_source_id = ts.id
            INNER JOIN group_tool_sources gts ON gts.tool_source_id = ts.id
            WHERE dts.deployment_id = $1
              AND gts.group_id = $2
            ORDER BY ts.name
            "#,
            deployment_id,
            group_id,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(ToolSourceDBResponse::from).collect())
    }

    /// Record a tool_call_analytics row.
    #[instrument(skip(self), err)]
    pub async fn record_tool_call(
        &mut self,
        analytics_id: Option<i64>,
        tool_source_id: Option<Uuid>,
        tool_name: &str,
        started_at: chrono::DateTime<Utc>,
        duration_ms: i64,
        http_status_code: Option<i32>,
        success: bool,
        error_kind: Option<&str>,
    ) -> Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO tool_call_analytics
                (analytics_id, tool_source_id, tool_name, started_at, duration_ms,
                 http_status_code, success, error_kind)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
            analytics_id,
            tool_source_id,
            tool_name,
            started_at,
            duration_ms,
            http_status_code,
            success,
            error_kind,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }
}
