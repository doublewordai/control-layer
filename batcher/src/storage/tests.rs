use rstest::{fixture, rstest};
use uuid::Uuid;

use crate::request::{AnyRequest, Completed, DaemonId, Pending, Request, RequestData, RequestId};
use crate::storage::{in_memory::InMemoryStorage, Storage};

#[cfg(feature = "postgres")]
use crate::storage::postgres::PostgresStorage;

/// Helper to create sample request data
fn sample_request_data(id: RequestId) -> RequestData {
    RequestData {
        id,
        endpoint: "https://api.example.com".to_string(),
        method: "POST".to_string(),
        path: "/v1/test".to_string(),
        body: r#"{"test": true}"#.to_string(),
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
    }
}

/// Fixture that returns InMemoryStorage
#[fixture]
fn in_memory_storage() -> InMemoryStorage {
    InMemoryStorage::new()
}

async fn run_test_submit_new_pending_request<S: Storage>(storage: &S) {
    let id = RequestId::from(Uuid::new_v4());

    let request = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: sample_request_data(id),
    };

    let result = storage.submit(request).await;
    assert!(result.is_ok());

    // Verify it's stored
    let pending = storage.view_pending_requests(10, None).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].data.id, id);
}

#[rstest]
#[tokio::test]
async fn test_submit_new_pending_request(in_memory_storage: InMemoryStorage) {
    run_test_submit_new_pending_request(&in_memory_storage).await;
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn test_submit_new_pending_request_postgres(pool: sqlx::PgPool) {
    let storage = PostgresStorage::new(pool);
    run_test_submit_new_pending_request(&storage).await;
}

async fn run_test_claim_requests_atomically<S: Storage>(storage: &S) {
    let daemon1 = DaemonId::from(Uuid::new_v4());
    let daemon2 = DaemonId::from(Uuid::new_v4());

    // Submit two pending requests
    let id1 = RequestId::from(Uuid::new_v4());
    let id2 = RequestId::from(Uuid::new_v4());

    let request1 = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: sample_request_data(id1),
    };
    storage.submit(request1).await.unwrap();

    let request2 = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: sample_request_data(id2),
    };
    storage.submit(request2).await.unwrap();

    // Daemon 1 claims both
    let claimed = storage.claim_requests(10, daemon1).await.unwrap();
    assert_eq!(claimed.len(), 2);
    assert_eq!(claimed[0].state.daemon_id, daemon1);
    assert_eq!(claimed[1].state.daemon_id, daemon1);

    // Daemon 2 tries to claim - should get nothing
    let claimed2 = storage.claim_requests(10, daemon2).await.unwrap();
    assert_eq!(claimed2.len(), 0);
}

#[rstest]
#[tokio::test]
async fn test_claim_requests_atomically(in_memory_storage: InMemoryStorage) {
    run_test_claim_requests_atomically(&in_memory_storage).await;
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn test_claim_requests_atomically_postgres(pool: sqlx::PgPool) {
    let storage = PostgresStorage::new(pool);
    run_test_claim_requests_atomically(&storage).await;
}

async fn run_test_unclaim_and_reclaim<S: Storage>(storage: &S) {
    let daemon_id = DaemonId::from(Uuid::new_v4());
    let id = RequestId::from(Uuid::new_v4());

    // Submit a pending request
    let request = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: sample_request_data(id),
    };
    storage.submit(request).await.unwrap();

    // Claim it
    let claimed = storage.claim_requests(1, daemon_id).await.unwrap();
    assert_eq!(claimed.len(), 1);

    // Unclaim it
    let unclaimed = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: sample_request_data(id),
    };
    storage.persist(&unclaimed).await.unwrap();

    // Should be claimable again
    let reclaimed = storage.claim_requests(1, daemon_id).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
}
#[rstest]
#[tokio::test]
async fn test_unclaim_and_reclaim(in_memory_storage: InMemoryStorage) {
    run_test_unclaim_and_reclaim(&in_memory_storage).await;
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn test_unclaim_and_reclaim_postgres(pool: sqlx::PgPool) {
    let storage = PostgresStorage::new(pool);
    run_test_unclaim_and_reclaim(&storage).await;
}

async fn run_test_persist_updates_state<S: Storage>(storage: &S) {
    let daemon_id = DaemonId::from(Uuid::new_v4());
    let id = RequestId::from(Uuid::new_v4());

    // Submit and claim a request
    let request = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: sample_request_data(id),
    };
    storage.submit(request).await.unwrap();

    let claimed = storage.claim_requests(1, daemon_id).await.unwrap();
    assert_eq!(claimed.len(), 1);

    // Transition to completed
    let completed = Request {
        state: Completed {
            response_status: 200,
            response_body: "OK".to_string(),
            claimed_at: chrono::Utc::now(),
            started_at: chrono::Utc::now(),
            completed_at: chrono::Utc::now(),
        },
        data: sample_request_data(id),
    };
    storage.persist(&completed).await.unwrap();

    // Should no longer be pending
    let pending = storage.view_pending_requests(10, None).await.unwrap();
    assert_eq!(pending.len(), 0);
}

#[rstest]
#[tokio::test]
async fn test_persist_updates_state(in_memory_storage: InMemoryStorage) {
    run_test_persist_updates_state(&in_memory_storage).await;
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn test_persist_updates_state_postgres(pool: sqlx::PgPool) {
    let storage = PostgresStorage::new(pool);
    run_test_persist_updates_state(&storage).await;
}

async fn run_test_cancel_pending_request<S: Storage>(storage: &S) {
    let id = RequestId::from(Uuid::new_v4());

    // Submit a pending request
    let request = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: sample_request_data(id),
    };
    storage.submit(request.clone()).await.unwrap();

    // Cancel it
    let canceled = request.cancel(storage).await.unwrap();
    assert_eq!(canceled.data.id, id);

    // Should no longer be pending
    let pending = storage.view_pending_requests(10, None).await.unwrap();
    assert_eq!(pending.len(), 0);

    // Should be retrievable as canceled
    let retrieved = storage.get_requests(vec![id]).await.unwrap();
    assert!(matches!(
        retrieved[0].as_ref().unwrap(),
        AnyRequest::Canceled(_)
    ));
}

#[rstest]
#[tokio::test]
async fn test_cancel_pending_request(in_memory_storage: InMemoryStorage) {
    run_test_cancel_pending_request(&in_memory_storage).await;
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn test_cancel_pending_request_postgres(pool: sqlx::PgPool) {
    let storage = PostgresStorage::new(pool);
    run_test_cancel_pending_request(&storage).await;
}

async fn run_test_cancel_claimed_request<S: Storage>(storage: &S) {
    let daemon_id = DaemonId::from(Uuid::new_v4());
    let id = RequestId::from(Uuid::new_v4());

    // Submit and claim a request
    let request = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: sample_request_data(id),
    };
    storage.submit(request).await.unwrap();

    let claimed = storage.claim_requests(1, daemon_id).await.unwrap();
    assert_eq!(claimed.len(), 1);

    // Cancel it
    let canceled = claimed[0].clone().cancel(storage).await.unwrap();
    assert_eq!(canceled.data.id, id);

    // Should be retrievable as canceled
    let retrieved = storage.get_requests(vec![id]).await.unwrap();
    assert!(matches!(
        retrieved[0].as_ref().unwrap(),
        AnyRequest::Canceled(_)
    ));
}
#[rstest]
#[tokio::test]
async fn test_cancel_claimed_request(in_memory_storage: InMemoryStorage) {
    run_test_cancel_claimed_request(&in_memory_storage).await;
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn test_cancel_claimed_request_postgres(pool: sqlx::PgPool) {
    let storage = PostgresStorage::new(pool);
    run_test_cancel_claimed_request(&storage).await;
}

async fn run_test_get_requests_not_found<S: Storage>(storage: &S) {
    let id = RequestId::from(Uuid::new_v4());

    let results = storage.get_requests(vec![id]).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[rstest]
#[tokio::test]
async fn test_get_requests_not_found(in_memory_storage: InMemoryStorage) {
    run_test_get_requests_not_found(&in_memory_storage).await;
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn test_get_requests_not_found_postgres(pool: sqlx::PgPool) {
    let storage = PostgresStorage::new(pool);
    run_test_get_requests_not_found(&storage).await;
}

async fn run_test_claim_respects_limit<S: Storage>(storage: &S) {
    let daemon_id = DaemonId::from(Uuid::new_v4());

    // Submit 5 pending requests
    for _ in 0..5 {
        let request = Request {
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(RequestId::from(Uuid::new_v4())),
        };
        storage.submit(request).await.unwrap();
    }

    // Claim only 3
    let claimed = storage.claim_requests(3, daemon_id).await.unwrap();
    assert_eq!(claimed.len(), 3);

    // Should still be 2 pending
    let pending = storage.view_pending_requests(10, None).await.unwrap();
    assert_eq!(pending.len(), 2);
}

#[rstest]
#[tokio::test]
async fn test_claim_respects_limit(in_memory_storage: InMemoryStorage) {
    run_test_claim_respects_limit(&in_memory_storage).await;
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn test_claim_respects_limit_postgres(pool: sqlx::PgPool) {
    let storage = PostgresStorage::new(pool);
    run_test_claim_respects_limit(&storage).await;
}

async fn run_test_claim_respects_not_before<S: Storage>(storage: &S) {
    let daemon_id = DaemonId::from(Uuid::new_v4());
    let id = RequestId::from(Uuid::new_v4());

    // Submit a pending request with not_before in the future
    let future_time = chrono::Utc::now() + chrono::Duration::hours(1);
    let request = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: Some(future_time),
        },
        data: sample_request_data(id),
    };
    storage.submit(request).await.unwrap();

    // Should not be claimable yet
    let claimed = storage.claim_requests(10, daemon_id).await.unwrap();
    assert_eq!(claimed.len(), 0);

    // Submit another request that can be claimed immediately
    let id2 = RequestId::from(Uuid::new_v4());
    let request2 = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: sample_request_data(id2),
    };
    storage.submit(request2).await.unwrap();

    // Should claim only the second one
    let claimed = storage.claim_requests(10, daemon_id).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].data.id, id2);
}

#[rstest]
#[tokio::test]
async fn test_claim_respects_not_before(in_memory_storage: InMemoryStorage) {
    run_test_claim_respects_not_before(&in_memory_storage).await;
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn test_claim_respects_not_before_postgres(pool: sqlx::PgPool) {
    let storage = PostgresStorage::new(pool);
    run_test_claim_respects_not_before(&storage).await;
}
