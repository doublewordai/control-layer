//! Model enrichment utilities for adding groups, metrics, status, and pricing to deployed models.
//!
//! This module provides reusable logic for enriching model responses with additional data
//! based on include parameters and user permissions. It's used by both the list and get
//! model endpoints to maintain consistency.

use crate::{
    api::models::deployments::{DeployedModelResponse, ModelMetrics, ModelProbeStatus},
    db::{
        handlers::{Groups, Repository, analytics::get_model_metrics},
        models::{deployments::ModelPricing, groups::GroupDBResponse},
    },
    errors::{Error, Result},
    types::{DeploymentId, GroupId},
};
use chrono::{DateTime, Utc};
use sqlx::{Acquire, PgPool};
use std::collections::HashMap;
use uuid::Uuid;

/// Configuration for model enrichment operations
pub struct DeployedModelEnricher<'a> {
    /// Database connection pool
    pub db: &'a PgPool,
    /// Whether to include group information
    pub include_groups: bool,
    /// Whether to include metrics/analytics data
    pub include_metrics: bool,
    /// Whether to include probe status information
    pub include_status: bool,
    /// Whether to include pricing information
    pub include_pricing: bool,
    /// Whether the user can read full pricing details
    pub can_read_pricing: bool,
    /// Whether the user can read rate limiting information
    pub can_read_rate_limits: bool,
}

impl<'a> DeployedModelEnricher<'a> {
    /// Enriches multiple models in bulk with requested additional data.
    ///
    /// This method fetches all required data in parallel for maximum performance:
    /// - Groups: Fetches model-to-group associations and group details
    /// - Metrics: Fetches usage statistics and analytics
    /// - Status: Fetches probe health check information
    /// - Pricing: Applies pricing based on user permissions
    ///
    /// # Arguments
    /// * `models` - Vector of models to enrich (with their pricing info)
    ///
    /// # Returns
    /// Vector of enriched model responses with requested data included
    #[tracing::instrument(skip(self, models), fields(count = models.len()))]
    pub async fn enrich_many(&self, models: Vec<(DeployedModelResponse, Option<ModelPricing>)>) -> Result<Vec<DeployedModelResponse>> {
        if models.is_empty() {
            return Ok(vec![]);
        }

        let model_ids: Vec<DeploymentId> = models.iter().map(|(m, _)| m.id).collect();
        let model_aliases: Vec<String> = models.iter().map(|(m, _)| m.alias.clone()).collect();

        // Start a transaction for atomic reads
        let mut tx = self.db.begin().await.map_err(|e| Error::Database(e.into()))?;

        // Fetch all includes in parallel for maximum performance
        let (groups_result, status_map, metrics_map) = tokio::join!(
            // Groups query
            async {
                if self.include_groups {
                    let groups_conn = tx.acquire().await.map_err(|e| Error::Database(e.into())).ok()?;
                    let mut groups_repo = Groups::new(&mut *groups_conn);

                    let model_groups_map = groups_repo.get_deployments_groups_bulk(&model_ids).await.ok()?;

                    // Collect all unique group IDs that we need to fetch
                    let all_group_ids: Vec<GroupId> = model_groups_map
                        .values()
                        .flatten()
                        .copied()
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();

                    let groups_map = groups_repo.get_bulk(all_group_ids).await.ok()?;

                    Some((model_groups_map, groups_map))
                } else {
                    None
                }
            },
            // Probe status query
            async {
                if self.include_status {
                    use crate::probes::db::ProbeManager;
                    ProbeManager::get_deployment_statuses(self.db, &model_ids).await.ok()
                } else {
                    None
                }
            },
            // Metrics query
            async {
                if self.include_metrics {
                    match get_model_metrics(self.db, model_aliases).await {
                        Ok(map) => Some(map),
                        Err(e) => {
                            tracing::warn!("Failed to fetch bulk metrics: {:?}", e);
                            None
                        }
                    }
                } else {
                    None
                }
            }
        );

        let (model_groups_map, groups_map) = match groups_result {
            Some((model_groups_map, groups_map)) => (Some(model_groups_map), Some(groups_map)),
            None => (None, None),
        };

        // Build enriched responses
        let mut enriched_models = Vec::with_capacity(models.len());

        for (mut model_response, model_pricing) in models {
            // Add groups if requested and available
            if self.include_groups {
                model_response = Self::apply_groups(model_response, &model_groups_map, &groups_map);
            }

            // Add metrics if requested and available
            if self.include_metrics {
                model_response = Self::apply_metrics(model_response, &metrics_map);
            }

            // Add probe status if requested and available
            if self.include_status {
                model_response = Self::apply_status(model_response, &status_map);
            }

            // Add pricing if requested (filtered by user role)
            if self.include_pricing {
                model_response = Self::apply_pricing(model_response, model_pricing, self.can_read_pricing);
            }

            // Mask rate limiting info for users without ModelRateLimits permission
            if !self.can_read_rate_limits {
                model_response = model_response.mask_rate_limiting();
            }

            enriched_models.push(model_response);
        }

        // Commit the transaction to ensure all reads were atomic
        tx.commit().await.map_err(|e| Error::Database(e.into()))?;

        Ok(enriched_models)
    }

    /// Enriches a single model with requested additional data.
    ///
    /// This is a convenience method that wraps `enrich_many` for single-model enrichment.
    /// It provides the same functionality but is optimized for the single-model use case.
    ///
    /// # Arguments
    /// * `model` - The model to enrich
    /// * `pricing` - Optional pricing information for the model
    ///
    /// # Returns
    /// Enriched model response with requested data included
    #[tracing::instrument(skip(self, model))]
    pub async fn enrich_one(&self, model: DeployedModelResponse, pricing: Option<ModelPricing>) -> Result<DeployedModelResponse> {
        let enriched = self.enrich_many(vec![(model, pricing)]).await?;
        enriched.into_iter().next().ok_or_else(|| Error::BadRequest {
            message: "No model returned from enrichment".to_string(),
        })
    }

    /// Apply groups to a model response
    fn apply_groups(
        mut model: DeployedModelResponse,
        model_groups_map: &Option<HashMap<DeploymentId, Vec<GroupId>>>,
        groups_map: &Option<HashMap<GroupId, GroupDBResponse>>,
    ) -> DeployedModelResponse {
        if let (Some(model_groups_map), Some(groups_map)) = (model_groups_map, groups_map) {
            if let Some(group_ids) = model_groups_map.get(&model.id) {
                let model_groups: Vec<_> = group_ids
                    .iter()
                    .filter_map(|group_id| groups_map.get(group_id))
                    .cloned()
                    .map(|group| group.into())
                    .collect();
                model = model.with_groups(model_groups);
            } else {
                // No groups for this model, but groups were requested, so set empty array
                model = model.with_groups(vec![]);
            }
        } else {
            // Groups requested but no data available, set empty array
            model = model.with_groups(vec![]);
        }
        model
    }

    /// Apply metrics to a model response
    fn apply_metrics(mut model: DeployedModelResponse, metrics_map: &Option<HashMap<String, ModelMetrics>>) -> DeployedModelResponse {
        if let Some(metrics_map) = metrics_map {
            if let Some(metrics) = metrics_map.get(&model.alias) {
                model = model.with_metrics(metrics.clone());
            }
        }
        // If no metrics found for this model, just skip it (no warning needed - model might be new)
        model
    }

    /// Apply probe status to a model response
    fn apply_status(
        mut model: DeployedModelResponse,
        status_map: &Option<HashMap<DeploymentId, (Option<Uuid>, bool, Option<i32>, Option<DateTime<Utc>>, Option<bool>, Option<f64>)>>,
    ) -> DeployedModelResponse {
        if let Some(statuses) = status_map {
            if let Some((probe_id, active, interval_seconds, last_check, last_success, uptime_percentage)) = statuses.get(&model.id) {
                let status = ModelProbeStatus {
                    probe_id: *probe_id,
                    active: *active,
                    interval_seconds: *interval_seconds,
                    last_check: *last_check,
                    last_success: *last_success,
                    uptime_percentage: *uptime_percentage,
                };
                model = model.with_status(status);
            } else {
                // No probe for this model - set default status
                let status = ModelProbeStatus {
                    probe_id: None,
                    active: false,
                    interval_seconds: None,
                    last_check: None,
                    last_success: None,
                    uptime_percentage: None,
                };
                model = model.with_status(status);
            }
        }
        model
    }

    /// Apply pricing to a model response based on user permissions
    fn apply_pricing(mut model: DeployedModelResponse, pricing: Option<ModelPricing>, can_read_pricing: bool) -> DeployedModelResponse {
        if let Some(pricing) = pricing {
            // All users get customer-facing pricing
            model = model.with_pricing(pricing.to_customer_pricing());

            // Only privileged users get downstream pricing
            if can_read_pricing {
                model = model.with_downstream_pricing(pricing.downstream);
            }
        }
        model
    }
}
