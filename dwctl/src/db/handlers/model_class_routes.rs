//! Read the class configuration without multiplying canonical model list rows.
use crate::db::errors::Result;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ClassRouteView {
    pub cache_pricing: crate::api::models::cache_pricing::CachePricingResponse,
    pub model_id: Uuid,
    pub class_id: Uuid,
    pub class_key: String,
    pub display_name: String,
    pub routing_mode: String,
    pub inference_endpoint_id: Uuid,
    pub upstream_model_name: String,
}

pub struct ModelClassRoutes<'c> {
    db: &'c mut PgConnection,
}
impl<'c> ModelClassRoutes<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }
    pub async fn list_for_models(&mut self, models: &[Uuid]) -> Result<Vec<ClassRouteView>> {
        let rows = sqlx::query!(
            r#"SELECT dm.id AS model_id, c.id AS class_id, c.class_key, c.display_name,
                      dm.routing_mode, c.inference_endpoint_id, c.upstream_model_name,
                      price.write_multiplier_5m AS "write_multiplier_5m?", price.write_multiplier_1h AS "write_multiplier_1h?", price.write_multiplier_24h AS "write_multiplier_24h?",
                      price.read_multiplier AS "read_multiplier?", price.min_prefix_tokens AS "min_prefix_tokens?", price.valid_from AS "valid_from?", price.valid_until AS "valid_until?"
               FROM deployed_models dm JOIN model_serving_classes c ON c.deployed_model_id=dm.id
               LEFT JOIN LATERAL (
                   SELECT write_multiplier_5m,write_multiplier_1h,write_multiplier_24h,read_multiplier,min_prefix_tokens,valid_from,valid_until
                   FROM model_cache_tariffs WHERE deployed_model_id=dm.id AND user_id IS NULL
                     AND (serving_class=c.class_key OR serving_class IS NULL)
                     AND valid_from<=now() AND (valid_until IS NULL OR valid_until>now())
                     AND EXISTS (
                         SELECT 1 FROM model_cache_tariffs general
                         WHERE general.deployed_model_id=dm.id AND general.user_id IS NULL
                           AND general.serving_class IS NULL AND general.valid_from<=now()
                           AND (general.valid_until IS NULL OR general.valid_until>now())
                     )
                   ORDER BY (serving_class IS NOT NULL) DESC,valid_from DESC LIMIT 1
               ) price ON true
               WHERE dm.id=ANY($1) ORDER BY dm.id, c.class_key"#, models
        ).fetch_all(&mut *self.db).await?;
        Ok(rows
            .into_iter()
            .map(|r| ClassRouteView {
                model_id: r.model_id,
                class_id: r.class_id,
                class_key: r.class_key,
                display_name: r.display_name,
                routing_mode: r.routing_mode,
                inference_endpoint_id: r.inference_endpoint_id,
                upstream_model_name: r.upstream_model_name,
                cache_pricing: crate::api::models::cache_pricing::CachePricingResponse {
                    enabled: r.valid_from.is_some(),
                    write_multiplier_5m: r.write_multiplier_5m,
                    write_multiplier_1h: r.write_multiplier_1h,
                    write_multiplier_24h: r.write_multiplier_24h,
                    read_multiplier: r.read_multiplier,
                    min_prefix_tokens: r.min_prefix_tokens,
                    valid_from: r.valid_from,
                    valid_until: r.valid_until,
                },
            })
            .collect())
    }
}
