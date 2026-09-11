//! Schema migrations: applying them, checking compatibility, and the
//! `dwctl migrate` command.
//!
//! Three SQLx migrators plus the `underway` task-queue schema make up the
//! control layer's schema:
//!
//! | target      | migrator                       | where                                  |
//! |-------------|--------------------------------|----------------------------------------|
//! | `main`      | `dwctl/migrations`             | main database, default schema          |
//! | `fusillade` | `fusillade-arsenal/migrations` | `database.fusillade` (schema or DB)    |
//! | `underway`  | the `underway` crate           | `underway` schema of the main database |
//! | `outlet`    | the `outlet-postgres` crate    | `database.outlet`, when logging is on  |
//!
//! Historically every API and daemon pod ran all four at boot. That couples
//! DDL to pod lifecycle and startup deadlines, and a migration that fails
//! part-way (see [`fusillade_arsenal::managed_index`]) stops every new pod
//! from starting. This module separates the two concerns:
//!
//! * [`apply`] runs a target's pending migrations, repairing managed indexes
//!   first and verifying them afterwards. `dwctl migrate` calls it for every
//!   target from a Kubernetes Job before the application rolls.
//! * [`check`] never executes DDL. It proves the database carries every
//!   migration this binary ships (matching checksums) and tolerates a database
//!   that is *ahead*, so old replicas keep serving while an additive migration
//!   for the next release is applied.
//!
//! Which one runs at startup is `migrations.mode` in `config.yaml`
//! (`DWCTL_MIGRATIONS__MODE`): `run` keeps the historical single-process
//! behaviour for local development; `check` is what deployments with a
//! migration Job use.
//!
//! Every migrator holds SQLx's database-scoped advisory lock while it runs, so
//! competing runners (two Jobs, two regions, a developer's laptop) serialise
//! rather than interleave. All of this must use direct connections: a
//! transaction pooler cannot carry a session advisory lock or a concurrent
//! index build.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use fusillade_arsenal::managed_index::{self, ManagedIndex, RepairAction, RepairReport};
use sqlx::migrate::{Migrate, Migrator};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgPool};
use tracing::{info, warn};

use crate::config::{ComponentDb, Config, DatabaseConfig, MigrationsMode, PoolSettings};
use crate::{connect_options, create_schema_pool, verify_same_live_database};

/// One SQLx-managed schema.
pub struct Target {
    pub name: &'static str,
    pub migrator: Migrator,
    /// Indexes whose concurrent builds are repaired before and verified after
    /// the migrator runs.
    pub managed_indexes: &'static [ManagedIndex],
}

impl Target {
    pub fn main() -> Self {
        Self {
            name: "main",
            migrator: crate::migrator(),
            managed_indexes: &[],
        }
    }

    pub fn fusillade() -> Self {
        let shared = fusillade_arsenal::migrator();
        Self {
            name: "fusillade",
            migrator: Migrator {
                migrations: Cow::Borrowed(shared.migrations.as_ref()),
                ..*shared
            },
            managed_indexes: managed_index::managed_indexes(),
        }
    }

    pub fn outlet() -> Self {
        Self {
            name: "outlet",
            migrator: outlet_postgres::migrator(),
            managed_indexes: &[],
        }
    }

    /// A migrator restricted to versions `<= version` (SQLx 0.8 has no
    /// `run_to`), with the same checksum validation. `locking` false hands the
    /// advisory lock to the caller, which must already hold it.
    ///
    /// `ignore_missing` is set because the restricted migrator does not know
    /// about later versions the database may legitimately carry: a gap being
    /// filled (an older-numbered migration merged after a newer one, or a
    /// validation migration re-run after a repair) or a database that is ahead
    /// of this binary. [`check`] is what rejects a diverged history.
    fn up_to(&self, version: i64, locking: bool) -> Migrator {
        let prefix: Vec<_> = self.migrator.iter().filter(|m| m.version <= version).cloned().collect();
        Migrator {
            migrations: Cow::Owned(prefix),
            locking,
            ignore_missing: true,
            ..self.migrator
        }
    }

    /// Apply every migration up to and including `version` on a dedicated
    /// connection that is closed afterwards.
    ///
    /// SQLx takes a session-level advisory lock and, when a migration fails,
    /// returns without releasing it. Run through a pool that connection goes
    /// back idle still holding the lock and every later migrator call blocks
    /// forever. A detached connection closed on every path cannot leak it.
    pub async fn run_to(&self, version: i64, pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
        let mut conn = pool.acquire().await?.detach();
        let result = self.up_to(version, true).run_direct(&mut conn).await;
        let _ = conn.close().await;
        result
    }

    /// The newest migration version this binary ships for the target.
    pub fn latest_version(&self) -> Option<i64> {
        self.migrator
            .iter()
            .filter(|m| !m.migration_type.is_down_migration())
            .map(|m| m.version)
            .max()
    }
}

/// Migration versions the `underway` crate applies to the `underway` schema.
///
/// `underway` keeps its migrator private, so the compatibility check compares
/// against this list. `underway_versions_match_crate` fails when the crate is
/// bumped without updating it.
pub const UNDERWAY_MIGRATION_VERSIONS: &[i64] = &[
    20240921151751,
    20241024174106,
    20241105164503,
    20241110164319,
    20241111174958,
    20241126224448,
];

/// Outcome of [`apply`] for one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub target: &'static str,
    /// Versions applied by this run, in order.
    pub applied: Vec<i64>,
    /// Versions that were already recorded before this run.
    pub already_applied: usize,
    pub repairs: Vec<RepairReport>,
}

impl ApplyReport {
    pub fn repaired(&self) -> impl Iterator<Item = &RepairReport> {
        self.repairs
            .iter()
            .filter(|r| !matches!(r.action, RepairAction::Untouched(_)) || !r.dropped_leftovers.is_empty())
    }
}

/// Outcome of [`check`] for one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub target: &'static str,
    /// Migrations this binary ships that are recorded with matching checksums.
    pub applied: usize,
    /// Versions the database has that this binary does not know about.
    pub ahead: Vec<i64>,
}

#[derive(Debug, sqlx::FromRow)]
struct AppliedRow {
    version: i64,
    checksum: Vec<u8>,
    success: bool,
}

/// Recorded migrations for the schema on the connection's search path.
/// `None` when the migrations table does not exist.
async fn recorded(conn: &mut sqlx::PgConnection) -> anyhow::Result<Option<HashMap<i64, AppliedRow>>> {
    let table: Option<String> = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations')::text")
        .fetch_one(&mut *conn)
        .await?;
    if table.is_none() {
        return Ok(None);
    }
    let rows: Vec<AppliedRow> = sqlx::query_as("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
        .fetch_all(conn)
        .await?;
    Ok(Some(rows.into_iter().map(|r| (r.version, r)).collect()))
}

fn version_set(rows: &HashMap<i64, AppliedRow>) -> HashSet<i64> {
    rows.values().filter(|r| r.success).map(|r| r.version).collect()
}

/// Apply every pending migration of `target`, repairing managed indexes first
/// and verifying them afterwards. `pool` must hand out direct connections.
///
/// The whole run — bookkeeping table, repair, migrations, verification — is
/// serialised behind SQLx's database-scoped advisory lock, held on one
/// dedicated connection that is closed on every path (see [`Target::run_to`]
/// for why a pooled connection must not outlive a failure with that lock).
/// Competing runners therefore wait for each other rather than race on
/// `CREATE TABLE IF NOT EXISTS` or on a concurrent index build.
pub async fn apply(target: &Target, pool: &PgPool) -> anyhow::Result<ApplyReport> {
    let name = target.name;
    let mut conn = pool
        .acquire()
        .await
        .with_context(|| format!("{name}: acquiring a direct connection"))?
        .detach();
    let result = apply_locked(target, pool, &mut conn).await;
    let _ = conn.close().await;
    result
}

/// SQLx's advisory lock id for a database (`sqlx-postgres/src/migrate.rs`),
/// so a runner here is mutually exclusive with any SQLx migrator on the same
/// database: an older release's pod still migrating at startup, a developer's
/// `sqlx migrate run`, a Job in another region.
fn sqlx_lock_id(database_name: &str) -> i64 {
    const CRC_IEEE: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    0x3d32ad9e * (CRC_IEEE.checksum(database_name.as_bytes()) as i64)
}

/// Take the migration lock by polling `pg_try_advisory_lock`.
///
/// Not `pg_advisory_lock`: a session blocked in that call sits inside a
/// statement, holding a snapshot, and `CREATE INDEX CONCURRENTLY` in the lock
/// holder must wait for every snapshot older than its own. Postgres sees the
/// cycle (holder waits for waiter's snapshot, waiter waits for holder's lock),
/// aborts the index build with "deadlock detected", and leaves an INVALID
/// index behind — `competing_runners_serialise` reproduces it with the SQLx
/// migrator's own blocking lock. Polling waiters hold nothing between polls.
async fn acquire_migration_lock(conn: &mut sqlx::PgConnection, name: &str) -> anyhow::Result<i64> {
    let database: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&mut *conn).await?;
    let id = sqlx_lock_id(&database);
    let started = Instant::now();
    let mut last_report = started;
    loop {
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(id)
            .fetch_one(&mut *conn)
            .await
            .with_context(|| format!("{name}: acquiring the migration lock"))?;
        if acquired {
            if started.elapsed() > Duration::from_secs(1) {
                info!(target = name, waited_s = started.elapsed().as_secs(), "migration lock acquired");
            }
            return Ok(id);
        }
        if last_report.elapsed() >= Duration::from_secs(10) {
            info!(
                target = name,
                waited_s = started.elapsed().as_secs(),
                "another migration runner holds the lock; waiting"
            );
            last_report = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn apply_locked(target: &Target, pool: &PgPool, conn: &mut sqlx::PgConnection) -> anyhow::Result<ApplyReport> {
    let name = target.name;
    let lock_id = acquire_migration_lock(conn, name).await?;
    conn.ensure_migrations_table()
        .await
        .with_context(|| format!("{name}: creating the migrations table"))?;
    if let Some(version) = conn.dirty_version().await? {
        bail!(
            "{name}: migration {version} is recorded as failed (success = false); \
             inspect it and remove the row before retrying"
        );
    }
    let before = recorded(conn).await?.unwrap_or_default();
    let already = version_set(&before);

    // Checksum drift is the one thing repair must not paper over: report it
    // before touching anything.
    let mismatched: Vec<i64> = target
        .migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .filter(|m| before.get(&m.version).is_some_and(|row| row.checksum != *m.checksum))
        .map(|m| m.version)
        .collect();
    if !mismatched.is_empty() {
        bail!(
            "{name}: released migration file(s) changed after they were applied: {mismatched:?}. \
             Migration files are immutable once released; restore them and ship a new migration instead"
        );
    }

    let repairs = managed_index::repair_all(pool, target.managed_indexes, &already)
        .await
        .with_context(|| format!("{name}: repairing managed indexes"))?;
    for report in &repairs {
        if !report.dropped_leftovers.is_empty() {
            warn!(target = name, index = report.index, dropped = ?report.dropped_leftovers, "dropped invalid rebuild leftovers");
        }
        if !matches!(report.action, RepairAction::Untouched(_)) {
            info!(target = name, index = report.index, action = ?report.action, "managed index repaired");
        }
    }

    let pending: Vec<_> = target
        .migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration() && !already.contains(&m.version))
        .collect();
    if pending.is_empty() {
        info!(target = name, recorded = already.len(), "no pending migrations");
    } else {
        info!(
            target = name,
            pending = pending.len(),
            recorded = already.len(),
            "applying migrations"
        );
    }
    let mut applied = Vec::with_capacity(pending.len());
    for migration in pending {
        let version = migration.version;
        let description = migration.description.as_ref();
        info!(
            target = name,
            version,
            description,
            no_transaction = migration.no_tx,
            "applying migration"
        );
        let started = Instant::now();
        target
            .up_to(version, false)
            .run_direct(conn)
            .await
            .with_context(|| format!("{name}: migration {version} ({description}) failed"))?;
        info!(
            target = name,
            version,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "applied migration"
        );
        applied.push(version);
    }

    let after = recorded(conn).await?.unwrap_or_default();
    managed_index::verify_all(pool, target.managed_indexes, &version_set(&after))
        .await
        .with_context(|| format!("{name}: verifying managed indexes after migration"))?;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(lock_id)
        .execute(&mut *conn)
        .await?;
    Ok(ApplyReport {
        target: name,
        applied,
        already_applied: already.len(),
        repairs,
    })
}

/// Verify, without DDL, that the database carries every migration this binary
/// ships. A database that is ahead passes; one that is behind, diverged, or
/// has different checksums fails with the offending versions.
pub async fn check(target: &Target, pool: &PgPool) -> anyhow::Result<CheckReport> {
    let name = target.name;
    let mut conn = pool.acquire().await.with_context(|| format!("{name}: acquiring a connection"))?;
    let Some(rows) = recorded(&mut conn).await? else {
        bail!("{name}: the database has no migrations table; run `dwctl migrate` before starting the application");
    };
    if let Some(row) = rows.values().find(|r| !r.success) {
        bail!(
            "{name}: migration {} is recorded as failed; run `dwctl migrate` to repair",
            row.version
        );
    }
    let known: BTreeSet<i64> = target
        .migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .map(|m| m.version)
        .collect();
    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    for migration in target.migrator.iter().filter(|m| !m.migration_type.is_down_migration()) {
        match rows.get(&migration.version) {
            None => missing.push(migration.version),
            Some(row) if row.checksum != *migration.checksum => mismatched.push(migration.version),
            Some(_) => {}
        }
    }
    let latest_known = known.iter().next_back().copied().unwrap_or(0);
    let (ahead, diverged): (Vec<i64>, Vec<i64>) = rows.keys().filter(|v| !known.contains(v)).copied().partition(|v| *v > latest_known);
    if !missing.is_empty() || !mismatched.is_empty() || !diverged.is_empty() {
        let mut reasons = Vec::new();
        if !missing.is_empty() {
            reasons.push(format!("{} migration(s) not applied: {missing:?}", missing.len()));
        }
        if !mismatched.is_empty() {
            reasons.push(format!("checksum mismatch for {mismatched:?}"));
        }
        if !diverged.is_empty() {
            reasons.push(format!(
                "database records version(s) {diverged:?} that this binary does not ship and that are older than its newest ({latest_known}); the migration histories diverged"
            ));
        }
        bail!(
            "{name}: schema is not compatible with this release ({}). Run `dwctl migrate` with this release's image, or set migrations.mode to `run` for a single-instance install",
            reasons.join("; ")
        );
    }
    let mut ahead = ahead;
    ahead.sort_unstable();
    if ahead.is_empty() {
        info!(target = name, applied = known.len(), "schema compatible");
    } else {
        info!(target = name, applied = known.len(), ahead = ?ahead, "schema compatible; database is ahead of this binary");
    }
    Ok(CheckReport {
        target: name,
        applied: known.len(),
        ahead,
    })
}

/// Apply or check according to `mode`.
pub async fn at_startup(mode: MigrationsMode, target: &Target, pool: &PgPool) -> anyhow::Result<()> {
    match mode {
        MigrationsMode::Run => apply(target, pool).await.map(|_| ()),
        MigrationsMode::Check => check(target, pool).await.map(|_| ()),
    }
}

/// Apply the `underway` task-queue migrations.
pub async fn apply_underway(pool: &PgPool) -> anyhow::Result<()> {
    underway::run_migrations(pool).await.context("underway: applying migrations")?;
    info!(target = "underway", "migrations applied");
    Ok(())
}

/// Verify the `underway` schema carries every migration this binary's
/// `underway` crate expects, without DDL.
pub async fn check_underway(pool: &PgPool) -> anyhow::Result<()> {
    let table: Option<String> = sqlx::query_scalar("SELECT to_regclass('underway._sqlx_migrations')::text")
        .fetch_one(pool)
        .await?;
    if table.is_none() {
        bail!("underway: the task-queue schema has not been migrated; run `dwctl migrate` before starting the application");
    }
    let rows: Vec<(i64, bool)> = sqlx::query_as("SELECT version, success FROM underway._sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await?;
    let present: HashSet<i64> = rows.iter().filter(|(_, ok)| *ok).map(|(v, _)| *v).collect();
    let missing: Vec<i64> = UNDERWAY_MIGRATION_VERSIONS
        .iter()
        .copied()
        .filter(|v| !present.contains(v))
        .collect();
    if !missing.is_empty() {
        bail!(
            "underway: schema is not compatible with this release ({} migration(s) not applied: {missing:?}); run `dwctl migrate`",
            missing.len()
        );
    }
    info!(
        target = "underway",
        applied = UNDERWAY_MIGRATION_VERSIONS.len(),
        "schema compatible"
    );
    Ok(())
}

pub async fn underway_at_startup(mode: MigrationsMode, pool: &PgPool) -> anyhow::Result<()> {
    match mode {
        MigrationsMode::Run => apply_underway(pool).await,
        MigrationsMode::Check => check_underway(pool).await,
    }
}

/// Pool settings for the migrate command: two direct connections per target
/// (one for the migrator's lock and statements, one spare for inspection).
fn command_pool_settings() -> PoolSettings {
    PoolSettings {
        max_connections: 2,
        min_connections: 0,
        acquire_timeout_secs: 60,
        idle_timeout_secs: 60,
        max_lifetime_secs: 0,
    }
}

/// `host:port/database` for logs; never the URL, which carries the password.
fn describe(options: &PgConnectOptions) -> String {
    format!(
        "{}:{}/{}",
        options.get_host(),
        options.get_port(),
        options.get_database().unwrap_or("<default>")
    )
}

/// A direct pool for a component database, mirroring the connection rules of
/// the serving process but without any pooled endpoint: schema mode reuses the
/// main direct credentials (or its own `url`) with a pinned `search_path`,
/// dedicated mode connects to its own `url`.
async fn connect_component(name: &str, component: &ComponentDb, main: &PgPool, slow: Duration) -> anyhow::Result<PgPool> {
    match component {
        ComponentDb::Schema { name: schema, url, .. } => {
            let options = match url {
                Some(url) => connect_options(url, slow)?,
                None => main.connect_options().as_ref().clone(),
            };
            info!(component = name, schema, endpoint = %describe(&options), "connecting");
            let pool = create_schema_pool(schema, options, &command_pool_settings()).await?;
            if url.is_some() {
                verify_same_live_database(name, main, &pool).await?;
            }
            let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)")
                .bind(schema)
                .fetch_one(&pool)
                .await?;
            if !exists {
                info!(component = name, schema, "creating schema");
                sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", schema.replace('"', "\"\"")))
                    .execute(&pool)
                    .await?;
            }
            Ok(pool)
        }
        ComponentDb::Dedicated { url, .. } => {
            let options = connect_options(url, slow)?;
            info!(component = name, endpoint = %describe(&options), "connecting to dedicated database");
            Ok(crate::db::pool_options(&command_pool_settings()).connect_with(options).await?)
        }
    }
}

/// Summary of a `dwctl migrate` run.
#[derive(Debug, Default)]
pub struct CommandSummary {
    pub applied: Vec<ApplyReport>,
    pub checked: Vec<CheckReport>,
}

/// The `dwctl migrate` command: migrate (or, with `check_only`, verify) every
/// configured database in dependency order. Exits non-zero on the first
/// failure; a partial run is safe to repeat.
pub async fn run_command(config: &Config, check_only: bool) -> anyhow::Result<CommandSummary> {
    let slow = Duration::from_millis(config.slow_statement_threshold_ms);
    let url = match &config.database {
        DatabaseConfig::External { url, .. } => url,
        DatabaseConfig::Embedded { .. } => bail!("`dwctl migrate` requires database.type: external"),
    };
    let main_options = PgConnectOptions::from_str(url).context("database.url")?;
    info!(endpoint = %describe(&main_options), check_only, "dwctl migrate: main database");
    let main = crate::db::pool_options(&command_pool_settings())
        .connect_with(connect_options(url, slow)?)
        .await
        .context("connecting to the main database (direct endpoint)")?;

    let mut summary = CommandSummary::default();
    let mut targets: Vec<(Target, PgPool)> = vec![(Target::main(), main.clone())];
    let fusillade = connect_component("fusillade", config.database.fusillade(), &main, slow).await?;
    targets.push((Target::fusillade(), fusillade));
    let outlet = if config.enable_request_logging {
        Some(connect_component("outlet", config.database.outlet(), &main, slow).await?)
    } else {
        info!("request logging disabled; skipping outlet");
        None
    };

    for (target, pool) in &targets {
        if check_only {
            summary.checked.push(check(target, pool).await?);
        } else {
            summary.applied.push(apply(target, pool).await?);
        }
    }
    if check_only {
        check_underway(&main).await?;
    } else {
        apply_underway(&main).await?;
    }
    if let Some(outlet) = &outlet {
        let target = Target::outlet();
        if check_only {
            summary.checked.push(check(&target, outlet).await?);
        } else {
            summary.applied.push(apply(&target, outlet).await?);
        }
    }

    for report in &summary.applied {
        let repaired: Vec<_> = report.repaired().map(|r| (r.index, r.action.clone())).collect();
        info!(
            target = report.target,
            applied = report.applied.len(),
            already_applied = report.already_applied,
            repaired = ?repaired,
            "migration target complete"
        );
    }
    info!(check_only, "dwctl migrate: all targets complete");
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MigrationsMode;
    use crate::test::utils::setup_fusillade_pool;
    use sqlx::PgPool;

    async fn fusillade_pool_without_migrations(pool: &PgPool) -> PgPool {
        sqlx::query("CREATE SCHEMA IF NOT EXISTS fusillade").execute(pool).await.unwrap();
        let opts = pool.connect_options().as_ref().clone().options([("search_path", "fusillade")]);
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = false)]
    async fn fresh_database_is_initialised(pool: PgPool) {
        let main = apply(&Target::main(), &pool).await.unwrap();
        assert_eq!(main.already_applied, 0);
        assert_eq!(
            main.applied.len(),
            Target::main()
                .migrator
                .iter()
                .filter(|m| !m.migration_type.is_down_migration())
                .count()
        );
        let fusillade_pool = fusillade_pool_without_migrations(&pool).await;
        let fusillade = apply(&Target::fusillade(), &fusillade_pool).await.unwrap();
        assert!(fusillade.applied.len() > 80);
        assert!(fusillade.repaired().next().is_none(), "a fresh database needs no repair");
        apply_underway(&pool).await.unwrap();
        check(&Target::main(), &pool).await.unwrap();
        check(&Target::fusillade(), &fusillade_pool).await.unwrap();
        check_underway(&pool).await.unwrap();
    }

    #[sqlx::test(migrations = false)]
    async fn already_migrated_database_is_a_noop(pool: PgPool) {
        apply(&Target::main(), &pool).await.unwrap();
        let fusillade_pool = fusillade_pool_without_migrations(&pool).await;
        apply(&Target::fusillade(), &fusillade_pool).await.unwrap();
        let second = apply(&Target::main(), &pool).await.unwrap();
        assert!(second.applied.is_empty());
        let second = apply(&Target::fusillade(), &fusillade_pool).await.unwrap();
        assert!(second.applied.is_empty());
        assert!(second.repaired().next().is_none());
    }

    #[sqlx::test(migrations = false)]
    async fn check_rejects_an_unmigrated_database(pool: PgPool) {
        let err = check(&Target::main(), &pool).await.unwrap_err().to_string();
        assert!(err.contains("no migrations table"), "{err}");
        assert!(err.contains("dwctl migrate"), "{err}");
        assert!(check_underway(&pool).await.is_err());
    }

    #[sqlx::test(migrations = false)]
    async fn check_rejects_a_database_that_is_behind(pool: PgPool) {
        let target = Target::main();
        let latest = target.latest_version().unwrap();
        // Everything except the newest migration, as a pod of this release
        // would find a database that the previous release migrated.
        let previous = target
            .migrator
            .iter()
            .filter(|m| !m.migration_type.is_down_migration() && m.version < latest)
            .map(|m| m.version)
            .max()
            .unwrap();
        target.run_to(previous, &pool).await.unwrap();
        let err = check(&target, &pool).await.unwrap_err().to_string();
        assert!(err.contains("not compatible"), "{err}");
        assert!(err.contains(&latest.to_string()), "{err}");
        // Applying the rest makes it compatible.
        apply(&target, &pool).await.unwrap();
        check(&target, &pool).await.unwrap();
    }

    #[sqlx::test(migrations = false)]
    async fn check_accepts_a_database_that_is_ahead(pool: PgPool) {
        let target = Target::main();
        apply(&target, &pool).await.unwrap();
        let future = target.latest_version().unwrap() + 1;
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($1, 'from the next release', true, '\\x00', 1)")
            .bind(future)
            .execute(&pool)
            .await
            .unwrap();
        let report = check(&target, &pool).await.unwrap();
        assert_eq!(report.ahead, vec![future]);
    }

    #[sqlx::test(migrations = false)]
    async fn check_rejects_diverged_history_and_checksum_drift(pool: PgPool) {
        let target = Target::main();
        apply(&target, &pool).await.unwrap();
        // Older than every migration this binary ships: not "ahead", diverged.
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (0, 'hotfix from another branch', true, '\\x00', 1)")
            .execute(&pool)
            .await
            .unwrap();
        let err = check(&target, &pool).await.unwrap_err().to_string();
        assert!(err.contains("diverged"), "{err}");
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 0")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 1")
            .execute(&pool)
            .await
            .unwrap();
        let err = check(&target, &pool).await.unwrap_err().to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        let err = apply(&target, &pool).await.unwrap_err().to_string();
        assert!(err.contains("changed after they were applied"), "{err}");
    }

    #[sqlx::test(migrations = false)]
    async fn apply_refuses_a_dirty_migration_row(pool: PgPool) {
        let target = Target::main();
        apply(&target, &pool).await.unwrap();
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (999999, 'crashed', false, '\\x00', 1)")
            .execute(&pool)
            .await
            .unwrap();
        let err = apply(&target, &pool).await.unwrap_err().to_string();
        assert!(err.contains("999999"), "{err}");
        assert!(check(&target, &pool).await.is_err());
    }

    /// The 11.9.1 incident: the creating migration is recorded, the index is
    /// invalid, the validation migration has not run. `apply` must repair the
    /// index and then get the validation migration through.
    #[sqlx::test(migrations = false)]
    async fn invalid_index_with_recorded_creation_migration_is_recovered(pool: PgPool) {
        let fusillade_pool = fusillade_pool_without_migrations(&pool).await;
        let target = Target::fusillade();
        target.run_to(20260910010000, &fusillade_pool).await.unwrap();
        sqlx::query("UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'idx_batches_owner_created_at_id'::regclass")
            .execute(&fusillade_pool)
            .await
            .unwrap();
        // Without repair the next migration fails exactly as it did in production.
        let raw = target.run_to(20260910010001, &fusillade_pool).await.unwrap_err().to_string();
        assert!(raw.contains("missing, invalid, or has the wrong definition"), "{raw}");

        let report = apply(&target, &fusillade_pool).await.unwrap();
        let repaired: Vec<_> = report.repaired().collect();
        assert_eq!(repaired.len(), 1);
        assert_eq!(repaired[0].index, "idx_batches_owner_created_at_id");
        assert_eq!(repaired[0].action, RepairAction::Reindexed);
        assert!(report.applied.contains(&20260910010001));
        let valid: bool =
            sqlx::query_scalar("SELECT indisvalid FROM pg_index WHERE indexrelid = 'idx_batches_owner_created_at_id'::regclass")
                .fetch_one(&fusillade_pool)
                .await
                .unwrap();
        assert!(valid);
        check(&target, &fusillade_pool).await.unwrap();
    }

    /// An interrupted build *before* the migration was recorded: invalid
    /// index, no row. The retry must not let `IF NOT EXISTS` record a broken
    /// index as success.
    #[sqlx::test(migrations = false)]
    async fn interrupted_concurrent_build_is_recovered_on_retry(pool: PgPool) {
        let fusillade_pool = fusillade_pool_without_migrations(&pool).await;
        let target = Target::fusillade();
        target.run_to(20260910000000, &fusillade_pool).await.unwrap();
        // What an interrupted CREATE INDEX CONCURRENTLY leaves behind.
        sqlx::query(
            "CREATE INDEX idx_batches_owner_created_at_id ON batches (created_by, created_at DESC, id DESC) WHERE deleted_at IS NULL",
        )
        .execute(&fusillade_pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE pg_index SET indisvalid = false, indisready = false WHERE indexrelid = 'idx_batches_owner_created_at_id'::regclass",
        )
        .execute(&fusillade_pool)
        .await
        .unwrap();
        let report = apply(&target, &fusillade_pool).await.unwrap();
        assert!(report.applied.contains(&20260910010000));
        assert!(report.applied.contains(&20260910010001));
        assert_eq!(report.repaired().next().unwrap().action, RepairAction::Reindexed);
        check(&target, &fusillade_pool).await.unwrap();
    }

    /// A gap in the recorded history (the 11.9.1 repair deleted the failed
    /// validation row while later migrations were already applied) must be
    /// filled, not rejected as "previously applied but missing".
    #[sqlx::test(migrations = false)]
    async fn apply_fills_a_gap_behind_later_migrations(pool: PgPool) {
        let fusillade_pool = fusillade_pool_without_migrations(&pool).await;
        let target = Target::fusillade();
        apply(&target, &fusillade_pool).await.unwrap();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 20260910010001")
            .execute(&fusillade_pool)
            .await
            .unwrap();
        let report = apply(&target, &fusillade_pool).await.unwrap();
        assert_eq!(report.applied, vec![20260910010001]);
        check(&target, &fusillade_pool).await.unwrap();
    }

    #[sqlx::test(migrations = false)]
    async fn wrong_definition_fails_clearly_and_leaves_the_index(pool: PgPool) {
        let fusillade_pool = fusillade_pool_without_migrations(&pool).await;
        let target = Target::fusillade();
        target.run_to(20260910000000, &fusillade_pool).await.unwrap();
        sqlx::query("CREATE INDEX idx_batches_owner_created_at_id ON batches (created_by, created_at, id)")
            .execute(&fusillade_pool)
            .await
            .unwrap();
        let err = apply(&target, &fusillade_pool).await.unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("wrong definition"), "{message}");
        assert!(message.contains("was not modified"), "{message}");
        let def: String = sqlx::query_scalar("SELECT pg_get_indexdef('idx_batches_owner_created_at_id'::regclass)")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert!(def.contains("(created_by, created_at, id)"), "index must be untouched: {def}");
        // Nothing past the repair point was recorded.
        let recorded: Option<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE version = 20260910010000")
            .fetch_optional(&fusillade_pool)
            .await
            .unwrap();
        assert!(recorded.is_none());
    }

    /// Upgrade from the previous supported release (11.9.1): its newest
    /// fusillade migration was 20260910010002, its newest main migration 138.
    #[sqlx::test(migrations = false)]
    async fn upgrade_from_previous_release(pool: PgPool) {
        const PREVIOUS_MAIN: i64 = 138;
        const PREVIOUS_FUSILLADE: i64 = 20260910010002;
        let main = Target::main();
        main.run_to(PREVIOUS_MAIN, &pool).await.unwrap();
        let fusillade_pool = fusillade_pool_without_migrations(&pool).await;
        let fusillade = Target::fusillade();
        fusillade.run_to(PREVIOUS_FUSILLADE, &fusillade_pool).await.unwrap();
        apply_underway(&pool).await.unwrap();
        assert!(check(&main, &pool).await.is_err(), "the previous release's schema is behind");

        let report = apply(&main, &pool).await.unwrap();
        assert!(report.applied.iter().all(|v| *v > PREVIOUS_MAIN));
        let report = apply(&fusillade, &fusillade_pool).await.unwrap();
        assert!(report.applied.iter().all(|v| *v > PREVIOUS_FUSILLADE));
        check(&main, &pool).await.unwrap();
        check(&fusillade, &fusillade_pool).await.unwrap();
    }

    #[sqlx::test(migrations = false)]
    async fn competing_runners_serialise(pool: PgPool) {
        let fusillade_pool = fusillade_pool_without_migrations(&pool).await;
        // Three runners on separate connections; each takes SQLx's advisory
        // lock in turn. (Polled on one task: the migrator future is not
        // spawnable, but contention is on the database, not the executor.)
        let runner = |pool: PgPool, fusillade_pool: PgPool| async move {
            apply(&Target::main(), &pool).await?;
            apply(&Target::fusillade(), &fusillade_pool).await?;
            apply_underway(&pool).await
        };
        let (a, b, c) = tokio::join!(
            runner(pool.clone(), fusillade_pool.clone()),
            runner(pool.clone(), fusillade_pool.clone()),
            runner(pool.clone(), fusillade_pool.clone()),
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        let main_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            main_rows as usize,
            Target::main()
                .migrator
                .iter()
                .filter(|m| !m.migration_type.is_down_migration())
                .count()
        );
        check(&Target::main(), &pool).await.unwrap();
        check(&Target::fusillade(), &fusillade_pool).await.unwrap();
    }

    /// The polling lock and SQLx's blocking lock exclude each other: an old
    /// release's pod running startup migrations waits for the Job and vice
    /// versa.
    #[sqlx::test(migrations = false)]
    async fn migration_lock_excludes_the_sqlx_migrator(pool: PgPool) {
        let mut holder = pool.acquire().await.unwrap().detach();
        let id = acquire_migration_lock(&mut holder, "test").await.unwrap();
        let mut other = pool.acquire().await.unwrap().detach();
        let blocked = tokio::time::timeout(Duration::from_millis(500), Target::main().up_to(1, true).run_direct(&mut other)).await;
        assert!(blocked.is_err(), "sqlx's migrator must block while the polling lock is held");
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(id)
            .execute(&mut holder)
            .await
            .unwrap();
        holder.close().await.unwrap();
        let _ = other.close().await;
        // Fresh connection: the migrator proceeds once the lock is free.
        Target::main().run_to(1, &pool).await.unwrap();
    }

    #[sqlx::test(migrations = false)]
    async fn underway_versions_match_crate(pool: PgPool) {
        apply_underway(&pool).await.unwrap();
        let versions: Vec<i64> = sqlx::query_scalar("SELECT version FROM underway._sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            versions, UNDERWAY_MIGRATION_VERSIONS,
            "update UNDERWAY_MIGRATION_VERSIONS after bumping the underway crate"
        );
        check_underway(&pool).await.unwrap();
    }

    #[sqlx::test(migrations = false)]
    async fn startup_policy_dispatches(pool: PgPool) {
        assert!(at_startup(MigrationsMode::Check, &Target::main(), &pool).await.is_err());
        at_startup(MigrationsMode::Run, &Target::main(), &pool).await.unwrap();
        at_startup(MigrationsMode::Check, &Target::main(), &pool).await.unwrap();
        assert!(underway_at_startup(MigrationsMode::Check, &pool).await.is_err());
        underway_at_startup(MigrationsMode::Run, &pool).await.unwrap();
        underway_at_startup(MigrationsMode::Check, &pool).await.unwrap();
    }

    #[sqlx::test]
    async fn fusillade_test_helper_stays_compatible(pool: PgPool) {
        let fusillade_pool = setup_fusillade_pool(&pool).await;
        check(&Target::fusillade(), &fusillade_pool).await.unwrap();
    }

    #[test]
    fn describe_never_includes_credentials() {
        let options = PgConnectOptions::from_str("postgres://user:hunter2@db.example:5433/clay").unwrap();
        let text = describe(&options);
        assert_eq!(text, "db.example:5433/clay");
    }
}
