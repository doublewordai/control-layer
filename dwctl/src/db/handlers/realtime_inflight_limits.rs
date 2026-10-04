use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use crate::db::errors::Result;
use crate::types::{DeploymentId, UserId};

#[derive(Debug, Clone, PartialEq)]
pub struct RealtimeInflightOverride {
    pub account_id: UserId,
    pub account_name: String,
    pub inflight_limit: Option<i32>,
    pub reason: String,
    pub set_by: UserId,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
}

pub struct RealtimeInflightLimits<'c> {
    db: &'c mut PgConnection,
}

impl<'c> RealtimeInflightLimits<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    pub async fn list_current(&mut self, deployed_model_id: DeploymentId) -> Result<Vec<RealtimeInflightOverride>> {
        Ok(sqlx::query_as!(
            RealtimeInflightOverride,
            r#"
            SELECT o.user_id AS account_id, u.username AS account_name, o.inflight_limit, o.reason, o.set_by,
                   o.valid_from, o.valid_until
            FROM realtime_inflight_limit_overrides o
            INNER JOIN users u ON u.id = o.user_id
            WHERE o.deployed_model_id = $1
              AND o.valid_until IS NULL
              AND o.inflight_limit IS NOT NULL
            ORDER BY u.username
            "#,
            deployed_model_id
        )
        .fetch_all(&mut *self.db)
        .await?)
    }

    pub async fn history(&mut self, deployed_model_id: DeploymentId, account_id: UserId) -> Result<Vec<RealtimeInflightOverride>> {
        Ok(sqlx::query_as!(
            RealtimeInflightOverride,
            r#"
            SELECT o.user_id AS account_id, u.username AS account_name, o.inflight_limit, o.reason, o.set_by,
                   o.valid_from, o.valid_until
            FROM realtime_inflight_limit_overrides o
            INNER JOIN users u ON u.id = o.user_id
            WHERE o.deployed_model_id = $1
              AND o.user_id = $2
            ORDER BY o.valid_from DESC, o.valid_until DESC NULLS FIRST
            "#,
            deployed_model_id,
            account_id
        )
        .fetch_all(&mut *self.db)
        .await?)
    }

    pub async fn replace(
        &mut self,
        deployed_model_id: DeploymentId,
        account_id: UserId,
        inflight_limit: Option<i32>,
        reason: &str,
        set_by: UserId,
    ) -> Result<RealtimeInflightOverride> {
        sqlx::query!(
            r#"
            UPDATE realtime_inflight_limit_overrides
            SET valid_until = NOW()
            WHERE deployed_model_id = $1 AND user_id = $2 AND valid_until IS NULL
            "#,
            deployed_model_id,
            account_id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(sqlx::query_as!(
            RealtimeInflightOverride,
            r#"
            WITH inserted AS (
                INSERT INTO realtime_inflight_limit_overrides (deployed_model_id, user_id, inflight_limit, reason, set_by)
                VALUES ($1, $2, $3, $4, $5)
                RETURNING user_id, inflight_limit, reason, set_by, valid_from, valid_until
            )
            SELECT inserted.user_id AS "account_id!", u.username AS "account_name!", inserted.inflight_limit,
                   inserted.reason AS "reason!", inserted.set_by AS "set_by!", inserted.valid_from AS "valid_from!",
                   inserted.valid_until
            FROM inserted
            INNER JOIN users u ON u.id = inserted.user_id
            "#,
            deployed_model_id,
            account_id,
            inflight_limit,
            reason,
            set_by
        )
        .fetch_one(&mut *self.db)
        .await?)
    }
}
