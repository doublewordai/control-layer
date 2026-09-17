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
//! rewritten on every start, exactly like the model catalog; a row the
//! catalog does not mention is left alone.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::model_provisioning::ServingClassName;

/// Prefix of the `provisioning_source` marker on rows this catalog owns.
const SOURCE_PREFIX: &str = "org-overlays:";

#[derive(Debug, Clone)]
pub struct OrgCatalog {
    pub(crate) orgs: Vec<OrgCatalogEntry>,
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

/// The organisation's overrides on one virtual model.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgModelOverlay {
    /// A virtual model alias from the model catalog.
    pub alias: String,
    /// Overrides the account's default serving class on this model. Applies
    /// only where the organisation holds the class and the model offers it.
    #[serde(default)]
    pub default_class: Option<ServingClassName>,
    /// Overrides the account's `self_hosted_only` setting on this model.
    #[serde(default)]
    pub self_hosted_only: Option<bool>,
}

impl OrgCatalog {
    /// Load every YAML document in `directory`. A missing directory is an
    /// empty catalog: the chart may not mount one yet.
    pub fn load(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        if !directory.is_dir() {
            return Ok(Self { orgs: Vec::new() });
        }
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
        let catalog = Self { orgs };
        catalog.validate()?;
        Ok(catalog)
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
                    model.default_class.is_some() || model.self_hosted_only.is_some(),
                    "{source}: overlay for model {:?} overrides nothing",
                    model.alias
                );
            }
        }
        Ok(())
    }
}

/// Apply the catalog in one transaction. An empty catalog is a no-op that
/// leaves existing rows untouched, mirroring the model catalog.
pub async fn apply(pool: &PgPool, catalog: &OrgCatalog) -> Result<()> {
    if catalog.orgs.is_empty() {
        return Ok(());
    }
    let mut transaction = pool.begin().await.context("begin org overlay transaction")?;
    apply_in(&mut transaction, catalog).await?;
    transaction.commit().await.context("commit org overlay transaction")?;
    Ok(())
}

async fn apply_in(db: &mut PgConnection, catalog: &OrgCatalog) -> Result<()> {
    let orgs = resolve_orgs(db, catalog).await?;
    let aliases = resolve_aliases(db, catalog).await?;

    let mut desired: Vec<(Uuid, Uuid)> = Vec::new();
    for entry in &catalog.orgs {
        let org_id = orgs[entry.document.org.trim()];
        let source = format!("{SOURCE_PREFIX}{}", entry.source);
        for model in &entry.document.models {
            let model_id = aliases[&model.alias];
            desired.push((org_id, model_id));
            sqlx::query(
                r#"INSERT INTO model_overlays (user_id, deployed_model_id, default_serving_class, self_hosted_only, provisioning_source)
                   VALUES ($1, $2, $3, $4, $5)
                   ON CONFLICT (user_id, deployed_model_id) DO UPDATE SET
                       default_serving_class = EXCLUDED.default_serving_class,
                       self_hosted_only = EXCLUDED.self_hosted_only,
                       provisioning_source = EXCLUDED.provisioning_source,
                       updated_at = NOW()
                   WHERE model_overlays.default_serving_class IS DISTINCT FROM EXCLUDED.default_serving_class
                      OR model_overlays.self_hosted_only IS DISTINCT FROM EXCLUDED.self_hosted_only
                      OR model_overlays.provisioning_source IS DISTINCT FROM EXCLUDED.provisioning_source"#,
            )
            .bind(org_id)
            .bind(model_id)
            .bind(model.default_class.map(|class| class.as_db_str()))
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
    fn missing_directory_is_an_empty_catalog() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("nope");
        assert!(OrgCatalog::load(&missing).unwrap().orgs.is_empty());
        assert!(OrgCatalog::load(directory.path()).unwrap().orgs.is_empty());
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

        fs::remove_file(directory.path().join("acme2.yaml")).unwrap();
        let catalog = OrgCatalog::load(directory.path()).unwrap();
        assert_eq!(catalog.orgs.len(), 1);
        assert_eq!(catalog.orgs[0].document.models[0].default_class, Some(ServingClassName::Throughput));
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
        let row = sqlx::query("SELECT default_serving_class, self_hosted_only, provisioning_source FROM model_overlays WHERE user_id = $1 AND deployed_model_id = $2")
            .bind(org_id)
            .bind(model_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.try_get::<Option<String>, _>("default_serving_class").unwrap().as_deref(),
            Some("throughput")
        );
        assert_eq!(row.try_get::<Option<bool>, _>("self_hosted_only").unwrap(), Some(true));
        assert_eq!(
            row.try_get::<Option<String>, _>("provisioning_source").unwrap().as_deref(),
            Some("org-overlays:acme.yaml")
        );

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
}
