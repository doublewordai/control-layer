//! The `dispatch_throughput_samples` migration applies, reverts and
//! re-applies cleanly, and the table enforces its key and checks.

use fusillade_arsenal::MIGRATOR;

const MIGRATION: i64 = 20261008120000;

async fn table_exists(pool: &sqlx::PgPool) -> bool {
    sqlx::query_scalar("SELECT to_regclass('dispatch_throughput_samples') IS NOT NULL")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = false)]
async fn dispatch_throughput_samples_migrates_up_and_down(pool: sqlx::PgPool) {
    MIGRATOR.run(&pool).await.unwrap();
    assert!(table_exists(&pool).await);

    let daemon = uuid::Uuid::new_v4();
    let insert = "INSERT INTO dispatch_throughput_samples \
        (daemon_id, model, completions_decayed, slot_seconds_decayed, samples) \
        VALUES ($1, $2, $3, 1.0, 1)";
    sqlx::query(insert)
        .bind(daemon)
        .bind("m")
        .bind(1.0_f64)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query(insert)
            .bind(daemon)
            .bind("m")
            .bind(1.0_f64)
            .execute(&pool)
            .await
            .is_err(),
        "one row per (daemon, model)"
    );
    assert!(
        sqlx::query(insert)
            .bind(daemon)
            .bind("n")
            .bind(-1.0_f64)
            .execute(&pool)
            .await
            .is_err(),
        "decayed sums are non-negative"
    );

    // Revert exactly this migration: everything before it stays applied.
    let previous = MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .filter(|version| *version < MIGRATION)
        .max()
        .unwrap();
    MIGRATOR.undo(&pool, previous).await.unwrap();
    assert!(!table_exists(&pool).await);

    MIGRATOR.run(&pool).await.unwrap();
    assert!(table_exists(&pool).await);
}
