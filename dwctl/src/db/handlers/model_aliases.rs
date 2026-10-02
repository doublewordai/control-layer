//! Read the small ingress-synonym map independently of Onwards' routing/auth sync.

use sqlx::PgConnection;

use crate::db::errors::Result;
use crate::db::models::model_aliases::ModelAliasTarget;

pub struct ModelAliases<'c> {
    db: &'c mut PgConnection,
}

impl<'c> ModelAliases<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Primary names of activated classes only. Used by batch ingestion, which
    /// already reads model access once per file, never once per uploaded line.
    pub async fn active_primary_names(&mut self) -> Result<Vec<ModelAliasTarget>> {
        Ok(sqlx::query_as!(
            ModelAliasTarget,
            r#"SELECT CASE WHEN c.class_key='standard' THEN dm.alias
                   ELSE dm.alias || ':' || c.class_key END AS "alias!",
                   dm.id AS deployed_model_id, c.id AS serving_class_id,
                   dm.alias AS canonical_alias, c.class_key
               FROM deployed_models dm JOIN model_serving_classes c ON c.deployed_model_id=dm.id
               WHERE NOT dm.deleted AND dm.routing_mode='class_routes'"#
        )
        .fetch_all(&mut *self.db)
        .await?)
    }

    /// Load identity only, including dormant classes. Activation, authorization,
    /// prices and destinations must come from the live configuration at dispatch,
    /// never from this startup snapshot. Read from primary after reconciliation.
    pub async fn list_targets(&mut self) -> Result<Vec<ModelAliasTarget>> {
        Ok(sqlx::query_as!(
            ModelAliasTarget,
            r#"
            SELECT a.alias, a.deployed_model_id, a.serving_class_id,
                   dm.alias AS canonical_alias, c.class_key
            FROM model_aliases a
            JOIN deployed_models dm ON dm.id = a.deployed_model_id
            JOIN model_serving_classes c
              ON c.id = a.serving_class_id AND c.deployed_model_id = a.deployed_model_id
            WHERE NOT dm.deleted
            "#
        )
        .fetch_all(&mut *self.db)
        .await?)
    }
}
