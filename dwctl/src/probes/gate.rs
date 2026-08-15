use std::collections::HashMap;

use dashmap::DashMap;
use fusillade::ModelGateState;
use sqlx::PgPool;
use tracing::info;

const RESOLUTION_DEPTH_LIMIT: usize = 8;

pub struct CatalogHealth {
    pub leaf_health: HashMap<String, bool>,
    pub composite_primaries: HashMap<String, String>,
}

pub async fn load_catalog_health(pool: &PgPool) -> Result<CatalogHealth, anyhow::Error> {
    let leaves = sqlx::query!(
        r#"
        SELECT dm.alias, COALESCE(pr.success, TRUE) AS "healthy!"
        FROM deployed_models dm
        LEFT JOIN probes p ON p.deployment_id = dm.id AND p.active = TRUE
        LEFT JOIN LATERAL (
            SELECT r.success
            FROM probe_results r
            WHERE r.probe_id = p.id
              AND r.executed_at >= NOW() - make_interval(secs => (3 * p.interval_seconds)::double precision)
            ORDER BY r.executed_at DESC
            LIMIT 1
        ) pr ON TRUE
        WHERE dm.deleted = FALSE
          AND dm.is_composite = FALSE
        "#
    )
    .fetch_all(pool)
    .await?;

    let composites = sqlx::query!(
        r#"
        SELECT DISTINCT ON (cm.id) cm.alias AS composite_alias, dm.alias AS primary_alias
        FROM deployed_models cm
        INNER JOIN deployed_model_components dmc ON dmc.composite_model_id = cm.id AND dmc.enabled = TRUE
        INNER JOIN deployed_models dm ON dm.id = dmc.deployed_model_id AND dm.deleted = FALSE
        WHERE cm.deleted = FALSE
          AND cm.is_composite = TRUE
        ORDER BY cm.id, dmc.sort_order ASC, dmc.weight DESC, dmc.created_at ASC
        "#
    )
    .fetch_all(pool)
    .await?;

    Ok(CatalogHealth {
        leaf_health: leaves.into_iter().map(|row| (row.alias, row.healthy)).collect(),
        composite_primaries: composites.into_iter().map(|row| (row.composite_alias, row.primary_alias)).collect(),
    })
}

fn resolves_to_healthy_leaf(alias: &str, catalog: &CatalogHealth) -> bool {
    let mut current = alias;
    for _ in 0..RESOLUTION_DEPTH_LIMIT {
        if let Some(healthy) = catalog.leaf_health.get(current) {
            return *healthy;
        }
        match catalog.composite_primaries.get(current) {
            Some(primary) => current = primary,
            None => return true,
        }
    }
    true
}

pub fn compute_gate_states(catalog: &CatalogHealth) -> HashMap<String, ModelGateState> {
    catalog
        .leaf_health
        .keys()
        .chain(catalog.composite_primaries.keys())
        .map(|alias| {
            let state = if resolves_to_healthy_leaf(alias, catalog) {
                ModelGateState::Open
            } else {
                ModelGateState::Throttled
            };
            (alias.clone(), state)
        })
        .collect()
}

pub fn apply_gate_states(gate_states: &DashMap<String, ModelGateState>, computed: &HashMap<String, ModelGateState>) {
    for (alias, state) in computed {
        if *state == ModelGateState::Throttled && gate_states.insert(alias.clone(), ModelGateState::Throttled).is_none() {
            info!(model = %alias, "claim gate throttled model {alias}");
        }
    }
    gate_states.retain(|alias, _| {
        if computed.get(alias) == Some(&ModelGateState::Throttled) {
            true
        } else {
            info!(model = %alias, "reopened model {alias}");
            false
        }
    });
}

pub async fn refresh_gate_states(pool: &PgPool, gate_states: &DashMap<String, ModelGateState>) -> Result<(), anyhow::Error> {
    let catalog = load_catalog_health(pool).await?;
    apply_gate_states(gate_states, &compute_gate_states(&catalog));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn catalog(leaves: &[(&str, bool)], composites: &[(&str, &str)]) -> CatalogHealth {
        CatalogHealth {
            leaf_health: leaves.iter().map(|(alias, healthy)| (alias.to_string(), *healthy)).collect(),
            composite_primaries: composites
                .iter()
                .map(|(composite, primary)| (composite.to_string(), primary.to_string()))
                .collect(),
        }
    }

    #[test]
    fn leaf_health_maps_directly_to_gate_state() {
        let states = compute_gate_states(&catalog(&[("healthy", true), ("unhealthy", false)], &[]));
        assert_eq!(states["healthy"], ModelGateState::Open);
        assert_eq!(states["unhealthy"], ModelGateState::Throttled);
    }

    #[test]
    fn composite_takes_primary_leaf_health() {
        let states = compute_gate_states(&catalog(&[("primary", false), ("secondary", true)], &[("composite", "primary")]));
        assert_eq!(states["composite"], ModelGateState::Throttled);
        assert_eq!(states["secondary"], ModelGateState::Open);
    }

    #[test]
    fn nested_composites_resolve_recursively() {
        let states = compute_gate_states(&catalog(&[("leaf", false)], &[("outer", "inner"), ("inner", "leaf")]));
        assert_eq!(states["outer"], ModelGateState::Throttled);
    }

    #[test]
    fn resolution_dead_end_is_open() {
        let states = compute_gate_states(&catalog(&[], &[("composite", "vanished")]));
        assert_eq!(states["composite"], ModelGateState::Open);
    }

    #[test]
    fn resolution_cycle_is_bounded_and_open() {
        let states = compute_gate_states(&catalog(&[], &[("a", "b"), ("b", "a")]));
        assert_eq!(states["a"], ModelGateState::Open);
        assert_eq!(states["b"], ModelGateState::Open);
    }

    #[test]
    fn apply_keeps_map_sparse_and_only_names_throttled_models() {
        let map = DashMap::new();
        map.insert("reopened".to_string(), ModelGateState::Throttled);

        let computed = HashMap::from([
            ("reopened".to_string(), ModelGateState::Open),
            ("open".to_string(), ModelGateState::Open),
            ("throttled".to_string(), ModelGateState::Throttled),
        ]);
        apply_gate_states(&map, &computed);

        assert_eq!(map.len(), 1);
        assert_eq!(*map.get("throttled").unwrap(), ModelGateState::Throttled);
    }

    async fn create_endpoint(pool: &PgPool) -> Uuid {
        sqlx::query_scalar!(
            "INSERT INTO inference_endpoints (name, url, created_by) VALUES ($1, $2, $3) RETURNING id",
            format!("endpoint-{}", Uuid::new_v4()),
            "http://localhost:8080",
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn create_leaf_model(pool: &PgPool, alias: &str, endpoint_id: Uuid) -> Uuid {
        sqlx::query_scalar!(
            "INSERT INTO deployed_models (model_name, alias, type, hosted_on, created_by) VALUES ($1, $2, $3, $4, $5) RETURNING id",
            alias,
            alias,
            "chat" as _,
            endpoint_id,
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn create_composite(pool: &PgPool, alias: &str, components: &[(Uuid, i32)]) -> Uuid {
        let composite_id = sqlx::query_scalar!(
            "INSERT INTO deployed_models (model_name, alias, type, is_composite, created_by) VALUES ($1, $2, $3, TRUE, $4) RETURNING id",
            alias,
            alias,
            "chat" as _,
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap();

        for (deployed_model_id, sort_order) in components {
            sqlx::query!(
                "INSERT INTO deployed_model_components (composite_model_id, deployed_model_id, sort_order) VALUES ($1, $2, $3)",
                composite_id,
                deployed_model_id,
                sort_order
            )
            .execute(pool)
            .await
            .unwrap();
        }

        composite_id
    }

    async fn create_probe(pool: &PgPool, deployment_id: Uuid, active: bool) -> Uuid {
        sqlx::query_scalar!(
            "INSERT INTO probes (name, deployment_id, active) VALUES ($1, $2, $3) RETURNING id",
            format!("probe-{}", Uuid::new_v4()),
            deployment_id,
            active
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn record_result(pool: &PgPool, probe_id: Uuid, success: bool, age_seconds: f64) {
        sqlx::query!(
            "INSERT INTO probe_results (probe_id, executed_at, success) VALUES ($1, NOW() - make_interval(secs => $2), $3)",
            probe_id,
            age_seconds,
            success
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn healthy_and_unmonitored_leaves_stay_open(pool: PgPool) {
        let endpoint_id = create_endpoint(&pool).await;

        let healthy_id = create_leaf_model(&pool, "leaf-healthy", endpoint_id).await;
        let healthy_probe = create_probe(&pool, healthy_id, true).await;
        record_result(&pool, healthy_probe, true, 30.0).await;

        create_leaf_model(&pool, "leaf-no-probe", endpoint_id).await;

        let inactive_id = create_leaf_model(&pool, "leaf-inactive-probe", endpoint_id).await;
        let inactive_probe = create_probe(&pool, inactive_id, false).await;
        record_result(&pool, inactive_probe, false, 30.0).await;

        let no_results_id = create_leaf_model(&pool, "leaf-no-results", endpoint_id).await;
        create_probe(&pool, no_results_id, true).await;

        let gate_states = DashMap::new();
        refresh_gate_states(&pool, &gate_states).await.unwrap();
        assert!(gate_states.is_empty());
    }

    #[sqlx::test]
    async fn failed_recent_probe_throttles_then_reopens(pool: PgPool) {
        let endpoint_id = create_endpoint(&pool).await;
        let leaf_id = create_leaf_model(&pool, "leaf-failing", endpoint_id).await;
        let probe_id = create_probe(&pool, leaf_id, true).await;
        record_result(&pool, probe_id, false, 30.0).await;

        let gate_states = DashMap::new();
        refresh_gate_states(&pool, &gate_states).await.unwrap();
        assert_eq!(*gate_states.get("leaf-failing").unwrap(), ModelGateState::Throttled);
        assert_eq!(gate_states.len(), 1);

        record_result(&pool, probe_id, true, 5.0).await;
        refresh_gate_states(&pool, &gate_states).await.unwrap();
        assert!(gate_states.is_empty());
    }

    #[sqlx::test]
    async fn stale_failure_outside_window_is_healthy(pool: PgPool) {
        let endpoint_id = create_endpoint(&pool).await;
        let leaf_id = create_leaf_model(&pool, "leaf-stale", endpoint_id).await;
        let probe_id = create_probe(&pool, leaf_id, true).await;
        record_result(&pool, probe_id, false, 240.0).await;

        let gate_states = DashMap::new();
        refresh_gate_states(&pool, &gate_states).await.unwrap();
        assert!(gate_states.is_empty());
    }

    #[sqlx::test]
    async fn recent_failure_wins_over_older_success_in_window(pool: PgPool) {
        let endpoint_id = create_endpoint(&pool).await;
        let leaf_id = create_leaf_model(&pool, "leaf-flapping", endpoint_id).await;
        let probe_id = create_probe(&pool, leaf_id, true).await;
        record_result(&pool, probe_id, true, 120.0).await;
        record_result(&pool, probe_id, false, 30.0).await;

        let gate_states = DashMap::new();
        refresh_gate_states(&pool, &gate_states).await.unwrap();
        assert_eq!(*gate_states.get("leaf-flapping").unwrap(), ModelGateState::Throttled);
    }

    #[sqlx::test]
    async fn composite_follows_primary_component_health(pool: PgPool) {
        let endpoint_id = create_endpoint(&pool).await;

        let primary_id = create_leaf_model(&pool, "component-primary", endpoint_id).await;
        let primary_probe = create_probe(&pool, primary_id, true).await;
        record_result(&pool, primary_probe, false, 30.0).await;

        let secondary_id = create_leaf_model(&pool, "component-secondary", endpoint_id).await;
        let secondary_probe = create_probe(&pool, secondary_id, true).await;
        record_result(&pool, secondary_probe, true, 30.0).await;

        create_composite(&pool, "composite-model", &[(primary_id, 0), (secondary_id, 1)]).await;

        let gate_states = DashMap::new();
        refresh_gate_states(&pool, &gate_states).await.unwrap();
        assert_eq!(*gate_states.get("composite-model").unwrap(), ModelGateState::Throttled);
        assert_eq!(*gate_states.get("component-primary").unwrap(), ModelGateState::Throttled);
        assert!(!gate_states.contains_key("component-secondary"));

        record_result(&pool, primary_probe, true, 5.0).await;
        refresh_gate_states(&pool, &gate_states).await.unwrap();
        assert!(gate_states.is_empty());
    }

    #[sqlx::test]
    async fn composite_ignores_disabled_primary_component(pool: PgPool) {
        let endpoint_id = create_endpoint(&pool).await;

        let failing_id = create_leaf_model(&pool, "component-disabled", endpoint_id).await;
        let failing_probe = create_probe(&pool, failing_id, true).await;
        record_result(&pool, failing_probe, false, 30.0).await;

        let healthy_id = create_leaf_model(&pool, "component-enabled", endpoint_id).await;
        let healthy_probe = create_probe(&pool, healthy_id, true).await;
        record_result(&pool, healthy_probe, true, 30.0).await;

        let composite_id = create_composite(&pool, "composite-disabled-primary", &[(failing_id, 0), (healthy_id, 1)]).await;
        sqlx::query!(
            "UPDATE deployed_model_components SET enabled = FALSE WHERE composite_model_id = $1 AND deployed_model_id = $2",
            composite_id,
            failing_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let gate_states = DashMap::new();
        refresh_gate_states(&pool, &gate_states).await.unwrap();
        assert!(!gate_states.contains_key("composite-disabled-primary"));
        assert_eq!(*gate_states.get("component-disabled").unwrap(), ModelGateState::Throttled);
    }
}
