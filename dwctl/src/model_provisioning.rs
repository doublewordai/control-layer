//! Declarative model catalog loading, validation and startup application.
//!
//! Files are validated as a complete directory before a transaction is opened.
//! Database IDs deliberately do not appear in the format: stable natural keys
//! (model aliases, endpoint names and group names) are resolved during apply.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
    str::FromStr,
};

use anyhow::{Context, Result, bail, ensure};
use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::db::handlers::ModelProvisioning;

const PER_MILLION: i64 = 1_000_000;

#[derive(Debug, Clone)]
pub struct Catalog {
    pub(crate) models: Vec<CatalogModel>,
}

#[derive(Debug, Clone)]
pub(crate) struct CatalogModel {
    pub source: String,
    pub canonical_model: String,
    pub clay: ClayModel,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelDocument {
    /// Canonical Hugging Face model identifier.
    pub model: String,
    /// Reserved for the inference deployment automation. Ignored by dwctl.
    #[serde(default)]
    pub backend: Option<serde_json::Value>,
    /// Control-layer model graph. Omit for a backend-only document.
    #[serde(default)]
    pub clay: Option<ClayModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClayModel {
    pub alias: String,
    /// Defaults to the document's top-level `model` value.
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type", default)]
    pub model_type: Option<ModelKind>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub settings: ModelSettings,
    #[serde(default)]
    pub deployments: Vec<PhysicalDeployment>,
    #[serde(default)]
    pub routing: Routing,
    #[serde(default)]
    pub tariffs: Vec<Tariff>,
    #[serde(default)]
    pub cache_tariff: Option<CacheTariff>,
    #[serde(default)]
    pub access_groups: Vec<String>,
    #[serde(default)]
    pub traffic_rules: Vec<TrafficRule>,
    /// The serving classes this model offers, each a preset of targets the
    /// serving stack's router maps onto a pool. Declaring `interactive` or
    /// `throughput` is what makes an organisation that holds that class get
    /// it on this model; declare them once the serving side has pools for the
    /// model. A `standard` preset is optional and pins what unclassed traffic
    /// asks for; without one, standard sends no targets. Empty = standard only.
    #[serde(default)]
    pub serving_classes: BTreeMap<PresetClass, ServingPreset>,
}

/// The two elevated serving classes a model can activate and an org can be
/// granted. `standard` is the absence of a choice and is never granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServingClassName {
    Interactive,
    Throughput,
}

/// The classes a model can declare a preset for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PresetClass {
    Interactive,
    Throughput,
    Standard,
}

/// The objective targets a class maps to on a model, or an organisation's
/// explicit targets on one model. Milliseconds; `priority` is the serving
/// stack's scheduling priority (higher wins, 0 = today's realtime value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServingPreset {
    /// Time-to-first-token target, milliseconds.
    #[schemars(range(min = 1))]
    pub ttft_ms: u32,
    /// Inter-token-latency target, milliseconds.
    #[schemars(range(min = 1))]
    pub itl_ms: u32,
    /// Scheduling priority; omit for 0.
    #[serde(default)]
    pub priority: i32,
}

impl ServingPreset {
    pub(crate) fn validate(&self, context: &str) -> Result<()> {
        ensure!(self.ttft_ms > 0, "{context}: ttft_ms must be positive");
        ensure!(self.itl_ms > 0, "{context}: itl_ms must be positive");
        Ok(())
    }
}

impl ServingClassName {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Throughput => "throughput",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    #[default]
    Chat,
    Embeddings,
    Reranker,
}

impl ModelKind {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::Chat => "CHAT",
            Self::Embeddings => "EMBEDDINGS",
            Self::Reranker => "RERANKER",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSettings {
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub capacity: Option<i32>,
    pub batch_capacity: Option<i32>,
    pub throughput: Option<f32>,
    pub sanitize_responses: bool,
    pub trusted: bool,
    pub allowed_batch_completion_windows: Option<Vec<String>>,
    pub metadata: serde_json::Value,
    pub reasoning_translation_overrides: Option<serde_json::Value>,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            requests_per_second: None,
            burst_size: None,
            capacity: None,
            batch_capacity: None,
            throughput: None,
            sanitize_responses: false,
            trusted: false,
            allowed_batch_completion_windows: None,
            metadata: serde_json::json!({}),
            reasoning_translation_overrides: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhysicalDeployment {
    pub alias: String,
    pub model_name: String,
    pub endpoint: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type", default)]
    pub model_type: Option<ModelKind>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub settings: ModelSettings,
    #[serde(default)]
    pub provider_pricing: Option<ProviderPricing>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderPricing {
    PerToken {
        #[serde(default)]
        input_per_million_tokens: Option<String>,
        #[serde(default)]
        output_per_million_tokens: Option<String>,
    },
    Hourly {
        rate: String,
        input_token_cost_ratio: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    pub strategy: RoutingStrategy,
    pub fallback: Fallback,
    /// Named component pools. Supported names are `default` and `completions`.
    pub pools: BTreeMap<String, Vec<Component>>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    #[default]
    WeightedRandom,
    Priority,
}

impl RoutingStrategy {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::WeightedRandom => "weighted_random",
            Self::Priority => "priority",
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_fallback_statuses() -> Vec<i32> {
    vec![429, 499, 500, 502, 503, 504]
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Fallback {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub on_rate_limit: bool,
    #[serde(default = "default_fallback_statuses")]
    pub on_status: Vec<i32>,
    /// Extra statuses that fail over realtime traffic only. Omit to keep the
    /// value already stored for the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realtime_on_status: Option<Vec<i32>>,
    pub with_replacement: bool,
    pub max_attempts: Option<i32>,
    pub backoff: Option<Backoff>,
    pub max_total_backoff_ms: Option<i32>,
}

impl Default for Fallback {
    fn default() -> Self {
        Self {
            enabled: true,
            on_rate_limit: true,
            on_status: default_fallback_statuses(),
            realtime_on_status: None,
            with_replacement: false,
            max_attempts: None,
            backoff: None,
            max_total_backoff_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Backoff {
    pub initial_ms: i32,
    pub max_ms: i32,
    pub factor: f64,
    #[serde(default)]
    pub jitter: Jitter,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Jitter {
    None,
    #[default]
    Full,
}

impl Jitter {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub deployment: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_weight")]
    pub weight: i32,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default)]
    pub strip_leading_bos: bool,
    #[serde(default)]
    pub render_kwargs: Option<serde_json::Value>,
}

fn default_weight() -> i32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tariff {
    pub name: String,
    pub purpose: TariffPurpose,
    #[serde(default)]
    pub completion_window: Option<String>,
    pub input_per_million_tokens: String,
    pub output_per_million_tokens: String,
}

/// Customer inference pricing is independent of internal key purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TariffPurpose {
    Realtime,
    Batch,
    Playground,
}

impl TariffPurpose {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::Realtime => "realtime",
            Self::Batch => "batch",
            Self::Playground => "playground",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Platform,
    Realtime,
    Batch,
    Playground,
    Continuation,
}

impl Purpose {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Realtime => "realtime",
            Self::Batch => "batch",
            Self::Playground => "playground",
            Self::Continuation => "continuation",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheTariff {
    pub write_multiplier_5m: String,
    pub write_multiplier_1h: String,
    pub write_multiplier_24h: String,
    #[serde(default = "default_read_multiplier")]
    pub read_multiplier: String,
    pub min_prefix_tokens: i32,
}

fn default_read_multiplier() -> String {
    "0.1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrafficRule {
    Deny { purpose: Purpose },
    Redirect { purpose: Purpose, target: String },
}

impl TrafficRule {
    pub(crate) fn purpose(&self) -> Purpose {
        match self {
            Self::Deny { purpose } | Self::Redirect { purpose, .. } => *purpose,
        }
    }
}

impl Catalog {
    pub fn load(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        ensure!(
            directory.is_dir(),
            "model provisioning directory {} does not exist or is not a directory",
            directory.display()
        );

        let mut paths = fs::read_dir(directory)
            .with_context(|| format!("read model provisioning directory {}", directory.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.retain(|path| matches!(path.extension().and_then(|value| value.to_str()), Some("yaml" | "yml")));
        paths.sort();
        let mut models = Vec::new();
        for path in paths {
            let source = path.strip_prefix(directory).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            let contents = fs::read_to_string(&path).with_context(|| format!("read model provisioning file {}", path.display()))?;
            let document: ModelDocument =
                serde_yaml::from_str(&contents).with_context(|| format!("parse model provisioning file {}", path.display()))?;
            ensure_nonempty(&document.model, &source, "model")?;
            if let Some(clay) = document.clay {
                models.push(CatalogModel {
                    source,
                    canonical_model: document.model,
                    clay,
                });
            }
        }
        let catalog = Self { models };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn json_schema() -> Result<String> {
        serde_json::to_string_pretty(&schemars::schema_for!(ModelDocument)).context("serialize model provisioning JSON Schema")
    }

    fn validate(&self) -> Result<()> {
        let mut aliases: HashMap<String, (&str, &'static str)> = HashMap::new();
        let mut deployments = HashSet::new();

        for model in &self.models {
            validate_alias(&mut aliases, &model.clay.alias, &model.source, "virtual model")?;
            ensure_nonempty(
                model.clay.model_name.as_deref().unwrap_or(&model.canonical_model),
                &model.source,
                "clay.model_name",
            )?;
            validate_settings(&model.clay.settings, &model.source, "clay.settings")?;

            for deployment in &model.clay.deployments {
                validate_alias(&mut aliases, &deployment.alias, &model.source, "physical deployment")?;
                ensure_nonempty(&deployment.model_name, &model.source, "deployment.model_name")?;
                ensure_nonempty(&deployment.endpoint, &model.source, "deployment.endpoint")?;
                validate_settings(&deployment.settings, &model.source, "deployment.settings")?;
                validate_provider_pricing(deployment.provider_pricing.as_ref(), &model.source, &deployment.alias)?;
                deployments.insert(deployment.alias.clone());
            }
        }

        for model in &self.models {
            let mut component_keys = HashSet::new();
            for (pool, components) in &model.clay.routing.pools {
                ensure!(
                    pool == "default" || pool == "completions",
                    "{}: unsupported component pool {pool:?}",
                    model.source
                );
                for component in components {
                    ensure!(
                        deployments.contains(&component.deployment),
                        "{}: component references undeclared physical deployment {:?}",
                        model.source,
                        component.deployment
                    );
                    ensure!(
                        (1..=100).contains(&component.weight),
                        "{}: component weight must be between 1 and 100",
                        model.source
                    );
                    ensure!(
                        component.sort_order >= 0,
                        "{}: component sort_order cannot be negative",
                        model.source
                    );
                    ensure!(
                        component_keys.insert((pool.clone(), component.deployment.to_lowercase())),
                        "{}: duplicate component {:?} in pool {:?}",
                        model.source,
                        component.deployment,
                        pool
                    );
                }
            }
            ensure!(
                model.clay.routing.pools.get("default").is_some_and(|pool| !pool.is_empty()),
                "{}: virtual model must have a non-empty default routing pool",
                model.source
            );
            validate_fallback(&model.clay.routing.fallback, &model.source)?;

            validate_tariffs(&model.clay.tariffs, &model.source)?;
            if let Some(cache) = &model.clay.cache_tariff {
                validate_cache_tariff(cache, &model.source)?;
            }

            ensure_unique_strings(&model.clay.access_groups, &model.source, "access group")?;

            for (class, preset) in &model.clay.serving_classes {
                preset.validate(&format!("{}: serving class {class:?} preset", model.source))?;
            }

            let mut purposes = HashSet::new();
            for rule in &model.clay.traffic_rules {
                ensure!(
                    rule.purpose() != Purpose::Platform,
                    "{}: platform keys cannot be used for model traffic routing",
                    model.source
                );
                ensure!(purposes.insert(rule.purpose()), "{}: duplicate traffic rule purpose", model.source);
                if let TrafficRule::Redirect { target, .. } = rule {
                    ensure_nonempty(target, &model.source, "traffic rule redirect target")?;
                }
            }
        }
        Ok(())
    }
}

pub async fn apply(pool: &PgPool, catalog: &Catalog) -> Result<()> {
    // An empty mounted directory is an unconfigured catalog, not an
    // authoritative request to clear provisioning ownership. Return before
    // opening a transaction so it is a true database no-op.
    if catalog.models.is_empty() {
        return Ok(());
    }

    let mut transaction = pool.begin().await.context("begin model provisioning transaction")?;
    ModelProvisioning::new(&mut transaction).apply(catalog).await?;
    transaction.commit().await.context("commit model provisioning transaction")?;
    Ok(())
}

/// The tariff rules shared by the model catalog (a model's general price) and the
/// organisation catalog (a deal on one model): batch rows carry a completion window,
/// others do not, one row per (purpose, window), prices exact at 8 dp per token.
pub(crate) fn validate_tariffs(tariffs: &[Tariff], source: &str) -> Result<()> {
    let mut tariff_keys = HashSet::new();
    for tariff in tariffs {
        ensure_nonempty(&tariff.name, source, "tariff.name")?;
        match tariff.purpose {
            TariffPurpose::Batch => ensure!(
                tariff
                    .completion_window
                    .as_deref()
                    .is_some_and(|value| !value.is_empty() && value == value.trim()),
                "{source}: batch tariff {:?} requires a non-empty completion_window without surrounding whitespace",
                tariff.name
            ),
            _ => ensure!(
                tariff.completion_window.is_none(),
                "{source}: non-batch tariff {:?} must not set completion_window",
                tariff.name
            ),
        }
        ensure!(
            tariff_keys.insert((tariff.purpose, tariff.completion_window.clone())),
            "{source}: duplicate tariff for purpose {:?} and completion window {:?}",
            tariff.purpose,
            tariff.completion_window
        );
        parse_per_million(&tariff.input_per_million_tokens).with_context(|| format!("{source}: tariff {:?} input price", tariff.name))?;
        parse_per_million(&tariff.output_per_million_tokens).with_context(|| format!("{source}: tariff {:?} output price", tariff.name))?;
    }
    Ok(())
}

pub(crate) fn validate_cache_tariff(cache: &CacheTariff, source: &str) -> Result<()> {
    for (field, value) in [
        ("write_multiplier_5m", &cache.write_multiplier_5m),
        ("write_multiplier_1h", &cache.write_multiplier_1h),
        ("write_multiplier_24h", &cache.write_multiplier_24h),
        ("read_multiplier", &cache.read_multiplier),
    ] {
        let parsed = parse_decimal(value).with_context(|| format!("{source}: cache tariff {field}"))?;
        ensure!(parsed >= Decimal::ZERO, "{source}: cache tariff {field} cannot be negative");
    }
    ensure!(
        cache.min_prefix_tokens > 0,
        "{source}: cache tariff min_prefix_tokens must be positive"
    );
    Ok(())
}

pub(crate) fn parse_decimal(value: &str) -> Result<Decimal> {
    Decimal::from_str(value).with_context(|| format!("invalid decimal string {value:?}"))
}

pub(crate) fn parse_per_million(value: &str) -> Result<Decimal> {
    let per_token = parse_decimal(value)? / Decimal::from(PER_MILLION);
    ensure!(per_token >= Decimal::ZERO, "price cannot be negative");
    ensure!(
        per_token.round_dp(8) == per_token,
        "price {value:?} cannot be represented exactly at 8 decimal places per token"
    );
    Ok(per_token)
}

fn ensure_nonempty(value: &str, source: &str, field: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{source}: {field} cannot be empty");
    Ok(())
}

fn validate_alias<'a>(
    aliases: &mut HashMap<String, (&'a str, &'static str)>,
    alias: &str,
    source: &'a str,
    kind: &'static str,
) -> Result<()> {
    ensure_nonempty(alias, source, "alias")?;
    let normalized = alias.to_lowercase();
    if let Some((previous_source, previous_kind)) = aliases.insert(normalized, (source, kind)) {
        bail!("{source}: {kind} alias {alias:?} conflicts case-insensitively with {previous_kind} in {previous_source}");
    }
    Ok(())
}

fn validate_settings(settings: &ModelSettings, source: &str, field: &str) -> Result<()> {
    if let Some(value) = settings.requests_per_second {
        ensure!(value > 0.0, "{source}: {field}.requests_per_second must be positive");
    }
    if let Some(value) = settings.burst_size {
        ensure!(value > 0, "{source}: {field}.burst_size must be positive");
    }
    if let Some(value) = settings.capacity {
        ensure!(value > 0, "{source}: {field}.capacity must be positive");
    }
    if let Some(value) = settings.batch_capacity {
        ensure!(value > 0, "{source}: {field}.batch_capacity must be positive");
    }
    if let Some(value) = settings.throughput {
        ensure!(value > 0.0, "{source}: {field}.throughput must be positive");
    }
    ensure!(settings.metadata.is_object(), "{source}: {field}.metadata must be an object");
    Ok(())
}

fn validate_fallback(fallback: &Fallback, source: &str) -> Result<()> {
    if let Some(attempts) = fallback.max_attempts {
        ensure!(attempts > 0, "{source}: fallback.max_attempts must be positive");
    }
    if let Some(backoff) = &fallback.backoff {
        ensure!(backoff.initial_ms > 0, "{source}: fallback.backoff.initial_ms must be positive");
        ensure!(
            backoff.max_ms >= backoff.initial_ms,
            "{source}: fallback.backoff.max_ms must be at least initial_ms"
        );
        ensure!(backoff.factor >= 1.0, "{source}: fallback.backoff.factor must be at least 1");
        if let Some(total) = fallback.max_total_backoff_ms {
            ensure!(
                total >= backoff.max_ms,
                "{source}: fallback.max_total_backoff_ms must be at least max_ms"
            );
        }
    } else {
        ensure!(
            fallback.max_total_backoff_ms.is_none(),
            "{source}: max_total_backoff_ms requires fallback.backoff"
        );
    }
    Ok(())
}

fn validate_provider_pricing(pricing: Option<&ProviderPricing>, source: &str, alias: &str) -> Result<()> {
    match pricing {
        Some(ProviderPricing::PerToken {
            input_per_million_tokens,
            output_per_million_tokens,
        }) => {
            if let Some(value) = input_per_million_tokens {
                parse_per_million(value).with_context(|| format!("{source}: deployment {alias:?} provider input price"))?;
            }
            if let Some(value) = output_per_million_tokens {
                parse_per_million(value).with_context(|| format!("{source}: deployment {alias:?} provider output price"))?;
            }
        }
        Some(ProviderPricing::Hourly {
            rate,
            input_token_cost_ratio,
        }) => {
            ensure!(
                parse_decimal(rate)? >= Decimal::ZERO,
                "{source}: deployment {alias:?} hourly rate cannot be negative"
            );
            let ratio = parse_decimal(input_token_cost_ratio)?;
            ensure!(
                (Decimal::ZERO..=Decimal::ONE).contains(&ratio),
                "{source}: deployment {alias:?} input_token_cost_ratio must be between 0 and 1"
            );
        }
        None => {}
    }
    Ok(())
}

fn ensure_unique_strings(values: &[String], source: &str, kind: &str) -> Result<()> {
    let mut seen = HashSet::new();
    for value in values {
        ensure_nonempty(value, source, kind)?;
        ensure!(seen.insert(value.to_lowercase()), "{source}: duplicate {kind} {value:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use tempfile::tempdir;
    use uuid::Uuid;

    fn write(directory: &Path, name: &str, contents: &str) {
        fs::write(directory.join(name), contents).unwrap();
    }

    #[test]
    fn tariffs_accept_only_customer_inference_purposes() {
        for purpose in ["realtime", "batch", "playground", "continuation", "platform"] {
            let value = serde_json::json!({"name":"price", "purpose":purpose,
                "input_per_million_tokens":"1", "output_per_million_tokens":"2"});
            assert_eq!(
                serde_json::from_value::<Tariff>(value).is_ok(),
                matches!(purpose, "realtime" | "batch" | "playground"),
                "{purpose}"
            );
        }
    }

    #[test]
    fn catalog_batch_windows_reject_noncanonical_whitespace() {
        for window in ["24h", " 24h ", "", " "] {
            let tariff: Tariff = serde_json::from_value(serde_json::json!({
                "name":"batch", "purpose":"batch", "completion_window":window,
                "input_per_million_tokens":"1", "output_per_million_tokens":"2"
            }))
            .unwrap();
            assert_eq!(validate_tariffs(&[tariff], "test").is_ok(), window == "24h", "{window:?}");
        }
    }

    #[test]
    fn loads_minimal_valid_catalog() {
        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "model.yaml",
            r#"
model: org/model
clay:
  alias: org/model
  deployments:
    - alias: provider-org-model
      model_name: org/model
      endpoint: onwards
  routing:
    pools:
      default:
        - deployment: provider-org-model
  tariffs:
    - name: realtime
      purpose: realtime
      input_per_million_tokens: "0.50"
      output_per_million_tokens: "1.50"
"#,
        );
        let catalog = Catalog::load(directory.path()).unwrap();
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(parse_per_million("0.50").unwrap(), Decimal::new(50, 8));
    }

    #[test]
    fn rejects_case_insensitive_duplicate_aliases() {
        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "a.yaml",
            r#"
model: org/a
clay:
  alias: shared
  deployments:
    - alias: provider-a
      model_name: org/a
      endpoint: onwards
  routing:
    pools:
      default:
        - deployment: provider-a
"#,
        );
        write(
            directory.path(),
            "b.yaml",
            r#"
model: org/b
clay:
  alias: SHARED
  deployments:
    - alias: provider-b
      model_name: org/b
      endpoint: onwards
  routing:
    pools:
      default:
        - deployment: provider-b
"#,
        );
        assert!(
            Catalog::load(directory.path())
                .unwrap_err()
                .to_string()
                .contains("case-insensitively")
        );
    }

    fn classes_catalog_yaml(serving_classes: &str) -> String {
        format!(
            r#"
model: org/model
clay:
  alias: org/model
  deployments:
    - alias: provider-org-model
      model_name: org/model
      endpoint: onwards
  routing:
    pools:
      default:
        - deployment: provider-org-model
  tariffs:
    - name: general
      purpose: realtime
      input_per_million_tokens: "0.50"
      output_per_million_tokens: "1.50"
  serving_classes:
{serving_classes}
"#
        )
    }

    const BOTH_PRESETS: &str = "    interactive: {ttft_ms: 500, itl_ms: 20, priority: 200}\n    throughput: {ttft_ms: 5000, itl_ms: 100}";

    #[test]
    fn serving_classes_are_validated() {
        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "model.yaml",
            &classes_catalog_yaml("    interactive: {ttft_ms: 0, itl_ms: 20}"),
        );
        let err = Catalog::load(directory.path()).unwrap_err().to_string();
        assert!(err.contains("ttft_ms must be positive"), "{err}");

        write(
            directory.path(),
            "model.yaml",
            &classes_catalog_yaml("    fast: {ttft_ms: 1, itl_ms: 1}"),
        );
        assert!(
            Catalog::load(directory.path()).is_err(),
            "unknown class names are rejected by the schema"
        );
        write(
            directory.path(),
            "model.yaml",
            &classes_catalog_yaml("    interactive: {ttft_ms: 1, itl_ms: 1, pool: x}"),
        );
        assert!(Catalog::load(directory.path()).is_err(), "unknown preset fields are rejected");

        write(directory.path(), "model.yaml", &classes_catalog_yaml(BOTH_PRESETS));
        let catalog = Catalog::load(directory.path()).unwrap();
        let presets = &catalog.models[0].clay.serving_classes;
        assert_eq!(
            presets[&PresetClass::Interactive],
            ServingPreset {
                ttft_ms: 500,
                itl_ms: 20,
                priority: 200
            }
        );
        assert_eq!(presets[&PresetClass::Throughput].priority, 0, "priority defaults to 0");
    }

    #[sqlx::test]
    async fn apply_materialises_offered_classes(pool: PgPool) {
        sqlx::query(
            "INSERT INTO inference_endpoints (name, url, created_by) VALUES ('onwards', 'http://onwards.test', '00000000-0000-0000-0000-000000000000')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let directory = tempdir().unwrap();
        write(directory.path(), "model.yaml", &classes_catalog_yaml(BOTH_PRESETS));
        apply(&pool, &Catalog::load(directory.path()).unwrap()).await.unwrap();

        let presets: serde_json::Value = sqlx::query_scalar("SELECT serving_classes FROM deployed_models WHERE alias = 'org/model'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            presets,
            serde_json::json!({
                "interactive": {"ttft_ms": 500, "itl_ms": 20, "priority": 200},
                "throughput": {"ttft_ms": 5000, "itl_ms": 100, "priority": 0}
            })
        );
        let physical_presets: serde_json::Value =
            sqlx::query_scalar("SELECT serving_classes FROM deployed_models WHERE alias = 'provider-org-model'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            physical_presets,
            serde_json::json!({}),
            "a physical member never offers classes of its own"
        );

        // The file is the source: dropping a class removes it on the next apply.
        write(
            directory.path(),
            "model.yaml",
            &classes_catalog_yaml("    interactive: {ttft_ms: 500, itl_ms: 20, priority: 200}"),
        );
        apply(&pool, &Catalog::load(directory.path()).unwrap()).await.unwrap();
        let presets: serde_json::Value = sqlx::query_scalar("SELECT serving_classes FROM deployed_models WHERE alias = 'org/model'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(presets.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["interactive"]);
    }

    #[test]
    fn accepts_empty_or_backend_only_directory_as_noop() {
        let directory = tempdir().unwrap();
        assert!(Catalog::load(directory.path()).unwrap().models.is_empty());
        write(directory.path(), "backend.yaml", "model: org/model\nbackend: {}\n");
        assert!(Catalog::load(directory.path()).unwrap().models.is_empty());
    }

    #[sqlx::test]
    async fn empty_catalog_does_not_clear_provisioning_sources(pool: PgPool) {
        let model_id: Uuid = sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name, alias, created_by, is_composite, provisioning_source) VALUES ('existing', 'existing', '00000000-0000-0000-0000-000000000000', TRUE, 'existing.yaml') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let directory = tempdir().unwrap();
        let catalog = Catalog::load(directory.path()).unwrap();
        apply(&pool, &catalog).await.unwrap();

        let source: Option<String> = sqlx::query_scalar("SELECT provisioning_source FROM deployed_models WHERE id = $1")
            .bind(model_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(source.as_deref(), Some("existing.yaml"));
    }

    fn catalog_yaml(realtime_input: &str, include_batch: bool, cache_read: &str) -> String {
        let batch = if include_batch {
            r#"
    - name: batch-24h
      purpose: batch
      completion_window: 24h
      input_per_million_tokens: "0.25"
      output_per_million_tokens: "0.75""#
        } else {
            ""
        };
        format!(
            r#"
model: org/model
clay:
  alias: org/model
  settings:
    sanitize_responses: true
  deployments:
    - alias: provider-org-model
      model_name: org/model
      endpoint: onwards
      settings:
        sanitize_responses: true
  routing:
    pools:
      default:
        - deployment: provider-org-model
  tariffs:
    - name: realtime
      purpose: realtime
      input_per_million_tokens: "{realtime_input}"
      output_per_million_tokens: "1.50"{batch}
  cache_tariff:
    write_multiplier_5m: "1.25"
    write_multiplier_1h: "2.0"
    write_multiplier_24h: "3.0"
    read_multiplier: "{cache_read}"
    min_prefix_tokens: 1024
"#
        )
    }

    #[sqlx::test]
    async fn catalog_reconciles_incompatible_aimd_overrides(pool: PgPool) {
        sqlx::query("INSERT INTO inference_endpoints (name, url, created_by) VALUES ('onwards', 'http://onwards.test', '00000000-0000-0000-0000-000000000000')")
            .execute(&pool).await.unwrap();
        let directory = tempdir().unwrap();
        let weighted = catalog_yaml("0.50", false, "0.1");
        let priority = weighted.replace("  routing:\n", "  routing:\n    strategy: priority\n");
        write(directory.path(), "model.yaml", &priority);
        let catalog = Catalog::load(directory.path()).unwrap();
        apply(&pool, &catalog).await.unwrap();
        let config = serde_json::to_value(crate::db::models::deployments::AimdConfig::default()).unwrap();
        sqlx::query("UPDATE deployed_models SET aimd = $1, first_token_timeout_ms = 10000, lb_strategy = 'priority', fallback_enabled = true WHERE alias = 'org/model'")
            .bind(&config).execute(&pool).await.unwrap();
        apply(&pool, &catalog).await.unwrap();
        let preserved: Option<serde_json::Value> = sqlx::query_scalar("SELECT aimd FROM deployed_models WHERE alias = 'org/model'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(preserved, Some(config));
        write(directory.path(), "model.yaml", &weighted);
        apply(&pool, &Catalog::load(directory.path()).unwrap()).await.unwrap();
        let cleared: (Option<serde_json::Value>, Option<i64>) =
            sqlx::query_as("SELECT aimd, first_token_timeout_ms FROM deployed_models WHERE alias = 'org/model'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cleared, (None, Some(10000)));
        sqlx::query("UPDATE deployed_models SET aimd = '{\"enabled\":false}'::jsonb WHERE alias = 'org/model'")
            .execute(&pool)
            .await
            .unwrap();
        apply(&pool, &Catalog::load(directory.path()).unwrap()).await.unwrap();
        let disabled: Option<serde_json::Value> = sqlx::query_scalar("SELECT aimd FROM deployed_models WHERE alias = 'org/model'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(disabled, Some(serde_json::json!({"enabled":false})));
    }

    #[sqlx::test]
    async fn catalog_keeps_realtime_fallback_statuses_unless_declared(pool: PgPool) {
        sqlx::query("INSERT INTO inference_endpoints (name, url, created_by) VALUES ('onwards', 'http://onwards.test', '00000000-0000-0000-0000-000000000000')")
            .execute(&pool).await.unwrap();
        let realtime_statuses = || async {
            sqlx::query_scalar::<_, Vec<i32>>("SELECT fallback_realtime_on_status FROM deployed_models WHERE alias = 'org/model'")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let directory = tempdir().unwrap();
        let omitted = catalog_yaml("0.50", false, "0.1");
        write(directory.path(), "model.yaml", &omitted);
        apply(&pool, &Catalog::load(directory.path()).unwrap()).await.unwrap();
        assert_eq!(realtime_statuses().await, Vec::<i32>::new());

        sqlx::query("UPDATE deployed_models SET fallback_realtime_on_status = '{529}' WHERE alias = 'org/model'")
            .execute(&pool)
            .await
            .unwrap();
        apply(&pool, &Catalog::load(directory.path()).unwrap()).await.unwrap();
        assert_eq!(realtime_statuses().await, vec![529]);

        let declared = omitted.replace("  routing:\n", "  routing:\n    fallback:\n      realtime_on_status: []\n");
        write(directory.path(), "model.yaml", &declared);
        apply(&pool, &Catalog::load(directory.path()).unwrap()).await.unwrap();
        assert_eq!(realtime_statuses().await, Vec::<i32>::new());
    }

    #[sqlx::test]
    async fn startup_apply_is_idempotent_and_versions_tariffs(pool: PgPool) {
        sqlx::query(
            "INSERT INTO inference_endpoints (name, url, created_by) VALUES ('onwards', 'http://onwards.test', '00000000-0000-0000-0000-000000000000')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let orphan_id: Uuid = sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name, alias, created_by, is_composite, provisioning_source) VALUES ('orphan', 'orphan', '00000000-0000-0000-0000-000000000000', TRUE, 'old-source') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let directory = tempdir().unwrap();
        write(directory.path(), "model.yaml", &catalog_yaml("0.50", true, "0.1"));
        let catalog = Catalog::load(directory.path()).unwrap();
        apply(&pool, &catalog).await.unwrap();

        let virtual_id: Uuid = sqlx::query_scalar("SELECT id FROM deployed_models WHERE alias = 'org/model'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let physical_id: Uuid = sqlx::query_scalar("SELECT id FROM deployed_models WHERE alias = 'provider-org-model'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let first_tariff: (Uuid, DateTime<Utc>) = sqlx::query_as(
            "SELECT id, valid_from FROM model_tariffs WHERE deployed_model_id = $1 AND api_key_purpose = 'realtime' AND valid_until IS NULL",
        )
        .bind(virtual_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let first_cache: (Uuid, DateTime<Utc>) =
            sqlx::query_as("SELECT id, valid_from FROM model_cache_tariffs WHERE deployed_model_id = $1 AND valid_until IS NULL")
                .bind(virtual_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        apply(&pool, &catalog).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM deployed_models WHERE alias = 'org/model'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            virtual_id
        );
        assert_eq!(
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM deployed_models WHERE alias = 'provider-org-model'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            physical_id
        );
        assert_eq!(
            sqlx::query_as::<_, (Uuid, DateTime<Utc>)>(
                "SELECT id, valid_from FROM model_tariffs WHERE deployed_model_id = $1 AND api_key_purpose = 'realtime' AND valid_until IS NULL",
            )
            .bind(virtual_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            first_tariff
        );
        assert_eq!(
            sqlx::query_as::<_, (Uuid, DateTime<Utc>)>(
                "SELECT id, valid_from FROM model_cache_tariffs WHERE deployed_model_id = $1 AND valid_until IS NULL",
            )
            .bind(virtual_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            first_cache
        );

        write(directory.path(), "model.yaml", &catalog_yaml("0.60", false, "0.2"));
        apply(&pool, &Catalog::load(directory.path()).unwrap()).await.unwrap();

        let realtime_rows = sqlx::query(
            "SELECT id, valid_from, valid_until FROM model_tariffs WHERE deployed_model_id = $1 AND api_key_purpose = 'realtime' ORDER BY valid_from",
        )
        .bind(virtual_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(realtime_rows.len(), 2);
        let old_until: DateTime<Utc> = realtime_rows[0].try_get("valid_until").unwrap();
        let new_from: DateTime<Utc> = realtime_rows[1].try_get("valid_from").unwrap();
        assert_eq!(old_until, new_from);
        assert!(
            realtime_rows[1]
                .try_get::<Option<DateTime<Utc>>, _>("valid_until")
                .unwrap()
                .is_none()
        );

        let active_batch: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM model_tariffs WHERE deployed_model_id = $1 AND api_key_purpose = 'batch' AND valid_until IS NULL",
        )
        .bind(virtual_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_batch, 0);
        let cache_versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_cache_tariffs WHERE deployed_model_id = $1")
            .bind(virtual_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(cache_versions, 2);

        let orphan = sqlx::query("SELECT id, provisioning_source, deleted FROM deployed_models WHERE id = $1")
            .bind(orphan_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(orphan.try_get::<Uuid, _>("id").unwrap(), orphan_id);
        assert!(orphan.try_get::<Option<String>, _>("provisioning_source").unwrap().is_none());
        assert!(!orphan.try_get::<bool, _>("deleted").unwrap());
    }
}
