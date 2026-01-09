//! Model enrichment utilities for adding groups, metrics, status, and pricing to deployed models.
//!
//! This module provides reusable logic for enriching model responses with additional data
//! based on include parameters and user permissions. It's used by both the list and get
//! model endpoints to maintain consistency.

use crate::{
    api::models::{
        deployments::{DeployedModelResponse, ModelMetrics, ModelProbeStatus},
        inference_endpoints::InferenceEndpointResponse,
    },
    db::{
        handlers::{Groups, InferenceEndpoints, Repository, analytics::get_model_metrics},
        models::groups::GroupDBResponse,
    },
    errors::{Error, Result},
    types::{DeploymentId, GroupId, InferenceEndpointId},
};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
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
    /// Whether to include pricing information (includes tariffs)
    pub include_pricing: bool,
    /// Whether to include endpoint information
    pub include_endpoints: bool,
    /// Whether the user can read full pricing details
    pub can_read_pricing: bool,
    /// Whether the user can read rate limiting information
    pub can_read_rate_limits: bool,
    /// Whether the user can read who created models
    pub can_read_users: bool,
}

type ProbeStatusTuple = (Option<Uuid>, bool, Option<i32>, Option<DateTime<Utc>>, Option<bool>, Option<f64>);

impl<'a> DeployedModelEnricher<'a> {
    /// Enriches multiple models in bulk with requested additional data.
    ///
    /// This method fetches all required data in parallel for maximum performance:
    /// - Groups: Fetches model-to-group associations and group details
    /// - Metrics: Fetches usage statistics and analytics
    /// - Status: Fetches probe health check information
    /// - Pricing: Fetches tariffs from database (provider_pricing is added directly by handlers)
    ///
    /// # Arguments
    /// * `models` - Vector of models to enrich
    ///
    /// # Returns
    /// Vector of enriched model responses with requested data included
    #[tracing::instrument(skip(self, models), fields(count = models.len()))]
    pub async fn enrich_many(&self, models: Vec<DeployedModelResponse>) -> Result<Vec<DeployedModelResponse>> {
        if models.is_empty() {
            return Ok(vec![]);
        }

        let model_ids: Vec<DeploymentId> = models.iter().map(|m| m.id).collect();
        let model_aliases: Vec<String> = models.iter().map(|m| m.alias.clone()).collect();

        // Fetch all includes in parallel for maximum performance
        let (groups_result, status_map, metrics_map, endpoints_map, pricing_tariffs_map) = tokio::join!(
            // Groups query
            async {
                if self.include_groups {
                    let mut groups_conn = self.db.acquire().await.map_err(|e| Error::Database(e.into())).ok()?;
                    let mut groups_repo = Groups::new(&mut groups_conn);

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
            },
            // Endpoints query
            async {
                if self.include_endpoints {
                    let mut endpoints_conn = self.db.acquire().await.map_err(|e| Error::Database(e.into())).ok()?;
                    let mut endpoints_repo = InferenceEndpoints::new(&mut endpoints_conn);

                    // Collect all unique endpoint IDs
                    let endpoint_ids: Vec<InferenceEndpointId> = models
                        .iter()
                        .map(|m| m.hosted_on)
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();

                    let endpoints_db = endpoints_repo.get_bulk(endpoint_ids).await.ok()?;

                    // Convert DB responses to API responses
                    let endpoints_map: HashMap<InferenceEndpointId, InferenceEndpointResponse> =
                        endpoints_db.into_iter().map(|(id, endpoint)| (id, endpoint.into())).collect();

                    Some(endpoints_map)
                } else {
                    None
                }
            },
            // Tariffs query (only if pricing is requested and user can read pricing)
            async {
                if self.include_pricing {
                    use crate::{api::models::tariffs::TariffResponse, db::handlers::Tariffs};

                    let mut tariffs_map: HashMap<DeploymentId, Vec<TariffResponse>> = HashMap::new();

                    for model_id in &model_ids {
                        let mut tariffs_conn = self.db.acquire().await.map_err(|e| Error::Database(e.into())).ok()?;
                        let mut tariffs_repo = Tariffs::new(&mut tariffs_conn);

                        if let Ok(tariffs) = tariffs_repo.list_current_by_model(*model_id).await {
                            tariffs_map.insert(*model_id, tariffs.into_iter().map(TariffResponse::from).collect());
                        }
                    }

                    Some(tariffs_map)
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

        for mut model_response in models {
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

            // Add endpoint if requested and available
            if self.include_endpoints {
                model_response = Self::apply_endpoint(model_response, &endpoints_map);
            }

            // Add tariffs if pricing is requested and available
            if self.include_pricing {
                model_response = Self::apply_tariffs(model_response, &pricing_tariffs_map);
            }

            // Mask rate limiting info for users without ModelRateLimits permission
            if !self.can_read_rate_limits {
                model_response = model_response.mask_rate_limiting();
                model_response = model_response.mask_capacity();
            }

            if !self.can_read_users {
                model_response = model_response.mask_created_by();
            }

            enriched_models.push(model_response);
        }

        Ok(enriched_models)
    }

    /// Enriches a single model with requested additional data.
    ///
    /// This is a convenience method that wraps `enrich_many` for single-model enrichment.
    /// It provides the same functionality but is optimized for the single-model use case.
    ///
    /// # Arguments
    /// * `model` - The model to enrich
    ///
    /// # Returns
    /// Enriched model response with requested data included
    #[tracing::instrument(skip(self, model))]
    pub async fn enrich_one(&self, model: DeployedModelResponse) -> Result<DeployedModelResponse> {
        let enriched = self.enrich_many(vec![model]).await?;
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
        }
        // If groups were not requested (both None), leave model.groups as None
        model
    }

    /// Apply metrics to a model response
    fn apply_metrics(mut model: DeployedModelResponse, metrics_map: &Option<HashMap<String, ModelMetrics>>) -> DeployedModelResponse {
        if let Some(metrics_map) = metrics_map
            && let Some(metrics) = metrics_map.get(&model.alias)
        {
            model = model.with_metrics(metrics.clone());
        }
        // If no metrics found for this model, just skip it (no warning needed - model might be new)
        model
    }

    /// Apply probe status to a model response
    fn apply_status(
        mut model: DeployedModelResponse,
        status_map: &Option<HashMap<DeploymentId, ProbeStatusTuple>>,
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

    /// Apply endpoint to a model response
    fn apply_endpoint(
        mut model: DeployedModelResponse,
        endpoints_map: &Option<HashMap<InferenceEndpointId, InferenceEndpointResponse>>,
    ) -> DeployedModelResponse {
        if let Some(endpoints_map) = endpoints_map
            && let Some(endpoint) = endpoints_map.get(&model.hosted_on)
        {
            model = model.with_endpoint(endpoint.clone());
        }
        model
    }

    /// Apply tariffs to a model response
    fn apply_tariffs(
        mut model: DeployedModelResponse,
        tariffs_map: &Option<HashMap<DeploymentId, Vec<crate::api::models::tariffs::TariffResponse>>>,
    ) -> DeployedModelResponse {
        if let Some(tariffs_map) = tariffs_map
            && let Some(tariffs) = tariffs_map.get(&model.id)
        {
            model = model.with_tariffs(tariffs.clone());
        }
        model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::models::deployments::ModelMetrics, db::models::groups::GroupDBResponse};
    use chrono::Utc;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn create_test_model() -> DeployedModelResponse {
        DeployedModelResponse {
            id: Uuid::new_v4(),
            model_name: "test-model".to_string(),
            alias: "test-alias".to_string(),
            description: None,
            model_type: None,
            capabilities: None,
            created_by: Some(Uuid::new_v4()),
            hosted_on: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            requests_per_second: Some(100.0),
            burst_size: Some(200),
            capacity: None,
            batch_capacity: None,
            groups: None,
            metrics: None,
            status: None,
            provider_pricing: None,
            endpoint: None,
            tariffs: None,
        }
    }

    #[test]
    fn test_apply_groups_with_data() {
        let model = create_test_model();
        let model_id = model.id;

        let group_id: GroupId = Uuid::new_v4();
        let mut model_groups_map = HashMap::new();
        model_groups_map.insert(model_id, vec![group_id]);

        let mut groups_map = HashMap::new();
        groups_map.insert(
            group_id,
            GroupDBResponse {
                id: group_id,
                name: "Test Group".to_string(),
                description: Some("Test description".to_string()),
                created_by: Uuid::new_v4(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                source: "native".to_string(),
            },
        );

        let result = DeployedModelEnricher::apply_groups(model, &Some(model_groups_map), &Some(groups_map));

        assert!(result.groups.is_some());
        let groups = result.groups.unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Test Group");
    }

    #[test]
    fn test_apply_groups_empty() {
        let model = create_test_model();

        let model_groups_map = HashMap::new();
        let groups_map = HashMap::new();

        let result = DeployedModelEnricher::apply_groups(model, &Some(model_groups_map), &Some(groups_map));

        // Model has no groups, but groups were requested, so should be empty array
        assert!(result.groups.is_some());
        assert_eq!(result.groups.unwrap().len(), 0);
    }

    #[test]
    fn test_apply_groups_not_requested() {
        let model = create_test_model();

        let result = DeployedModelEnricher::apply_groups(model, &None, &None);

        // Groups not requested, should be None
        assert!(result.groups.is_none());
    }

    #[test]
    fn test_apply_metrics_with_data() {
        let model = create_test_model();
        let alias = model.alias.clone();

        let mut metrics_map = HashMap::new();
        metrics_map.insert(
            alias.clone(),
            ModelMetrics {
                avg_latency_ms: Some(123.45),
                total_requests: 100,
                total_input_tokens: 1000,
                total_output_tokens: 2000,
                last_active_at: Some(Utc::now()),
                time_series: None,
            },
        );

        let result = DeployedModelEnricher::apply_metrics(model, &Some(metrics_map));

        assert!(result.metrics.is_some());
        let metrics = result.metrics.unwrap();
        assert_eq!(metrics.total_requests, 100);
        assert_eq!(metrics.total_input_tokens, 1000);
        assert_eq!(metrics.avg_latency_ms, Some(123.45));
    }

    #[test]
    fn test_apply_metrics_no_data() {
        let model = create_test_model();
        let metrics_map = HashMap::new();

        let result = DeployedModelEnricher::apply_metrics(model, &Some(metrics_map));

        // No metrics for this model, should remain None
        assert!(result.metrics.is_none());
    }

    #[test]
    fn test_apply_status_with_data() {
        let model = create_test_model();
        let model_id = model.id;
        let probe_id = Uuid::new_v4();
        let last_check = Utc::now();

        let mut status_map = HashMap::new();
        status_map.insert(model_id, (Some(probe_id), true, Some(60), Some(last_check), Some(true), Some(99.5)));

        let result = DeployedModelEnricher::apply_status(model, &Some(status_map));

        assert!(result.status.is_some());
        let status = result.status.unwrap();
        assert_eq!(status.probe_id, Some(probe_id));
        assert!(status.active);
        assert_eq!(status.interval_seconds, Some(60));
        assert_eq!(status.uptime_percentage, Some(99.5));
    }

    #[test]
    fn test_apply_status_no_probe() {
        let model = create_test_model();
        let status_map = HashMap::new();

        let result = DeployedModelEnricher::apply_status(model, &Some(status_map));

        // No probe for this model, should have default status
        assert!(result.status.is_some());
        let status = result.status.unwrap();
        assert_eq!(status.probe_id, None);
        assert!(!status.active);
        assert_eq!(status.interval_seconds, None);
    }

    #[test]
    fn test_mask_rate_limiting() {
        let mut model = create_test_model();
        model.requests_per_second = Some(100.0);
        model.burst_size = Some(200);
        model.capacity = Some(50);

        let masked = model.mask_rate_limiting();

        // Rate limits should be masked
        assert_eq!(masked.requests_per_second, None);
        assert_eq!(masked.burst_size, None);
        // Capacity is not a rate limit, should remain
        assert_eq!(masked.capacity, Some(50));
    }

    #[test]
    fn test_apply_endpoint_with_data() {
        let model = create_test_model();
        let endpoint_id = model.hosted_on;

        let mut endpoints_map = HashMap::new();
        endpoints_map.insert(
            endpoint_id,
            InferenceEndpointResponse {
                id: endpoint_id,
                name: "Test Endpoint".to_string(),
                description: Some("Test endpoint description".to_string()),
                url: "https://api.example.com".to_string(),
                model_filter: None,
                requires_api_key: true,
                auth_header_name: "Authorization".to_string(),
                auth_header_prefix: "Bearer ".to_string(),
                created_by: Uuid::new_v4(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        );

        let result = DeployedModelEnricher::apply_endpoint(model, &Some(endpoints_map));

        assert!(result.endpoint.is_some());
        let endpoint = result.endpoint.unwrap();
        assert_eq!(endpoint.name, "Test Endpoint");
        assert_eq!(endpoint.url, "https://api.example.com");
    }

    #[test]
    fn test_apply_endpoint_no_data() {
        let model = create_test_model();
        let endpoints_map = HashMap::new();

        let result = DeployedModelEnricher::apply_endpoint(model, &Some(endpoints_map));

        // No endpoint found for this model, should remain None
        assert!(result.endpoint.is_none());
    }

    #[test]
    fn test_apply_endpoint_not_requested() {
        let model = create_test_model();

        let result = DeployedModelEnricher::apply_endpoint(model, &None);

        // Endpoints not requested, should remain None
        assert!(result.endpoint.is_none());
    }

    #[test]
    fn test_apply_tariffs_with_data() {
        use crate::api::models::tariffs::TariffResponse;
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let model = create_test_model();
        let model_id = model.id;

        let mut tariffs_map = HashMap::new();
        tariffs_map.insert(
            model_id,
            vec![TariffResponse {
                id: Uuid::new_v4(),
                deployed_model_id: model_id,
                name: "Standard Tariff".to_string(),
                input_price_per_token: Decimal::from_str("0.001").unwrap(),
                output_price_per_token: Decimal::from_str("0.002").unwrap(),
                api_key_purpose: None,
                completion_window: None,
                valid_from: Utc::now(),
                valid_until: None,
                is_active: true,
            }],
        );

        let result = DeployedModelEnricher::apply_tariffs(model, &Some(tariffs_map));

        // Tariffs should be applied
        assert!(result.tariffs.is_some());
        let tariffs = result.tariffs.unwrap();
        assert_eq!(tariffs.len(), 1);
        assert_eq!(tariffs[0].name, "Standard Tariff");
        assert_eq!(tariffs[0].input_price_per_token, Decimal::from_str("0.001").unwrap());
        assert_eq!(tariffs[0].output_price_per_token, Decimal::from_str("0.002").unwrap());
    }

    #[test]
    fn test_apply_tariffs_no_data() {
        let model = create_test_model();
        let tariffs_map = HashMap::new();

        let result = DeployedModelEnricher::apply_tariffs(model, &Some(tariffs_map));

        // No tariffs for this model, should remain None
        assert!(result.tariffs.is_none());
    }

    #[test]
    fn test_apply_tariffs_not_requested() {
        let model = create_test_model();

        let result = DeployedModelEnricher::apply_tariffs(model, &None);

        // Tariffs not requested, should remain None
        assert!(result.tariffs.is_none());
    }
}
