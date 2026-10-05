use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::model_provisioning::Catalog;

const ACCOUNT_LIMITS_LOCK: i64 = 0x4457_4143_4354_4c4d;

#[derive(Debug, Clone)]
pub struct AccountLimitsCatalog {
    files: Vec<AccountLimitsFile>,
    managed: bool,
}

#[derive(Debug, Clone)]
struct AccountLimitsFile {
    source: String,
    document: AccountLimitsDocument,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountLimitsDocument {
    account: String,
    realtime_inflight: BTreeMap<String, i32>,
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
            let document = serde_yaml::from_str(&contents).with_context(|| format!("parse account limits file {}", path.display()))?;
            files.push(AccountLimitsFile {
                source: path.display().to_string(),
                document,
            });
        }
        let catalog = Self { files, managed: true };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate_models(&self, models: &Catalog) -> Result<()> {
        let aliases: HashSet<_> = models.models.iter().map(|model| model.clay.alias.as_str()).collect();
        for file in &self.files {
            for alias in file.document.realtime_inflight.keys() {
                ensure!(
                    aliases.contains(alias.as_str()),
                    "{}: model {alias:?} is absent from the model catalog",
                    file.source
                );
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for file in &self.files {
            let account = file.document.account.as_str();
            if let Some(previous) = seen.insert(account, &file.source) {
                bail!("{}: account {account:?} is also declared in {previous}", file.source);
            }
            for (alias, limit) in &file.document.realtime_inflight {
                ensure!(*limit > 0, "{}: realtime_inflight limit on {alias:?} must be positive", file.source);
            }
        }
        Ok(())
    }
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

    let mut model_ids = Vec::new();
    let mut account_ids = Vec::new();
    let mut limits = Vec::new();
    for file in &catalog.files {
        for (alias, limit) in &file.document.realtime_inflight {
            model_ids.push(models[alias]);
            account_ids.push(accounts[&file.document.account]);
            limits.push(*limit);
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
    .execute(&mut *transaction)
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
    .execute(&mut *transaction)
    .await
    .context("write account limits")?;
    transaction.commit().await.context("commit account limits")?;
    Ok(())
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
        .flat_map(|file| file.document.realtime_inflight.keys().cloned())
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
}
