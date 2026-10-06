use sqlx::PgConnection;

use crate::db::errors::Result;
use crate::types::{DeploymentId, UserId};

#[derive(Debug, Clone, PartialEq)]
pub struct RealtimeInflightOverride {
    pub account_id: UserId,
    pub account_name: String,
    pub inflight_limit: i32,
}

pub struct RealtimeInflightLimits<'c> {
    db: &'c mut PgConnection,
}

impl<'c> RealtimeInflightLimits<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    pub async fn list(&mut self, deployed_model_id: DeploymentId) -> Result<Vec<RealtimeInflightOverride>> {
        Ok(sqlx::query_as!(
            RealtimeInflightOverride,
            r#"
            SELECT o.user_id AS account_id, u.username AS account_name, o.inflight_limit
            FROM realtime_inflight_limit_overrides o
            INNER JOIN users u ON u.id = o.user_id
            WHERE o.deployed_model_id = $1
              AND u.is_deleted = FALSE
            ORDER BY u.username
            "#,
            deployed_model_id
        )
        .fetch_all(&mut *self.db)
        .await?)
    }
}
