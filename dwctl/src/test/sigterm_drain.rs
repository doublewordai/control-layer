//! Test the SIGTERM drain protocol on the BackgroundServices shutdown
//! path.
//!
//! Validates the two operational guarantees from the multi-step plan
//! (COR-353):
//!
//! - The onwards-instance daemon row is marked `dead` on shutdown so
//!   the next pod's claim cycle treats this instance as gone
//!   immediately rather than waiting for stale-daemon detection.
//! - Any rows still in `claimed` or `processing` state owned by this
//!   instance are released back to `pending` so they're picked up
//!   without waiting for the time-based fallback path.
//!
//! Both guarantees are best-effort (errors logged, shutdown proceeds);
//! the test asserts the happy path.

use sqlx::PgPool;
use uuid::Uuid;

use crate::test::utils::setup_fusillade_pool;

#[sqlx::test]
async fn shutdown_marks_onwards_daemon_dead_and_releases_rows(pool: PgPool) {
    let pool = setup_fusillade_pool(&pool).await;

    let daemon_id = Uuid::new_v4();
    let template_id = Uuid::new_v4();
    let request_id = Uuid::new_v4();
    let now = chrono::Utc::now();

    // Register a daemon row in the same shape Application does (the
    // onwards-instance registration).
    sqlx::query(
        "INSERT INTO daemons (id, hostname, pid, version, config_snapshot, status, started_at, last_heartbeat) \
         VALUES ($1, 'test-host', 1, '0', '{}'::jsonb, 'running', NOW(), NOW())",
    )
    .bind(daemon_id)
    .execute(&pool)
    .await
    .expect("insert daemon");

    // Create a template + request_processing row owned by this daemon.
    sqlx::query(
        "INSERT INTO request_templates \
         (id, file_id, custom_id, endpoint, method, path, body, model, api_key, body_byte_size) \
         VALUES ($1, NULL, NULL, 'http://upstream', 'POST', '/v1/responses', '{}', 'm', '', 0)",
    )
    .bind(template_id)
    .execute(&pool)
    .await
    .expect("insert template");
    sqlx::query(
        "INSERT INTO requests \
         (id, batch_id, template_id, model, custom_id, state, daemon_id, claimed_at, started_at, created_by) \
         VALUES ($1, NULL, $2, 'm', NULL, 'processing', $3, $4, $4, 'test-user')",
    )
    .bind(request_id)
    .bind(template_id)
    .bind(daemon_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert request in processing state");

    // Run the same drain queries Application::shutdown runs.
    drain_onwards_daemon(&pool, daemon_id).await;

    // Daemon row is now Dead.
    let status: String = sqlx::query_scalar("SELECT status FROM daemons WHERE id = $1")
        .bind(daemon_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "dead", "daemon row should be marked dead");

    // Request row was released back to pending.
    let row: (String, Option<Uuid>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT state, daemon_id, started_at FROM requests WHERE id = $1")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, "pending", "request should be released back to pending");
    assert_eq!(row.1, None, "daemon_id should be cleared");
    assert_eq!(row.2, None, "started_at should be cleared");
}

#[sqlx::test]
async fn drain_with_no_owned_rows_is_a_noop(pool: PgPool) {
    let pool = setup_fusillade_pool(&pool).await;
    let daemon_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO daemons (id, hostname, pid, version, config_snapshot, status, started_at, last_heartbeat) \
         VALUES ($1, 'test', 1, '0', '{}'::jsonb, 'running', NOW(), NOW())",
    )
    .bind(daemon_id)
    .execute(&pool)
    .await
    .unwrap();

    drain_onwards_daemon(&pool, daemon_id).await;

    // Daemon is Dead, no rows touched (because there were none).
    let status: String = sqlx::query_scalar("SELECT status FROM daemons WHERE id = $1")
        .bind(daemon_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "dead");
}

#[sqlx::test]
async fn drain_does_not_touch_other_daemons_rows(pool: PgPool) {
    let pool = setup_fusillade_pool(&pool).await;
    let our_daemon = Uuid::new_v4();
    let other_daemon = Uuid::new_v4();
    let template_id = Uuid::new_v4();
    let other_request = Uuid::new_v4();

    for d in [our_daemon, other_daemon] {
        sqlx::query(
            "INSERT INTO daemons (id, hostname, pid, version, config_snapshot, status, started_at, last_heartbeat) \
             VALUES ($1, 'test', 1, '0', '{}'::jsonb, 'running', NOW(), NOW())",
        )
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO request_templates \
         (id, file_id, custom_id, endpoint, method, path, body, model, api_key, body_byte_size) \
         VALUES ($1, NULL, NULL, 'http://upstream', 'POST', '/v1/responses', '{}', 'm', '', 0)",
    )
    .bind(template_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO requests \
         (id, batch_id, template_id, model, custom_id, state, daemon_id, claimed_at, started_at, created_by) \
         VALUES ($1, NULL, $2, 'm', NULL, 'processing', $3, NOW(), NOW(), 'test-user')",
    )
    .bind(other_request)
    .bind(template_id)
    .bind(other_daemon)
    .execute(&pool)
    .await
    .unwrap();

    drain_onwards_daemon(&pool, our_daemon).await;

    // The OTHER daemon's row is untouched.
    let row: (String, Option<Uuid>) = sqlx::query_as("SELECT state, daemon_id FROM requests WHERE id = $1")
        .bind(other_request)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "processing", "other daemon's row should be untouched");
    assert_eq!(row.1, Some(other_daemon));
    let other_status: String = sqlx::query_scalar("SELECT status FROM daemons WHERE id = $1")
        .bind(other_daemon)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(other_status, "running", "other daemon's status should be untouched");
}

/// Mirrors the drain logic in [`crate::BackgroundServices::shutdown`].
/// Kept inline so the test can exercise it without spinning up a full
/// `Application`.
async fn drain_onwards_daemon(pool: &PgPool, daemon_id: Uuid) {
    let _ = sqlx::query("UPDATE daemons SET status = 'dead', stopped_at = NOW() WHERE id = $1")
        .bind(daemon_id)
        .execute(pool)
        .await;

    let _ = sqlx::query(
        "UPDATE requests \
         SET state = 'pending', daemon_id = NULL, claimed_at = NULL, started_at = NULL \
         WHERE daemon_id = $1 AND state IN ('claimed', 'processing')",
    )
    .bind(daemon_id)
    .execute(pool)
    .await;
}
