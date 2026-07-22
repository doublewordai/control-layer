//! Regression tests for [`Storage::persist_completed_realtime_batch`] choosing
//! the terminal `state` from the HTTP status code.
//!
//! Background: realtime (service_tier `priority`) proxy completions used to be
//! persisted as `state = 'completed'` regardless of the response status, so a
//! non-2xx (e.g. a 402/503) was stored as a success with the real code only in
//! `response_status`. Now a 2xx stays `completed` and anything else becomes
//! `failed`, carrying `error` + `failed_at` (failed_fields_check) while keeping
//! `response_status`/`response_body` populated for the dashboard.
//!
//! Both write branches are covered: the bulk UPDATE (a row already exists in
//! `processing` from create_realtime) and the synthesize INSERT (no row yet).

use chrono::Utc;
use fusillade_arsenal::{PostgresRequestManager, TestDbPools};
use fusillade_core::manager::Storage;
use fusillade_core::{CreateRealtimeInput, FailureReason, PersistCompletedRealtimeInput};
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

async fn manager(pool: sqlx::PgPool) -> Arc<PostgresRequestManager<TestDbPools>> {
    Arc::new(PostgresRequestManager::with_client(
        TestDbPools::new(pool).await.unwrap(),
        Arc::new(()),
    ))
}

fn processing_input(request_id: Uuid) -> CreateRealtimeInput {
    CreateRealtimeInput {
        request_id,
        body: r#"{"messages":[]}"#.to_string(),
        model: "test-model".to_string(),
        endpoint: "http://localhost".to_string(),
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        api_key: "test-key".to_string(),
        created_by: "test-user".to_string(),
    }
}

fn realtime_input(
    request_id: Uuid,
    status_code: u16,
    response_body: &str,
) -> PersistCompletedRealtimeInput {
    PersistCompletedRealtimeInput {
        request_id,
        response_body: response_body.to_string(),
        status_code,
        request_body: r#"{"messages":[]}"#.to_string(),
        model: "test-model".to_string(),
        endpoint: "http://localhost".to_string(),
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        api_key: "test-key".to_string(),
        created_by: "test-user".to_string(),
        started_at: Utc::now(),
        completed_at: Utc::now(),
    }
}

struct TerminalRow {
    state: String,
    response_status: Option<i16>,
    response_body: Option<String>,
    error: Option<String>,
    has_failed_at: bool,
    has_completed_at: bool,
}

async fn fetch_terminal(pool: &sqlx::PgPool, id: Uuid) -> TerminalRow {
    let row = sqlx::query(
        "SELECT state, response_status, response_body, error, \
         failed_at IS NOT NULL AS has_failed_at, \
         completed_at IS NOT NULL AS has_completed_at \
         FROM requests WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("request row should exist");

    TerminalRow {
        state: row.get("state"),
        response_status: row.get("response_status"),
        response_body: row.get("response_body"),
        error: row.get("error"),
        has_failed_at: row.get("has_failed_at"),
        has_completed_at: row.get("has_completed_at"),
    }
}

/// Assert the `error` column holds a `FailureReason::NonRetriableHttpStatus`
/// carrying the given status and body. Parses the envelope structurally rather
/// than substring-matching the serialized JSON.
fn assert_http_failure(error: Option<String>, status: u16, body: &str) {
    let raw = error.expect("failed row must carry error (failed_fields_check)");
    let reason: FailureReason =
        serde_json::from_str(&raw).expect("error must deserialize as FailureReason");
    match reason {
        FailureReason::NonRetriableHttpStatus {
            status: got_status,
            body: got_body,
        } => {
            assert_eq!(got_status, status);
            assert_eq!(got_body, body);
        }
        other => panic!("expected NonRetriableHttpStatus, got {other:?}"),
    }
}

/// UPDATE branch: a row already exists in `processing` (create_realtime ran
/// inline before a 202), and a non-2xx completion comes back.
#[sqlx::test]
async fn persist_realtime_non_2xx_marks_failed_update_path(pool: sqlx::PgPool) {
    let m = manager(pool.clone()).await;
    let id = Uuid::new_v4();
    m.create_realtime(processing_input(id)).await.unwrap();

    let body = "Account balance too low. Please add credits to continue.";
    m.persist_completed_realtime_batch(&[realtime_input(id, 402, body)])
        .await
        .expect("persist should succeed");

    let row = fetch_terminal(&pool, id).await;
    assert_eq!(row.state, "failed");
    assert!(row.has_failed_at, "failed row must set failed_at");
    assert!(
        !row.has_completed_at,
        "failed row must not set completed_at"
    );
    assert_eq!(row.response_status, Some(402), "response_status preserved");
    assert_eq!(
        row.response_body.as_deref(),
        Some(body),
        "response_body preserved for the dashboard filter"
    );
    assert_http_failure(row.error, 402, body);
}

/// INSERT branch: non-background realtime, no row exists yet, so the row is
/// synthesized directly in its terminal state.
#[sqlx::test]
async fn persist_realtime_non_2xx_marks_failed_insert_path(pool: sqlx::PgPool) {
    let m = manager(pool.clone()).await;
    let id = Uuid::new_v4();

    let body =
        r#"{"error":{"code":"service_unavailable","message":"An internal error occurred."}}"#;
    m.persist_completed_realtime_batch(&[realtime_input(id, 503, body)])
        .await
        .expect("persist should succeed");

    let row = fetch_terminal(&pool, id).await;
    assert_eq!(row.state, "failed");
    assert!(row.has_failed_at);
    assert!(!row.has_completed_at);
    assert_eq!(row.response_status, Some(503));
    assert_eq!(row.response_body.as_deref(), Some(body));
    assert_http_failure(row.error, 503, body);
}

/// Happy path: a 2xx completion still lands in `completed` with no error.
#[sqlx::test]
async fn persist_realtime_2xx_marks_completed(pool: sqlx::PgPool) {
    let m = manager(pool.clone()).await;
    let id = Uuid::new_v4();

    m.persist_completed_realtime_batch(&[realtime_input(id, 200, r#"{"ok":true}"#)])
        .await
        .expect("persist should succeed");

    let row = fetch_terminal(&pool, id).await;
    assert_eq!(row.state, "completed");
    assert!(row.has_completed_at, "completed row must set completed_at");
    assert!(!row.has_failed_at, "completed row must not set failed_at");
    assert_eq!(row.response_status, Some(200));
    assert!(row.error.is_none(), "completed row must not carry error");
}
