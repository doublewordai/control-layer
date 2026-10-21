//! The account catalog: one YAML file per account (an organisation or a
//! personal account) holding that account's realtime in-flight limits, its
//! scheduling priority and latency targets, and its own prices, per model and
//! per serving class.
//!
//! The older shape, a top-level `realtime_inflight: {alias: limit}`, is still
//! accepted and means the same as `models.<alias>.classes.standard.realtime_inflight`.
//!
//! Ownership: limits are owned table-wide (every undeclared override is
//! removed, as before). Policies and prices are owned per row through their
//! provisioning marker, so rows written by hand or by the organisation catalog
//! are never touched. A missing directory is unmanaged; an empty one clears
//! what this catalog owns.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::Deserialize;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::db::handlers::ModelProvisioning;
use crate::db::handlers::model_provisioning::{ACCOUNT_CATALOG_TARIFF_SOURCE, retire_omitted_account_prices};
use crate::model_provisioning::{CachePrices, Catalog, Tariff, TariffPurpose, validate_cache_tariff, validate_tariffs};
use crate::org_overlays::OrgCatalog;

const ACCOUNT_LIMITS_LOCK: i64 = 0x4457_4143_4354_4c4d;

/// Ownership marker on policy rows written by this catalog.
const POLICY_SOURCE: &str = "account-catalog";

/// Classes an account file may configure. Only `standard` exists until
/// models gain further serving classes.
const CONFIGURABLE_CLASSES: [&str; 1] = ["standard"];
const STANDARD_CLASS: &str = "standard";

#[derive(Debug, Clone)]
pub struct AccountLimitsCatalog {
    files: Vec<AccountLimitsFile>,
    managed: bool,
}

#[derive(Debug, Clone)]
struct AccountLimitsFile {
    source: String,
    document: AccountDocument,
}

/// One account's file.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountDocument {
    /// The account, by its username: an organisation or a personal account.
    pub account: String,
    /// Scheduling priority for the account's realtime requests on every model,
    /// unless a model or class sets its own. Omit to send the default.
    #[serde(default)]
    pub priority: Option<i32>,
    /// Older shape: realtime in-flight limits by model alias. Equivalent to
    /// `models.<alias>.classes.standard.realtime_inflight`.
    #[serde(default)]
    pub realtime_inflight: BTreeMap<String, i32>,
    /// Per-model policy and prices, keyed by model alias.
    #[serde(default)]
    pub models: BTreeMap<String, AccountModelPolicy>,
}

/// The account's policy and prices on one model.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountModelPolicy {
    /// Scheduling priority on this model, overriding the account's.
    #[serde(default)]
    pub priority: Option<i32>,
    /// Scheduling-toleration preset id for this model, in place of the
    /// account's own preset.
    #[serde(default)]
    pub toleration_preset: Option<String>,
    /// The account's batch and flex prices on this model, per completion
    /// window. Realtime prices are set per class.
    #[serde(default)]
    pub tariffs: Vec<Tariff>,
    /// Per-class policy, keyed by class. An omitted class keeps the model's
    /// defaults for that class.
    #[serde(default)]
    pub classes: BTreeMap<String, AccountClassPolicy>,
}

/// The account's policy and prices on one class of a model.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountClassPolicy {
    /// Concurrent realtime requests the account may have in flight.
    #[serde(default)]
    pub realtime_inflight: Option<i32>,
    /// Realtime (and playground) prices.
    #[serde(default)]
    pub tariffs: Vec<Tariff>,
    /// Prompt-cache multipliers this account pays on the class.
    #[serde(default)]
    pub cache_tariff: Option<CachePrices>,
    /// Time-to-first-token target, milliseconds.
    #[serde(default)]
    pub ttft_ms: Option<u32>,
    /// Inter-token-latency target, milliseconds.
    #[serde(default)]
    pub itl_ms: Option<u32>,
    /// Scheduling priority on this class, overriding the model's and the account's.
    #[serde(default)]
    pub priority: Option<i32>,
    /// Scheduling-toleration preset id for this class, overriding the model's
    /// and the account's.
    #[serde(default)]
    pub toleration_preset: Option<String>,
}

impl AccountModelPolicy {
    fn is_empty(&self) -> bool {
        self.priority.is_none() && self.toleration_preset.is_none() && self.tariffs.is_empty() && self.classes.is_empty()
    }
}

impl AccountClassPolicy {
    fn is_empty(&self) -> bool {
        self.realtime_inflight.is_none()
            && self.tariffs.is_empty()
            && self.cache_tariff.is_none()
            && self.ttft_ms.is_none()
            && self.itl_ms.is_none()
            && self.priority.is_none()
            && self.toleration_preset.is_none()
    }

    fn has_policy(&self) -> bool {
        self.priority.is_some() || self.ttft_ms.is_some() || self.itl_ms.is_some() || self.toleration_preset.is_some()
    }
}

impl AccountDocument {
    /// Every model alias the file names.
    fn aliases(&self) -> impl Iterator<Item = &String> {
        self.realtime_inflight.keys().chain(self.models.keys())
    }

    /// The effective limits, merging both shapes (equal duplicates are one limit).
    fn limits(&self) -> BTreeMap<&str, i32> {
        let mut limits: BTreeMap<&str, i32> = self
            .realtime_inflight
            .iter()
            .map(|(alias, limit)| (alias.as_str(), *limit))
            .collect();
        for (alias, model) in &self.models {
            if let Some(limit) = model.classes.get(STANDARD_CLASS).and_then(|class| class.realtime_inflight) {
                limits.insert(alias.as_str(), limit);
            }
        }
        limits
    }
}

impl AccountLimitsCatalog {
    pub fn load(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        if !directory.exists() {
            return Ok(Self {
                files: Vec::new(),
                managed: false,
            });
        }
        let mut paths = fs::read_dir(directory)
            .with_context(|| format!("read account limits directory {}", directory.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.retain(|path| matches!(path.extension().and_then(|value| value.to_str()), Some("yaml" | "yml")));
        paths.sort();
        let mut files = Vec::new();
        for path in paths {
            let contents = fs::read_to_string(&path).with_context(|| format!("read account limits file {}", path.display()))?;
            let value: serde_yaml::Value =
                serde_yaml::from_str(&contents).with_context(|| format!("parse account limits file {}", path.display()))?;
            let document = serde_yaml::from_value(value).with_context(|| format!("parse account limits file {}", path.display()))?;
            files.push(AccountLimitsFile {
                source: path.display().to_string(),
                document,
            });
        }
        let catalog = Self { files, managed: true };
        catalog.validate()?;
        Ok(catalog)
    }

    /// The JSON Schema of one account document.
    pub fn json_schema() -> Result<String> {
        Ok(serde_json::to_string_pretty(&schemars::schema_for!(AccountDocument))?)
    }

    pub fn validate_models(&self, models: &Catalog) -> Result<()> {
        let aliases: HashSet<_> = models.models.iter().map(|model| model.clay.alias.as_str()).collect();
        for file in &self.files {
            for alias in file.document.aliases() {
                ensure!(
                    aliases.contains(alias.as_str()),
                    "{}: model {alias:?} is absent from the model catalog",
                    file.source
                );
            }
        }
        Ok(())
    }

    /// An account/model pair is configured in one catalog only: either an
    /// account file's `models` entry or an organisation overlay, never both.
    /// A limit in the older top-level shape may sit alongside an overlay.
    /// Every toleration preset id the files name, for checking against the
    /// deployment's configured presets.
    pub fn toleration_presets(&self) -> BTreeSet<&str> {
        self.files
            .iter()
            .flat_map(|file| file.document.models.values())
            .flat_map(|model| {
                model
                    .toleration_preset
                    .as_deref()
                    .into_iter()
                    .chain(model.classes.values().filter_map(|class| class.toleration_preset.as_deref()))
            })
            .collect()
    }

    pub fn validate_against_overlays(&self, overlays: &OrgCatalog) -> Result<()> {
        let mut declared: HashMap<(String, String), &str> = HashMap::new();
        for entry in &overlays.orgs {
            for model in &entry.document.models {
                declared.insert(
                    (entry.document.org.trim().to_lowercase(), model.alias.to_lowercase()),
                    entry.source.as_str(),
                );
            }
        }
        for file in &self.files {
            for alias in file.document.models.keys() {
                let key = (file.document.account.trim().to_lowercase(), alias.to_lowercase());
                if let Some(overlay) = declared.get(&key) {
                    bail!(
                        "{}: model {alias:?} for account {:?} is also configured by organisation overlay {overlay}; configure it in one place",
                        file.source,
                        file.document.account
                    );
                }
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for file in &self.files {
            let source = file.source.as_str();
            let document = &file.document;
            let account = document.account.as_str();
            ensure!(!account.trim().is_empty(), "{source}: account cannot be empty");
            if let Some(previous) = seen.insert(account, source) {
                bail!("{source}: account {account:?} is also declared in {previous}");
            }
            // A file that declares nothing would silently clear the account.
            ensure!(
                document.priority.is_some() || !document.realtime_inflight.is_empty() || !document.models.is_empty(),
                "{source}: account {account:?} declares nothing; set `priority`, `models` or `realtime_inflight`"
            );
            validate_priority(document.priority, &format!("{source}: account {account:?}"))?;
            for (alias, limit) in &document.realtime_inflight {
                ensure!(*limit > 0, "{source}: realtime_inflight limit on {alias:?} must be positive");
            }
            for (alias, model) in &document.models {
                let context = format!("{source}: model {alias:?}");
                ensure!(!model.is_empty(), "{context} configures nothing");
                validate_priority(model.priority, &context)?;
                validate_preset(model.toleration_preset.as_deref(), &context)?;
                validate_tariffs(&model.tariffs, &context)?;
                ensure!(
                    model.tariffs.iter().all(|t| t.purpose == TariffPurpose::Batch),
                    "{context}: realtime and playground prices belong under `classes.<class>.tariffs`"
                );
                for (class, policy) in &model.classes {
                    let context = format!("{context}, class {class:?}");
                    ensure!(
                        CONFIGURABLE_CLASSES.contains(&class.as_str()),
                        "{context}: unknown class; configurable classes are {}",
                        CONFIGURABLE_CLASSES.join(", ")
                    );
                    ensure!(!policy.is_empty(), "{context} configures nothing");
                    if let Some(limit) = policy.realtime_inflight {
                        ensure!(limit > 0, "{context}: realtime_inflight must be positive");
                        if let Some(older) = document.realtime_inflight.get(alias) {
                            ensure!(
                                *older == limit,
                                "{context}: realtime_inflight {limit} conflicts with the top-level realtime_inflight {older}"
                            );
                        }
                    }
                    validate_tariffs(&policy.tariffs, &context)?;
                    ensure!(
                        policy.tariffs.iter().all(|t| t.purpose != TariffPurpose::Batch),
                        "{context}: batch prices belong in the model's `tariffs`"
                    );
                    if let Some(cache) = &policy.cache_tariff {
                        validate_cache_tariff(&cache.as_tariff(), &context)?;
                    }
                    validate_priority(policy.priority, &context)?;
                    validate_preset(policy.toleration_preset.as_deref(), &context)?;
                    validate_target(policy.ttft_ms, "ttft_ms", &context)?;
                    validate_target(policy.itl_ms, "itl_ms", &context)?;
                }
            }
        }
        Ok(())
    }
}

/// `i32::MIN` is the batch daemon's background tier; realtime policy may not
/// tie with it.
fn validate_priority(priority: Option<i32>, context: &str) -> Result<()> {
    ensure!(
        priority != Some(i32::MIN),
        "{context}: priority {} is reserved for background work",
        i32::MIN
    );
    Ok(())
}

/// A preset id is the operator's slug from `scheduling.toleration_presets`.
/// Whether it is configured is checked at startup (and an unconfigured id
/// fails closed at request time), since the files are validated without the
/// deployment's config.
fn validate_preset(preset: Option<&str>, context: &str) -> Result<()> {
    if let Some(preset) = preset {
        ensure!(
            !preset.is_empty() && preset.trim() == preset,
            "{context}: toleration_preset must be a non-empty id without surrounding whitespace"
        );
    }
    Ok(())
}

/// Latency targets are stored as positive `INTEGER`s.
fn validate_target(target: Option<u32>, field: &str, context: &str) -> Result<()> {
    if let Some(target) = target {
        ensure!(
            target > 0 && i32::try_from(target).is_ok(),
            "{context}: {field} must be positive and at most {}",
            i32::MAX
        );
    }
    Ok(())
}

/// One policy row: an account's priority and targets on a model (class
/// `None`) or on one of its classes.
struct PolicyRow {
    account: Uuid,
    model: Uuid,
    class: Option<String>,
    priority: Option<i32>,
    ttft_ms: Option<i32>,
    itl_ms: Option<i32>,
    toleration_preset: Option<String>,
}

pub async fn apply(pool: &PgPool, catalog: &AccountLimitsCatalog) -> Result<()> {
    if !catalog.managed {
        return Ok(());
    }
    let mut transaction = pool.begin().await.context("begin account limits transaction")?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ACCOUNT_LIMITS_LOCK)
        .execute(&mut *transaction)
        .await
        .context("acquire account limits advisory lock")?;
    let accounts = resolve_accounts(&mut transaction, catalog).await?;
    let models = resolve_models(&mut transaction, catalog).await?;
    // A replica may have begun its transaction before the lock winner. Use the
    // wall clock after locking so the winner's committed prices are not "future".
    let effective_at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *transaction)
        .await
        .context("read account catalog effective timestamp")?;

    apply_limits(&mut transaction, catalog, &accounts, &models).await?;
    apply_priorities(&mut transaction, catalog, &accounts).await?;
    apply_policies(&mut transaction, catalog, &accounts, &models).await?;
    apply_prices(&mut transaction, catalog, &accounts, &models, effective_at).await?;

    transaction.commit().await.context("commit account limits")?;
    Ok(())
}

async fn apply_limits(
    db: &mut PgConnection,
    catalog: &AccountLimitsCatalog,
    accounts: &HashMap<String, Uuid>,
    models: &HashMap<String, Uuid>,
) -> Result<()> {
    let mut model_ids = Vec::new();
    let mut account_ids = Vec::new();
    let mut limits = Vec::new();
    for file in &catalog.files {
        for (alias, limit) in file.document.limits() {
            model_ids.push(models[alias]);
            account_ids.push(accounts[&file.document.account]);
            limits.push(limit);
        }
    }

    sqlx::query(
        "DELETE FROM realtime_inflight_limit_overrides o
         WHERE NOT EXISTS (
             SELECT 1 FROM UNNEST($1::uuid[], $2::uuid[]) AS d(deployed_model_id, user_id)
             WHERE d.deployed_model_id = o.deployed_model_id AND d.user_id = o.user_id
         )",
    )
    .bind(&model_ids)
    .bind(&account_ids)
    .execute(&mut *db)
    .await
    .context("remove undeclared account limits")?;
    sqlx::query(
        "INSERT INTO realtime_inflight_limit_overrides (deployed_model_id, user_id, inflight_limit)
         SELECT d.deployed_model_id, d.user_id, d.inflight_limit
         FROM UNNEST($1::uuid[], $2::uuid[], $3::int[]) AS d(deployed_model_id, user_id, inflight_limit)
         ON CONFLICT (deployed_model_id, user_id) DO UPDATE SET inflight_limit = EXCLUDED.inflight_limit
         WHERE realtime_inflight_limit_overrides.inflight_limit IS DISTINCT FROM EXCLUDED.inflight_limit",
    )
    .bind(&model_ids)
    .bind(&account_ids)
    .bind(&limits)
    .execute(&mut *db)
    .await
    .context("write account limits")?;
    Ok(())
}

/// The account-wide priority column is written only by this catalog: declared
/// accounts get their value (or none), every other account is cleared.
async fn apply_priorities(db: &mut PgConnection, catalog: &AccountLimitsCatalog, accounts: &HashMap<String, Uuid>) -> Result<()> {
    let (ids, priorities): (Vec<Uuid>, Vec<Option<i32>>) = catalog
        .files
        .iter()
        .map(|file| (accounts[&file.document.account], file.document.priority))
        .unzip();
    sqlx::query(
        "UPDATE users u SET scheduling_priority = d.priority
         FROM UNNEST($1::uuid[], $2::int[]) AS d(id, priority)
         WHERE u.id = d.id AND u.scheduling_priority IS DISTINCT FROM d.priority",
    )
    .bind(&ids)
    .bind(&priorities)
    .execute(&mut *db)
    .await
    .context("write account priorities")?;
    sqlx::query("UPDATE users SET scheduling_priority = NULL WHERE scheduling_priority IS NOT NULL AND NOT (id = ANY($1))")
        .bind(&ids)
        .execute(&mut *db)
        .await
        .context("clear undeclared account priorities")?;
    Ok(())
}

async fn apply_policies(
    db: &mut PgConnection,
    catalog: &AccountLimitsCatalog,
    accounts: &HashMap<String, Uuid>,
    models: &HashMap<String, Uuid>,
) -> Result<()> {
    let mut rows = Vec::new();
    for file in &catalog.files {
        let account = accounts[&file.document.account];
        for (alias, model) in &file.document.models {
            let model_id = models[alias];
            if model.priority.is_some() || model.toleration_preset.is_some() {
                rows.push(PolicyRow {
                    account,
                    model: model_id,
                    class: None,
                    priority: model.priority,
                    ttft_ms: None,
                    itl_ms: None,
                    toleration_preset: model.toleration_preset.clone(),
                });
            }
            for (class, policy) in &model.classes {
                if policy.has_policy() {
                    rows.push(PolicyRow {
                        account,
                        model: model_id,
                        class: Some(class.clone()),
                        priority: policy.priority,
                        ttft_ms: policy.ttft_ms.map(i32::try_from).transpose()?,
                        itl_ms: policy.itl_ms.map(i32::try_from).transpose()?,
                        toleration_preset: policy.toleration_preset.clone(),
                    });
                }
            }
        }
    }
    let accounts_col: Vec<Uuid> = rows.iter().map(|r| r.account).collect();
    let models_col: Vec<Uuid> = rows.iter().map(|r| r.model).collect();
    let classes_col: Vec<Option<String>> = rows.iter().map(|r| r.class.clone()).collect();
    let priorities_col: Vec<Option<i32>> = rows.iter().map(|r| r.priority).collect();
    let ttft_col: Vec<Option<i32>> = rows.iter().map(|r| r.ttft_ms).collect();
    let itl_col: Vec<Option<i32>> = rows.iter().map(|r| r.itl_ms).collect();
    let presets_col: Vec<Option<String>> = rows.iter().map(|r| r.toleration_preset.clone()).collect();

    sqlx::query(
        "DELETE FROM account_model_policies p
         WHERE p.provisioning_source = $4
           AND NOT EXISTS (
               SELECT 1 FROM UNNEST($1::uuid[], $2::uuid[], $3::text[]) AS d(user_id, deployed_model_id, serving_class)
               WHERE d.user_id = p.user_id AND d.deployed_model_id = p.deployed_model_id
                 AND d.serving_class IS NOT DISTINCT FROM p.serving_class
           )",
    )
    .bind(&accounts_col)
    .bind(&models_col)
    .bind(&classes_col)
    .bind(POLICY_SOURCE)
    .execute(&mut *db)
    .await
    .context("remove undeclared account policies")?;
    // Unowned rows on a declared scope are adopted, as manual prices are; a
    // row another writer owns is never taken over.
    let foreign: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT u.username, dm.alias, p.serving_class, p.provisioning_source
         FROM account_model_policies p
         JOIN UNNEST($1::uuid[], $2::uuid[], $3::text[]) AS d(user_id, deployed_model_id, serving_class)
           ON d.user_id = p.user_id AND d.deployed_model_id = p.deployed_model_id
          AND d.serving_class IS NOT DISTINCT FROM p.serving_class
         JOIN users u ON u.id = p.user_id
         JOIN deployed_models dm ON dm.id = p.deployed_model_id
         WHERE p.provisioning_source IS NOT NULL AND p.provisioning_source <> $4
         ORDER BY u.username, dm.alias, p.serving_class NULLS FIRST",
    )
    .bind(&accounts_col)
    .bind(&models_col)
    .bind(&classes_col)
    .bind(POLICY_SOURCE)
    .fetch_all(&mut *db)
    .await
    .context("check account policy ownership")?;
    if let Some((account, alias, class, owner)) = foreign.first() {
        bail!(
            "account {account:?} policy on {alias:?} (class {}) is owned by {owner:?}; remove it there first",
            class.as_deref().unwrap_or("all")
        );
    }
    sqlx::query(
        "INSERT INTO account_model_policies
             (user_id, deployed_model_id, serving_class, priority, ttft_ms, itl_ms, toleration_preset, provisioning_source)
         SELECT d.user_id, d.deployed_model_id, d.serving_class, d.priority, d.ttft_ms, d.itl_ms, d.toleration_preset, $8
         FROM UNNEST($1::uuid[], $2::uuid[], $3::text[], $4::int[], $5::int[], $6::int[], $7::text[])
              AS d(user_id, deployed_model_id, serving_class, priority, ttft_ms, itl_ms, toleration_preset)
         ON CONFLICT ON CONSTRAINT uq_account_model_policies_scope DO UPDATE SET
             priority = EXCLUDED.priority,
             ttft_ms = EXCLUDED.ttft_ms,
             itl_ms = EXCLUDED.itl_ms,
             toleration_preset = EXCLUDED.toleration_preset,
             provisioning_source = EXCLUDED.provisioning_source,
             updated_at = NOW()
         WHERE (account_model_policies.priority, account_model_policies.ttft_ms, account_model_policies.itl_ms,
                account_model_policies.toleration_preset, account_model_policies.provisioning_source)
               IS DISTINCT FROM (EXCLUDED.priority, EXCLUDED.ttft_ms, EXCLUDED.itl_ms, EXCLUDED.toleration_preset,
                                 EXCLUDED.provisioning_source)",
    )
    .bind(&accounts_col)
    .bind(&models_col)
    .bind(&classes_col)
    .bind(&priorities_col)
    .bind(&ttft_col)
    .bind(&itl_col)
    .bind(&presets_col)
    .bind(POLICY_SOURCE)
    .execute(&mut *db)
    .await
    .context("write account policies")?;
    Ok(())
}

/// Prices for every declared account/model: a temporal ledger scoped to the
/// account, versioned like the model catalog's general prices. A declared
/// pair is wholly catalog-owned: its manual prices are adopted, and prices it
/// no longer declares are closed without deleting history.
async fn apply_prices(
    db: &mut PgConnection,
    catalog: &AccountLimitsCatalog,
    accounts: &HashMap<String, Uuid>,
    models: &HashMap<String, Uuid>,
    effective_at: DateTime<Utc>,
) -> Result<()> {
    let mut declared = Vec::new();
    for file in &catalog.files {
        let account = accounts[&file.document.account];
        let context = |alias: &str| format!("{}: account {:?}, model {alias:?}", file.source, file.document.account);
        if !file.document.models.is_empty() {
            ModelProvisioning::for_account_catalog(db)
                .preflight_future_account_tariffs(account, effective_at)
                .await
                .with_context(|| format!("{}: account {:?}", file.source, file.document.account))?;
        }
        for (alias, model) in &file.document.models {
            let model_id = models[alias];
            declared.push((account, model_id));
            let mut provisioning = ModelProvisioning::for_account_catalog(db);
            provisioning
                .adopt_account_model_tariffs(account, model_id, effective_at)
                .await
                .with_context(|| context(alias))?;
            provisioning
                .reconcile_tariffs(model_id, Some(account), None, &model.tariffs, effective_at)
                .await
                .with_context(|| context(alias))?;
            provisioning
                .reconcile_cache_tariff(model_id, Some(account), None, None, effective_at)
                .await
                .with_context(|| context(alias))?;
            let standard = model.classes.get(STANDARD_CLASS);
            provisioning
                .reconcile_tariffs(
                    model_id,
                    Some(account),
                    Some(STANDARD_CLASS),
                    standard.map_or(&[], |class| class.tariffs.as_slice()),
                    effective_at,
                )
                .await
                .with_context(|| context(alias))?;
            let cache = standard.and_then(|class| class.cache_tariff.as_ref()).map(CachePrices::as_tariff);
            provisioning
                .reconcile_cache_tariff(model_id, Some(account), Some(STANDARD_CLASS), cache.as_ref(), effective_at)
                .await
                .with_context(|| context(alias))?;
            // Rows adopted under any other class scope are not declared: close them.
            for table in ["model_tariffs", "model_cache_tariffs"] {
                sqlx::query(&format!(
                    "UPDATE {table} SET valid_until = $4
                     WHERE user_id = $1 AND deployed_model_id = $2 AND provisioning_source = $3
                       AND serving_class IS NOT NULL AND serving_class <> 'standard'
                       AND valid_from <= $4 AND (valid_until IS NULL OR valid_until > $4)"
                ))
                .bind(account)
                .bind(model_id)
                .bind(ACCOUNT_CATALOG_TARIFF_SOURCE)
                .bind(effective_at)
                .execute(&mut *db)
                .await
                .with_context(|| context(alias))?;
            }
        }
    }
    retire_omitted_account_prices(db, ACCOUNT_CATALOG_TARIFF_SOURCE, &declared, effective_at).await
}

async fn resolve_accounts(db: &mut PgConnection, catalog: &AccountLimitsCatalog) -> Result<HashMap<String, Uuid>> {
    let names: Vec<String> = catalog.files.iter().map(|file| file.document.account.clone()).collect();
    let resolved: HashMap<String, Uuid> =
        sqlx::query_as::<_, (String, Uuid)>("SELECT username, id FROM users WHERE username = ANY($1) AND is_deleted = FALSE")
            .bind(&names)
            .fetch_all(&mut *db)
            .await
            .context("resolve account limits accounts")?
            .into_iter()
            .collect();
    let missing: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| !resolved.contains_key(*name))
        .collect();
    ensure!(missing.is_empty(), "unknown account(s) in account limits: {}", missing.join(", "));
    Ok(resolved)
}

async fn resolve_models(db: &mut PgConnection, catalog: &AccountLimitsCatalog) -> Result<HashMap<String, Uuid>> {
    let aliases: Vec<String> = catalog
        .files
        .iter()
        .flat_map(|file| file.document.aliases().cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let resolved: HashMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT alias, id FROM deployed_models WHERE alias = ANY($1) AND deleted = FALSE AND is_composite = TRUE",
    )
    .bind(&aliases)
    .fetch_all(&mut *db)
    .await
    .context("resolve account limits models")?
    .into_iter()
    .collect();
    let missing: Vec<&str> = aliases
        .iter()
        .map(String::as_str)
        .filter(|alias| !resolved.contains_key(*alias))
        .collect();
    ensure!(
        missing.is_empty(),
        "unknown or non-virtual model(s) in account limits: {}",
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

    async fn insert_account(pool: &PgPool, username: &str, user_type: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO users (username, email, display_name, auth_source, user_type) VALUES ($1, $1 || '@example.com', $1, 'test', $2) RETURNING id",
        )
        .bind(username)
        .bind(user_type)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn insert_virtual_model(pool: &PgPool, alias: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name, alias, created_by, is_composite) VALUES ($1, $1, '00000000-0000-0000-0000-000000000000', TRUE) RETURNING id",
        )
        .bind(alias)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn insert_standard_model(pool: &PgPool, alias: &str) {
        let endpoint: Uuid = sqlx::query_scalar(
            "INSERT INTO inference_endpoints (name, url, created_by) VALUES ('provider', 'http://provider.test', '00000000-0000-0000-0000-000000000000') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO deployed_models (model_name, alias, created_by, hosted_on) VALUES ($1, $1, '00000000-0000-0000-0000-000000000000', $2)",
        )
        .bind(alias)
        .bind(endpoint)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn stored(pool: &PgPool) -> Vec<(Uuid, Uuid, i32)> {
        sqlx::query_as("SELECT deployed_model_id, user_id, inflight_limit FROM realtime_inflight_limit_overrides ORDER BY inflight_limit")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn gateway_limit(pool: &PgPool, alias: &str, account: Uuid) -> u32 {
        crate::sync::onwards_config::load_targets_from_db(pool, &[], false)
            .await
            .unwrap()
            .targets
            .get(alias)
            .and_then(|pools| pools.default_pool().inflight_limits().cloned())
            .unwrap()
            .for_account(&account.to_string())
    }

    #[test]
    fn a_missing_directory_is_unmanaged() {
        let directory = tempdir().unwrap();
        let catalog = AccountLimitsCatalog::load(directory.path().join("absent")).unwrap();
        assert!(!catalog.managed);
        assert!(AccountLimitsCatalog::load(directory.path()).unwrap().managed);
    }

    #[test]
    fn files_need_positive_limits_known_fields_and_one_file_per_account() {
        for (files, diagnostic) in [
            (
                vec![("a.yaml", "account: acme\nrealtime_inflight:\n  org/model: 0\n")],
                "must be positive",
            ),
            (vec![("a.yaml", "account: acme\nrealtime:\n  org/model: 10\n")], "unknown field"),
            // A file that declares nothing is refused rather than read as an
            // empty (and therefore clearing) declaration.
            (vec![("a.yaml", "account: acme\n")], "declares nothing"),
            (
                vec![("a.yaml", "account: acme\nrealtime_inflight:\n  org/model: 10\n  org/model: 20\n")],
                "duplicate entry",
            ),
            (
                vec![
                    ("a.yaml", "account: acme\nrealtime_inflight:\n  org/model: 10\n"),
                    ("b.yaml", "account: acme\nrealtime_inflight:\n  org/other: 10\n"),
                ],
                "also declared",
            ),
        ] {
            let directory = tempdir().unwrap();
            for (name, contents) in files {
                write(directory.path(), name, contents);
            }
            let error = format!("{:#}", AccountLimitsCatalog::load(directory.path()).unwrap_err());
            assert!(error.contains(diagnostic), "{error}");
        }
    }

    #[dwctl_test_macros::test]
    async fn limits_follow_the_files_for_organisations_and_personal_accounts(pool: PgPool) {
        let org = insert_account(&pool, "acme", "organization").await;
        let person = insert_account(&pool, "bob", "individual").await;
        let other = insert_account(&pool, "carol", "individual").await;
        let model = insert_virtual_model(&pool, "org/model").await;

        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "acme.yaml",
            "account: acme\nrealtime_inflight:\n  org/model: 250\n",
        );
        write(directory.path(), "bob.yaml", "account: bob\nrealtime_inflight:\n  org/model: 70\n");
        apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap()).await.unwrap();
        assert_eq!(stored(&pool).await, vec![(model, person, 70), (model, org, 250)]);
        assert_eq!(gateway_limit(&pool, "org/model", org).await, 250);
        assert_eq!(gateway_limit(&pool, "org/model", person).await, 70);
        assert_eq!(gateway_limit(&pool, "org/model", other).await, 14);

        write(
            directory.path(),
            "acme.yaml",
            "account: acme\nrealtime_inflight:\n  org/model: 300\n",
        );
        fs::remove_file(directory.path().join("bob.yaml")).unwrap();
        apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap()).await.unwrap();
        assert_eq!(stored(&pool).await, vec![(model, org, 300)]);
        assert_eq!(gateway_limit(&pool, "org/model", person).await, 14);

        apply(&pool, &AccountLimitsCatalog::load(directory.path().join("absent")).unwrap())
            .await
            .unwrap();
        assert_eq!(stored(&pool).await, vec![(model, org, 300)]);

        fs::remove_file(directory.path().join("acme.yaml")).unwrap();
        apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap()).await.unwrap();
        assert!(stored(&pool).await.is_empty());
    }

    #[dwctl_test_macros::test]
    async fn unknown_accounts_and_standard_models_are_refused_before_anything_is_written(pool: PgPool) {
        insert_account(&pool, "acme", "organization").await;
        insert_account(&pool, "bob", "individual").await;
        insert_virtual_model(&pool, "org/model").await;
        insert_standard_model(&pool, "org/component").await;

        for (contents, diagnostic) in [
            ("account: nobody\nrealtime_inflight:\n  org/model: 10\n", "unknown account"),
            ("account: bob\nrealtime_inflight:\n  org/component: 10\n", "non-virtual model"),
        ] {
            let directory = tempdir().unwrap();
            write(directory.path(), "a.yaml", "account: acme\nrealtime_inflight:\n  org/model: 10\n");
            write(directory.path(), "b.yaml", contents);
            let error = format!(
                "{:#}",
                apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap())
                    .await
                    .unwrap_err()
            );
            assert!(error.contains(diagnostic), "{error}");
            assert!(stored(&pool).await.is_empty());
        }
    }

    const REALTIME: &str = "{name: deal, purpose: realtime, input_per_million_tokens: '1', output_per_million_tokens: '2'}";
    const BATCH: &str =
        "{name: deal-24h, purpose: batch, completion_window: 24h, input_per_million_tokens: '0.5', output_per_million_tokens: '1'}";

    #[test]
    fn the_new_shape_is_validated() {
        for (contents, diagnostic) in [
            (
                format!("account: acme\nmodels:\n  org/model:\n    tariffs: [{REALTIME}]\n"),
                "realtime and playground prices belong under",
            ),
            (
                format!("account: acme\nmodels:\n  org/model:\n    classes:\n      standard:\n        tariffs: [{BATCH}]\n"),
                "batch prices belong in the model's",
            ),
            (
                "account: acme\nmodels:\n  org/model:\n    classes:\n      fast:\n        realtime_inflight: 5\n".to_owned(),
                "unknown class",
            ),
            (
                "account: acme\nrealtime_inflight:\n  org/model: 10\nmodels:\n  org/model:\n    classes:\n      standard:\n        realtime_inflight: 20\n".to_owned(),
                "conflicts with the top-level",
            ),
            ("account: acme\nmodels:\n  org/model: {}\n".to_owned(), "configures nothing"),
            (
                "account: acme\nmodels:\n  org/model:\n    classes:\n      standard: {}\n".to_owned(),
                "configures nothing",
            ),
            (
                "account: acme\nmodels:\n  org/model:\n    classes:\n      standard:\n        ttft_ms: 0\n".to_owned(),
                "ttft_ms must be positive",
            ),
            (
                "account: acme\nmodels:\n  org/model:\n    toleration_preset: ' gpu'\n".to_owned(),
                "toleration_preset must be a non-empty id",
            ),
            (
                "account: acme\nmodels:\n  org/model:\n    classes:\n      standard:\n        toleration_preset: ''\n".to_owned(),
                "toleration_preset must be a non-empty id",
            ),
            (
                "account: acme\nmodels:\n  org/model:\n    classes:\n      standard:\n        itl_ms: 2147483648\n".to_owned(),
                "itl_ms must be positive and at most",
            ),
            ("account: acme\npriority: -2147483648\n".to_owned(), "reserved for background work"),
            (
                "account: acme\nmodels:\n  org/model:\n    priority: -2147483648\n".to_owned(),
                "reserved for background work",
            ),
            (
                "account: acme\nmodels:\n  org/model:\n    classes:\n      standard:\n        realtime_inflight: 0\n".to_owned(),
                "must be positive",
            ),
            (
                "account: acme\nmodels:\n  org/model:\n    classes:\n      standard:\n        unknown: 1\n".to_owned(),
                "unknown field",
            ),
        ] {
            let directory = tempdir().unwrap();
            write(directory.path(), "a.yaml", &contents);
            let error = format!("{:#}", AccountLimitsCatalog::load(directory.path()).unwrap_err());
            assert!(error.contains(diagnostic), "{contents}\n{error}");
        }

        // Equal limits in both shapes are one limit; priority alone is a declaration.
        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "a.yaml",
            "account: acme\nrealtime_inflight:\n  org/model: 10\nmodels:\n  org/model:\n    classes:\n      standard:\n        realtime_inflight: 10\n",
        );
        write(directory.path(), "b.yaml", "account: bob\npriority: 100\n");
        let catalog = AccountLimitsCatalog::load(directory.path()).unwrap();
        assert_eq!(
            catalog.files[0].document.limits().into_iter().collect::<Vec<_>>(),
            vec![("org/model", 10)]
        );
    }

    #[test]
    fn an_account_model_pair_is_configured_in_one_catalog() {
        let overlays = tempdir().unwrap();
        fs::write(
            overlays.path().join("acme.yaml"),
            "org: acme\nmodels:\n  - alias: org/model\n    self_hosted_only: true\n",
        )
        .unwrap();
        let overlays = OrgCatalog::load(overlays.path()).unwrap();

        let directory = tempdir().unwrap();
        // The older limits shape may sit alongside an overlay...
        write(
            directory.path(),
            "acme.yaml",
            "account: acme\nrealtime_inflight:\n  org/model: 10\n",
        );
        AccountLimitsCatalog::load(directory.path())
            .unwrap()
            .validate_against_overlays(&overlays)
            .unwrap();
        // ...a `models` entry for the same pair may not.
        write(
            directory.path(),
            "acme.yaml",
            "account: acme\nmodels:\n  org/model:\n    priority: 5\n",
        );
        let error = AccountLimitsCatalog::load(directory.path())
            .unwrap()
            .validate_against_overlays(&overlays)
            .unwrap_err()
            .to_string();
        assert!(error.contains("also configured by organisation overlay"), "{error}");
    }

    async fn policies(pool: &PgPool) -> Vec<(Uuid, Option<String>, Option<i32>, Option<i32>, Option<i32>)> {
        sqlx::query_as(
            "SELECT user_id, serving_class, priority, ttft_ms, itl_ms FROM account_model_policies ORDER BY user_id, serving_class NULLS FIRST",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn active_prices(pool: &PgPool, account: Uuid) -> Vec<(Option<String>, String, Option<String>, String)> {
        sqlx::query_as(
            "SELECT serving_class, api_key_purpose, completion_window, provisioning_source FROM model_tariffs
             WHERE user_id = $1 AND valid_until IS NULL ORDER BY serving_class NULLS FIRST, api_key_purpose",
        )
        .bind(account)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[dwctl_test_macros::test]
    async fn the_account_catalog_writes_priorities_policies_limits_and_prices(pool: PgPool) {
        let org = insert_account(&pool, "acme", "organization").await;
        let person = insert_account(&pool, "bob", "individual").await;
        let model = insert_virtual_model(&pool, "org/model").await;

        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "acme.yaml",
            &format!(
                "account: acme\npriority: 100\nmodels:\n  org/model:\n    priority: 300\n    tariffs: [{BATCH}]\n    classes:\n      standard:\n        realtime_inflight: 900\n        tariffs: [{REALTIME}]\n        cache_tariff: {{read_multiplier: '0.2', write_multiplier_5m: '1', write_multiplier_1h: '1', write_multiplier_24h: '1'}}\n        ttft_ms: 1000\n        itl_ms: 30\n"
            ),
        );
        write(
            directory.path(),
            "bob.yaml",
            &format!("account: bob\nmodels:\n  org/model:\n    classes:\n      standard:\n        tariffs: [{REALTIME}]\n"),
        );
        let catalog = AccountLimitsCatalog::load(directory.path()).unwrap();
        apply(&pool, &catalog).await.unwrap();
        // A second identical start writes nothing new.
        apply(&pool, &catalog).await.unwrap();

        let priority: Option<i32> = sqlx::query_scalar("SELECT scheduling_priority FROM users WHERE id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(priority, Some(100));
        assert_eq!(
            policies(&pool).await,
            vec![
                (org, None, Some(300), None, None),
                (org, Some("standard".into()), None, Some(1000), Some(30))
            ]
        );
        assert_eq!(stored(&pool).await, vec![(model, org, 900)]);
        assert_eq!(
            active_prices(&pool, org).await,
            vec![
                (None, "batch".into(), Some("24h".into()), ACCOUNT_CATALOG_TARIFF_SOURCE.into()),
                (
                    Some("standard".into()),
                    "realtime".into(),
                    None,
                    ACCOUNT_CATALOG_TARIFF_SOURCE.into()
                ),
            ]
        );
        let cache: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM model_cache_tariffs WHERE user_id = $1 AND serving_class = 'standard' AND valid_until IS NULL",
        )
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cache, 1);
        assert_eq!(
            active_prices(&pool, person).await,
            vec![(
                Some("standard".into()),
                "realtime".into(),
                None,
                ACCOUNT_CATALOG_TARIFF_SOURCE.into()
            )],
            "personal accounts carry deals too"
        );

        // The gateway sees the account default and the model/class policy.
        let targets = crate::sync::onwards_config::load_targets_from_db(&pool, &[], false).await.unwrap();
        assert_eq!(targets.accounts.get(&org.to_string()).unwrap().priority, Some(100));
        let overlays = targets.targets.get("org/model").unwrap().default_pool().overlays().clone();
        let overlay = &overlays[&org.to_string()];
        assert_eq!(
            (overlay.priority, overlay.ttft_ms, overlay.itl_ms),
            (Some(300), Some(1000), Some(30))
        );
        assert!(overlay.targets.is_none(), "policy never becomes billed custom targets");
        assert_eq!(gateway_limit(&pool, "org/model", org).await, 900);

        // Removing the files clears what this catalog owns, keeping history.
        fs::remove_file(directory.path().join("acme.yaml")).unwrap();
        fs::remove_file(directory.path().join("bob.yaml")).unwrap();
        apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap()).await.unwrap();
        let priority: Option<i32> = sqlx::query_scalar("SELECT scheduling_priority FROM users WHERE id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(priority, None);
        assert!(policies(&pool).await.is_empty());
        assert!(stored(&pool).await.is_empty());
        assert!(active_prices(&pool, org).await.is_empty());
        assert!(active_prices(&pool, person).await.is_empty());
        let history: i64 = sqlx::query_scalar("SELECT count(*) FROM model_tariffs WHERE user_id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(history, 2, "closed price versions stay for billing history");
    }

    #[dwctl_test_macros::test]
    async fn manual_prices_are_adopted_and_other_catalogs_prices_are_refused(pool: PgPool) {
        let org = insert_account(&pool, "acme", "organization").await;
        let model = insert_virtual_model(&pool, "org/model").await;
        // A hand-written deal on the pair is adopted and replaced by the file.
        sqlx::query(
            "INSERT INTO model_tariffs (deployed_model_id, user_id, serving_class, name, api_key_purpose, input_price_per_token, output_price_per_token)
             VALUES ($1, $2, 'standard', 'hand', 'realtime', 0.000009, 0.000009)",
        )
        .bind(model)
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "acme.yaml",
            &format!("account: acme\nmodels:\n  org/model:\n    classes:\n      standard:\n        tariffs: [{REALTIME}]\n"),
        );
        apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap()).await.unwrap();
        let active: Vec<(String, String)> =
            sqlx::query_as("SELECT name, provisioning_source FROM model_tariffs WHERE user_id = $1 AND valid_until IS NULL")
                .bind(org)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(active, vec![("deal".into(), ACCOUNT_CATALOG_TARIFF_SOURCE.into())]);

        // A price the organisation catalog owns is never taken over.
        let other = insert_account(&pool, "other", "organization").await;
        sqlx::query(
            "INSERT INTO model_tariffs (deployed_model_id, user_id, serving_class, name, api_key_purpose, input_price_per_token, output_price_per_token, provisioning_source)
             VALUES ($1, $2, 'standard', 'overlay', 'realtime', 0.000003, 0.000003, 'org-overlays')",
        )
        .bind(model)
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
        write(
            directory.path(),
            "other.yaml",
            &format!("account: other\nmodels:\n  org/model:\n    classes:\n      standard:\n        tariffs: [{REALTIME}]\n"),
        );
        let error = format!(
            "{:#}",
            apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap())
                .await
                .unwrap_err()
        );
        assert!(error.contains("conflicts with a manually managed tariff"), "{error}");
    }

    #[dwctl_test_macros::test]
    async fn unowned_policies_are_adopted_and_other_writers_policies_are_refused(pool: PgPool) {
        let org = insert_account(&pool, "acme", "organization").await;
        let model = insert_virtual_model(&pool, "org/model").await;
        sqlx::query("INSERT INTO account_model_policies (user_id, deployed_model_id, priority) VALUES ($1, $2, 7)")
            .bind(org)
            .bind(model)
            .execute(&pool)
            .await
            .unwrap();
        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "acme.yaml",
            "account: acme\nmodels:\n  org/model:\n    priority: 9\n",
        );
        let catalog = AccountLimitsCatalog::load(directory.path()).unwrap();
        apply(&pool, &catalog).await.unwrap();
        let adopted: (Option<i32>, Option<String>) =
            sqlx::query_as("SELECT priority, provisioning_source FROM account_model_policies WHERE user_id = $1")
                .bind(org)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(adopted, (Some(9), Some(POLICY_SOURCE.to_owned())));

        sqlx::query("UPDATE account_model_policies SET priority = 7, provisioning_source = 'elsewhere' WHERE user_id = $1")
            .bind(org)
            .execute(&pool)
            .await
            .unwrap();
        let error = format!("{:#}", apply(&pool, &catalog).await.unwrap_err());
        assert!(error.contains("is owned by \"elsewhere\""), "{error}");
        let untouched: (Option<i32>, Option<String>) =
            sqlx::query_as("SELECT priority, provisioning_source FROM account_model_policies WHERE user_id = $1")
                .bind(org)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(untouched, (Some(7), Some("elsewhere".to_owned())));
    }

    #[dwctl_test_macros::test]
    async fn toleration_presets_are_written_per_model_and_class_and_cleared(pool: PgPool) {
        let org = insert_account(&pool, "acme", "organization").await;
        insert_virtual_model(&pool, "org/model").await;
        let directory = tempdir().unwrap();
        write(
            directory.path(),
            "acme.yaml",
            "account: acme\nmodels:\n  org/model:\n    toleration_preset: no-overflow\n    classes:\n      standard:\n        toleration_preset: gpu\n",
        );
        let catalog = AccountLimitsCatalog::load(directory.path()).unwrap();
        assert_eq!(
            catalog.toleration_presets().into_iter().collect::<Vec<_>>(),
            vec!["gpu", "no-overflow"]
        );
        apply(&pool, &catalog).await.unwrap();
        let presets = || async {
            sqlx::query_as::<_, (Option<String>, Option<String>)>(
                "SELECT serving_class, toleration_preset FROM account_model_policies WHERE user_id = $1 ORDER BY serving_class NULLS FIRST",
            )
            .bind(org)
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        assert_eq!(
            presets().await,
            vec![(None, Some("no-overflow".into())), (Some("standard".into()), Some("gpu".into()))]
        );

        // The per-key policy sees the class preset folded over the model's.
        let key = format!("sk-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO api_keys (name, secret, user_id, created_by, purpose) VALUES ('k', $1, $2, $2, 'realtime')")
            .bind(&key)
            .bind(org)
            .execute(&pool)
            .await
            .unwrap();
        let cache = crate::sync::key_policy::KeyPolicyCache::empty();
        crate::sync::key_policy::refresh(&pool, &cache).await.unwrap();
        assert_eq!(cache.policy(&key).toleration_preset_for("org/model"), Some("gpu"));
        assert_eq!(cache.policy(&key).toleration_preset_for("org/other"), None);
        let mut conn = pool.acquire().await.unwrap();
        let direct = crate::sync::key_policy::load_policy(&mut conn, org).await.unwrap();
        assert_eq!(direct.toleration_preset_for("org/model"), Some("gpu"));

        write(directory.path(), "acme.yaml", "account: acme\npriority: 1\n");
        apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap()).await.unwrap();
        assert!(presets().await.is_empty());
    }
}
