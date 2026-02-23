//! Prometheus info/gauge metrics for the onwards routing cache state.
//!
//! Emits gauge metrics that reflect the current model configuration loaded into
//! the onwards routing cache. Updated on every sync cycle (LISTEN/NOTIFY and
//! fallback). Uses the `metrics` crate facade — gauges appear at
//! `/internal/metrics` automatically when a recorder is installed.

use std::collections::HashSet;

use metrics::gauge;
use onwards::target::Targets;
use serde::Deserialize;
use sqlx::PgPool;
use tracing::warn;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ModelLabels {
    alias: String,
    model_name: String,
    model_type: String,
    endpoint_name: String,
    endpoint_host: String,
    is_composite: String,
    lb_strategy: String,
    sanitize_responses: String,
    is_metered: String,
}

/// Tracks previous-cycle label sets so stale gauge series can be zeroed.
///
/// Must be owned by the caller and passed into [`update_cache_info_metrics`] on
/// each sync cycle. The first call (when the sets are empty) skips zeroing;
/// subsequent calls diff against the previous state.
pub struct CacheInfoState {
    prev_models: HashSet<ModelLabels>,
    prev_groups: HashSet<(String, String, String)>,
    prev_components: HashSet<(String, String, String, String, String)>,
    /// Whether at least one cycle has run (skip zeroing on the first call).
    initialized: bool,
}

impl CacheInfoState {
    pub fn new() -> Self {
        Self {
            prev_models: HashSet::new(),
            prev_groups: HashSet::new(),
            prev_components: HashSet::new(),
            initialized: false,
        }
    }
}

#[derive(Deserialize)]
struct GroupInfo {
    group_id: String,
    group_name: String,
}

#[derive(Deserialize)]
struct ComponentInfo {
    component: String,
    component_endpoint: Option<String>,
    weight: i32,
    sort_order: i32,
    enabled: bool,
}

/// Update Prometheus gauges reflecting the current cache state.
///
/// Queries PostgreSQL for model metadata (groups, components, tariffs) and
/// iterates the Targets DashMap for API key counts. Multi-label info gauges
/// (`dwctl_model_group_info`, `dwctl_model_component_weight`) are zeroed when
/// their label combination disappears between cycles, so PromQL `group_left`
/// joins stay time-accurate after group reassignments or component removals.
pub async fn update_cache_info_metrics(pool: &PgPool, targets: &Targets, state: &mut CacheInfoState) -> Result<(), anyhow::Error> {
    // Single query: all model metadata with groups and components as JSON arrays
    let rows = sqlx::query!(
        r#"
        SELECT
            dm.alias as "alias!",
            dm.model_name as "model_name!",
            dm.type as "model_type?",
            ie.name as "endpoint_name?",
            ie.url as "endpoint_url?",
            dm.is_composite as "is_composite!",
            dm.lb_strategy,
            dm.sanitize_responses as "sanitize_responses!",
            dm.requests_per_second,
            dm.capacity,
            dm.batch_capacity,
            dm.throughput,
            EXISTS(
                SELECT 1 FROM model_tariffs mt
                WHERE mt.deployed_model_id = dm.id
                  AND mt.valid_until IS NULL
                  AND (mt.input_price_per_token > 0 OR mt.output_price_per_token > 0)
            ) as "is_metered!",
            (
                SELECT json_agg(json_build_object('group_id', g.id::text, 'group_name', g.name))::text
                FROM deployment_groups dg
                INNER JOIN groups g ON dg.group_id = g.id
                WHERE dg.deployment_id = dm.id
            ) as "groups_json?",
            (
                SELECT json_agg(json_build_object(
                    'component', comp.alias,
                    'component_endpoint', ie2.name,
                    'weight', dmc.weight,
                    'sort_order', dmc.sort_order,
                    'enabled', dmc.enabled
                ))::text
                FROM deployed_model_components dmc
                INNER JOIN deployed_models comp ON dmc.deployed_model_id = comp.id
                LEFT JOIN inference_endpoints ie2 ON comp.hosted_on = ie2.id
                WHERE dmc.composite_model_id = dm.id AND comp.deleted = FALSE
            ) as "components_json?"
        FROM deployed_models dm
        LEFT JOIN inference_endpoints ie ON dm.hosted_on = ie.id
        WHERE dm.deleted = FALSE
        "#
    )
    .fetch_all(pool)
    .await?;

    let mut current_models: HashSet<ModelLabels> = HashSet::new();
    let mut current_groups: HashSet<(String, String, String)> = HashSet::new();
    let mut current_components: HashSet<(String, String, String, String, String)> = HashSet::new();

    for row in &rows {
        let alias = &row.alias;
        let model_name = &row.model_name;
        let model_type = row.model_type.as_deref().unwrap_or("");
        let endpoint_name = row.endpoint_name.as_deref().unwrap_or("");

        // Extract host from endpoint URL for the label
        let endpoint_host = row
            .endpoint_url
            .as_deref()
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_default();

        let is_composite = if row.is_composite { "true" } else { "false" };
        let lb_strategy = row.lb_strategy.as_deref().unwrap_or("");
        let sanitize = if row.sanitize_responses { "true" } else { "false" };
        let is_metered = if row.is_metered { "true" } else { "false" };

        let labels = ModelLabels {
            alias: alias.clone(),
            model_name: model_name.clone(),
            model_type: model_type.to_string(),
            endpoint_name: endpoint_name.to_string(),
            endpoint_host: endpoint_host.clone(),
            is_composite: is_composite.to_string(),
            lb_strategy: lb_strategy.to_string(),
            sanitize_responses: sanitize.to_string(),
            is_metered: is_metered.to_string(),
        };
        current_models.insert(labels);

        // Info metric — constant 1.0, labels carry the metadata
        gauge!(
            "dwctl_model_info",
            "model" => alias.clone(),
            "model_name" => model_name.clone(),
            "model_type" => model_type.to_string(),
            "endpoint_name" => endpoint_name.to_string(),
            "endpoint_host" => endpoint_host.clone(),
            "is_composite" => is_composite.to_string(),
            "lb_strategy" => lb_strategy.to_string(),
            "sanitize_responses" => sanitize.to_string(),
            "is_metered" => is_metered.to_string(),
        )
        .set(1.0);

        // Rate limit gauge — zero when unset so removal is reflected
        gauge!("dwctl_model_rate_limit_rps", "model" => alias.clone()).set(row.requests_per_second.unwrap_or(0.0) as f64);

        // Concurrency limit gauge
        gauge!("dwctl_model_concurrency_limit", "model" => alias.clone()).set(row.capacity.unwrap_or(0) as f64);

        // Batch capacity gauge
        gauge!("dwctl_model_batch_capacity", "model" => alias.clone()).set(row.batch_capacity.unwrap_or(0) as f64);

        // Throughput gauge
        gauge!("dwctl_model_throughput_rps", "model" => alias.clone()).set(row.throughput.unwrap_or(0.0) as f64);

        // Group info metrics — one gauge per (model, group) pair
        if let Some(ref json) = row.groups_json {
            match serde_json::from_str::<Vec<GroupInfo>>(json) {
                Ok(groups) => {
                    for g in &groups {
                        current_groups.insert((alias.clone(), g.group_id.clone(), g.group_name.clone()));
                        gauge!(
                            "dwctl_model_group_info",
                            "model" => alias.clone(),
                            "group_id" => g.group_id.clone(),
                            "group_name" => g.group_name.clone(),
                        )
                        .set(1.0);
                    }
                }
                Err(e) => warn!("Failed to parse groups JSON for model '{}': {}", alias, e),
            }
        }

        // Component weight metrics — one gauge per component in a composite model
        if let Some(ref json) = row.components_json {
            match serde_json::from_str::<Vec<ComponentInfo>>(json) {
                Ok(components) => {
                    for c in &components {
                        current_components.insert((
                            alias.clone(),
                            c.component.clone(),
                            c.component_endpoint.clone().unwrap_or_default(),
                            c.sort_order.to_string(),
                            c.enabled.to_string(),
                        ));
                        gauge!(
                            "dwctl_model_component_weight",
                            "composite" => alias.clone(),
                            "component" => c.component.clone(),
                            "component_endpoint" => c.component_endpoint.clone().unwrap_or_default(),
                            "sort_order" => c.sort_order.to_string(),
                            "enabled" => c.enabled.to_string(),
                        )
                        .set(c.weight as f64);
                    }
                }
                Err(e) => warn!("Failed to parse components JSON for model '{}': {}", alias, e),
            }
        }
    }

    // Zero stale gauges by diffing against previous cycle's state.
    // Skip on the first call — there's nothing to zero yet.
    if state.initialized {
        // Info gauge uses full label set (so metadata changes zero the old series).
        // Single-label gauges only zero when the alias itself disappears.
        let current_aliases: HashSet<&str> = current_models.iter().map(|m| m.alias.as_str()).collect();

        for m in state.prev_models.difference(&current_models) {
            gauge!(
                "dwctl_model_info",
                "model" => m.alias.clone(),
                "model_name" => m.model_name.clone(),
                "model_type" => m.model_type.clone(),
                "endpoint_name" => m.endpoint_name.clone(),
                "endpoint_host" => m.endpoint_host.clone(),
                "is_composite" => m.is_composite.clone(),
                "lb_strategy" => m.lb_strategy.clone(),
                "sanitize_responses" => m.sanitize_responses.clone(),
                "is_metered" => m.is_metered.clone(),
            )
            .set(0.0);

            // Only zero single-label gauges if the alias is truly gone
            // (not just a metadata change like is_metered flipping)
            if !current_aliases.contains(m.alias.as_str()) {
                gauge!("dwctl_model_rate_limit_rps", "model" => m.alias.clone()).set(0.0);
                gauge!("dwctl_model_concurrency_limit", "model" => m.alias.clone()).set(0.0);
                gauge!("dwctl_model_batch_capacity", "model" => m.alias.clone()).set(0.0);
                gauge!("dwctl_model_throughput_rps", "model" => m.alias.clone()).set(0.0);
                gauge!("dwctl_model_api_key_count", "model" => m.alias.clone()).set(0.0);
            }
        }

        for (model, group_id, group_name) in state.prev_groups.difference(&current_groups) {
            gauge!("dwctl_model_group_info", "model" => model.clone(), "group_id" => group_id.clone(), "group_name" => group_name.clone())
                .set(0.0);
        }

        for (composite, component, component_endpoint, sort_order, enabled) in state.prev_components.difference(&current_components) {
            gauge!("dwctl_model_component_weight", "composite" => composite.clone(), "component" => component.clone(), "component_endpoint" => component_endpoint.clone(), "sort_order" => sort_order.clone(), "enabled" => enabled.clone()).set(0.0);
        }
    }

    state.prev_models = current_models;
    state.prev_groups = current_groups;
    state.prev_components = current_components;
    state.initialized = true;

    // API key counts — from the Targets DashMap (no SQL needed)
    for entry in targets.targets.iter() {
        let model = entry.key().clone();
        let count = entry.value().keys().map(|k| k.len()).unwrap_or(0);
        gauge!("dwctl_model_api_key_count", "model" => model).set(count as f64);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::Role;
    use crate::db::handlers::{Deployments, Groups, InferenceEndpoints, Repository, Tariffs};
    use crate::db::models::{
        deployments::{DeploymentCreateDBRequest, LoadBalancingStrategy},
        groups::GroupCreateDBRequest,
        inference_endpoints::InferenceEndpointCreateDBRequest,
        tariffs::TariffCreateDBRequest,
    };
    use crate::sync::onwards_config::load_targets_from_db;
    use rust_decimal::Decimal;

    /// Ensure the global Prometheus recorder is installed and return the handle.
    /// Must be called before any `metrics::gauge!()` calls so they aren't no-ops.
    fn ensure_recorder() -> metrics_exporter_prometheus::PrometheusHandle {
        crate::get_or_install_prometheus_handle()
    }

    #[sqlx::test]
    async fn test_model_info_and_group_metrics(pool: sqlx::PgPool) {
        let handle = ensure_recorder();
        let mut state = super::CacheInfoState::new();
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Create endpoint
        let mut tx = pool.begin().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut tx);
        let endpoint = repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: test_user.id,
                name: "test-ep".to_string(),
                description: None,
                url: url::Url::from_str("https://api.openai.com/v1").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create deployment with rate limit and capacity
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Deployments::new(&mut tx);
        let deployment = repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "gpt-4o".to_string(),
                alias: "cache-info-test-model".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: Some(endpoint.id),
                requests_per_second: Some(100.0),
                burst_size: None,
                capacity: Some(50),
                batch_capacity: Some(10),
                throughput: Some(25.0),
                provider_pricing: None,
                is_composite: false,
                lb_strategy: None,
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: true,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create group and assign deployment
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Groups::new(&mut tx);
        let group = repo
            .create(&GroupCreateDBRequest {
                created_by: test_user.id,
                name: "cache-info-test-group".to_string(),
                description: None,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        sqlx::query!(
            "INSERT INTO deployment_groups (deployment_id, group_id) VALUES ($1, $2)",
            deployment.id,
            group.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create a tariff so is_metered = true
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Tariffs::new(&mut tx);
        repo.create(&TariffCreateDBRequest {
            deployed_model_id: deployment.id,
            name: "default".to_string(),
            input_price_per_token: Decimal::new(1, 6),
            output_price_per_token: Decimal::new(2, 6),
            api_key_purpose: None,
            completion_window: None,
            valid_from: None,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // Load targets and update metrics
        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();

        // Verify model info gauge exists with expected labels
        assert!(output.contains("dwctl_model_info{"), "Should emit dwctl_model_info gauge");
        assert!(output.contains(r#"model="cache-info-test-model""#), "Should have model label");
        assert!(output.contains(r#"endpoint_host="api.openai.com""#), "Should extract endpoint host");
        assert!(output.contains(r#"is_metered="true""#), "Should be metered (tariff exists)");

        // Verify limit gauges
        assert!(output.contains("dwctl_model_rate_limit_rps{"), "Should emit rate limit gauge");
        assert!(
            output.contains("dwctl_model_concurrency_limit{"),
            "Should emit concurrency limit gauge"
        );
        assert!(output.contains("dwctl_model_batch_capacity{"), "Should emit batch capacity gauge");
        assert!(output.contains("dwctl_model_throughput_rps{"), "Should emit throughput gauge");

        // Verify group info gauge
        assert!(output.contains("dwctl_model_group_info{"), "Should emit group info gauge");
        assert!(
            output.contains(r#"group_name="cache-info-test-group""#),
            "Should have group name label"
        );
    }

    #[sqlx::test]
    async fn test_composite_model_component_metrics(pool: sqlx::PgPool) {
        let handle = ensure_recorder();
        let mut state = super::CacheInfoState::new();
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Create endpoint
        let mut tx = pool.begin().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut tx);
        let endpoint = repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: test_user.id,
                name: "comp-ep".to_string(),
                description: None,
                url: url::Url::from_str("https://api.test.com").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create component model
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Deployments::new(&mut tx);
        let component = repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "gpt-4o".to_string(),
                alias: "cache-info-comp-child".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: Some(endpoint.id),
                requests_per_second: None,
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: false,
                lb_strategy: None,
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: true,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create composite model
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Deployments::new(&mut tx);
        let composite = repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "composite".to_string(),
                alias: "cache-info-composite".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: None,
                requests_per_second: None,
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: true,
                lb_strategy: Some(LoadBalancingStrategy::WeightedRandom),
                fallback_enabled: Some(true),
                fallback_on_rate_limit: Some(true),
                fallback_on_status: Some(vec![429, 500]),
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: true,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Link component
        sqlx::query!(
            "INSERT INTO deployed_model_components (composite_model_id, deployed_model_id, weight, sort_order, enabled)
             VALUES ($1, $2, 80, 0, TRUE)",
            composite.id,
            component.id,
        )
        .execute(&pool)
        .await
        .unwrap();

        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();

        // Verify composite model appears in info
        assert!(
            output.contains(r#"model="cache-info-composite""#),
            "Composite model should appear in dwctl_model_info"
        );

        // Verify component weight gauge
        assert!(
            output.contains("dwctl_model_component_weight{"),
            "Should emit component weight gauge"
        );
        assert!(
            output.contains(r#"composite="cache-info-composite""#),
            "Should have composite label"
        );
        assert!(
            output.contains(r#"component="cache-info-comp-child""#),
            "Should have component label"
        );
    }

    #[sqlx::test]
    async fn test_no_gauges_for_missing_optional_fields(pool: sqlx::PgPool) {
        let handle = ensure_recorder();
        let mut state = super::CacheInfoState::new();
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Create endpoint
        let mut tx = pool.begin().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut tx);
        let endpoint = repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: test_user.id,
                name: "opt-ep".to_string(),
                description: None,
                url: url::Url::from_str("https://api.test.com").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create deployment with NO rate limit, capacity, batch_capacity, or throughput
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Deployments::new(&mut tx);
        repo.create(&DeploymentCreateDBRequest {
            created_by: test_user.id,
            model_name: "bare-model".to_string(),
            alias: "cache-info-bare".to_string(),
            description: None,
            model_type: None,
            capabilities: None,
            hosted_on: Some(endpoint.id),
            requests_per_second: None,
            burst_size: None,
            capacity: None,
            batch_capacity: None,
            throughput: None,
            provider_pricing: None,
            is_composite: false,
            lb_strategy: None,
            fallback_enabled: None,
            fallback_on_rate_limit: None,
            fallback_on_status: None,
            fallback_with_replacement: None,
            fallback_max_attempts: None,
            sanitize_responses: false,
            trusted: false,
            open_responses_adapter: true,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();

        // Model info should still appear
        assert!(
            output.contains(r#"model="cache-info-bare""#),
            "Bare model should appear in dwctl_model_info"
        );

        // Without a tariff, is_metered should be false
        assert!(
            output.contains(r#"is_metered="false""#),
            "Models without tariffs should be marked is_metered=\"false\""
        );

        // API key count gauge should exist (even if 0)
        // The DashMap has the model since load_targets_from_db creates targets
        assert!(output.contains("dwctl_model_api_key_count{"), "Should emit api key count gauge");
    }

    /// Helper to find all Prometheus lines matching a metric name and a label filter.
    /// Returns lines from the rendered output that contain both the metric name and
    /// the filter string (e.g. a specific label value).
    fn find_metric_lines<'a>(output: &'a str, metric: &str, filter: &str) -> Vec<&'a str> {
        output.lines().filter(|l| l.starts_with(metric) && l.contains(filter)).collect()
    }

    #[sqlx::test]
    async fn test_removed_group_is_zeroed(pool: sqlx::PgPool) {
        // When a group is removed from a model, the original series (with the
        // real group_name) should be zeroed. No phantom series with empty labels
        // should be created.
        let handle = ensure_recorder();
        let mut state = super::CacheInfoState::new();
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Create endpoint
        let mut tx = pool.begin().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut tx);
        let endpoint = repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: test_user.id,
                name: "phantom-ep".to_string(),
                description: None,
                url: url::Url::from_str("https://api.test.com").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create deployment
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Deployments::new(&mut tx);
        let deployment = repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "gpt-4o".to_string(),
                alias: "phantom-test-model".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: Some(endpoint.id),
                requests_per_second: None,
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: false,
                lb_strategy: None,
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: false,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create group and assign to deployment
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Groups::new(&mut tx);
        let group = repo
            .create(&GroupCreateDBRequest {
                created_by: test_user.id,
                name: "PhantomGroup".to_string(),
                description: None,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        sqlx::query!(
            "INSERT INTO deployment_groups (deployment_id, group_id) VALUES ($1, $2)",
            deployment.id,
            group.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Cycle 1: group is present — populates PREV_GROUPS
        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();
        let group_id_str = group.id.to_string();

        // Verify the original series exists with the real group name at 1.0
        let lines = find_metric_lines(&output, "dwctl_model_group_info", &group_id_str);
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r#"group_name="PhantomGroup""#) && l.ends_with(" 1")),
            "Original series should have group_name=\"PhantomGroup\" at 1.0, got: {:?}",
            lines
        );

        // Remove the group assignment
        sqlx::query!(
            "DELETE FROM deployment_groups WHERE deployment_id = $1 AND group_id = $2",
            deployment.id,
            group.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Cycle 2: group is gone — zeroing should zero the ORIGINAL series
        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();
        let lines = find_metric_lines(&output, "dwctl_model_group_info", &group_id_str);

        // The original series with group_name="PhantomGroup" should be zeroed
        let original_zeroed = lines
            .iter()
            .any(|l| l.contains(r#"group_name="PhantomGroup""#) && l.ends_with(" 0"));
        assert!(
            original_zeroed,
            "Original series {{group_name=\"PhantomGroup\"}} should be zeroed to 0. Lines: {:?}",
            lines
        );

        // No phantom series with group_name="" should exist
        let phantom_exists = lines.iter().any(|l| l.contains(r#"group_name="""#));
        assert!(
            !phantom_exists,
            "No phantom series with group_name=\"\" should be created. Lines: {:?}",
            lines
        );
    }

    #[sqlx::test]
    async fn test_removed_component_is_zeroed(pool: sqlx::PgPool) {
        // When a component is removed from a composite model, the original
        // series (with real sort_order/enabled labels) should be zeroed.
        // No phantom series with empty labels should be created.
        let handle = ensure_recorder();
        let mut state = super::CacheInfoState::new();
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Create endpoint
        let mut tx = pool.begin().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut tx);
        let endpoint = repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: test_user.id,
                name: "phantom-comp-ep".to_string(),
                description: None,
                url: url::Url::from_str("https://api.test.com").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create component model
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Deployments::new(&mut tx);
        let component = repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "gpt-4o".to_string(),
                alias: "phantom-comp-child".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: Some(endpoint.id),
                requests_per_second: None,
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: false,
                lb_strategy: None,
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: false,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create composite model
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Deployments::new(&mut tx);
        let composite = repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "composite".to_string(),
                alias: "phantom-composite".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: None,
                requests_per_second: None,
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: true,
                lb_strategy: Some(LoadBalancingStrategy::WeightedRandom),
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: false,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Link component with weight=70, sort_order=0, enabled=true
        sqlx::query!(
            "INSERT INTO deployed_model_components (composite_model_id, deployed_model_id, weight, sort_order, enabled)
             VALUES ($1, $2, 70, 0, TRUE)",
            composite.id,
            component.id,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Cycle 1: component is present
        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();
        let lines = find_metric_lines(&output, "dwctl_model_component_weight", r#"composite="phantom-composite""#);
        assert!(
            lines.iter().any(|l| l.contains(r#"component="phantom-comp-child""#)
                && l.contains(r#"sort_order="0""#)
                && l.contains(r#"enabled="true""#)
                && l.ends_with(" 70")),
            "Original component series should have real labels at weight 70. Lines: {:?}",
            lines
        );

        // Remove the component link
        sqlx::query!(
            "DELETE FROM deployed_model_components WHERE composite_model_id = $1 AND deployed_model_id = $2",
            composite.id,
            component.id,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Cycle 2: component is gone — should zero the original series
        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();
        let lines = find_metric_lines(&output, "dwctl_model_component_weight", r#"composite="phantom-composite""#);

        // The original series with real labels should be zeroed
        let original_zeroed = lines.iter().any(|l| {
            l.contains(r#"component="phantom-comp-child""#)
                && l.contains(r#"sort_order="0""#)
                && l.contains(r#"enabled="true""#)
                && l.ends_with(" 0")
        });
        assert!(
            original_zeroed,
            "Original component series should be zeroed to 0. Lines: {:?}",
            lines
        );

        // No phantom series with empty sort_order/enabled labels should exist
        let phantom_exists = lines.iter().any(|l| l.contains(r#"sort_order="""#) && l.contains(r#"enabled="""#));
        assert!(
            !phantom_exists,
            "No phantom series with empty sort_order/enabled labels should be created. Lines: {:?}",
            lines
        );
    }

    #[sqlx::test]
    async fn test_deleted_model_gauges_are_zeroed(pool: sqlx::PgPool) {
        // When a model is soft-deleted, all its gauges (info, rate limit,
        // concurrency, etc.) should be zeroed so dashboards reflect reality.
        let handle = ensure_recorder();
        let mut state = super::CacheInfoState::new();
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Create endpoint
        let mut tx = pool.begin().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut tx);
        let endpoint = repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: test_user.id,
                name: "ghost-ep".to_string(),
                description: None,
                url: url::Url::from_str("https://api.test.com").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create deployment with distinctive values
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Deployments::new(&mut tx);
        let deployment = repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "gpt-4o".to_string(),
                alias: "ghost-model".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: Some(endpoint.id),
                requests_per_second: Some(42.0),
                burst_size: None,
                capacity: Some(99),
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: false,
                lb_strategy: None,
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: false,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Cycle 1: model is active
        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();

        // Verify the model is emitting metrics
        let info_lines = find_metric_lines(&output, "dwctl_model_info", r#"model="ghost-model""#);
        assert!(
            info_lines.iter().any(|l| l.ends_with(" 1")),
            "dwctl_model_info should be 1.0 for active model"
        );

        let rps_lines = find_metric_lines(&output, "dwctl_model_rate_limit_rps", r#"model="ghost-model""#);
        assert!(
            rps_lines.iter().any(|l| l.ends_with(" 42")),
            "Rate limit should be 42 for active model"
        );

        // Soft-delete the model
        sqlx::query!("UPDATE deployed_models SET deleted = TRUE WHERE id = $1", deployment.id)
            .execute(&pool)
            .await
            .unwrap();

        // Cycle 2: model is deleted — gauges should be zeroed
        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();

        // dwctl_model_info should be zeroed to 0
        let info_lines = find_metric_lines(&output, "dwctl_model_info", r#"model="ghost-model""#);
        assert!(
            info_lines.iter().any(|l| l.ends_with(" 0")),
            "dwctl_model_info should be 0 after model deletion. Lines: {:?}",
            info_lines
        );

        // Rate limit gauge should be zeroed
        let rps_lines = find_metric_lines(&output, "dwctl_model_rate_limit_rps", r#"model="ghost-model""#);
        assert!(
            rps_lines.iter().any(|l| l.ends_with(" 0")),
            "Rate limit should be 0 after model deletion. Lines: {:?}",
            rps_lines
        );

        // Concurrency limit gauge should be zeroed
        let cap_lines = find_metric_lines(&output, "dwctl_model_concurrency_limit", r#"model="ghost-model""#);
        assert!(
            cap_lines.iter().any(|l| l.ends_with(" 0")),
            "Concurrency limit should be 0 after model deletion. Lines: {:?}",
            cap_lines
        );
    }

    #[sqlx::test]
    async fn test_metadata_change_preserves_single_label_gauges(pool: sqlx::PgPool) {
        // When a model's metadata changes (e.g., tariff added so is_metered
        // flips), the old info gauge series should be zeroed but single-label
        // gauges (rate_limit, concurrency, etc.) must NOT be zeroed because
        // the model still exists.
        let handle = ensure_recorder();
        let mut state = super::CacheInfoState::new();
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Create endpoint
        let mut tx = pool.begin().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut tx);
        let endpoint = repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: test_user.id,
                name: "meta-ep".to_string(),
                description: None,
                url: url::Url::from_str("https://api.test.com").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Create deployment with rate limit
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Deployments::new(&mut tx);
        let deployment = repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "gpt-4o".to_string(),
                alias: "meta-change-model".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: Some(endpoint.id),
                requests_per_second: Some(77.0),
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: false,
                lb_strategy: None,
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: false,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Cycle 1: model exists, is_metered=false (no tariff)
        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();
        let info_lines = find_metric_lines(&output, "dwctl_model_info", r#"model="meta-change-model""#);
        assert!(
            info_lines.iter().any(|l| l.contains(r#"is_metered="false""#) && l.ends_with(" 1")),
            "Info gauge should show is_metered=false before tariff. Lines: {:?}",
            info_lines
        );

        // Add a tariff — flips is_metered to true
        let mut tx = pool.begin().await.unwrap();
        let mut repo = Tariffs::new(&mut tx);
        repo.create(&TariffCreateDBRequest {
            deployed_model_id: deployment.id,
            name: "default".to_string(),
            input_price_per_token: Decimal::new(1, 6),
            output_price_per_token: Decimal::new(2, 6),
            api_key_purpose: None,
            completion_window: None,
            valid_from: None,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // Cycle 2: same model, but is_metered changed
        let targets = load_targets_from_db(&pool, &[], false).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets, &mut state).await.unwrap();

        let output = handle.render();

        // New info gauge with is_metered=true should be at 1.0
        let info_lines = find_metric_lines(&output, "dwctl_model_info", r#"model="meta-change-model""#);
        assert!(
            info_lines.iter().any(|l| l.contains(r#"is_metered="true""#) && l.ends_with(" 1")),
            "New info gauge should show is_metered=true. Lines: {:?}",
            info_lines
        );

        // Old info gauge with is_metered=false should be zeroed
        assert!(
            info_lines.iter().any(|l| l.contains(r#"is_metered="false""#) && l.ends_with(" 0")),
            "Old info gauge with is_metered=false should be zeroed. Lines: {:?}",
            info_lines
        );

        // Single-label gauges must NOT be zeroed — model still exists
        let rps_lines = find_metric_lines(&output, "dwctl_model_rate_limit_rps", r#"model="meta-change-model""#);
        assert!(
            rps_lines.iter().any(|l| l.ends_with(" 77")),
            "Rate limit should still be 77 after metadata change (not zeroed). Lines: {:?}",
            rps_lines
        );
    }
}
