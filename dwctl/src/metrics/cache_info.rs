//! Prometheus info/gauge metrics for the onwards routing cache state.
//!
//! Emits gauge metrics that reflect the current model configuration loaded into
//! the onwards routing cache. Updated on every sync cycle (LISTEN/NOTIFY and
//! fallback). Uses the `metrics` crate facade — gauges appear at
//! `/internal/metrics` automatically when a recorder is installed.

use metrics::gauge;
use onwards::target::Targets;
use serde::Deserialize;
use sqlx::PgPool;
use tracing::warn;

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
/// iterates the Targets DashMap for API key counts. All gauges are set
/// unconditionally — stale series from deleted models persist in the exporter
/// but produce no PromQL joins since operational metrics also stop.
pub async fn update_cache_info_metrics(pool: &PgPool, targets: &Targets) -> Result<(), anyhow::Error> {
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

        // Info metric — constant 1.0, labels carry the metadata
        gauge!(
            "dwctl_model_info",
            "model" => alias.clone(),
            "model_name" => model_name.clone(),
            "model_type" => model_type.to_string(),
            "endpoint_name" => endpoint_name.to_string(),
            "endpoint_host" => endpoint_host,
            "is_composite" => is_composite.to_string(),
            "lb_strategy" => lb_strategy.to_string(),
            "sanitize_responses" => sanitize.to_string(),
            "is_metered" => is_metered.to_string(),
        )
        .set(1.0);

        // Rate limit gauge
        if let Some(rps) = row.requests_per_second {
            gauge!("dwctl_model_rate_limit_rps", "model" => alias.clone()).set(rps as f64);
        }

        // Concurrency limit gauge
        if let Some(capacity) = row.capacity {
            gauge!("dwctl_model_concurrency_limit", "model" => alias.clone()).set(capacity as f64);
        }

        // Batch capacity gauge
        if let Some(batch_capacity) = row.batch_capacity {
            gauge!("dwctl_model_batch_capacity", "model" => alias.clone()).set(batch_capacity as f64);
        }

        // Throughput gauge
        if let Some(throughput) = row.throughput {
            gauge!("dwctl_model_throughput_rps", "model" => alias.clone()).set(throughput as f64);
        }

        // Group info metrics — one gauge per (model, group) pair
        if let Some(ref json) = row.groups_json {
            match serde_json::from_str::<Vec<GroupInfo>>(json) {
                Ok(groups) => {
                    for g in &groups {
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
                sanitize_responses: true,
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
        let targets = load_targets_from_db(&pool, &[]).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets).await.unwrap();

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
                sanitize_responses: true,
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
                sanitize_responses: true,
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

        let targets = load_targets_from_db(&pool, &[]).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets).await.unwrap();

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
            sanitize_responses: false,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let targets = load_targets_from_db(&pool, &[]).await.unwrap();
        super::update_cache_info_metrics(&pool, &targets).await.unwrap();

        let output = handle.render();

        // Model info should still appear
        assert!(
            output.contains(r#"model="cache-info-bare""#),
            "Bare model should appear in dwctl_model_info"
        );

        // No metering without tariff
        assert!(output.contains(r#"model="cache-info-bare""#), "Model should appear");

        // API key count gauge should exist (even if 0)
        // The DashMap has the model since load_targets_from_db creates targets
        assert!(output.contains("dwctl_model_api_key_count{"), "Should emit api key count gauge");
    }
}
