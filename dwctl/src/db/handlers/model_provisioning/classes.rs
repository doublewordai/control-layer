//! Dormant catalog class reconciliation. The parent owns the transaction and lock.
use std::collections::{HashMap, HashSet};

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{MODEL_PROVISIONING_LOCK, ModelProvisioning};
use crate::db::errors::{DbError, Result as DbResult};
use crate::model_provisioning::{Catalog, CatalogModel};

/// Model-name writers and catalog reconciliation share this lock. Alias authoring
/// stays catalog-only; direct SQL edits must perform the same collision preflight.
pub(crate) async fn lock_model_names(db: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MODEL_PROVISIONING_LOCK)
        .execute(db)
        .await?;
    Ok(())
}

/// Called under the catalog lock by ordinary model create/rename writers.
pub(crate) async fn validate_model_name(db: &mut PgConnection, model: Option<Uuid>, alias: &str) -> DbResult<()> {
    let collision: Option<String> = sqlx::query_scalar(
        "SELECT alias FROM model_aliases WHERE lower(alias) = lower($1)
         UNION ALL
         SELECT dm.alias || ':' || c.class_key FROM model_serving_classes c
         JOIN deployed_models dm ON dm.id=c.deployed_model_id
         WHERE c.class_key <> 'standard' AND lower(dm.alias || ':' || c.class_key)=lower($1)
         UNION ALL
         SELECT a.alias FROM model_aliases a JOIN model_serving_classes c ON c.deployed_model_id=$2
         WHERE c.class_key <> 'standard' AND lower(a.alias)=lower($1 || ':' || c.class_key)
         UNION ALL
         SELECT other.alias FROM deployed_models other JOIN model_serving_classes c ON c.deployed_model_id=$2
         WHERE c.class_key <> 'standard' AND other.id IS DISTINCT FROM $2
           AND lower(other.alias)=lower($1 || ':' || c.class_key)
         LIMIT 1",
    )
    .bind(alias)
    .bind(model)
    .fetch_optional(db)
    .await?;
    if collision.is_some() {
        return Err(DbError::UniqueViolation {
            constraint: None,
            table: Some("deployed_models".into()),
            message: format!("model name {alias:?} conflicts with a registered class name or synonym"),
            conflicting_value: Some(alias.into()),
        });
    }
    Ok(())
}

impl ModelProvisioning<'_> {
    pub(super) async fn preflight_class_names(&mut self, catalog: &Catalog) -> Result<()> {
        let models: Vec<(Uuid, String, String)> = sqlx::query_as("SELECT id, alias, routing_mode FROM deployed_models")
            .fetch_all(&mut *self.db)
            .await?;
        let managed: HashSet<&str> = catalog.models.iter().map(|m| m.clay.alias.as_str()).collect();
        // Activation is intentionally unsupported until every runtime reader/writer
        // understands class routing. In particular, never overwrite an active route.
        for (_, alias, mode) in &models {
            ensure!(
                !managed.contains(alias.as_str()) || mode == "legacy",
                "model {alias:?} uses class_routes; this catalog writer cannot edit activated models yet"
            );
        }
        let managed_ids: HashSet<Uuid> = models
            .iter()
            .filter(|(_, a, _)| managed.contains(a.as_str()))
            .map(|(id, _, _)| *id)
            .collect();
        let mut primary: HashSet<String> = models.iter().map(|(_, a, _)| a.to_lowercase()).collect();
        for m in &catalog.models {
            primary.insert(m.clay.alias.to_lowercase());
            primary.extend(m.clay.deployments.iter().map(|d| d.alias.to_lowercase()));
        }
        let existing: Vec<(Uuid,String)> = sqlx::query_as("SELECT c.deployed_model_id, dm.alias || ':' || c.class_key FROM model_serving_classes c JOIN deployed_models dm ON dm.id=c.deployed_model_id WHERE c.class_key <> 'standard'")
            .fetch_all(&mut *self.db).await?;
        for (model, name) in existing {
            if !managed_ids.contains(&model) {
                ensure!(
                    primary.insert(name.to_lowercase()),
                    "primary model name conflicts with existing class name {name:?}"
                );
            }
        }
        for m in &catalog.models {
            for key in m.clay.class_routes.keys().filter(|k| k.as_str() != "standard") {
                let name = format!("{}:{key}", m.clay.alias);
                ensure!(
                    primary.insert(name.to_lowercase()),
                    "primary class name {name:?} conflicts with an existing model/name"
                );
            }
        }
        let stored: Vec<(String, Uuid, String)> = sqlx::query_as(
            "SELECT a.alias,a.deployed_model_id,c.class_key FROM model_aliases a JOIN model_serving_classes c ON c.id=a.serving_class_id",
        )
        .fetch_all(&mut *self.db)
        .await?;
        let mut synonyms = HashSet::new();
        for (alias, model, _) in &stored {
            if !managed_ids.contains(model) {
                ensure!(
                    !primary.contains(&alias.to_lowercase()),
                    "primary name conflicts with existing synonym {alias:?}"
                );
                synonyms.insert(alias.to_lowercase());
            }
        }
        for m in &catalog.models {
            let id = models.iter().find(|(_, a, _)| a == &m.clay.alias).map(|(id, _, _)| *id);
            for (key, class) in &m.clay.class_routes {
                for alias in &class.aliases {
                    ensure!(
                        !primary.contains(&alias.to_lowercase()) && synonyms.insert(alias.to_lowercase()),
                        "synonym {alias:?} conflicts with an existing model/name"
                    );
                    if let Some((_, owner, old_class)) = stored.iter().find(|(a, _, _)| a.eq_ignore_ascii_case(alias)) {
                        ensure!(
                            Some(*owner) == id && old_class == key,
                            "synonym {alias:?} cannot be rebound to another model/class; retire it explicitly first"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) async fn reconcile_classes(
        &mut self,
        model_id: Uuid,
        model: &CatalogModel,
        endpoints: &HashMap<String, Uuid>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let aliases: Vec<&str> = model
            .clay
            .class_routes
            .values()
            .flat_map(|c| c.aliases.iter().map(String::as_str))
            .collect();
        sqlx::query("DELETE FROM model_aliases WHERE deployed_model_id=$1 AND NOT (alias=ANY($2))")
            .bind(model_id)
            .bind(&aliases)
            .execute(&mut *self.db)
            .await?;
        let old: Vec<String> = sqlx::query_scalar("SELECT class_key FROM model_serving_classes WHERE deployed_model_id=$1")
            .bind(model_id)
            .fetch_all(&mut *self.db)
            .await?;
        for key in old.iter().filter(|k| !model.clay.class_routes.contains_key(*k)) {
            self.reconcile_tariffs(model_id, None, Some(key), &[], at).await?;
            self.reconcile_cache_tariff(model_id, None, Some(key), None, at).await?;
            sqlx::query("DELETE FROM model_serving_classes WHERE deployed_model_id=$1 AND class_key=$2")
                .bind(model_id)
                .bind(key)
                .execute(&mut *self.db)
                .await?;
        }
        for (key, class) in &model.clay.class_routes {
            // The explicit no-op condition avoids notification/timestamp churn on restart.
            sqlx::query("INSERT INTO model_serving_classes (deployed_model_id,class_key,display_name,inference_endpoint_id,upstream_model_name)
                VALUES ($1,$2,$3,$4,$5) ON CONFLICT (deployed_model_id,class_key) DO UPDATE SET
                    display_name=EXCLUDED.display_name,inference_endpoint_id=EXCLUDED.inference_endpoint_id,upstream_model_name=EXCLUDED.upstream_model_name
                WHERE ROW(model_serving_classes.display_name,model_serving_classes.inference_endpoint_id,model_serving_classes.upstream_model_name)
                   IS DISTINCT FROM ROW(EXCLUDED.display_name,EXCLUDED.inference_endpoint_id,EXCLUDED.upstream_model_name)")
                .bind(model_id).bind(key).bind(&class.display_name).bind(endpoints[&class.endpoint]).bind(&class.upstream_model_name)
                .execute(&mut *self.db).await?;
            let id: Uuid = sqlx::query_scalar("SELECT id FROM model_serving_classes WHERE deployed_model_id=$1 AND class_key=$2")
                .bind(model_id)
                .bind(key)
                .fetch_one(&mut *self.db)
                .await?;
            for alias in &class.aliases {
                sqlx::query(
                    "INSERT INTO model_aliases (alias,deployed_model_id,serving_class_id) VALUES ($1,$2,$3) ON CONFLICT (alias) DO NOTHING",
                )
                .bind(alias)
                .bind(model_id)
                .bind(id)
                .execute(&mut *self.db)
                .await?;
            }
            self.reconcile_tariffs(model_id, None, Some(key), &class.tariffs, at).await?;
            self.reconcile_cache_tariff(
                model_id,
                None,
                Some(key),
                class.cache_tariff.as_ref().map(|c| c.as_tariff()).as_ref(),
                at,
            )
            .await?;
        }
        Ok(())
    }
}
