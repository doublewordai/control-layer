//! The tolerations-release migrations (the `dispatched_tolerated` column on
//! requests and its archive twin, `model_release_cutoffs`, and the
//! concurrently built tolerated-completions index) apply, revert and
//! re-apply cleanly.

use fusillade_arsenal::MIGRATOR;

const FIRST: i64 = 20261008120000;

async fn exists(pool: &sqlx::PgPool) -> (bool, bool, bool, bool) {
    sqlx::query_as(
        "SELECT
            EXISTS (SELECT 1 FROM information_schema.columns
                    WHERE table_schema = current_schema() AND table_name = 'requests'
                      AND column_name = 'dispatched_tolerated'),
            EXISTS (SELECT 1 FROM information_schema.columns
                    WHERE table_schema = current_schema() AND table_name = 'batch_requests_archive'
                      AND column_name = 'dispatched_tolerated'),
            to_regclass('model_release_cutoffs') IS NOT NULL,
            to_regclass('idx_requests_tolerated_completions') IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = false)]
async fn tolerations_release_migrates_up_and_down(pool: sqlx::PgPool) {
    MIGRATOR.run(&pool).await.unwrap();
    assert_eq!(exists(&pool).await, (true, true, true, true));

    // One cutoff row per model.
    let insert = "INSERT INTO model_release_cutoffs
        (model, release_before_deadline, throughput, backlog_requests, samples, computed_at)
        VALUES ('m', NULL, 1.0, 0, 0, NOW())";
    sqlx::query(insert).execute(&pool).await.unwrap();
    assert!(sqlx::query(insert).execute(&pool).await.is_err());

    // Revert this feature's migrations: everything before them stays.
    let previous = MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .filter(|version| *version < FIRST)
        .max()
        .unwrap();
    MIGRATOR.undo(&pool, previous).await.unwrap();
    assert_eq!(exists(&pool).await, (false, false, false, false));

    MIGRATOR.run(&pool).await.unwrap();
    assert_eq!(exists(&pool).await, (true, true, true, true));
}
