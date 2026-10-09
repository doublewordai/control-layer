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
    #[serde(default)]
    realtime_inflight: BTreeMap<String, i32>,
    /// Scheduling tolerations pinned to every request from this account.
    /// `None` (the key absent) means not pinned; `Some(vec![])` is a real pin
    /// (forbid tainted capacity). Same object shape as a daemon toleration.
    #[serde(default)]
    pinned_tolerations: Option<Vec<Toleration>>,
}

/// One Kubernetes-style scheduling toleration, the shape written to
/// `nvext.routing_constraints.tolerations`. Validated at load so a malformed
/// entry fails startup (and the validate-account-limits CLI) rather than
/// producing an invalid request body later.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Toleration {
    key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operator: Option<TolerationOperator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effect: Option<TolerationEffect>,
}

#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize)]
enum TolerationOperator {
    Equal,
    Exists,
}

#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize)]
enum TolerationEffect {
    #[serde(rename = "NoSchedule")]
    NoSchedule,
    #[serde(rename = "PreferNoSchedule")]
    PreferNoSchedule,
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
            for toleration in file.document.pinned_tolerations.iter().flatten() {
                validate_toleration(toleration).with_context(|| format!("{}: pinned_tolerations", file.source))?;
            }
        }
        Ok(())
    }

    /// The pinned tolerations declared across the catalog, keyed by account
    /// username. Only accounts that declare `pinned_tolerations` appear;
    /// accounts without the key are unpinned and left out. The value is the
    /// list serialised exactly as it will be written to the request body.
    pub fn pinned_tolerations(&self) -> Result<BTreeMap<String, serde_json::Value>> {
        self.files
            .iter()
            .filter_map(|file| {
                file.document
                    .pinned_tolerations
                    .as_ref()
                    .map(|tolerations| (file.document.account.clone(), tolerations))
            })
            .map(|(account, tolerations)| {
                let value = serde_json::to_value(tolerations).with_context(|| format!("serialise pinned tolerations for {account:?}"))?;
                Ok((account, value))
            })
            .collect::<Result<_>>()
    }
}

/// Reject a toleration the request path could not send: `Equal` must carry a
/// value, `Exists` must not.
fn validate_toleration(toleration: &Toleration) -> Result<()> {
    match toleration.operator.unwrap_or(TolerationOperator::Equal) {
        TolerationOperator::Equal => ensure!(toleration.value.is_some(), "operator Equal needs a value (or set operator: Exists)"),
        TolerationOperator::Exists => ensure!(toleration.value.is_none(), "operator Exists must not carry a value"),
    }
    Ok(())
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

    // Pinned tolerations replace what the files declare, like the limits above:
    // one array per pinned account, and every other stored pin cleared. The
    // account is matched by username (already resolved as existing above).
    let pinned = catalog.pinned_tolerations()?;
    let pinned_accounts: Vec<String> = pinned.keys().cloned().collect();
    let pinned_values: Vec<serde_json::Value> = pinned.into_values().collect();
    sqlx::query(
        "UPDATE users u
         SET pinned_tolerations = d.pinned
         FROM UNNEST($1::text[], $2::jsonb[]) AS d(account, pinned)
         WHERE u.username = d.account
           AND u.pinned_tolerations IS DISTINCT FROM d.pinned",
    )
    .bind(&pinned_accounts)
    .bind(&pinned_values)
    .execute(&mut *transaction)
    .await
    .context("write pinned tolerations")?;
    sqlx::query(
        "UPDATE users
         SET pinned_tolerations = NULL
         WHERE pinned_tolerations IS NOT NULL
           AND username <> ALL($1::text[])",
    )
    .bind(&pinned_accounts)
    .execute(&mut *transaction)
    .await
    .context("clear undeclared pinned tolerations")?;

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
            // An `Equal` (the default operator) toleration needs a value.
            (
                vec![("a.yaml", "account: acme\npinned_tolerations:\n  - key: dedicated\n")],
                "Equal needs a value",
            ),
            // `Exists` must not carry a value.
            (
                vec![(
                    "a.yaml",
                    "account: acme\npinned_tolerations:\n  - {key: k, operator: Exists, value: nope}\n",
                )],
                "must not carry a value",
            ),
            // Unknown enum spellings and fields are refused.
            (
                vec![(
                    "a.yaml",
                    "account: acme\npinned_tolerations:\n  - {key: k, value: v, effect: Bogus}\n",
                )],
                "unknown variant",
            ),
            (
                vec![(
                    "a.yaml",
                    "account: acme\npinned_tolerations:\n  - {key: k, value: v, operator: Sometimes}\n",
                )],
                "unknown variant",
            ),
            (
                vec![(
                    "a.yaml",
                    "account: acme\npinned_tolerations:\n  - {key: k, value: v, priority: 1}\n",
                )],
                "unknown field",
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

    /// The stored pins: only accounts that are pinned, username to the JSON
    /// list, in a stable order.
    async fn stored_pins(pool: &PgPool) -> Vec<(String, serde_json::Value)> {
        sqlx::query_as("SELECT username, pinned_tolerations FROM users WHERE pinned_tolerations IS NOT NULL ORDER BY username")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[dwctl_test_macros::test]
    async fn pinned_tolerations_are_applied_and_cleared_like_the_other_keys(pool: PgPool) {
        insert_account(&pool, "acme", "organization").await;
        insert_account(&pool, "bob", "individual").await;

        // An account may be pinned with no realtime_inflight entry at all.
        let directory = tempdir().unwrap();
        write(directory.path(), "acme.yaml", "account: acme\npinned_tolerations: []\n");
        write(
            directory.path(),
            "bob.yaml",
            "account: bob\npinned_tolerations:\n  - key: dedicated\n    value: only\n    effect: NoSchedule\n",
        );
        apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap()).await.unwrap();
        assert_eq!(
            stored_pins(&pool).await,
            vec![
                ("acme".to_string(), serde_json::json!([])),
                (
                    "bob".to_string(),
                    serde_json::json!([{"key": "dedicated", "value": "only", "effect": "NoSchedule"}])
                ),
            ]
        );

        // Editing one file and removing another: the edit lands, the removal
        // clears that account's pin.
        write(
            directory.path(),
            "bob.yaml",
            "account: bob\npinned_tolerations:\n  - {key: dedicated, value: churned}\n",
        );
        fs::remove_file(directory.path().join("acme.yaml")).unwrap();
        apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap()).await.unwrap();
        assert_eq!(
            stored_pins(&pool).await,
            vec![("bob".to_string(), serde_json::json!([{"key": "dedicated", "value": "churned"}]))]
        );

        // An unmanaged (missing) directory changes nothing.
        apply(&pool, &AccountLimitsCatalog::load(directory.path().join("absent")).unwrap())
            .await
            .unwrap();
        assert_eq!(stored_pins(&pool).await.len(), 1);

        // An empty directory clears every pin.
        fs::remove_file(directory.path().join("bob.yaml")).unwrap();
        apply(&pool, &AccountLimitsCatalog::load(directory.path()).unwrap()).await.unwrap();
        assert!(stored_pins(&pool).await.is_empty());
    }

    #[test]
    fn pinned_tolerations_accessor_returns_only_declared_accounts() {
        let directory = tempdir().unwrap();
        write(directory.path(), "a.yaml", "account: acme\nrealtime_inflight:\n  org/model: 10\n");
        write(directory.path(), "b.yaml", "account: bob\npinned_tolerations: []\n");
        let declared = AccountLimitsCatalog::load(directory.path()).unwrap().pinned_tolerations().unwrap();
        assert_eq!(
            declared.into_iter().collect::<Vec<_>>(),
            vec![("bob".to_string(), serde_json::json!([]))]
        );
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
