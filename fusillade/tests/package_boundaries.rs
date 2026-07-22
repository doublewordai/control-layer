#[test]
fn arsenal_depends_on_core_not_root_fusillade() {
    let manifest = include_str!("../../fusillade-arsenal/Cargo.toml");

    assert!(
        manifest.contains("fusillade-core"),
        "fusillade-arsenal should depend on fusillade-core for shared types"
    );
    assert!(
        !manifest
            .lines()
            .any(|line| line.trim_start().starts_with("fusillade =")),
        "fusillade-arsenal must not depend on the root fusillade daemon crate"
    );
}

#[test]
fn workspace_contains_the_three_release_packages() {
    let manifest = include_str!("../../Cargo.toml");

    for package in ["fusillade", "fusillade-core", "fusillade-arsenal"] {
        assert!(
            manifest.contains(package),
            "workspace manifest should include {package}"
        );
    }
}

#[test]
fn core_is_limited_to_store_contracts() {
    let manifest = include_str!("../../fusillade-core/Cargo.toml");
    let lib = include_str!("../../fusillade-core/src/lib.rs");

    for dependency in [
        "reqwest",
        "eventsource-stream",
        "openai-reassembler",
        "metrics",
        "hostname",
    ] {
        assert!(
            !manifest.lines().any(|line| line.starts_with(dependency)),
            "fusillade-core should not depend on daemon/runtime crate {dependency}"
        );
    }

    for module in [
        "pub mod daemon;",
        "pub mod http;",
        "pub mod processor;",
        "pub mod transform;",
    ] {
        assert!(
            !lib.contains(module),
            "fusillade-core should only expose shared storage contracts, not {module}"
        );
    }
}

#[test]
fn daemon_modes_stay_on_root_crate_and_db_retries_stay_in_arsenal() {
    let root = include_str!("../src/lib.rs");
    let arsenal = include_str!("../../fusillade-arsenal/src/lib.rs");

    assert!(
        root.contains("DaemonMode"),
        "daemon mode selection should remain part of the root daemon crate"
    );
    assert!(
        arsenal.contains("DbRetryConfig"),
        "database retry configuration should remain owned by fusillade-arsenal"
    );
}

#[test]
fn root_crate_exposes_postgres_daemon_not_postgres_store() {
    let root = include_str!("../src/lib.rs");
    let manager = include_str!("../src/manager/mod.rs");

    assert!(
        root.contains("PostgresDaemon") || manager.contains("PostgresDaemon"),
        "the root crate should expose the postgres scheduling runtime"
    );
    assert!(
        !root.contains("PostgresRequestManager")
            && !manager.contains("pub use postgres::PostgresRequestManager"),
        "the reusable postgres store should be imported from fusillade-arsenal, not re-exported by the root daemon crate"
    );
    for db_only_export in [
        "DbRetryConfig",
        "PostgresStorageConfig",
        "TestDbPools",
        "migrator",
    ] {
        assert!(
            !root.contains(db_only_export),
            "{db_only_export} should stay on fusillade-arsenal instead of the root daemon crate"
        );
    }
}

#[test]
fn arsenal_pooling_paths_avoid_session_scoped_advisory_locks() {
    let postgres = include_str!("../../fusillade-arsenal/src/postgres.rs");

    assert!(
        !postgres.contains("pg_try_advisory_lock("),
        "pooled storage must use transaction-scoped advisory locks so it works with transaction pooling"
    );
    assert!(
        !postgres.contains("pg_advisory_unlock("),
        "transaction-scoped advisory locks must be released by commit or rollback, not a later pooled statement"
    );
}
