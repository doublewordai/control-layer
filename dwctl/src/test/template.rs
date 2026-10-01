//! Reuse migrated schemas, while keeping an isolated database for every test.
//!
//! Templates are immutable and keyed by migration checksums and Cargo.lock
//! (Underway's migrator is private). A session lock coordinates builders across
//! nextest processes. Only a fully migrated, disconnected database is sealed.

use std::future::Future;
use std::panic::{AssertUnwindSafe, resume_unwind};
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::FutureExt;
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::testing::TestTermination;
use sqlx::{Connection, Executor, PgConnection, PgPool};
use uuid::Uuid;

use crate::migrations::{Target, apply_underway};

fn template_name() -> String {
    let mut hash = Sha256::new();
    // Change this version when the template construction procedure changes.
    hash.update(b"dwctl-test-template-v1");
    hash.update(include_str!("../../../Cargo.lock"));
    for target in [Target::main(), Target::fusillade(), Target::outlet(), Target::underway_extensions()] {
        hash.update(target.name);
        for migration in target.migrator.iter() {
            hash.update(migration.version.to_le_bytes());
            hash.update(&migration.checksum);
        }
    }
    format!("dwctl_template_{:.32}", hex::encode(hash.finalize()))
}

async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    crate::migrator().run(pool).await?;
    for target in [Target::fusillade(), Target::outlet()] {
        pool.execute(format!("CREATE SCHEMA {}", target.name).as_str()).await?;
        let component = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(pool.connect_options().as_ref().clone().options([("search_path", target.name)]))
            .await?;
        let result = target.migrator.run(&component).await;
        component.close().await;
        result?;
    }
    apply_underway(pool).await?;
    Ok(())
}

async fn create_database() -> anyhow::Result<(PgPool, PgConnection, String)> {
    // SQLx uses dotenvy too; loading through the environment avoids placing any
    // connection details in generated code or the template's cache key.
    let options: PgConnectOptions = dotenvy::var("DATABASE_URL")?.parse()?;
    let mut admin = PgConnection::connect_with(&options).await?;
    // Test flags deliberately avoid DWCTL_, which the app's config loader reads.
    // This path compares fresh migrations against cloning the same schemas.
    if std::env::var("DW_TEST_FRESH_DATABASES").as_deref() == Ok("1") {
        let name = format!("dwctl_test_{}", Uuid::new_v4().simple());
        admin.execute(format!("CREATE DATABASE {name} TEMPLATE template0").as_str()).await?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options.database(&name))
            .await
            .with_context(|| format!("failed to connect to retained test database {name}"))?;
        if let Err(error) = migrate(&pool).await {
            pool.close().await;
            return Err(error.context(format!("failed to migrate retained test database {name}")));
        }
        return Ok((pool, admin, name));
    }
    let template = template_name();
    sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&template)
        .execute(&mut admin)
        .await?;
    let allows_connections: Option<bool> = sqlx::query_scalar("SELECT datallowconn FROM pg_database WHERE datname = $1")
        .bind(&template)
        .fetch_optional(&mut admin)
        .await?;
    if allows_connections != Some(false) {
        // A prior builder may have failed or been interrupted before sealing.
        admin
            .execute(format!("DROP DATABASE IF EXISTS {template} WITH (FORCE)").as_str())
            .await?;
        admin
            .execute(format!("CREATE DATABASE {template} TEMPLATE template0").as_str())
            .await?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone().database(&template))
            .await
            .with_context(|| format!("failed to connect to unsealed template {template}"))?;
        let result = migrate(&pool).await;
        pool.close().await;
        result.with_context(|| format!("failed to migrate unsealed template {template}"))?;
        admin
            .execute(format!("ALTER DATABASE {template} ALLOW_CONNECTIONS false").as_str())
            .await?;
    }
    sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(&template)
        .execute(&mut admin)
        .await?;

    // Random names allow two test runs/worktrees to use the same server safely.
    let name = format!("dwctl_test_{}", Uuid::new_v4().simple());
    admin
        .execute(format!("CREATE DATABASE {name} TEMPLATE {template} STRATEGY FILE_COPY").as_str())
        .await?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .idle_timeout(Duration::from_secs(1))
        .connect_with(options.database(&name))
        .await
        .with_context(|| format!("failed to connect to retained test database {name}"))?;
    Ok((pool, admin, name))
}

type TestFuture<T> = Pin<Box<dyn Future<Output = T>>>;

// Erase the future type so the migration/cleanup harness is compiled once,
// rather than monomorphized into each of the thousand-plus database tests.
pub async fn run<T: TestTermination>(path: &str, fixtures: &[(&str, &str)], test: fn(PgPool) -> TestFuture<T>) -> T {
    let start = Instant::now();
    let (pool, mut admin, name) = create_database().await.expect("failed to clone test database");
    let mut body_start = None;
    // Fixture failures need the same bounded close and retention diagnostics as
    // test-body failures. Include the database in the panic for filtered output.
    let result = AssertUnwindSafe(async {
        for (path, sql) in fixtures {
            pool.execute(*sql)
                .await
                .unwrap_or_else(|error| panic!("fixture {path} failed in retained database {name}: {error}"));
        }
        body_start = Some(Instant::now());
        test(pool.clone()).await
    })
    .catch_unwind()
    .await;
    let end = Instant::now();
    let setup = body_start.unwrap_or(end) - start;
    let body = body_start.map_or(Duration::ZERO, |start| end - start);
    // Match SQLx's bounded close and retain databases for failed assertions.
    if tokio::time::timeout(Duration::from_secs(10), pool.close()).await.is_err() {
        eprintln!("test {path} held onto its pool after exiting");
    }
    if result.as_ref().is_ok_and(TestTermination::is_success) {
        admin
            .execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
            .await
            .expect("failed to drop test database");
    } else {
        eprintln!("test {path} retained database {name}");
    }
    if std::env::var_os("DW_TEST_TIMINGS").is_some() {
        eprintln!("test-timing {path} setup={setup:?} body={body:?} total={:?}", start.elapsed());
    }
    match result {
        Ok(result) => result,
        Err(panic) => resume_unwind(panic),
    }
}

#[tokio::test]
async fn failed_fixture_reports_retained_database_and_closes_pool() {
    let failure = AssertUnwindSafe(run::<()>("failed_fixture_regression", &[("invalid.sql", "SELECT FROM")], |_| {
        Box::pin(async { panic!("test body must not run after a fixture failure") })
    }))
    .catch_unwind()
    .await
    .expect_err("invalid fixture must fail");
    let message = failure.downcast_ref::<String>().expect("fixture panic should describe the failure");
    assert!(message.contains("invalid.sql"), "{message}");
    let name = message
        .split_whitespace()
        .find(|part| part.starts_with("dwctl_test_"))
        .expect("fixture panic must identify the retained database")
        .trim_end_matches(':');
    let options: PgConnectOptions = dotenvy::var("DATABASE_URL").unwrap().parse().unwrap();
    let mut admin = PgConnection::connect_with(&options).await.unwrap();
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
        .bind(name)
        .fetch_one(&mut admin)
        .await
        .unwrap();
    assert!(exists, "failed fixture database should remain available for inspection");
    // A successful non-FORCE drop also verifies setup closed its connections.
    admin.execute(format!("DROP DATABASE {name}").as_str()).await.unwrap();
}

#[tokio::test]
async fn clones_are_isolated_and_all_migrators_are_already_applied() {
    let (first, second) = tokio::join!(create_database(), create_database());
    let (first, mut admin, first_name) = first.unwrap();
    let (second, _, second_name) = second.unwrap();
    let changed = first
        .execute("UPDATE users SET display_name = 'template isolation sentinel'")
        .await
        .unwrap();
    assert!(changed.rows_affected() > 0);
    let leaked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE display_name = 'template isolation sentinel')")
        .fetch_one(&second)
        .await
        .unwrap();
    assert!(!leaked);
    for (schema, target) in [
        ("public", Target::main()),
        ("fusillade", Target::fusillade()),
        ("outlet", Target::outlet()),
        ("underway_extensions", Target::underway_extensions()),
    ] {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(second.connect_options().as_ref().clone().options([("search_path", schema)]))
            .await
            .unwrap();
        crate::migrations::check(&target, &pool).await.unwrap();
        pool.close().await;
    }
    crate::migrations::check_underway(&second).await.unwrap();
    first.close().await;
    second.close().await;
    admin.execute(format!("DROP DATABASE {first_name}").as_str()).await.unwrap();
    admin.execute(format!("DROP DATABASE {second_name}").as_str()).await.unwrap();
}
