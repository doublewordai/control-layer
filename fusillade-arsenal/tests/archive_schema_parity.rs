//! Archive columns must mirror requests by name, type and nullability.
//! The partition key retains its physical position as new fields are appended;
//! archive writes therefore name every target column explicitly. Round-trip
//! tests below protect that mapping and the retry move-back column list.

use sqlx::PgPool;

#[derive(Debug, PartialEq)]
struct ColumnShape {
    name: String,
    ordinal: i32,
    data_type: String,
    is_nullable: String,
}

async fn column_shapes(pool: &PgPool, table: &str) -> Vec<ColumnShape> {
    sqlx::query_as!(
        ColumnShape,
        r#"
        SELECT column_name AS "name!",
               ordinal_position::INT AS "ordinal!",
               data_type AS "data_type!",
               is_nullable AS "is_nullable!"
        FROM information_schema.columns
        WHERE table_name = $1
          AND table_schema = current_schema()
        ORDER BY ordinal_position
        "#,
        table
    )
    .fetch_all(pool)
    .await
    .expect("failed to read information_schema.columns")
}

#[sqlx::test]
async fn archive_mirrors_requests_columns_plus_bucket(pool: PgPool) {
    let requests = column_shapes(&pool, "requests").await;
    let mut archive = column_shapes(&pool, "batch_requests_archive").await;

    assert!(
        !requests.is_empty() && !archive.is_empty(),
        "expected both tables to exist with columns"
    );

    let bucket = archive.remove(
        archive
            .iter()
            .position(|c| c.name == "archive_bucket")
            .unwrap(),
    );
    assert_eq!(bucket.data_type, "date");
    assert_eq!(bucket.is_nullable, "NO");

    // Remaining columns: identical names, order, types, and nullability.
    assert_eq!(
        requests.len(),
        archive.len(),
        "requests and batch_requests_archive have diverged in column count. \
         A migration changed one table's columns without mirroring the twin \
         in the same migration. requests={:?} archive={:?}",
        requests.iter().map(|c| &c.name).collect::<Vec<_>>(),
        archive.iter().map(|c| &c.name).collect::<Vec<_>>(),
    );
    // Compare the SEQUENCE of columns, not raw ordinal_position values:
    // `requests` carries attnum gaps from columns dropped over its migration
    // history (information_schema exposes raw attnums), while the archive is
    // gap-free. `SELECT r.*` alignment depends only on the visible-column
    // sequence, which is exactly what this asserts.
    for (position, (r, a)) in requests.iter().zip(archive.iter()).enumerate() {
        assert_eq!(
            (&r.name, &r.data_type, &r.is_nullable),
            (&a.name, &a.data_type, &a.is_nullable),
            "column mismatch between requests and batch_requests_archive at \
             visible position {}: requests has {r:?}, archive has {a:?}. \
             Mirror the change onto the twin table in the same migration.",
            position + 1
        );
    }
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
    // The positional-alignment contract, exercised end to end: the exact
    // `SELECT r.*, $bucket` forward shape and the explicit-column reverse
    // shape used by the move code. Guards against a column being mirrored
    // in name but not in position.
    sqlx::query(
        "INSERT INTO batches (id, endpoint, completion_window, created_by, total_requests, created_at, expires_at)
         VALUES ('11111111-1111-1111-1111-111111111111', '/v1/chat/completions', '24h', 'parity-test', 1, now(), now() + interval '1 day')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO requests (id, batch_id, state, model, response_status, response_body, completed_at, billing_mode, accepted_event_id)
         VALUES ('22222222-2222-2222-2222-222222222222', '11111111-1111-1111-1111-111111111111',
                 'completed', 'parity-model', 200, '{}', now(), 'durable', '33333333-3333-3333-3333-333333333333')",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO batch_requests_archive (id, batch_id, template_id, state, retry_attempt, not_before, daemon_id, claimed_at, started_at, response_status, response_body, completed_at, error, failed_at, canceled_at, created_at, updated_at, custom_id, model, response_size, routed_model, service_tier, created_by, billing_mode, accepted_event_id, archive_bucket)
         SELECT r.*, date_trunc('week', now() AT TIME ZONE 'UTC')::date
         FROM requests r WHERE r.batch_id = '11111111-1111-1111-1111-111111111111'",
    )
    .execute(&pool)
    .await
    .expect("forward move shape must stay valid: SELECT r.*, $bucket");

    sqlx::query("DELETE FROM requests WHERE id = '22222222-2222-2222-2222-222222222222'")
        .execute(&pool)
        .await
        .unwrap();

    // Reverse shape: all requests columns, bucket omitted. This column list
    // is the same one the retry move-back uses; if this breaks, update BOTH.
    sqlx::query(
        "INSERT INTO requests
         SELECT id, batch_id, template_id, state, retry_attempt, not_before, daemon_id,
                claimed_at, started_at, response_status, response_body, completed_at,
                error, failed_at, canceled_at, created_at, updated_at, custom_id,
                model, response_size, routed_model, service_tier, created_by, billing_mode, accepted_event_id
         FROM batch_requests_archive
         WHERE id = '22222222-2222-2222-2222-222222222222'",
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
    let acceptance: (String, uuid::Uuid) = sqlx::query_as(
        "SELECT billing_mode, accepted_event_id FROM requests WHERE id = '22222222-2222-2222-2222-222222222222'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(acceptance.0, "durable");
    assert_eq!(
        acceptance.1,
        uuid::Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap()
    );
}
