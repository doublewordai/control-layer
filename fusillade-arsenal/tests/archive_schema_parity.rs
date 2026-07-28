//! Schema-parity contract between `requests` and `batch_requests_archive`.
//!
//! The archive deliberately mirrors `requests` by column name, type, and
//! nullability, with exactly one addition: `archive_bucket DATE NOT NULL`.
//! Move queries use explicit named mappings, so physical column order may
//! evolve independently between the two tables.
//!
//! If this test fails, the correct fix is ALWAYS to mirror the column change
//! onto the twin table IN THE SAME MIGRATION (and update both move mappings if
//! it changed) — never to delete or weaken this test. See
//! fusillade-requests-phase3-plan.md §1 and
//! fusillade-phase3-partitioning-decisions.md §6 (clay/core workspace root).

use std::collections::BTreeMap;

use sqlx::{PgPool, Row};

async fn column_shapes(pool: &PgPool, table: &str) -> BTreeMap<String, (String, String)> {
    sqlx::query(
        r#"
        SELECT column_name, data_type, is_nullable
        FROM information_schema.columns
        WHERE table_name = $1
          AND table_schema = current_schema()
        "#,
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("failed to read information_schema.columns")
    .into_iter()
    .map(|row| {
        (
            row.get("column_name"),
            (row.get("data_type"), row.get("is_nullable")),
        )
    })
    .collect()
}

#[sqlx::test]
async fn archive_mirrors_requests_columns_by_name_and_shape(pool: PgPool) {
    let requests = column_shapes(&pool, "requests").await;
    let mut archive = column_shapes(&pool, "batch_requests_archive").await;

    assert!(
        !requests.is_empty() && !archive.is_empty(),
        "expected both tables to exist with columns"
    );

    // Exactly one extra column, independent of its physical position.
    let bucket = archive
        .remove("archive_bucket")
        .expect("archive must have archive_bucket");
    assert_eq!(
        bucket,
        ("date".to_string(), "NO".to_string()),
        "archive_bucket must remain DATE NOT NULL"
    );

    // Remaining columns: identical names, types, and nullability.
    assert_eq!(
        requests, archive,
        "requests and batch_requests_archive have diverged by name, type, or nullability; \
         mirror the change onto the twin table in the same migration"
    );
}

#[sqlx::test]
async fn archive_has_no_foreign_keys(pool: PgPool) {
    // Deliberate design (see table COMMENT + phase 3 plan): FK enforcement
    // would take KEY SHARE locks on referenced rows during every bulk move,
    // and an FK to request_templates with ON DELETE SET NULL would make
    // template purges UPDATE archived rows. Integrity holds by construction:
    // rows arrive only via the move transaction from already-FK-valid live
    // rows. Adding an FK here is a conscious design overturn, not a cleanup.
    let fk_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint
         WHERE conrelid = 'batch_requests_archive'::regclass AND contype = 'f'",
    )
    .fetch_one(&pool)
    .await
    .expect("failed to count archive foreign keys");
    assert_eq!(
        fk_count, 0,
        "batch_requests_archive must not gain foreign keys"
    );
}

#[sqlx::test]
async fn forward_move_shape_compiles_and_round_trips(pool: PgPool) {
    // Exercise the exact explicit forward and identity-reverse mappings end to
    // end. This remains valid even when the tables' physical orders differ.
    sqlx::query(
        "INSERT INTO batches (id, endpoint, completion_window, created_by, total_requests, created_at, expires_at)
         VALUES ('11111111-1111-1111-1111-111111111111', '/v1/chat/completions', '24h', 'parity-test', 1, now(), now() + interval '1 day')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO requests (id, batch_id, state, model, response_status, response_body, completed_at)
         VALUES ('22222222-2222-2222-2222-222222222222', '11111111-1111-1111-1111-111111111111',
                 'completed', 'parity-model', 200, '{}', now())",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO batch_requests_archive (
             id, batch_id, template_id, state, retry_attempt, not_before, daemon_id,
             claimed_at, started_at, response_status, response_body, completed_at,
             error, failed_at, canceled_at, created_at, updated_at, custom_id, model,
             response_size, routed_model, service_tier, created_by, archive_bucket
         )
         SELECT r.id, r.batch_id, r.template_id, r.state, r.retry_attempt, r.not_before,
                r.daemon_id, r.claimed_at, r.started_at, r.response_status, r.response_body,
                r.completed_at, r.error, r.failed_at, r.canceled_at, r.created_at,
                r.updated_at, r.custom_id, r.model, r.response_size, r.routed_model,
                r.service_tier, r.created_by,
                date_trunc('week', now() AT TIME ZONE 'UTC')::date
         FROM requests r WHERE r.batch_id = '11111111-1111-1111-1111-111111111111'",
    )
    .execute(&pool)
    .await
    .expect("explicit forward move mapping must stay valid");

    sqlx::query("DELETE FROM requests WHERE id = '22222222-2222-2222-2222-222222222222'")
        .execute(&pool)
        .await
        .unwrap();

    // Reverse shape: all requests columns, bucket omitted. This column list
    // is the same one the retry move-back uses; if this breaks, update BOTH.
    sqlx::query(
        "INSERT INTO requests (
             id, batch_id, template_id, state, retry_attempt, not_before, daemon_id,
             claimed_at, started_at, response_status, response_body, completed_at,
             error, failed_at, canceled_at, created_at, updated_at, custom_id, model,
             response_size, routed_model, service_tier, created_by
         )
         SELECT a.id, a.batch_id, a.template_id, a.state, a.retry_attempt, a.not_before,
                a.daemon_id, a.claimed_at, a.started_at, a.response_status, a.response_body,
                a.completed_at, a.error, a.failed_at, a.canceled_at, a.created_at,
                a.updated_at, a.custom_id, a.model, a.response_size, a.routed_model,
                a.service_tier, a.created_by
         FROM batch_requests_archive a
         WHERE a.id = '22222222-2222-2222-2222-222222222222'",
    )
    .execute(&pool)
    .await
    .expect("reverse move column list must stay valid");

    let back: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM requests WHERE id = '22222222-2222-2222-2222-222222222222'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(back, 1, "row must survive the round trip");
}
