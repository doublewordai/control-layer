//! Per-organisation overlay catalog: an organisation's per-model overrides of
//! its serving account settings, declared as YAML and applied at startup.
//!
//! One file per organisation. The organisation itself is referenced by its
//! account username, and each entry names a virtual model alias from the
//! model catalog. Anything not mentioned is the model's default and the
//! organisation's account setting. The account settings themselves (classes
//! held, default class, routing preference) are database settings toggled
//! through the API, not declared here: this catalog only carries what
//! references a model.
//!
//! Rows written here are owned by the file (`provisioning_source`) and are
//! rewritten on every start, exactly like the model catalog. Removed catalog
//! entries retire their deals. Rows on undeclared, unowned org/model pairs are
//! left alone; declaring a pair adopts its current overlay and prices.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::db::handlers::ModelProvisioning;
use crate::model_provisioning::{CacheTariff, ServingClassName, ServingPreset, Tariff, validate_cache_tariff, validate_tariffs};

/// Prefix of the `provisioning_source` marker on rows this catalog owns.
const SOURCE_PREFIX: &str = "org-overlays:";

/// Transaction-scoped advisory lock serialising overlay reconciliation across
/// replicas starting at once (mirrors the model catalog's own lock).
const ORG_OVERLAYS_LOCK: i64 = 0x4457_4f52_474f_564c;

#[derive(Debug, Clone)]
pub struct OrgCatalog {
    pub(crate) orgs: Vec<OrgCatalogEntry>,
    managed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct OrgCatalogEntry {
    pub source: String,
    pub document: OrgDocument,
}

/// One organisation's file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgDocument {
    /// The organisation, by its account username.
    pub org: String,
    /// Per-model overrides. A model not listed gets the organisation's
    /// account settings unchanged.
    #[serde(default)]
    pub models: Vec<OrgModelOverlay>,
}

/// Billing follows the resolved outcome, including operator-authored custom targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PricingClass {
    Standard,
    Interactive,
    Throughput,
    Custom,
}
impl PricingClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Interactive => "interactive",
            Self::Throughput => "throughput",
            Self::Custom => "custom",
        }
    }
}

/// Organization cache deals change prices only. The model owns enablement and
/// the minimum prefix length used by the classifier.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CachePrices {
    pub write_multiplier_5m: String,
    pub write_multiplier_1h: String,
    pub write_multiplier_24h: String,
    pub read_multiplier: String,
}
impl CachePrices {
    fn as_tariff(&self) -> CacheTariff {
        CacheTariff {
            write_multiplier_5m: self.write_multiplier_5m.clone(),
            write_multiplier_1h: self.write_multiplier_1h.clone(),
            write_multiplier_24h: self.write_multiplier_24h.clone(),
            read_multiplier: self.read_multiplier.clone(),
            min_prefix_tokens: 1,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClassPrices {
    #[serde(default)]
    pub tariffs: Vec<Tariff>,
    #[serde(default)]
    pub cache_tariff: Option<CachePrices>,
}

/// The organisation's overrides on one virtual model.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgModelOverlay {
    /// A virtual model alias from the model catalog.
    pub alias: String,
    /// Overrides the account's default serving class on this model. Applies
    /// only where the organisation holds the class and the model offers it.
    /// Mutually exclusive with `targets`.
    #[serde(default)]
    pub default_class: Option<ServingClassName>,
    /// Explicit targets for a bespoke deal on this model, sent as-is whenever
    /// a request names no class. Writing them here is the authority to use
    /// them: no class grant is checked. Mutually exclusive with `default_class`.
    #[serde(default)]
    pub targets: Option<ServingPreset>,
    /// Overrides the account's `self_hosted_only` setting on this model.
    #[serde(default)]
    pub self_hosted_only: Option<bool>,
    /// The organisation's own prices on this model, per purpose and completion
    /// window, same shape as the model catalog's `tariffs`. Billing uses these
    /// through the full purpose/window fallback before the model's general prices.
    #[serde(default)]
    pub tariffs: Vec<Tariff>,
    /// The organisation's own prompt-cache multipliers on this model. Caching
    /// itself is enabled by the model's general cache tariff; this only
    /// changes the multipliers this organisation pays.
    #[serde(default)]
    pub cache_tariff: Option<CachePrices>,
    /// Optional prices for a resolved class, falling back to the general deal.
    #[serde(default)]
    pub class_pricing: BTreeMap<PricingClass, ClassPrices>,
}

impl OrgModelOverlay {
    /// Whether this entry changes any serving or pricing field.
    fn overrides_something(&self) -> bool {
        self.default_class.is_some()
            || self.targets.is_some()
            || self.self_hosted_only.is_some()
            || !self.tariffs.is_empty()
            || self.cache_tariff.is_some()
            || !self.class_pricing.is_empty()
    }
}

impl OrgCatalog {
    /// Load every YAML document in `directory`. A missing directory is an
    /// empty catalog: the chart may not mount one yet. An existing path that
    /// is not a directory is a misconfiguration, not an empty catalog.
    pub fn load(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        if !directory.exists() {
            return Ok(Self {
                orgs: Vec::new(),
                managed: false,
            });
        }
        ensure!(directory.is_dir(), "org overlay path {} is not a directory", directory.display());
        let mut paths = fs::read_dir(directory)
            .with_context(|| format!("read org overlay directory {}", directory.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.retain(|path| matches!(path.extension().and_then(|value| value.to_str()), Some("yaml" | "yml")));
        paths.sort();
        let mut orgs = Vec::new();
        for path in paths {
            let source = path.strip_prefix(directory).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            let contents = fs::read_to_string(&path).with_context(|| format!("read org overlay file {}", path.display()))?;
            let document: OrgDocument =
                serde_yaml::from_str(&contents).with_context(|| format!("parse org overlay file {}", path.display()))?;
            orgs.push(OrgCatalogEntry { source, document });
        }
        let catalog = Self { orgs, managed: true };
        catalog.validate()?;
        Ok(catalog)
    }

    /// Offline references use the same public composite aliases as model startup.
    /// Database organisation existence and manual ownership still require DB preflight.
    pub fn validate_models(&self, models: &crate::model_provisioning::Catalog) -> Result<()> {
        let aliases: HashSet<_> = models.models.iter().map(|model| model.clay.alias.as_str()).collect();
        for entry in &self.orgs {
            for model in &entry.document.models {
                ensure!(
                    aliases.contains(model.alias.as_str()),
                    "{}: organisation {:?}, model {:?}: alias is absent from the model catalog",
                    entry.source,
                    entry.document.org,
                    model.alias
                );
            }
        }
        Ok(())
    }

    pub fn json_schema() -> Result<String> {
        serde_json::to_string_pretty(&schemars::schema_for!(OrgDocument)).context("serialize org overlay JSON Schema")
    }

    fn validate(&self) -> Result<()> {
        let mut seen_orgs: HashMap<String, &str> = HashMap::new();
        for entry in &self.orgs {
            let source = entry.source.as_str();
            let org = entry.document.org.trim();
            ensure!(!org.is_empty(), "{source}: org cannot be empty");
            if let Some(previous) = seen_orgs.insert(org.to_lowercase(), source) {
                anyhow::bail!("{source}: organisation {org:?} is also declared in {previous}");
            }
            let mut aliases = HashSet::new();
            for model in &entry.document.models {
                ensure!(!model.alias.trim().is_empty(), "{source}: model alias cannot be empty");
                ensure!(
                    aliases.insert(model.alias.to_lowercase()),
                    "{source}: duplicate overlay for model {:?}",
                    model.alias
                );
                ensure!(
                    model.overrides_something(),
                    "{source}: overlay for model {:?} overrides nothing",
                    model.alias
                );
                validate_tariffs(&model.tariffs, &format!("{source}: model {:?}", model.alias))?;
                if let Some(cache) = &model.cache_tariff {
                    validate_cache_tariff(&cache.as_tariff(), &format!("{source}: model {:?}", model.alias))?;
                }
                ensure!(
                    model.default_class.is_none() || model.targets.is_none(),
                    "{source}: overlay for model {:?} sets both default_class and targets; use one",
                    model.alias
                );
                for (class, prices) in &model.class_pricing {
                    ensure!(
                        !prices.tariffs.is_empty() || prices.cache_tariff.is_some(),
                        "{source}: empty class pricing"
                    );
                    let context = format!(
                        "{source}: organisation {:?}, model {:?}, class {:?}",
                        entry.document.org,
                        model.alias,
                        class.as_str()
                    );
                    validate_tariffs(&prices.tariffs, &context)?;
                    ensure!(
                        *class == PricingClass::Standard
                            || prices
                                .tariffs
                                .iter()
                                .all(|t| !matches!(t.purpose, crate::model_provisioning::TariffPurpose::Batch)),
                        "{source}: batch prices can only specialize standard"
                    );
                    if let Some(cache) = &prices.cache_tariff {
                        validate_cache_tariff(&cache.as_tariff(), &context)?;
                    }
                }
                if let Some(targets) = &model.targets {
                    targets.validate(&format!("{source}: overlay targets for model {:?}", model.alias))?;
                }
            }
        }
        Ok(())
    }
}

/// Apply the mounted catalog in one transaction. A missing directory is a
/// no-op; an explicitly empty directory retires only catalog-owned deals.
pub async fn apply(pool: &PgPool, catalog: &OrgCatalog) -> Result<()> {
    if !catalog.managed {
        return Ok(());
    }
    let mut transaction = pool.begin().await.context("begin org overlay transaction")?;
    apply_in(&mut transaction, catalog).await?;
    transaction.commit().await.context("commit org overlay transaction")?;
    Ok(())
}

async fn apply_in(db: &mut PgConnection, catalog: &OrgCatalog) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ORG_OVERLAYS_LOCK)
        .execute(&mut *db)
        .await
        .context("acquire org overlay advisory lock")?;
    let orgs = resolve_orgs(db, catalog).await?;
    let aliases = resolve_aliases(db, catalog).await?;
    // A replica may have begun its transaction before the lock winner. Use the
    // wall clock after locking so the winner's committed prices are not "future".
    let effective_at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *db)
        .await
        .context("read org overlay effective timestamp")?;
    for entry in &catalog.orgs {
        ModelProvisioning::for_org_catalog(db)
            .preflight_future_account_tariffs(orgs[entry.document.org.trim()], effective_at)
            .await
            .with_context(|| format!("{}: organisation {:?}", entry.source, entry.document.org))?;
    }

    let mut desired: Vec<(Uuid, Uuid)> = Vec::new();
    for entry in &catalog.orgs {
        let org_id = orgs[entry.document.org.trim()];
        let source = format!("{SOURCE_PREFIX}{}", entry.source);
        for model in &entry.document.models {
            let model_id = aliases[&model.alias];
            // A declared org/model is wholly catalog-owned. Omitted settings
            // inherit defaults; omitted prices retire without deleting history.
            ModelProvisioning::for_org_catalog(db)
                .adopt_account_model_tariffs(org_id, model_id, effective_at)
                .await
                .with_context(|| format!("{}: organisation {:?}, model {:?}", entry.source, entry.document.org, model.alias))?;
            desired.push((org_id, model_id));
            // The organisation's prices on this model: a temporal ledger scoped to the
            // organisation, versioned exactly like the model catalog's general prices.
            let mut provisioning = ModelProvisioning::for_org_catalog(db);
            provisioning
                .reconcile_tariffs(model_id, Some(org_id), None, &model.tariffs, effective_at)
                .await
                .with_context(|| format!("reconcile tariffs of org {:?} on {:?}", entry.document.org, model.alias))?;
            provisioning
                .reconcile_cache_tariff(
                    model_id,
                    Some(org_id),
                    None,
                    model.cache_tariff.as_ref().map(CachePrices::as_tariff).as_ref(),
                    effective_at,
                )
                .await
                .with_context(|| format!("reconcile cache tariff of org {:?} on {:?}", entry.document.org, model.alias))?;
            for class in [
                PricingClass::Standard,
                PricingClass::Interactive,
                PricingClass::Throughput,
                PricingClass::Custom,
            ] {
                let prices = model.class_pricing.get(&class);
                provisioning
                    .reconcile_tariffs(
                        model_id,
                        Some(org_id),
                        Some(class.as_str()),
                        prices.map_or(&[], |p| p.tariffs.as_slice()),
                        effective_at,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "{}: reconcile token prices of org {:?}, model {:?}, class {:?}",
                            entry.source,
                            entry.document.org,
                            model.alias,
                            class.as_str()
                        )
                    })?;
                let cache = prices.and_then(|p| p.cache_tariff.as_ref()).map(CachePrices::as_tariff);
                provisioning
                    .reconcile_cache_tariff(model_id, Some(org_id), Some(class.as_str()), cache.as_ref(), effective_at)
                    .await
                    .with_context(|| {
                        format!(
                            "{}: reconcile cache prices of org {:?}, model {:?}, class {:?}",
                            entry.source,
                            entry.document.org,
                            model.alias,
                            class.as_str()
                        )
                    })?;
            }
            sqlx::query(
                r#"INSERT INTO model_overlays (user_id, deployed_model_id, default_serving_class, targets, self_hosted_only, provisioning_source)
                   VALUES ($1, $2, $3, $4, $5, $6)
                   ON CONFLICT (user_id, deployed_model_id) DO UPDATE SET
                       default_serving_class = EXCLUDED.default_serving_class,
                       targets = EXCLUDED.targets,
                       self_hosted_only = EXCLUDED.self_hosted_only,
                       provisioning_source = EXCLUDED.provisioning_source,
                       updated_at = NOW()
                   WHERE model_overlays.default_serving_class IS DISTINCT FROM EXCLUDED.default_serving_class
                      OR model_overlays.targets IS DISTINCT FROM EXCLUDED.targets
                      OR model_overlays.self_hosted_only IS DISTINCT FROM EXCLUDED.self_hosted_only
                      OR model_overlays.provisioning_source IS DISTINCT FROM EXCLUDED.provisioning_source"#,
            )
            .bind(org_id)
            .bind(model_id)
            .bind(model.default_class.map(|class| class.as_db_str()))
            .bind(model.targets.map(serde_json::to_value).transpose().context("serialize overlay targets")?)
            .bind(model.self_hosted_only)
            .bind(&source)
            .execute(&mut *db)
            .await
            .with_context(|| format!("upsert overlay for org {:?} on {:?}", entry.document.org, model.alias))?;
        }
    }

    // Rows this catalog owns but no longer declares are removed; hand rows
    // (NULL source) and rows owned by anything else are left alone.
    let (org_ids, model_ids): (Vec<Uuid>, Vec<Uuid>) = desired.into_iter().unzip();
    // Price ownership is independent of routing overlays. Omission only retires
    // prices written by this catalog; manual prices survive even on owned models.
    for table in ["model_tariffs", "model_cache_tariffs"] {
        let omitted = "t.provisioning_source = 'org-overlays' AND NOT EXISTS (
            SELECT 1 FROM UNNEST($1::uuid[], $2::uuid[]) AS d(user_id, deployed_model_id)
            WHERE d.user_id = t.user_id AND d.deployed_model_id = t.deployed_model_id)";
        let future: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM {table} t WHERE t.valid_from > $3
             AND (t.valid_until IS NULL OR t.valid_until > t.valid_from) AND {omitted})"
        ))
        .bind(&org_ids)
        .bind(&model_ids)
        .bind(effective_at)
        .fetch_one(&mut *db)
        .await?;
        ensure!(!future, "removed overlay has future-dated prices; resolve these before removing it");
        sqlx::query(&format!(
            "UPDATE {table} t SET valid_until = $3 WHERE t.valid_from <= $3
             AND (t.valid_until IS NULL OR t.valid_until > $3) AND {omitted}"
        ))
        .bind(&org_ids)
        .bind(&model_ids)
        .bind(effective_at)
        .execute(&mut *db)
        .await?;
    }
    sqlx::query(
        r#"DELETE FROM model_overlays mo
           WHERE mo.provisioning_source LIKE $1
             AND NOT EXISTS (
                 SELECT 1 FROM UNNEST($2::uuid[], $3::uuid[]) AS d(user_id, deployed_model_id)
                 WHERE d.user_id = mo.user_id AND d.deployed_model_id = mo.deployed_model_id
             )"#,
    )
    .bind(format!("{SOURCE_PREFIX}%"))
    .bind(&org_ids)
    .bind(&model_ids)
    .execute(&mut *db)
    .await
    .context("remove omitted org overlays")?;

    tracing::info!(orgs = catalog.orgs.len(), "Applied org overlay catalog");
    Ok(())
}

/// Every organisation named must exist and be an organisation, never a
/// personal account: overlays are deals, and deals are with organisations.
async fn resolve_orgs(db: &mut PgConnection, catalog: &OrgCatalog) -> Result<HashMap<String, Uuid>> {
    let names: Vec<String> = catalog.orgs.iter().map(|entry| entry.document.org.trim().to_string()).collect();
    let rows = sqlx::query("SELECT id, username, user_type FROM users WHERE username = ANY($1) AND is_deleted = FALSE")
        .bind(&names)
        .fetch_all(&mut *db)
        .await
        .context("resolve overlay organisations")?;
    let mut resolved = HashMap::new();
    for row in rows {
        let username: String = row.try_get("username")?;
        let user_type: String = row.try_get("user_type")?;
        ensure!(
            user_type == "organization",
            "overlay organisation {username:?} is a personal account; overlays apply to organisations only"
        );
        resolved.insert(username, row.try_get::<Uuid, _>("id")?);
    }
    let missing: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| !resolved.contains_key(*name))
        .collect();
    ensure!(missing.is_empty(), "unknown overlay organisation(s): {}", missing.join(", "));
    Ok(resolved)
}

/// Every alias named must be a live virtual model.
async fn resolve_aliases(db: &mut PgConnection, catalog: &OrgCatalog) -> Result<HashMap<String, Uuid>> {
    let requested: HashSet<String> = catalog
        .orgs
        .iter()
        .flat_map(|entry| entry.document.models.iter().map(|model| model.alias.clone()))
        .collect();
    if requested.is_empty() {
        return Ok(HashMap::new());
    }
    let aliases: Vec<String> = requested.iter().cloned().collect();
    let rows = sqlx::query("SELECT id, alias FROM deployed_models WHERE alias = ANY($1) AND deleted = FALSE AND is_composite = TRUE")
        .bind(&aliases)
        .fetch_all(&mut *db)
        .await
        .context("resolve overlay model aliases")?;
    let resolved: HashMap<String, Uuid> = rows
        .into_iter()
        .map(|row| Ok((row.try_get("alias")?, row.try_get("id")?)))
        .collect::<Result<_, sqlx::Error>>()?;
    let missing: Vec<String> = requested.into_iter().filter(|alias| !resolved.contains_key(alias)).collect();
    ensure!(
        missing.is_empty(),
        "unknown or non-virtual overlay model alias(es): {}",
        missing.join(", ")
    );
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(directory: &Path, name: &str, contents: &str) {
        fs::write(directory.join(name), contents).unwrap();
    }

    #[test]
    fn internal_purposes_are_rejected_in_general_and_class_deals() {
        for purpose in ["continuation", "platform"] {
            let tariff = serde_json::json!({"name":"deal", "purpose":purpose,
                "input_per_million_tokens":"1", "output_per_million_tokens":"2"});
            for model in [
                serde_json::json!({"alias":"m", "tariffs":[tariff.clone()]}),
                serde_json::json!({"alias":"m", "class_pricing":{"interactive":{"tariffs":[tariff.clone()]}}}),
            ] {
                assert!(serde_json::from_value::<OrgDocument>(serde_json::json!({"org":"acme","models":[model]})).is_err());
            }
        }
    }

    #[test]
    fn missing_directory_is_an_empty_catalog_but_a_file_is_an_error() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("nope");
        assert!(OrgCatalog::load(&missing).unwrap().orgs.is_empty());
        assert!(OrgCatalog::load(directory.path()).unwrap().orgs.is_empty());
        let file = directory.path().join("overlays.yaml");
        fs::write(&file, "org: acme\n").unwrap();
        let err = OrgCatalog::load(&file).unwrap_err().to_string();
        assert!(err.contains("is not a directory"), "{err}");
    }

    #[test]
    fn documents_are_validated() {
        let directory = tempdir().unwrap();
        write(directory.path(), "acme.yaml", "org: acme\nmodels:\n  - alias: m\n");
        let err = OrgCatalog::load(directory.path()).unwrap_err().to_string();
        assert!(err.contains("overrides nothing"), "{err}");

        write(
            directory.path(),
            "acme.yaml",
            "org: acme\nmodels:\n  - alias: m\n    default_class: throughput\n  - alias: M\n    self_hosted_only: true\n",
        );
        let err = OrgCatalog::load(directory.path()).unwrap_err().to_string();
        assert!(err.contains("duplicate overlay for model"), "{err}");

        write(
            directory.path(),
            "acme.yaml",
            "org: acme\nmodels:\n  - alias: m\n    default_class: throughput\n",
        );
        write(directory.path(), "acme2.yaml", "org: ACME\nmodels: []\n");
        let err = OrgCatalog::load(directory.path()).unwrap_err().to_string();
        assert!(err.contains("is also declared in"), "{err}");

        write(
            directory.path(),
            "acme2.yaml",
            "org: other\nmodels:\n  - alias: m\n    classes: [interactive]\n",
        );
        assert!(OrgCatalog::load(directory.path()).is_err(), "unknown fields are rejected");

        // A class default and explicit targets are two ways of saying what a
        // request asks for; one entry says one thing.
        write(
            directory.path(),
            "acme2.yaml",
            "org: other\nmodels:\n  - alias: m\n    default_class: throughput\n    targets: {ttft_ms: 800, itl_ms: 30}\n",
        );
        let err = OrgCatalog::load(directory.path()).unwrap_err().to_string();
        assert!(err.contains("both default_class and targets"), "{err}");
        write(
            directory.path(),
            "acme2.yaml",
            "org: other\nmodels:\n  - alias: m\n    targets: {ttft_ms: 800, itl_ms: 0}\n",
        );
        let err = OrgCatalog::load(directory.path()).unwrap_err().to_string();
        assert!(err.contains("itl_ms must be positive"), "{err}");

        write(
            directory.path(),
            "acme2.yaml",
            "org: other\nmodels:\n  - alias: m\n    targets: {ttft_ms: 800, itl_ms: 30, priority: 50}\n",
        );
        let catalog = OrgCatalog::load(directory.path()).unwrap();
        assert_eq!(catalog.orgs.len(), 2);
        assert_eq!(catalog.orgs[0].document.models[0].default_class, Some(ServingClassName::Throughput));
        assert_eq!(
            catalog.orgs[1].document.models[0].targets,
            Some(ServingPreset {
                ttft_ms: 800,
                itl_ms: 30,
                priority: 50
            })
        );
    }

    #[sqlx::test]
    async fn apply_materialises_and_prunes_owned_rows(pool: PgPool) {
        let org_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (username, email, display_name, auth_source, user_type) VALUES ('acme', 'acme@example.com', 'Acme', 'test', 'organization') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO users (username, email, display_name, auth_source, user_type) VALUES ('bob', 'bob@example.com', 'Bob', 'test', 'individual')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let model_id: Uuid = sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name, alias, created_by, is_composite) VALUES ('m', 'org/model', '00000000-0000-0000-0000-000000000000', TRUE) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let directory = tempdir().unwrap();
        // A personal account is refused before anything is written.
        write(
            directory.path(),
            "bob.yaml",
            "org: bob\nmodels:\n  - alias: org/model\n    default_class: throughput\n",
        );
        let err = apply(&pool, &OrgCatalog::load(directory.path()).unwrap())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("personal account"), "{err}");
        fs::remove_file(directory.path().join("bob.yaml")).unwrap();

        // An unknown alias is refused.
        write(
            directory.path(),
            "acme.yaml",
            "org: acme\nmodels:\n  - alias: nope\n    default_class: throughput\n",
        );
        let err = apply(&pool, &OrgCatalog::load(directory.path()).unwrap())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown or non-virtual overlay model alias"), "{err}");

        write(
            directory.path(),
            "acme.yaml",
            "org: acme\nmodels:\n  - alias: org/model\n    default_class: throughput\n    self_hosted_only: true\n",
        );
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        let row = sqlx::query("SELECT default_serving_class, targets, self_hosted_only, provisioning_source FROM model_overlays WHERE user_id = $1 AND deployed_model_id = $2")
            .bind(org_id)
            .bind(model_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.try_get::<Option<String>, _>("default_serving_class").unwrap().as_deref(),
            Some("throughput")
        );
        assert_eq!(row.try_get::<Option<serde_json::Value>, _>("targets").unwrap(), None);
        assert_eq!(row.try_get::<Option<bool>, _>("self_hosted_only").unwrap(), Some(true));
        assert_eq!(
            row.try_get::<Option<String>, _>("provisioning_source").unwrap().as_deref(),
            Some("org-overlays:acme.yaml")
        );

        // Switching the entry to explicit targets replaces the class default.
        write(
            directory.path(),
            "acme.yaml",
            "org: acme\nmodels:\n  - alias: org/model\n    targets: {ttft_ms: 800, itl_ms: 30}\n",
        );
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        let row = sqlx::query(
            "SELECT default_serving_class, targets, self_hosted_only FROM model_overlays WHERE user_id = $1 AND deployed_model_id = $2",
        )
        .bind(org_id)
        .bind(model_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.try_get::<Option<String>, _>("default_serving_class").unwrap(), None);
        assert_eq!(
            row.try_get::<Option<serde_json::Value>, _>("targets").unwrap(),
            Some(serde_json::json!({"ttft_ms": 800, "itl_ms": 30, "priority": 0}))
        );
        assert_eq!(row.try_get::<Option<bool>, _>("self_hosted_only").unwrap(), None);

        // The organisation's prices live next to the model's general price, scoped by
        // user_id: general rows are untouched, org rows are versioned like the catalog's.
        sqlx::query(
            "INSERT INTO model_tariffs (deployed_model_id, name, input_price_per_token, output_price_per_token, api_key_purpose) VALUES ($1, 'general', 0.00000100, 0.00000200, 'realtime')",
        )
        .bind(model_id)
        .execute(&pool)
        .await
        .unwrap();
        write(
            directory.path(),
            "acme.yaml",
            "org: acme\nmodels:\n  - alias: org/model\n    tariffs:\n      - {name: deal, purpose: realtime, input_per_million_tokens: \"0.50\", output_per_million_tokens: \"1.00\"}\n      - {name: deal-24h, purpose: batch, completion_window: 24h, input_per_million_tokens: \"0.25\", output_per_million_tokens: \"0.50\"}\n    cache_tariff: {write_multiplier_5m: \"1.1\", write_multiplier_1h: \"1.5\", write_multiplier_24h: \"2.0\", read_multiplier: \"0.05\"}\n",
        );
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        let org_rows: Vec<(String, Option<String>, Option<Uuid>)> = sqlx::query_as(
            "SELECT name, completion_window, user_id FROM model_tariffs WHERE deployed_model_id = $1 AND valid_until IS NULL ORDER BY name",
        )
        .bind(model_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            org_rows,
            vec![
                ("deal".to_string(), None, Some(org_id)),
                ("deal-24h".to_string(), Some("24h".to_string()), Some(org_id)),
                ("general".to_string(), None, None),
            ]
        );
        let cache_scope: Option<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM model_cache_tariffs WHERE deployed_model_id = $1 AND valid_until IS NULL")
                .bind(model_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cache_scope, Some(org_id));

        // A changed price closes the organisation's row and inserts its successor.
        write(
            directory.path(),
            "acme.yaml",
            "org: acme\nmodels:\n  - alias: org/model\n    tariffs:\n      - {name: deal, purpose: realtime, input_per_million_tokens: \"0.40\", output_per_million_tokens: \"1.00\"}\n",
        );
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        let versions: Vec<(String, bool)> = sqlx::query_as(
            "SELECT name, valid_until IS NULL FROM model_tariffs WHERE deployed_model_id = $1 AND user_id = $2 ORDER BY valid_from, name",
        )
        .bind(model_id)
        .bind(org_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            versions,
            vec![
                ("deal".to_string(), false),
                ("deal-24h".to_string(), false),
                ("deal".to_string(), true)
            ]
        );
        let cache_open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM model_cache_tariffs WHERE deployed_model_id = $1 AND user_id = $2 AND valid_until IS NULL",
        )
        .bind(model_id)
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cache_open, 0, "an omitted cache_tariff closes the organisation's row");

        // Dropping the model from the file ends the deal: every org row is closed,
        // the general price stays open.
        write(directory.path(), "acme.yaml", "org: acme\nmodels: []\n");
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        let open: Vec<(String, Option<Uuid>)> =
            sqlx::query_as("SELECT name, user_id FROM model_tariffs WHERE deployed_model_id = $1 AND valid_until IS NULL")
                .bind(model_id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(open, vec![("general".to_string(), None)]);
        // put the entry back so the pruning assertions below run as before
        write(
            directory.path(),
            "acme.yaml",
            "org: acme\nmodels:\n  - alias: org/model\n    targets: {ttft_ms: 800, itl_ms: 30}\n",
        );
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();

        // A hand-written row for another org survives; the catalog's own row
        // goes when its entry is removed from the file.
        sqlx::query("INSERT INTO model_overlays (user_id, deployed_model_id, self_hosted_only) VALUES ('00000000-0000-0000-0000-000000000000', $1, true)")
            .bind(model_id)
            .execute(&pool)
            .await
            .unwrap();
        write(directory.path(), "acme.yaml", "org: acme\nmodels: []\n");
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        let remaining: Vec<(Uuid, Option<String>)> =
            sqlx::query_as("SELECT user_id, provisioning_source FROM model_overlays WHERE deployed_model_id = $1")
                .bind(model_id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_ne!(remaining[0].0, org_id);
        assert_eq!(remaining[0].1, None);
    }
    #[sqlx::test]
    async fn removing_last_catalog_file_retires_class_deals_but_preserves_hand_rows(pool: PgPool) {
        let org: Uuid = sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('class-org','class-org@example.com','test','organization') RETURNING id").fetch_one(&pool).await.unwrap();
        let model: Uuid = sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('class-model','class-model',true,$1) RETURNING id").bind(org).fetch_one(&pool).await.unwrap();
        let directory = tempdir().unwrap();
        let yaml = r#"org: class-org
models:
  - alias: class-model
    class_pricing:
      interactive:
        tariffs:
          - {name: interactive, purpose: realtime, input_per_million_tokens: '0', output_per_million_tokens: '1'}
        cache_tariff: {write_multiplier_5m: '1', write_multiplier_1h: '1', write_multiplier_24h: '1', read_multiplier: '0'}
"#;
        write(directory.path(), "org.yaml", yaml);
        let catalog = OrgCatalog::load(directory.path()).unwrap();
        apply(&pool, &catalog).await.unwrap();
        apply(&pool, &catalog).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM model_tariffs WHERE user_id=$1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "identical startup must not create another price version");
        let class: String = sqlx::query_scalar("SELECT serving_class FROM model_tariffs WHERE user_id=$1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(class, "interactive");
        // A missing mount is a no-op, distinct from an explicitly empty mounted catalog.
        apply(&pool, &OrgCatalog::load(directory.path().join("missing")).unwrap())
            .await
            .unwrap();
        let active: i64 = sqlx::query_scalar("SELECT count(*) FROM model_tariffs WHERE user_id=$1 AND valid_until IS NULL")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(active, 1);
        sqlx::query("INSERT INTO model_overlays(user_id,deployed_model_id,self_hosted_only) VALUES ('00000000-0000-0000-0000-000000000000',$1,true)").bind(model).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,name,input_price_per_token,output_price_per_token,api_key_purpose) VALUES ($1,'00000000-0000-0000-0000-000000000000','hand',1,1,'realtime')").bind(model).execute(&pool).await.unwrap();
        // A cancelled future version must not permanently block removing its file.
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,name,input_price_per_token,output_price_per_token,api_key_purpose,valid_from,valid_until) VALUES ($1,$2,'cancelled',1,1,'realtime',NOW()+INTERVAL '1 day',NOW())").bind(model).bind(org).execute(&pool).await.unwrap();
        fs::remove_file(directory.path().join("org.yaml")).unwrap();
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        for table in ["model_tariffs", "model_cache_tariffs"] {
            let active: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE user_id=$1 AND valid_until IS NULL"))
                .bind(org)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(active, 0, "removed file must retire {table}");
        }
        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM model_tariffs WHERE valid_until IS NULL")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(names, vec!["hand"]);
        let owners: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM model_overlays")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(owners, vec![Uuid::nil()]);
    }
    #[sqlx::test]
    async fn catalog_rejects_future_manual_price_collisions_atomically(pool: PgPool) {
        let org: Uuid = sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('future-org','future@example.com','test','organization') RETURNING id").fetch_one(&pool).await.unwrap();
        let directory = tempdir().unwrap();
        for class in [None, Some("interactive")] {
            for finite in [false, true] {
                for cache in [false, true] {
                    let alias = format!("future-{}-{finite}-{cache}", class.unwrap_or("all"));
                    let model: Uuid = sqlx::query_scalar(
                        "INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ($1,$1,true,$2) RETURNING id",
                    )
                    .bind(&alias)
                    .bind(org)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    // Future schedules cannot be adopted by an immediate-only catalog.
                    if cache {
                        sqlx::query("INSERT INTO model_cache_tariffs (deployed_model_id,user_id,serving_class,read_multiplier,min_prefix_tokens,write_multiplier_5m,write_multiplier_1h,write_multiplier_24h,valid_from,valid_until) VALUES ($1,$2,$3,0.5,1,1,1,1,NOW()+INTERVAL '1 day',CASE WHEN $4 THEN NOW()+INTERVAL '2 days' ELSE NULL END)")
                            .bind(model).bind(org).bind(class).bind(finite).execute(&pool).await.unwrap();
                    } else {
                        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,serving_class,name,api_key_purpose,input_price_per_token,output_price_per_token,valid_from,valid_until) VALUES ($1,$2,$3,'future','realtime',1,2,NOW()+INTERVAL '1 day',CASE WHEN $4 THEN NOW()+INTERVAL '2 days' ELSE NULL END)")
                            .bind(model).bind(org).bind(class).bind(finite).execute(&pool).await.unwrap();
                    }
                    let earlier: Uuid = sqlx::query_scalar(
                        "INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ($1,$1,true,$2) RETURNING id",
                    )
                    .bind(format!("early-{alias}"))
                    .bind(org)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    let base = format!(
                        "org: future-org\nmodels:\n  - alias: early-{alias}\n    self_hosted_only: false\n    tariffs:\n      - {{name: early, purpose: realtime, input_per_million_tokens: '1', output_per_million_tokens: '1'}}\n  - alias: {alias}\n    self_hosted_only: false\n"
                    );
                    // Positive control: this earlier entry really changes a price
                    // and overlay before the later entry is processed.
                    let changed = base
                        .replace("input_per_million_tokens: '1'", "input_per_million_tokens: '2'")
                        .replace("self_hosted_only: false", "self_hosted_only: true");
                    write(
                        directory.path(),
                        "org.yaml",
                        changed.split(&format!("  - alias: {alias}\n")).next().unwrap(),
                    );
                    apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
                    let rate: rust_decimal::Decimal = sqlx::query_scalar(
                        "SELECT input_price_per_token FROM model_tariffs WHERE deployed_model_id=$1 AND valid_until IS NULL",
                    )
                    .bind(earlier)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    assert_eq!(rate, rust_decimal::Decimal::new(2, 6));
                    write(
                        directory.path(),
                        "org.yaml",
                        base.split(&format!("  - alias: {alias}\n")).next().unwrap(),
                    );
                    apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
                    let mut before = Vec::new();
                    for table in ["model_tariffs", "model_cache_tariffs", "model_overlays"] {
                        let snapshot: Option<serde_json::Value> =
                            sqlx::query_scalar(&format!("SELECT jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text) FROM {table} t"))
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        before.push(snapshot);
                    }
                    let price = if cache {
                        "cache_tariff: {write_multiplier_5m: '1', write_multiplier_1h: '1', write_multiplier_24h: '1', read_multiplier: '0'}".to_owned()
                    } else {
                        "tariffs:\n  - {name: replacement, purpose: realtime, input_per_million_tokens: '3', output_per_million_tokens: '4'}".to_owned()
                    };
                    let scope = if class.is_some() {
                        "    class_pricing:\n      interactive:\n"
                    } else {
                        ""
                    };
                    let indent = if class.is_some() { "        " } else { "    " };
                    let price = price.lines().map(|line| format!("{indent}{line}\n")).collect::<String>();
                    let yaml = format!("{}{scope}{price}", changed);
                    write(directory.path(), "org.yaml", &yaml);
                    let err = apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap_err();
                    let expected = if cache { "model_cache_tariffs" } else { "model_tariffs" };
                    let message = format!("{err:#}");
                    assert!(message.contains(expected) && message.contains("future tariff"), "{alias}: {err:#}");
                    assert!(message.contains("future-org") && message.contains(&alias));
                    if let Some(class) = class {
                        assert!(message.contains(class));
                    }
                    for (table, before) in ["model_tariffs", "model_cache_tariffs", "model_overlays"].into_iter().zip(before) {
                        let after: Option<serde_json::Value> =
                            sqlx::query_scalar(&format!("SELECT jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text) FROM {table} t"))
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        assert_eq!(after, before, "{alias}: failed reconciliation must preserve {table}");
                    }
                    // Even a different tier adopts the entire declared org/model,
                    // so its existing future schedule must be resolved first.
                    write(
                        directory.path(),
                        "org.yaml",
                        &format!(
                            "{base}    tariffs:\n      - {{name: batch, purpose: batch, completion_window: 24h, input_per_million_tokens: '1', output_per_million_tokens: '2'}}\n"
                        ),
                    );
                    let error = apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap_err();
                    assert!(format!("{error:#}").contains("future tariff"));
                }
            }
        }
    }

    #[sqlx::test]
    async fn catalog_adopts_declared_prices_and_preserves_undeclared_models(pool: PgPool) {
        let org: Uuid = sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('manual-org','manual@example.com','test','organization') RETURNING id").fetch_one(&pool).await.unwrap();
        let mut models = Vec::new();
        for alias in ["manual-model", "omitted-model"] {
            let model: Uuid = sqlx::query_scalar(
                "INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ($1,$1,true,$2) RETURNING id",
            )
            .bind(alias)
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
            models.push(model);
            sqlx::query("INSERT INTO model_tariffs (deployed_model_id,user_id,name,api_key_purpose,input_price_per_token,output_price_per_token) VALUES ($1,$2,'manual','realtime',1,2)").bind(model).bind(org).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO model_cache_tariffs (deployed_model_id,user_id,read_multiplier,min_prefix_tokens,write_multiplier_5m,write_multiplier_1h,write_multiplier_24h) VALUES ($1,$2,0.5,1,1,1,1)").bind(model).bind(org).execute(&pool).await.unwrap();
        }
        let directory = tempdir().unwrap();
        // A routing-only declaration retires omitted same-model prices,
        // without changing undeclared models of the same organization.
        write(
            directory.path(),
            "org.yaml",
            "org: manual-org\nmodels:\n  - alias: manual-model\n    self_hosted_only: true\n",
        );
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        for table in ["model_tariffs", "model_cache_tariffs"] {
            let rows: Vec<(Uuid, Option<DateTime<Utc>>, Option<String>)> = sqlx::query_as(&format!(
                "SELECT deployed_model_id,valid_until,provisioning_source FROM {table} WHERE user_id=$1 ORDER BY deployed_model_id"
            ))
            .bind(org)
            .fetch_all(&pool)
            .await
            .unwrap();
            for (model, until, source) in rows {
                assert_eq!(until.is_some(), model == models[0]);
                assert_eq!(source.as_deref(), (model == models[0]).then_some("org-overlays"));
            }
        }
        // Adding a tier versions the declared model's ledger.
        let batch = "org: manual-org\nmodels:\n  - alias: manual-model\n    tariffs:\n      - {name: batch, purpose: batch, completion_window: 24h, input_per_million_tokens: '1', output_per_million_tokens: '2'}\n";
        write(directory.path(), "org.yaml", batch);
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        // A changed price replaces the current catalog version.
        write(
            directory.path(),
            "org.yaml",
            "org: manual-org\nmodels:\n  - alias: manual-model\n    tariffs:\n      - {name: replacement, purpose: realtime, input_per_million_tokens: '3', output_per_million_tokens: '4'}\n",
        );
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        write(
            directory.path(),
            "org.yaml",
            "org: manual-org\nmodels:\n  - alias: manual-model\n    cache_tariff: {write_multiplier_5m: '1', write_multiplier_1h: '1', write_multiplier_24h: '1', read_multiplier: '0'}\n",
        );
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        // Removing the file retires owned prices and preserves the unrelated model.
        std::fs::remove_file(directory.path().join("org.yaml")).unwrap();
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        for table in ["model_tariffs", "model_cache_tariffs"] {
            let n: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM {table} WHERE user_id=$1 AND valid_until IS NULL AND provisioning_source IS NULL"
            ))
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(n, 1, "the undeclared model retains its manual {table} price");
        }
        let active_owned: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM model_tariffs WHERE user_id=$1 AND provisioning_source='org-overlays' AND valid_until IS NULL",
        )
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_owned, 0);
    }
    async fn review_org_model(pool: &PgPool) -> (Uuid, Uuid) {
        let org: Uuid=sqlx::query_scalar("INSERT INTO users (username,email,auth_source,user_type) VALUES ('review-org','review@example.com','test','organization') RETURNING id").fetch_one(pool).await.unwrap();
        let model: Uuid=sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,is_composite,created_by) VALUES ('review-model','review-model',true,$1) RETURNING id").bind(org).fetch_one(pool).await.unwrap();
        (org, model)
    }

    #[sqlx::test]
    async fn waiting_replica_uses_time_after_org_catalog_lock(pool: PgPool) {
        review_org_model(&pool).await;
        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "org.yaml",
            "org: review-org\nmodels:\n  - alias: review-model\n    tariffs:\n      - {name: price, purpose: realtime, input_per_million_tokens: '1', output_per_million_tokens: '2'}\n    cache_tariff: {write_multiplier_5m: '1', write_multiplier_1h: '1', write_multiplier_24h: '1', read_multiplier: '0.10000'}\n",
        );
        let catalog = OrgCatalog::load(directory.path()).unwrap();
        let mut older = pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *older).await.unwrap();
        let mut winner = pool.begin().await.unwrap();
        apply_in(&mut winner, &catalog).await.unwrap();
        let (waiting, ()) = tokio::join!(
            async {
                apply_in(&mut older, &catalog).await?;
                older.commit().await?;
                Ok::<_, anyhow::Error>(())
            },
            async {
                crate::test::utils::wait_for_advisory_waiter(&pool, pid).await;
                winner.commit().await.unwrap();
            }
        );
        waiting.unwrap();
        for table in ["model_tariffs", "model_cache_tariffs"] {
            let versions: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(versions, 1, "{table}: waiting startup is idempotent");
        }
    }

    #[sqlx::test]
    async fn price_only_catalog_adopts_overlay_and_inherits_omitted_routing(pool: PgPool) {
        let (org, model) = review_org_model(&pool).await;
        sqlx::query("INSERT INTO model_overlays(user_id,deployed_model_id,self_hosted_only) VALUES ($1,$2,true)")
            .bind(org)
            .bind(model)
            .execute(&pool)
            .await
            .unwrap();
        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "org.yaml",
            "org: review-org\nmodels:\n  - alias: review-model\n    tariffs:\n      - {name: price, purpose: realtime, input_per_million_tokens: '1', output_per_million_tokens: '2'}\n",
        );
        let catalog = OrgCatalog::load(directory.path()).unwrap();
        apply(&pool, &catalog).await.unwrap();
        let row: (Option<bool>, Option<String>) =
            sqlx::query_as("SELECT self_hosted_only,provisioning_source FROM model_overlays WHERE user_id=$1")
                .bind(org)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, (None, Some("org-overlays:org.yaml".to_owned())));
        let prices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_tariffs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(prices, 1);
        apply(&pool, &catalog).await.unwrap();
    }

    #[sqlx::test]
    async fn class_prices_version_and_adopt_unchanged_manual_rows(pool: PgPool) {
        let (org, model) = review_org_model(&pool).await;
        let directory = tempdir().unwrap();
        for input in ["1", "2"] {
            write(
                directory.path(),
                "org.yaml",
                &format!(
                    "org: review-org\nmodels:\n  - alias: review-model\n    class_pricing:\n      interactive:\n        tariffs:\n          - {{name: deal, purpose: realtime, input_per_million_tokens: '{input}', output_per_million_tokens: '2'}}\n"
                ),
            );
            apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        }
        let rows: Vec<(rust_decimal::Decimal, DateTime<Utc>, Option<DateTime<Utc>>)> =
            sqlx::query_as("SELECT input_price_per_token,valid_from,valid_until FROM model_tariffs WHERE user_id=$1 ORDER BY valid_from")
                .bind(org)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, rust_decimal::Decimal::new(1, 6));
        assert_eq!(rows[1].0, rust_decimal::Decimal::new(2, 6));
        assert_eq!(rows[0].2, Some(rows[1].1));
        assert!(rows[1].2.is_none());
        sqlx::query("UPDATE model_tariffs SET provisioning_source=NULL WHERE deployed_model_id=$1 AND valid_until IS NULL")
            .bind(model)
            .execute(&pool)
            .await
            .unwrap();
        apply(&pool, &OrgCatalog::load(directory.path()).unwrap()).await.unwrap();
        let source: String =
            sqlx::query_scalar("SELECT provisioning_source FROM model_tariffs WHERE deployed_model_id=$1 AND valid_until IS NULL")
                .bind(model)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(source, "org-overlays");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_tariffs WHERE user_id=$1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }
}
