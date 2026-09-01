//! Regression tests for [`Storage::complete_request`] / [`Storage::fail_request`]
//! disambiguating "row missing" from "row in wrong state".
//!
//! Background: previously both cases returned `RequestNotFound`, which made
//! it impossible for callers (specifically `dwctl`'s `complete_response_idempotent`)
//! to be properly idempotent against concurrent writers — a zombie task
//! attempt completing the row out from under a fresh attempt looked
//! identical to "row was never created".

use fusillade_arsenal::{PostgresRequestManager, TestDbPools};
use fusillade_core::manager::Storage;
use fusillade_core::{CreateRealtimeInput, FusilladeError, RequestId};
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

#[sqlx::test]
async fn complete_request_succeeds_for_processing_row(pool: sqlx::PgPool) {
    let m = manager(pool).await;
    let id = Uuid::new_v4();
    m.create_realtime(processing_input(id)).await.unwrap();

    m.complete_request(RequestId(id), r#"{"ok":true}"#, 200)
        .await
        .expect("first complete_request should succeed");
}

#[sqlx::test]
async fn complete_request_returns_state_conflict_when_already_completed(pool: sqlx::PgPool) {
    let m = manager(pool).await;
    let id = Uuid::new_v4();
    m.create_realtime(processing_input(id)).await.unwrap();
    m.complete_request(RequestId(id), r#"{"ok":true}"#, 200)
        .await
        .unwrap();

    let err = m
        .complete_request(RequestId(id), r#"{"ok":true}"#, 200)
        .await
        .expect_err("second complete_request should error");

    match err {
        FusilladeError::RequestStateConflict {
            id: got_id,
            current_state,
            expected,
        } => {
            assert_eq!(got_id, RequestId(id));
            assert_eq!(current_state, "completed");
            assert_eq!(expected, "processing");
        }
        other => panic!("expected RequestStateConflict, got {other:?}"),
    }
}

#[sqlx::test]
async fn complete_request_returns_state_conflict_when_already_failed(pool: sqlx::PgPool) {
    let m = manager(pool).await;
    let id = Uuid::new_v4();
    m.create_realtime(processing_input(id)).await.unwrap();
    m.fail_request(RequestId(id), "boom", 500).await.unwrap();

    let err = m
        .complete_request(RequestId(id), r#"{"ok":true}"#, 200)
        .await
        .expect_err("complete_request on a failed row should error");

    match err {
        FusilladeError::RequestStateConflict { current_state, .. } => {
            assert_eq!(current_state, "failed");
        }
        other => panic!("expected RequestStateConflict, got {other:?}"),
    }
}

#[sqlx::test]
async fn complete_request_returns_not_found_for_missing_row(pool: sqlx::PgPool) {
    let m = manager(pool).await;
    let id = Uuid::new_v4();

    let err = m
        .complete_request(RequestId(id), r#"{"ok":true}"#, 200)
        .await
        .expect_err("complete_request on missing row should error");

    match err {
        FusilladeError::RequestNotFound(got_id) => assert_eq!(got_id, RequestId(id)),
        other => panic!("expected RequestNotFound, got {other:?}"),
    }
}

#[sqlx::test]
async fn fail_request_returns_state_conflict_when_already_completed(pool: sqlx::PgPool) {
    let m = manager(pool).await;
    let id = Uuid::new_v4();
    m.create_realtime(processing_input(id)).await.unwrap();
    m.complete_request(RequestId(id), r#"{"ok":true}"#, 200)
        .await
        .unwrap();

    let err = m
        .fail_request(RequestId(id), "boom", 500)
        .await
        .expect_err("fail_request on completed row should error");

    match err {
        FusilladeError::RequestStateConflict { current_state, .. } => {
            assert_eq!(current_state, "completed");
        }
        other => panic!("expected RequestStateConflict, got {other:?}"),
    }
}

#[sqlx::test]
async fn fail_request_returns_not_found_for_missing_row(pool: sqlx::PgPool) {
    let m = manager(pool).await;
    let id = Uuid::new_v4();

    let err = m
        .fail_request(RequestId(id), "boom", 500)
        .await
        .expect_err("fail_request on missing row should error");

    match err {
        FusilladeError::RequestNotFound(got_id) => assert_eq!(got_id, RequestId(id)),
        other => panic!("expected RequestNotFound, got {other:?}"),
    }
}
