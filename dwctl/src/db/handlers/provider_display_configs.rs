use crate::db::{
    errors::{DbError, Result},
    models::provider_display_configs::{
        KnownProviderDBResponse, ProviderDisplayConfigCreateDBRequest, ProviderDisplayConfigDBResponse,
        ProviderDisplayConfigUpdateDBRequest,
    },
};
use crate::types::UserId;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection};
use tracing::instrument;

#[derive(Debug, Clone, FromRow)]
struct ProviderDisplayConfig {
    pub provider_key: String,
    pub display_name: String,
    pub icon: Option<String>,
    pub created_by: crate::types::UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<ProviderDisplayConfig> for ProviderDisplayConfigDBResponse {
    fn from(value: ProviderDisplayConfig) -> Self {
        Self {
            provider_key: value.provider_key,
            display_name: value.display_name,
            icon: value.icon,
            created_by: value.created_by,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

pub struct ProviderDisplayConfigs<'c> {
    db: &'c mut PgConnection,
}

impl<'c> ProviderDisplayConfigs<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    #[instrument(skip(self, request), fields(provider_key = %request.provider_key), err)]
    pub async fn create(&mut self, request: &ProviderDisplayConfigCreateDBRequest) -> Result<ProviderDisplayConfigDBResponse> {
        let row = sqlx::query_as::<_, ProviderDisplayConfig>(
            r#"
            INSERT INTO provider_display_configs (provider_key, display_name, icon, created_by)
            VALUES ($1, $2, $3, $4)
            RETURNING provider_key, display_name, icon, created_by, created_at, updated_at
            "#,
        )
        .bind(&request.provider_key)
        .bind(&request.display_name)
        .bind(&request.icon)
        .bind(request.created_by)
        .fetch_one(&mut *self.db)
        .await?;

        Ok(row.into())
    }

    #[instrument(skip(self), fields(provider_key = %provider_key), err)]
    pub async fn get_by_key(&mut self, provider_key: &str) -> Result<Option<ProviderDisplayConfigDBResponse>> {
        let row = sqlx::query_as::<_, ProviderDisplayConfig>(
            r#"
            SELECT provider_key, display_name, icon, created_by, created_at, updated_at
            FROM provider_display_configs
            WHERE provider_key = $1
            "#,
        )
        .bind(provider_key)
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row.map(Into::into))
    }

    #[instrument(skip(self), err)]
    pub async fn list(&mut self) -> Result<Vec<ProviderDisplayConfigDBResponse>> {
        let rows = sqlx::query_as::<_, ProviderDisplayConfig>(
            r#"
            SELECT provider_key, display_name, icon, created_by, created_at, updated_at
            FROM provider_display_configs
            ORDER BY display_name
            "#,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    #[instrument(skip(self, request), fields(provider_key = %provider_key), err)]
    pub async fn update(
        &mut self,
        provider_key: &str,
        request: &ProviderDisplayConfigUpdateDBRequest,
    ) -> Result<ProviderDisplayConfigDBResponse> {
        let row = sqlx::query_as::<_, ProviderDisplayConfig>(
            r#"
            UPDATE provider_display_configs
            SET
                display_name = COALESCE($2, display_name),
                icon = CASE WHEN $3 THEN $4 ELSE icon END,
                updated_at = NOW()
            WHERE provider_key = $1
            RETURNING provider_key, display_name, icon, created_by, created_at, updated_at
            "#,
        )
        .bind(provider_key)
        .bind(&request.display_name)
        .bind(request.icon.is_some())
        .bind(request.icon.clone().flatten())
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or(DbError::NotFound)?;

        Ok(row.into())
    }

    #[instrument(skip(self), fields(provider_key = %provider_key), err)]
    pub async fn delete(&mut self, provider_key: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM provider_display_configs WHERE provider_key = $1")
            .bind(provider_key)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self), err)]
    pub async fn list_known_providers(&mut self) -> Result<Vec<KnownProviderDBResponse>> {
        let rows = sqlx::query!(
            r#"
            SELECT
                LOWER(BTRIM(metadata->>'provider')) AS provider_key,
                MIN(BTRIM(metadata->>'provider')) AS display_name,
                COUNT(*)::BIGINT AS model_count
            FROM deployed_models
            WHERE
                deleted = false
                AND metadata->>'provider' IS NOT NULL
                AND BTRIM(metadata->>'provider') <> ''
            GROUP BY LOWER(BTRIM(metadata->>'provider'))
            ORDER BY MIN(BTRIM(metadata->>'provider'))
            "#
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(KnownProviderDBResponse {
                    provider_key: row.provider_key?,
                    display_name: row.display_name?,
                    model_count: row.model_count?,
                })
            })
            .collect())
    }

    /// Like `list_known_providers`, but only counts models the given user can
    /// access via their group memberships (including the Everyone group).
    #[instrument(skip(self), fields(%user_id), err)]
    pub async fn list_known_providers_for_user(&mut self, user_id: UserId) -> Result<Vec<KnownProviderDBResponse>> {
        let rows = sqlx::query!(
            r#"
            SELECT
                LOWER(BTRIM(dm.metadata->>'provider')) AS provider_key,
                MIN(BTRIM(dm.metadata->>'provider')) AS display_name,
                COUNT(*)::BIGINT AS model_count
            FROM deployed_models dm
            JOIN deployment_groups dg ON dg.deployment_id = dm.id
            WHERE
                dm.deleted = false
                AND dm.metadata->>'provider' IS NOT NULL
                AND BTRIM(dm.metadata->>'provider') <> ''
                AND dg.group_id IN (
                    SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = $1
                    UNION
                    SELECT '00000000-0000-0000-0000-000000000000'::uuid
                    WHERE $1 != '00000000-0000-0000-0000-000000000000'::uuid
                )
            GROUP BY LOWER(BTRIM(dm.metadata->>'provider'))
            ORDER BY MIN(BTRIM(dm.metadata->>'provider'))
            "#,
            user_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(KnownProviderDBResponse {
                    provider_key: row.provider_key?,
                    display_name: row.display_name?,
                    model_count: row.model_count?,
                })
            })
            .collect())
    }
}
