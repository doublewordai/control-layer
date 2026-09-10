//! Transaction-scoped persistence for the declarative model catalog.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::model_provisioning::{
    CacheTariff, Catalog, CatalogModel, ClayModel, Component, PhysicalDeployment, ProviderPricing, Tariff, TrafficRule, parse_decimal,
    parse_per_million,
};

/// A fixed application-level lock ID. Transaction scope makes a crashed startup
/// release it automatically and works through transaction-pooled connections.
const MODEL_PROVISIONING_LOCK: i64 = 0x4457_4d4f_4445_4c50;

pub struct ModelProvisioning<'c> {
    db: &'c mut PgConnection,
}

#[derive(Debug, Clone)]
struct DesiredModel<'a> {
    source: &'a str,
    canonical_model: &'a str,
    clay: &'a ClayModel,
    physical: Option<&'a PhysicalDeployment>,
}

impl<'c> ModelProvisioning<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    pub async fn apply(&mut self, catalog: &Catalog) -> Result<()> {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(MODEL_PROVISIONING_LOCK)
            .execute(&mut *self.db)
            .await
            .context("acquire model provisioning advisory lock")?;

        let effective_at: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
            .fetch_one(&mut *self.db)
            .await
            .context("read model provisioning effective timestamp")?;

        let (endpoints, groups) = self.preflight_named_references(catalog).await?;
        self.preflight_existing_model_types(catalog).await?;
        self.preflight_future_tariffs(catalog, effective_at).await?;

        // This deliberately clears the whole column, not only sources in this
        // directory. The transaction prevents readers observing the interim state.
        sqlx::query("UPDATE deployed_models SET provisioning_source = NULL WHERE provisioning_source IS NOT NULL")
            .execute(&mut *self.db)
            .await
            .context("clear model provisioning source markers")?;

        let mut ids = HashMap::new();
        // Physical rows first so virtual component references can resolve without IDs in YAML.
        for model in &catalog.models {
            for physical in &model.clay.deployments {
                let desired = DesiredModel {
                    source: &model.source,
                    canonical_model: &model.canonical_model,
                    clay: &model.clay,
                    physical: Some(physical),
                };
                let endpoint_id = endpoints[&physical.endpoint];
                let id = self.upsert_model(&desired, Some(endpoint_id), effective_at).await?;
                ids.insert(physical.alias.clone(), id);
            }
        }
        for model in &catalog.models {
            let desired = DesiredModel {
                source: &model.source,
                canonical_model: &model.canonical_model,
                clay: &model.clay,
                physical: None,
            };
            let id = self.upsert_model(&desired, None, effective_at).await?;
            ids.insert(model.clay.alias.clone(), id);
        }

        let redirect_ids = self.resolve_redirect_targets(catalog, &ids).await?;
        for model in &catalog.models {
            let model_id = ids[&model.clay.alias];
            self.reconcile_components(model_id, model, &ids).await?;
            self.reconcile_tariffs(model_id, &model.clay.tariffs, effective_at).await?;
            self.reconcile_cache_tariff(model_id, model.clay.cache_tariff.as_ref(), effective_at)
                .await?;
            self.reconcile_groups(model_id, &model.clay.access_groups, &groups).await?;
            self.reconcile_traffic_rules(model_id, &model.clay.traffic_rules, &redirect_ids)
                .await?;
        }

        tracing::info!(models = catalog.models.len(), %effective_at, "Applied declarative model catalog");
        Ok(())
    }

    async fn preflight_named_references(&mut self, catalog: &Catalog) -> Result<(HashMap<String, Uuid>, HashMap<String, Uuid>)> {
        let endpoint_names: HashSet<String> = catalog
            .models
            .iter()
            .flat_map(|model| model.clay.deployments.iter().map(|deployment| deployment.endpoint.clone()))
            .collect();
        let group_names: HashSet<String> = catalog
            .models
            .iter()
            .flat_map(|model| model.clay.access_groups.iter().cloned())
            .collect();

        let endpoints = resolve_names(self.db, "inference_endpoints", endpoint_names).await?;
        let groups = resolve_names(self.db, "groups", group_names).await?;
        Ok((endpoints, groups))
    }

    async fn preflight_existing_model_types(&mut self, catalog: &Catalog) -> Result<()> {
        let mut expected = HashMap::new();
        for model in &catalog.models {
            expected.insert(model.clay.alias.clone(), true);
            for deployment in &model.clay.deployments {
                expected.insert(deployment.alias.clone(), false);
            }
        }
        let aliases: Vec<String> = expected.keys().cloned().collect();
        let rows = sqlx::query("SELECT alias, is_composite FROM deployed_models WHERE alias = ANY($1)")
            .bind(&aliases)
            .fetch_all(&mut *self.db)
            .await
            .context("resolve existing model aliases")?;
        for row in rows {
            let alias: String = row.try_get("alias")?;
            let actual: bool = row.try_get("is_composite")?;
            ensure!(
                expected[&alias] == actual,
                "model alias {alias:?} already exists as a {} model",
                if actual { "virtual" } else { "physical" }
            );
        }
        Ok(())
    }

    async fn preflight_future_tariffs(&mut self, catalog: &Catalog, effective_at: DateTime<Utc>) -> Result<()> {
        let aliases: Vec<String> = catalog.models.iter().map(|model| model.clay.alias.clone()).collect();
        let future_customer = sqlx::query(
            r#"SELECT dm.alias, mt.valid_from
               FROM model_tariffs mt
               JOIN deployed_models dm ON dm.id = mt.deployed_model_id
               WHERE dm.alias = ANY($1)
                 AND mt.valid_from > $2
                 AND (mt.valid_until IS NULL OR mt.valid_until > mt.valid_from)
               ORDER BY mt.valid_from
               LIMIT 1"#,
        )
        .bind(&aliases)
        .bind(effective_at)
        .fetch_optional(&mut *self.db)
        .await
        .context("check future model tariffs")?;
        if let Some(row) = future_customer {
            let alias: String = row.try_get("alias")?;
            let valid_from: DateTime<Utc> = row.try_get("valid_from")?;
            bail!(
                "provisioned model {alias:?} has a future tariff scheduled for {valid_from}; startup provisioning has no scheduling semantics"
            );
        }

        let future_cache = sqlx::query(
            r#"SELECT dm.alias, mt.valid_from
               FROM model_cache_tariffs mt
               JOIN deployed_models dm ON dm.id = mt.deployed_model_id
               WHERE dm.alias = ANY($1)
                 AND mt.valid_from > $2
                 AND (mt.valid_until IS NULL OR mt.valid_until > mt.valid_from)
               ORDER BY mt.valid_from
               LIMIT 1"#,
        )
        .bind(&aliases)
        .bind(effective_at)
        .fetch_optional(&mut *self.db)
        .await
        .context("check future cache tariffs")?;
        if let Some(row) = future_cache {
            let alias: String = row.try_get("alias")?;
            let valid_from: DateTime<Utc> = row.try_get("valid_from")?;
            bail!(
                "provisioned model {alias:?} has future cache pricing scheduled for {valid_from}; startup provisioning has no scheduling semantics"
            );
        }
        Ok(())
    }

    async fn upsert_model(&mut self, desired: &DesiredModel<'_>, hosted_on: Option<Uuid>, effective_at: DateTime<Utc>) -> Result<Uuid> {
        let (alias, model_name, display_name, description, model_type, capabilities, settings, provider_pricing, is_composite) =
            match desired.physical {
                Some(physical) => (
                    physical.alias.as_str(),
                    physical.model_name.as_str(),
                    physical.display_name.as_deref(),
                    physical.description.as_deref(),
                    physical.model_type,
                    physical.capabilities.as_deref(),
                    &physical.settings,
                    physical.provider_pricing.as_ref(),
                    false,
                ),
                None => (
                    desired.clay.alias.as_str(),
                    desired.clay.model_name.as_deref().unwrap_or(desired.canonical_model),
                    desired.clay.display_name.as_deref(),
                    desired.clay.description.as_deref(),
                    desired.clay.model_type,
                    desired.clay.capabilities.as_deref(),
                    &desired.clay.settings,
                    None,
                    true,
                ),
            };
        let source = format!("model-catalog:{}", desired.source);
        let pricing = pricing_fields(provider_pricing)?;
        let fallback = &desired.clay.routing.fallback;
        let backoff_enabled = is_composite && fallback.backoff.is_some();
        let backoff_initial = fallback.backoff.as_ref().map_or(100, |backoff| backoff.initial_ms);
        let backoff_max = fallback.backoff.as_ref().map_or(5_000, |backoff| backoff.max_ms);
        let backoff_factor = fallback.backoff.as_ref().map_or(2.0, |backoff| backoff.factor);
        let backoff_jitter = fallback.backoff.as_ref().map_or("full", |backoff| backoff.jitter.as_db_str());

        sqlx::query(
            r#"INSERT INTO deployed_models (
                   model_name, alias, display_name, description, type, capabilities, created_by, hosted_on,
                   requests_per_second, burst_size, capacity, batch_capacity, throughput,
                   downstream_pricing_mode, downstream_input_price_per_token, downstream_output_price_per_token,
                   downstream_hourly_rate, downstream_input_token_cost_ratio,
                   is_composite, lb_strategy, fallback_enabled, fallback_on_rate_limit, fallback_on_status,
                   fallback_with_replacement, fallback_max_attempts, backoff_enabled, backoff_initial_ms,
                   backoff_max_ms, backoff_factor, backoff_jitter, backoff_max_total_ms,
                   sanitize_responses, trusted, allowed_batch_completion_windows, metadata,
                   reasoning_translation_overrides, provisioning_source, deleted, updated_at
               ) VALUES (
                   $1,$2,$3,$4,$5,$6,'00000000-0000-0000-0000-000000000000',$7,
                   $8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,
                   $27,$28,$29,$30,$31,$32,$33,$34,$35,$36,FALSE,$37
               )
               ON CONFLICT (alias) DO UPDATE SET
                   model_name = EXCLUDED.model_name,
                   display_name = EXCLUDED.display_name,
                   description = EXCLUDED.description,
                   type = EXCLUDED.type,
                   capabilities = EXCLUDED.capabilities,
                   hosted_on = EXCLUDED.hosted_on,
                   requests_per_second = EXCLUDED.requests_per_second,
                   burst_size = EXCLUDED.burst_size,
                   capacity = EXCLUDED.capacity,
                   batch_capacity = EXCLUDED.batch_capacity,
                   throughput = EXCLUDED.throughput,
                   downstream_pricing_mode = EXCLUDED.downstream_pricing_mode,
                   downstream_input_price_per_token = EXCLUDED.downstream_input_price_per_token,
                   downstream_output_price_per_token = EXCLUDED.downstream_output_price_per_token,
                   downstream_hourly_rate = EXCLUDED.downstream_hourly_rate,
                   downstream_input_token_cost_ratio = EXCLUDED.downstream_input_token_cost_ratio,
                   lb_strategy = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.lb_strategy ELSE deployed_models.lb_strategy END,
                   fallback_enabled = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_enabled ELSE deployed_models.fallback_enabled END,
                   fallback_on_rate_limit = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_on_rate_limit ELSE deployed_models.fallback_on_rate_limit END,
                   fallback_on_status = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_on_status ELSE deployed_models.fallback_on_status END,
                   fallback_with_replacement = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_with_replacement ELSE deployed_models.fallback_with_replacement END,
                   fallback_max_attempts = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_max_attempts ELSE deployed_models.fallback_max_attempts END,
                   backoff_enabled = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_enabled ELSE deployed_models.backoff_enabled END,
                   backoff_initial_ms = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_initial_ms ELSE deployed_models.backoff_initial_ms END,
                   backoff_max_ms = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_max_ms ELSE deployed_models.backoff_max_ms END,
                   backoff_factor = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_factor ELSE deployed_models.backoff_factor END,
                   backoff_jitter = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_jitter ELSE deployed_models.backoff_jitter END,
                   backoff_max_total_ms = CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_max_total_ms ELSE deployed_models.backoff_max_total_ms END,
                   sanitize_responses = EXCLUDED.sanitize_responses,
                   trusted = EXCLUDED.trusted,
                   allowed_batch_completion_windows = EXCLUDED.allowed_batch_completion_windows,
                   metadata = EXCLUDED.metadata,
                   reasoning_translation_overrides = EXCLUDED.reasoning_translation_overrides,
                   provisioning_source = EXCLUDED.provisioning_source,
                   deleted = FALSE,
                   updated_at = CASE WHEN ROW(
                       deployed_models.model_name, deployed_models.display_name, deployed_models.description,
                       deployed_models.type, deployed_models.capabilities, deployed_models.hosted_on,
                       deployed_models.requests_per_second, deployed_models.burst_size, deployed_models.capacity,
                       deployed_models.batch_capacity, deployed_models.throughput, deployed_models.downstream_pricing_mode,
                       deployed_models.downstream_input_price_per_token, deployed_models.downstream_output_price_per_token,
                       deployed_models.downstream_hourly_rate, deployed_models.downstream_input_token_cost_ratio,
                       deployed_models.lb_strategy, deployed_models.fallback_enabled, deployed_models.fallback_on_rate_limit,
                       deployed_models.fallback_on_status, deployed_models.fallback_with_replacement,
                       deployed_models.fallback_max_attempts, deployed_models.backoff_enabled,
                       deployed_models.backoff_initial_ms, deployed_models.backoff_max_ms, deployed_models.backoff_factor,
                       deployed_models.backoff_jitter, deployed_models.backoff_max_total_ms,
                       deployed_models.sanitize_responses, deployed_models.trusted,
                       deployed_models.allowed_batch_completion_windows, deployed_models.metadata,
                       deployed_models.reasoning_translation_overrides, deployed_models.deleted
                   ) IS DISTINCT FROM ROW(
                       EXCLUDED.model_name, EXCLUDED.display_name, EXCLUDED.description,
                       EXCLUDED.type, EXCLUDED.capabilities, EXCLUDED.hosted_on,
                       EXCLUDED.requests_per_second, EXCLUDED.burst_size, EXCLUDED.capacity,
                       EXCLUDED.batch_capacity, EXCLUDED.throughput, EXCLUDED.downstream_pricing_mode,
                       EXCLUDED.downstream_input_price_per_token, EXCLUDED.downstream_output_price_per_token,
                       EXCLUDED.downstream_hourly_rate, EXCLUDED.downstream_input_token_cost_ratio,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.lb_strategy ELSE deployed_models.lb_strategy END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_enabled ELSE deployed_models.fallback_enabled END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_on_rate_limit ELSE deployed_models.fallback_on_rate_limit END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_on_status ELSE deployed_models.fallback_on_status END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_with_replacement ELSE deployed_models.fallback_with_replacement END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.fallback_max_attempts ELSE deployed_models.fallback_max_attempts END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_enabled ELSE deployed_models.backoff_enabled END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_initial_ms ELSE deployed_models.backoff_initial_ms END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_max_ms ELSE deployed_models.backoff_max_ms END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_factor ELSE deployed_models.backoff_factor END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_jitter ELSE deployed_models.backoff_jitter END,
                       CASE WHEN EXCLUDED.is_composite THEN EXCLUDED.backoff_max_total_ms ELSE deployed_models.backoff_max_total_ms END,
                       EXCLUDED.sanitize_responses, EXCLUDED.trusted, EXCLUDED.allowed_batch_completion_windows,
                       EXCLUDED.metadata, EXCLUDED.reasoning_translation_overrides, FALSE
                   ) THEN $37 ELSE deployed_models.updated_at END"#,
        )
        .bind(model_name)
        .bind(alias)
        .bind(display_name)
        .bind(description)
        .bind(model_type.map(|kind| kind.as_db_str()))
        .bind(capabilities)
        .bind(hosted_on)
        .bind(settings.requests_per_second)
        .bind(settings.burst_size)
        .bind(settings.capacity)
        .bind(settings.batch_capacity)
        .bind(settings.throughput)
        .bind(pricing.mode)
        .bind(pricing.input)
        .bind(pricing.output)
        .bind(pricing.hourly)
        .bind(pricing.input_ratio)
        .bind(is_composite)
        .bind(desired.clay.routing.strategy.as_db_str())
        .bind(if is_composite { fallback.enabled } else { false })
        .bind(if is_composite { fallback.on_rate_limit } else { false })
        .bind(if is_composite { fallback.on_status.as_slice() } else { &[] })
        .bind(if is_composite { fallback.with_replacement } else { false })
        .bind(if is_composite { fallback.max_attempts } else { None })
        .bind(backoff_enabled)
        .bind(backoff_initial)
        .bind(backoff_max)
        .bind(backoff_factor)
        .bind(backoff_jitter)
        .bind(if is_composite { fallback.max_total_backoff_ms } else { None })
        .bind(settings.sanitize_responses)
        .bind(settings.trusted)
        .bind(settings.allowed_batch_completion_windows.as_deref())
        .bind(&settings.metadata)
        .bind(&settings.reasoning_translation_overrides)
        .bind(source)
        .bind(effective_at)
        .execute(&mut *self.db)
        .await
        .with_context(|| format!("upsert model alias {alias:?}"))?;

        sqlx::query_scalar("SELECT id FROM deployed_models WHERE alias = $1")
            .bind(alias)
            .fetch_one(&mut *self.db)
            .await
            .with_context(|| format!("resolve upserted model alias {alias:?}"))
    }

    async fn resolve_redirect_targets(
        &mut self,
        catalog: &Catalog,
        provisioned_ids: &HashMap<String, Uuid>,
    ) -> Result<HashMap<String, Uuid>> {
        let targets: HashSet<String> = catalog
            .models
            .iter()
            .flat_map(|model| model.clay.traffic_rules.iter())
            .filter_map(|rule| match rule {
                TrafficRule::Redirect { target, .. } => Some(target.clone()),
                TrafficRule::Deny { .. } => None,
            })
            .collect();
        let unresolved: HashSet<String> = targets
            .iter()
            .filter(|target| !provisioned_ids.contains_key(*target))
            .cloned()
            .collect();
        let mut resolved = resolve_model_aliases(self.db, unresolved).await?;
        for target in targets {
            if let Some(id) = provisioned_ids.get(&target) {
                resolved.insert(target, *id);
            }
        }
        Ok(resolved)
    }

    async fn reconcile_components(&mut self, composite_id: Uuid, model: &CatalogModel, ids: &HashMap<String, Uuid>) -> Result<()> {
        let rows = sqlx::query("SELECT id, deployed_model_id, pool FROM deployed_model_components WHERE composite_model_id = $1")
            .bind(composite_id)
            .fetch_all(&mut *self.db)
            .await
            .context("read existing model components")?;
        let mut stale: HashMap<(Uuid, String), Uuid> = rows
            .into_iter()
            .map(|row| {
                let id = row.try_get("id")?;
                let deployed_model_id = row.try_get("deployed_model_id")?;
                let pool = row.try_get("pool")?;
                Ok(((deployed_model_id, pool), id))
            })
            .collect::<Result<_, sqlx::Error>>()?;

        for (pool, components) in &model.clay.routing.pools {
            for component in components {
                let deployed_id = ids[&component.deployment];
                stale.remove(&(deployed_id, pool.clone()));
                self.upsert_component(composite_id, deployed_id, pool, component).await?;
            }
        }
        let stale_ids: Vec<Uuid> = stale.into_values().collect();
        if !stale_ids.is_empty() {
            sqlx::query("DELETE FROM deployed_model_components WHERE id = ANY($1)")
                .bind(&stale_ids)
                .execute(&mut *self.db)
                .await
                .context("delete omitted model components")?;
        }
        Ok(())
    }

    async fn upsert_component(&mut self, composite_id: Uuid, deployed_id: Uuid, pool: &str, component: &Component) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO deployed_model_components (
                   composite_model_id, deployed_model_id, pool, weight, enabled, sort_order,
                   strip_leading_bos, render_kwargs
               ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
               ON CONFLICT (composite_model_id, deployed_model_id, pool) DO UPDATE SET
                   weight = EXCLUDED.weight,
                   enabled = EXCLUDED.enabled,
                   sort_order = EXCLUDED.sort_order,
                   continuation_validated_at = CASE
                       WHEN deployed_model_components.strip_leading_bos IS DISTINCT FROM EXCLUDED.strip_leading_bos
                         OR deployed_model_components.render_kwargs IS DISTINCT FROM EXCLUDED.render_kwargs
                       THEN NULL ELSE deployed_model_components.continuation_validated_at END,
                   strip_leading_bos = EXCLUDED.strip_leading_bos,
                   render_kwargs = EXCLUDED.render_kwargs"#,
        )
        .bind(composite_id)
        .bind(deployed_id)
        .bind(pool)
        .bind(component.weight)
        .bind(component.enabled)
        .bind(component.sort_order)
        .bind(component.strip_leading_bos)
        .bind(&component.render_kwargs)
        .execute(&mut *self.db)
        .await
        .context("upsert model component")?;
        Ok(())
    }

    async fn reconcile_tariffs(&mut self, model_id: Uuid, desired: &[Tariff], effective_at: DateTime<Utc>) -> Result<()> {
        let rows = sqlx::query(
            r#"SELECT id, name, input_price_per_token, output_price_per_token,
                      api_key_purpose, completion_window, valid_until
               FROM model_tariffs
               WHERE deployed_model_id = $1
                 AND valid_from <= $2
                 AND (valid_until IS NULL OR valid_until > $2)
               ORDER BY valid_from DESC"#,
        )
        .bind(model_id)
        .bind(effective_at)
        .fetch_all(&mut *self.db)
        .await
        .context("read active model tariffs")?;

        let mut active = HashMap::new();
        let mut legacy_or_duplicate = Vec::new();
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            let purpose: Option<String> = row.try_get("api_key_purpose")?;
            let window: Option<String> = row.try_get("completion_window")?;
            if let Some(purpose) = purpose {
                if active.contains_key(&(purpose.clone(), window.clone())) {
                    legacy_or_duplicate.push(id);
                } else {
                    active.insert((purpose, window), row);
                }
            } else {
                legacy_or_duplicate.push(id);
            }
        }

        // Close malformed legacy/overlapping rows before inserting successors,
        // otherwise the active-row unique indexes can reject the replacement.
        if !legacy_or_duplicate.is_empty() {
            sqlx::query("UPDATE model_tariffs SET valid_until = $2 WHERE id = ANY($1)")
                .bind(&legacy_or_duplicate)
                .bind(effective_at)
                .execute(&mut *self.db)
                .await
                .context("close duplicate or unkeyed model tariffs")?;
        }

        for tariff in desired {
            let key = (tariff.purpose.as_db_str().to_string(), tariff.completion_window.clone());
            let input = parse_per_million(&tariff.input_per_million_tokens)?;
            let output = parse_per_million(&tariff.output_per_million_tokens)?;
            if let Some(row) = active.remove(&key) {
                let unchanged = row.try_get::<String, _>("name")? == tariff.name
                    && row.try_get::<Decimal, _>("input_price_per_token")? == input
                    && row.try_get::<Decimal, _>("output_price_per_token")? == output
                    && row.try_get::<Option<DateTime<Utc>>, _>("valid_until")?.is_none();
                if unchanged {
                    continue;
                }
                let id: Uuid = row.try_get("id")?;
                close_tariff(self.db, "model_tariffs", id, effective_at).await?;
            }
            sqlx::query(
                r#"INSERT INTO model_tariffs (
                       deployed_model_id, name, input_price_per_token, output_price_per_token,
                       valid_from, api_key_purpose, completion_window
                   ) VALUES ($1,$2,$3,$4,$5,$6,$7)"#,
            )
            .bind(model_id)
            .bind(&tariff.name)
            .bind(input)
            .bind(output)
            .bind(effective_at)
            .bind(tariff.purpose.as_db_str())
            .bind(&tariff.completion_window)
            .execute(&mut *self.db)
            .await
            .with_context(|| format!("insert replacement tariff {:?}", tariff.name))?;
        }

        let omitted: Vec<Uuid> = active
            .into_values()
            .map(|row| row.try_get("id"))
            .collect::<Result<_, sqlx::Error>>()?;
        if !omitted.is_empty() {
            sqlx::query("UPDATE model_tariffs SET valid_until = $2 WHERE id = ANY($1)")
                .bind(&omitted)
                .bind(effective_at)
                .execute(&mut *self.db)
                .await
                .context("close omitted model tariffs")?;
        }
        Ok(())
    }

    async fn reconcile_cache_tariff(&mut self, model_id: Uuid, desired: Option<&CacheTariff>, effective_at: DateTime<Utc>) -> Result<()> {
        let rows = sqlx::query(
            r#"SELECT id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h,
                      read_multiplier, min_prefix_tokens, valid_until
               FROM model_cache_tariffs
               WHERE deployed_model_id = $1
                 AND valid_from <= $2
                 AND (valid_until IS NULL OR valid_until > $2)
               ORDER BY valid_from DESC"#,
        )
        .bind(model_id)
        .bind(effective_at)
        .fetch_all(&mut *self.db)
        .await
        .context("read active cache tariffs")?;
        let mut rows = rows.into_iter();
        let active = rows.next();
        let overlapping: Vec<Uuid> = rows.map(|row| row.try_get("id")).collect::<Result<_, sqlx::Error>>()?;
        if !overlapping.is_empty() {
            sqlx::query("UPDATE model_cache_tariffs SET valid_until = $2 WHERE id = ANY($1)")
                .bind(&overlapping)
                .bind(effective_at)
                .execute(&mut *self.db)
                .await
                .context("close overlapping cache tariffs")?;
        }

        let Some(desired) = desired else {
            if let Some(row) = active {
                close_tariff(self.db, "model_cache_tariffs", row.try_get("id")?, effective_at).await?;
            }
            return Ok(());
        };
        let write_5m = parse_decimal(&desired.write_multiplier_5m)?;
        let write_1h = parse_decimal(&desired.write_multiplier_1h)?;
        let write_24h = parse_decimal(&desired.write_multiplier_24h)?;
        let read = parse_decimal(&desired.read_multiplier)?;
        if let Some(row) = active {
            let unchanged = row.try_get::<Decimal, _>("write_multiplier_5m")? == write_5m
                && row.try_get::<Decimal, _>("write_multiplier_1h")? == write_1h
                && row.try_get::<Decimal, _>("write_multiplier_24h")? == write_24h
                && row.try_get::<Decimal, _>("read_multiplier")? == read
                && row.try_get::<i32, _>("min_prefix_tokens")? == desired.min_prefix_tokens
                && row.try_get::<Option<DateTime<Utc>>, _>("valid_until")?.is_none();
            if unchanged {
                return Ok(());
            }
            close_tariff(self.db, "model_cache_tariffs", row.try_get("id")?, effective_at).await?;
        }
        sqlx::query(
            r#"INSERT INTO model_cache_tariffs (
                   deployed_model_id, write_multiplier_5m, write_multiplier_1h,
                   write_multiplier_24h, read_multiplier, min_prefix_tokens, valid_from
               ) VALUES ($1,$2,$3,$4,$5,$6,$7)"#,
        )
        .bind(model_id)
        .bind(write_5m)
        .bind(write_1h)
        .bind(write_24h)
        .bind(read)
        .bind(desired.min_prefix_tokens)
        .bind(effective_at)
        .execute(&mut *self.db)
        .await
        .context("insert replacement cache tariff")?;
        Ok(())
    }

    async fn reconcile_groups(&mut self, model_id: Uuid, desired: &[String], groups: &HashMap<String, Uuid>) -> Result<()> {
        sqlx::query("DELETE FROM deployment_groups WHERE deployment_id = $1")
            .bind(model_id)
            .execute(&mut *self.db)
            .await
            .context("clear provisioned model access groups")?;
        for name in desired {
            sqlx::query(
                "INSERT INTO deployment_groups (deployment_id, group_id, granted_by) VALUES ($1,$2,'00000000-0000-0000-0000-000000000000')",
            )
            .bind(model_id)
            .bind(groups[name])
            .execute(&mut *self.db)
            .await
            .with_context(|| format!("grant access group {name:?}"))?;
        }
        Ok(())
    }

    async fn reconcile_traffic_rules(
        &mut self,
        model_id: Uuid,
        desired: &[TrafficRule],
        redirect_ids: &HashMap<String, Uuid>,
    ) -> Result<()> {
        sqlx::query("DELETE FROM model_traffic_rules WHERE deployed_model_id = $1")
            .bind(model_id)
            .execute(&mut *self.db)
            .await
            .context("clear provisioned traffic rules")?;
        for rule in desired {
            let (action, target) = match rule {
                TrafficRule::Deny { .. } => ("deny", None),
                TrafficRule::Redirect { target, .. } => ("redirect", Some(redirect_ids[target])),
            };
            sqlx::query(
                "INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action, redirect_target_id) VALUES ($1,$2,$3,$4)",
            )
            .bind(model_id)
            .bind(rule.purpose().as_db_str())
            .bind(action)
            .bind(target)
            .execute(&mut *self.db)
            .await
            .context("insert provisioned traffic rule")?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct PricingFields {
    mode: Option<&'static str>,
    input: Option<Decimal>,
    output: Option<Decimal>,
    hourly: Option<Decimal>,
    input_ratio: Option<Decimal>,
}

fn pricing_fields(pricing: Option<&ProviderPricing>) -> Result<PricingFields> {
    Ok(match pricing {
        Some(ProviderPricing::PerToken {
            input_per_million_tokens,
            output_per_million_tokens,
        }) => PricingFields {
            mode: Some("per_token"),
            input: input_per_million_tokens.as_deref().map(parse_per_million).transpose()?,
            output: output_per_million_tokens.as_deref().map(parse_per_million).transpose()?,
            ..Default::default()
        },
        Some(ProviderPricing::Hourly {
            rate,
            input_token_cost_ratio,
        }) => PricingFields {
            mode: Some("hourly"),
            hourly: Some(parse_decimal(rate)?),
            input_ratio: Some(parse_decimal(input_token_cost_ratio)?),
            ..Default::default()
        },
        None => PricingFields::default(),
    })
}

async fn resolve_names(db: &mut PgConnection, table: &str, requested: HashSet<String>) -> Result<HashMap<String, Uuid>> {
    if requested.is_empty() {
        return Ok(HashMap::new());
    }
    let names: Vec<String> = requested.iter().cloned().collect();
    let sql = format!("SELECT id, name FROM {table} WHERE name = ANY($1)");
    let rows = sqlx::query(&sql).bind(&names).fetch_all(&mut *db).await?;
    let resolved: HashMap<String, Uuid> = rows
        .into_iter()
        .map(|row| Ok((row.try_get("name")?, row.try_get("id")?)))
        .collect::<Result<_, sqlx::Error>>()?;
    let missing: Vec<String> = requested.into_iter().filter(|name| !resolved.contains_key(name)).collect();
    ensure!(missing.is_empty(), "unknown {table} name(s): {}", missing.join(", "));
    Ok(resolved)
}

async fn resolve_model_aliases(db: &mut PgConnection, requested: HashSet<String>) -> Result<HashMap<String, Uuid>> {
    if requested.is_empty() {
        return Ok(HashMap::new());
    }
    let aliases: Vec<String> = requested.iter().cloned().collect();
    let rows = sqlx::query("SELECT id, alias FROM deployed_models WHERE alias = ANY($1) AND deleted = FALSE")
        .bind(&aliases)
        .fetch_all(&mut *db)
        .await?;
    let resolved: HashMap<String, Uuid> = rows
        .into_iter()
        .map(|row| Ok((row.try_get("alias")?, row.try_get("id")?)))
        .collect::<Result<_, sqlx::Error>>()?;
    let missing: Vec<String> = requested.into_iter().filter(|alias| !resolved.contains_key(alias)).collect();
    ensure!(
        missing.is_empty(),
        "unknown traffic redirect model alias(es): {}",
        missing.join(", ")
    );
    Ok(resolved)
}

async fn close_tariff(db: &mut PgConnection, table: &str, id: Uuid, effective_at: DateTime<Utc>) -> Result<()> {
    let sql = format!("UPDATE {table} SET valid_until = $2 WHERE id = $1");
    sqlx::query(&sql)
        .bind(id)
        .bind(effective_at)
        .execute(&mut *db)
        .await
        .with_context(|| format!("close historical tariff row {id}"))?;
    Ok(())
}
