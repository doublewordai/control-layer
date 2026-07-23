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
use uuid::Uuid;

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
async fn attempt_id_is_nullable_without_default_and_reaches_existing_partitions(pool: PgPool) {
    let live: (String, String, Option<String>, i32) = sqlx::query_as(
        r#"
        SELECT data_type, is_nullable, column_default, ordinal_position::int
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'requests'
          AND column_name = 'attempt_id'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("requests.attempt_id must exist");
    assert_eq!(
        (&live.0[..], &live.1[..], live.2.as_deref()),
        ("uuid", "YES", None),
        "live attempt ownership must remain nullable and have no default",
    );

    let archive: (String, String, Option<String>, i32) = sqlx::query_as(
        r#"
        SELECT data_type, is_nullable, column_default, ordinal_position::int
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'batch_requests_archive'
          AND column_name = 'attempt_id'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("batch_requests_archive.attempt_id must exist");
    assert_eq!(
        (&archive.0[..], &archive.1[..], archive.2.as_deref()),
        ("uuid", "YES", None),
        "archived attempt ownership must remain nullable and have no default",
    );

    let archive_bucket_position: i32 = sqlx::query_scalar(
        r#"
        SELECT ordinal_position::int
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'batch_requests_archive'
          AND column_name = 'archive_bucket'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("archive_bucket must exist");
    assert!(
        archive.3 > archive_bucket_position,
        "an upgraded archive appends attempt_id after its existing partition key",
    );
    assert_ne!(
        live.3, archive.3,
        "the upgraded live/archive physical positions should differ, proving mappings cannot be positional",
    );

    // The archive migration creates weekly children before the later attempt
    // migration runs. ALTERing the partitioned parent must propagate the
    // additive ownership column to those already-attached partitions.
    let existing_partition: String = sqlx::query_scalar(
        r#"
        SELECT child.relname
        FROM pg_inherits
        JOIN pg_class parent ON parent.oid = inhparent
        JOIN pg_class child ON child.oid = inhrelid
        WHERE parent.oid = 'batch_requests_archive'::regclass
        ORDER BY child.relname
        LIMIT 1
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("archive migration must have created an existing weekly partition");
    let child: (String, String, Option<String>, i32) = sqlx::query_as(
        r#"
        SELECT data_type, is_nullable, column_default, ordinal_position::int
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = $1
          AND column_name = 'attempt_id'
        "#,
    )
    .bind(&existing_partition)
    .fetch_one(&pool)
    .await
    .expect("attempt_id must propagate to existing archive partitions");
    assert_eq!(
        (&child.0[..], &child.1[..], child.2.as_deref(), child.3),
        ("uuid", "YES", None, archive.3),
    );
}

#[sqlx::test]
async fn processing_admission_id_is_nullable_and_reaches_existing_partitions(pool: PgPool) {
    for table in ["requests", "batch_requests_archive"] {
        let shape: (String, String, Option<String>) = sqlx::query_as(
            r#"
            SELECT data_type, is_nullable, column_default
            FROM information_schema.columns
            WHERE table_schema = current_schema()
              AND table_name = $1
              AND column_name = 'processing_admission_id'
            "#,
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|_| panic!("{table}.processing_admission_id must exist"));
        assert_eq!(
            (&shape.0[..], &shape.1[..], shape.2.as_deref()),
            ("uuid", "YES", None),
            "{table}.processing_admission_id must remain nullable without a default",
        );
    }

    let existing_partition: String = sqlx::query_scalar(
        r#"
        SELECT child.relname
        FROM pg_inherits
        JOIN pg_class parent ON parent.oid = inhparent
        JOIN pg_class child ON child.oid = inhrelid
        WHERE parent.oid = 'batch_requests_archive'::regclass
        ORDER BY child.relname
        LIMIT 1
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("archive migration must have created an existing weekly partition");
    let child_shape: (String, String, Option<String>) = sqlx::query_as(
        r#"
        SELECT data_type, is_nullable, column_default
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = $1
          AND column_name = 'processing_admission_id'
        "#,
    )
    .bind(existing_partition)
    .fetch_one(&pool)
    .await
    .expect("processing_admission_id must propagate to existing archive partitions");
    assert_eq!(
        (
            &child_shape.0[..],
            &child_shape.1[..],
            child_shape.2.as_deref()
        ),
        ("uuid", "YES", None),
    );
}

#[sqlx::test]
async fn forward_move_shape_compiles_and_round_trips(pool: PgPool) {
    // Exercise the exact explicit forward and retry-to-pending reverse
    // mappings end to end. This remains valid when an upgraded archive has
    // attempt_id physically after archive_bucket.
    let attempt_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO batches (id, endpoint, completion_window, created_by, total_requests, created_at, expires_at)
         VALUES ('11111111-1111-1111-1111-111111111111', '/v1/chat/completions', '24h', 'parity-test', 1, now(), now() + interval '1 day')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO requests (id, batch_id, state, model, canceled_at, attempt_id)
         VALUES ('22222222-2222-2222-2222-222222222222', '11111111-1111-1111-1111-111111111111',
                 'canceled', 'parity-model', now(), $1)",
    )
    .bind(attempt_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO batch_requests_archive (
             id, batch_id, template_id, state, retry_attempt, not_before, daemon_id,
             claimed_at, started_at, response_status, response_body, completed_at,
             error, failed_at, canceled_at, created_at, updated_at, custom_id, model,
             response_size, routed_model, service_tier, created_by, attempt_id,
             processing_admission_id, archive_bucket
         )
         SELECT r.id, r.batch_id, r.template_id, r.state, r.retry_attempt, r.not_before,
                r.daemon_id, r.claimed_at, r.started_at, r.response_status, r.response_body,
                r.completed_at, r.error, r.failed_at, r.canceled_at, r.created_at,
                r.updated_at, r.custom_id, r.model, r.response_size, r.routed_model,
                r.service_tier, r.created_by, r.attempt_id, r.processing_admission_id,
                date_trunc('week', now() AT TIME ZONE 'UTC')::date
         FROM requests r WHERE r.batch_id = '11111111-1111-1111-1111-111111111111'",
    )
    .execute(&pool)
    .await
    .expect("explicit forward move mapping must stay valid");

    let archived_attempt: Option<Uuid> = sqlx::query_scalar(
        "SELECT attempt_id FROM batch_requests_archive
         WHERE id = '22222222-2222-2222-2222-222222222222'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        archived_attempt,
        Some(attempt_id),
        "forward archive mapping must preserve an in-flight soft-cancel token",
    );

    sqlx::query("DELETE FROM requests WHERE id = '22222222-2222-2222-2222-222222222222'")
        .execute(&pool)
        .await
        .unwrap();

    // Reverse shape: the retry move-back names every request column, omits the
    // bucket, and deliberately revokes archived ownership while re-pending.
    sqlx::query(
        "INSERT INTO requests (
             id, batch_id, template_id, state, retry_attempt, not_before, daemon_id,
             claimed_at, started_at, response_status, response_body, completed_at,
             error, failed_at, canceled_at, created_at, updated_at, custom_id, model,
             response_size, routed_model, service_tier, created_by, attempt_id,
             processing_admission_id
         )
         SELECT a.id, a.batch_id, a.template_id, 'pending', 0, NULL,
                NULL, NULL, NULL, NULL, NULL,
                NULL, NULL, NULL, NULL, a.created_at, now(),
                a.custom_id, a.model, 0, NULL,
                a.service_tier, a.created_by, NULL, NULL
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

    let (state, live_attempt): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT state, attempt_id FROM requests
         WHERE id = '22222222-2222-2222-2222-222222222222'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(
        live_attempt, None,
        "retry move-back must revoke archived execution ownership",
    );
}
