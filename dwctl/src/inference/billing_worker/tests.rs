use super::*;
use crate::{
    api::models::users::Role,
    db::{
        handlers::{Tariffs, credits::Credits},
        models::{
            credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
            tariffs::TariffCreateDBRequest,
        },
    },
    inference::{
        billing_events::{BillingEventQueue, capture},
        outbound_request::{OutboundConfig, StreamTimeouts, outbound_request_middleware},
    },
    test::utils::{
        create_test_api_key_for_user, create_test_config, create_test_deployment, create_test_endpoint, create_test_user,
        setup_fusillade_pool,
    },
};
use axum::{Router, middleware, routing::post};
use fusillade::{
    BatchInput, Completed, HttpClient, HttpResponse, Request as StoredRequest, RequestData, RequestId, RequestTemplateInput,
    ReqwestHttpClient, Storage,
};
use fusillade_arsenal::PostgresRequestManager;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};

const SSE: &str = "data: {\"id\":\"response-id\",\"model\":\"served-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"private-response-content\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\ndata: [DONE]\n\n";

struct Fixture {
    main: PgPool,
    storage: PostgresRequestManager<PgPool>,
    worker: BillingWorker,
    request: RequestData,
    owner: Uuid,
    key: Uuid,
    batch: Uuid,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(main: PgPool) -> Self {
        let user = create_test_user(&main, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&main, user.id).await;
        sqlx::query("UPDATE api_keys SET purpose='batch',spend_limit=50 WHERE id=$1")
            .bind(key.id)
            .execute(&main)
            .await
            .unwrap();
        create_test_endpoint(&main, "test", user.id).await;
        let model = create_test_deployment(&main, user.id, "durable-worker-model", "durable-worker-model").await;
        {
            let mut conn = main.acquire().await.unwrap();
            Tariffs::new(&mut conn)
                .create(&TariffCreateDBRequest {
                    deployed_model_id: model.id,
                    name: "batch-test".into(),
                    input_price_per_token: Decimal::ONE,
                    output_price_per_token: Decimal::ONE,
                    api_key_purpose: Some(ApiKeyPurpose::Batch),
                    completion_window: Some("24h".into()),
                    valid_from: None,
                })
                .await
                .unwrap();
            Credits::new(&mut conn)
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::Purchase,
                    amount: Decimal::from(100),
                    source_id: format!("test-topup-{}", Uuid::new_v4()),
                    description: None,
                    fusillade_batch_id: None,
                    api_key_id: None,
                })
                .await
                .unwrap();
        }
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(|| async { ([("content-type", "text/event-stream")], SSE) }),
            )
            .layer(middleware::from_fn_with_state(
                OutboundConfig {
                    timeouts: StreamTimeouts {
                        first_chunk: Duration::from_secs(2),
                        chunk: Duration::from_secs(2),
                        body: Duration::from_secs(2),
                    },
                },
                outbound_request_middleware,
            ))
            .layer(middleware::from_fn_with_state(BillingEventQueue::new(main.clone()), capture));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let fusillade = setup_fusillade_pool(&main).await;
        let storage = PostgresRequestManager::with_client(fusillade.clone(), Arc::new(()));
        let file = storage
            .create_file(
                "durable-worker-test".into(),
                None,
                vec![RequestTemplateInput {
                    custom_id: Some("worker-test".into()),
                    endpoint,
                    method: "POST".into(),
                    path: "/v1/chat/completions".into(),
                    body: r#"{"model":"durable-worker-model","messages":[{"role":"user","content":"private-request-content"}]}"#.into(),
                    model: "durable-worker-model".into(),
                    api_key: key.secret.clone(),
                }],
            )
            .await
            .unwrap();
        let batch = storage
            .create_batch(BatchInput {
                file_id: file,
                endpoint: "/v1/chat/completions".into(),
                completion_window: "24h".into(),
                metadata: None,
                created_by: Some(user.id.to_string()),
                api_key_id: Some(key.id),
                api_key: Some(key.secret.clone()),
                total_requests: Some(1),
            })
            .await
            .unwrap();
        let id: Uuid = sqlx::query_scalar("SELECT id FROM requests WHERE batch_id=$1")
            .bind(batch.id.0)
            .fetch_one(&fusillade)
            .await
            .unwrap();
        let requests = storage.get_requests(vec![RequestId(id)]).await.unwrap();
        let claimed = requests
            .into_iter()
            .next()
            .unwrap()
            .unwrap()
            .into_pending()
            .unwrap()
            .claim(Uuid::new_v4().into(), &storage)
            .await
            .unwrap();
        let mut request = claimed.data;
        assert!(storage.assign_billing_mode(request.id, true).await.unwrap());
        // get_requests is a lookup, not the daemon's bulk claim builder; copy
        // the persisted batch fields that claimed_rows_to_requests forwards.
        request.batch_metadata.extend([
            ("id".into(), batch.id.0.to_string()),
            ("created_at".into(), batch.created_at.to_rfc3339()),
            ("completion_window".into(), "24h".into()),
            ("stream".into(), "1".into()),
            ("billing-mode".into(), "durable".into()),
            ("billing-owner-id".into(), user.id.to_string()),
            ("billing-model".into(), "durable-worker-model".into()),
            ("billing-retry-attempt".into(), "0".into()),
        ]);
        let worker = BillingWorker::new(DynPools::new(main.clone()), DynPools::new(fusillade), create_test_config());
        Self {
            main,
            storage,
            worker,
            request,
            owner: user.id,
            key: key.id,
            batch: batch.id.0,
            server,
        }
    }

    async fn publish(&self, event: Uuid) -> HttpResponse {
        let mut request = self.request.clone();
        request.batch_metadata.insert("billing-event-id".into(), event.to_string());
        let response = ReqwestHttpClient::default().execute(&request, &request.api_key).await.unwrap();
        assert_eq!(response.status, 200);
        // The HTTP client already checked the exact event UUID acknowledgement.
        assert!(response.body.contains("private-response-content"));
        response
    }

    async fn accept(&self, event: Uuid, response: HttpResponse) {
        let mut data = self.request.clone();
        data.batch_metadata.insert("billing-event-id".into(), event.to_string());
        let now = Utc::now();
        self.storage
            .persist(&StoredRequest {
                data,
                state: Completed {
                    response_status: response.status,
                    response_body: response.body,
                    claimed_at: now,
                    started_at: now,
                    completed_at: now,
                    routed_model: "durable-worker-model".into(),
                },
            })
            .await
            .unwrap();
    }

    async fn completed(&self) -> Uuid {
        let event = Uuid::new_v4();
        let response = self.publish(event).await;
        self.accept(event, response).await;
        event
    }

    async fn balance(&self) -> Decimal {
        sqlx::query_scalar("SELECT balance FROM user_balance_checkpoints WHERE user_id=$1")
            .bind(self.owner)
            .fetch_one(&self.main)
            .await
            .unwrap()
    }

    async fn receipts(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM billing_receipts WHERE request_id=$1")
            .bind(self.request.id.0)
            .fetch_one(&self.main)
            .await
            .unwrap()
    }

    async fn due(&self, event: Uuid) {
        sqlx::query("UPDATE fusillade_billing_events SET next_attempt_at=now() WHERE event_id=$1")
            .bind(event)
            .execute(&self.main)
            .await
            .unwrap();
    }
}

#[sqlx::test]
async fn stream_capture_acceptance_worker_and_retained_receipt_form_one_charge(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let event = f.completed().await;
    let captured: String = sqlx::query_scalar("SELECT row_to_json(e)::text FROM fusillade_billing_events e WHERE event_id=$1")
        .bind(event)
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert!(!captured.contains(&f.request.api_key));
    assert!(!captured.contains("private-request-content"));
    assert!(!captured.contains("private-response-content"));
    assert_eq!(f.worker.tick().await.unwrap(), 1);
    assert_eq!(f.receipts().await, 1);
    assert_eq!(f.balance().await, Decimal::from(85));
    let aggregate: (i64, i64, Decimal) =
        sqlx::query_as("SELECT total_requests,total_tokens,total_amount FROM batch_aggregates WHERE fusillade_batch_id=$1")
            .bind(f.batch)
            .fetch_one(&f.main)
            .await
            .unwrap();
    assert_eq!(aggregate, (1, 15, Decimal::from(15)));
    let spend: Decimal = sqlx::query_scalar("SELECT total_spend FROM api_key_spend_checkpoints WHERE api_key_id=$1")
        .bind(f.key)
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(spend, Decimal::from(15));
    sqlx::query("UPDATE fusillade_billing_events SET processed_at=now()-interval '2 days' WHERE event_id=$1")
        .bind(event)
        .execute(&f.main)
        .await
        .unwrap();
    f.worker.cleanup().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fusillade_billing_events")
            .fetch_one(&f.main)
            .await
            .unwrap(),
        0
    );
    f.publish(event).await;
    assert_eq!(f.worker.tick().await.unwrap(), 1);
    assert_eq!(f.balance().await, Decimal::from(85));
    assert_eq!(f.receipts().await, 1);
    let ledger_count: i64 = sqlx::query_scalar("SELECT count(*) FROM credits_transactions WHERE source_id=$1")
        .bind(format!("durable-billing:{}", f.request.id.0))
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(ledger_count, 1);
    let aggregate_after: (i64, i64, Decimal) =
        sqlx::query_as("SELECT total_requests,total_tokens,total_amount FROM batch_aggregates WHERE fusillade_batch_id=$1")
            .bind(f.batch)
            .fetch_one(&f.main)
            .await
            .unwrap();
    assert_eq!(aggregate_after, aggregate);
}

#[sqlx::test]
async fn concurrent_workers_debit_and_fold_accepted_usage_once(pool: PgPool) {
    let f = Fixture::new(pool).await;
    f.completed().await;
    let second = BillingWorker::new(f.worker.main.clone(), f.worker.fusillade.clone(), create_test_config());
    let (a, b) = tokio::join!(f.worker.tick(), second.tick());
    a.unwrap();
    b.unwrap();
    assert_eq!(f.receipts().await, 1);
    assert_eq!(f.balance().await, Decimal::from(85));
    let requests: i64 = sqlx::query_scalar("SELECT total_requests FROM batch_aggregates WHERE fusillade_batch_id=$1")
        .bind(f.batch)
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(requests, 1);
}

#[sqlx::test]
async fn event_before_acceptance_is_retained_and_only_the_accepted_attempt_is_billed(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let discarded = Uuid::new_v4();
    f.publish(discarded).await;
    assert_eq!(f.worker.tick().await.unwrap(), 0);
    assert_eq!(f.receipts().await, 0);
    assert_eq!(f.balance().await, Decimal::from(100));
    let accepted = f.completed().await;
    f.due(discarded).await;
    assert_eq!(f.worker.tick().await.unwrap(), 2);
    let disposition: String = sqlx::query_scalar("SELECT disposition FROM fusillade_billing_events WHERE event_id=$1")
        .bind(discarded)
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(disposition, "unaccepted_attempt");
    let billed: Uuid = sqlx::query_scalar("SELECT event_id FROM billing_receipts WHERE request_id=$1")
        .bind(f.request.id.0)
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(billed, accepted);
    assert_eq!(f.balance().await, Decimal::from(85));
}

#[sqlx::test]
async fn acknowledgement_failure_rolls_back_receipt_ledger_balance_and_aggregates(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let event = f.completed().await;
    sqlx::raw_sql("CREATE FUNCTION reject_billing_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.processing_state='processed' THEN RAISE EXCEPTION 'injected acknowledgement failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_billing_ack BEFORE UPDATE ON fusillade_billing_events FOR EACH ROW EXECUTE FUNCTION reject_billing_ack();")
        .execute(&f.main).await.unwrap();
    assert_eq!(f.worker.tick().await.unwrap(), 0);
    assert_eq!(f.receipts().await, 0);
    assert_eq!(f.balance().await, Decimal::from(100));
    let effects: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM credits_transactions WHERE source_id=$1), (SELECT count(*) FROM batch_aggregates WHERE fusillade_batch_id=$2), (SELECT count(*) FROM http_analytics WHERE fusillade_request_id=$3)")
        .bind(format!("durable-billing:{}", f.request.id.0)).bind(f.batch).bind(f.request.id.0).fetch_one(&f.main).await.unwrap();
    assert_eq!(effects, (0, 0, 0));
    let cap_spend: Decimal =
        sqlx::query_scalar("SELECT COALESCE((SELECT total_spend FROM api_key_spend_checkpoints WHERE api_key_id=$1),0)")
            .bind(f.key)
            .fetch_one(&f.main)
            .await
            .unwrap();
    assert_eq!(cap_spend, Decimal::ZERO);
    let state: (String, String) = sqlx::query_as("SELECT processing_state,error_code FROM fusillade_billing_events WHERE event_id=$1")
        .bind(event)
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(state, ("pending".into(), "transaction_failed".into()));
    sqlx::query("DROP TRIGGER reject_billing_ack ON fusillade_billing_events")
        .execute(&f.main)
        .await
        .unwrap();
    f.due(event).await;
    assert_eq!(f.worker.tick().await.unwrap(), 1);
    assert_eq!(f.receipts().await, 1);
    assert_eq!(f.balance().await, Decimal::from(85));
}

#[sqlx::test]
async fn missing_usage_and_pricing_are_retained_without_charge(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let event = f.completed().await;
    sqlx::query("UPDATE fusillade_billing_events SET usage_present=false WHERE event_id=$1")
        .bind(event)
        .execute(&f.main)
        .await
        .unwrap();
    assert_eq!(f.worker.tick().await.unwrap(), 0);
    let code: String = sqlx::query_scalar("SELECT error_code FROM fusillade_billing_events WHERE event_id=$1")
        .bind(event)
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(code, "missing_usage");
    sqlx::query("UPDATE fusillade_billing_events SET usage_present=true,processing_state='pending',next_attempt_at=now(),batch_created_at='2000-01-01' WHERE event_id=$1")
        .bind(event).execute(&f.main).await.unwrap();
    assert_eq!(f.worker.tick().await.unwrap(), 0);
    let state: (String, String) = sqlx::query_as("SELECT processing_state,error_code FROM fusillade_billing_events WHERE event_id=$1")
        .bind(event)
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(state, ("unresolved".into(), "invalid_billing_inputs".into()));
    assert_eq!(f.receipts().await, 0);
    assert_eq!(f.balance().await, Decimal::from(100));
}

#[sqlx::test]
async fn admission_requires_live_worker_and_bounded_backlog_and_cleanup_retains_pending(pool: PgPool) {
    let f = Fixture::new(pool).await;
    assert!(!admission_ready(&f.worker.main, 60).await.unwrap());
    f.worker.tick().await.unwrap();
    assert!(admission_ready(&f.worker.main, 60).await.unwrap());
    let event = Uuid::new_v4();
    f.publish(event).await;
    sqlx::query("UPDATE fusillade_billing_events SET created_at=now()-interval '3 days' WHERE event_id=$1")
        .bind(event)
        .execute(&f.main)
        .await
        .unwrap();
    assert!(!admission_ready(&f.worker.main, 60).await.unwrap());
    f.worker.cleanup().await.unwrap();
    let retained: String = sqlx::query_scalar("SELECT processing_state FROM fusillade_billing_events WHERE event_id=$1")
        .bind(event)
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(retained, "pending");
    sqlx::query("UPDATE billing_worker_heartbeat SET last_seen_at=now()-interval '1 minute'")
        .execute(&f.main)
        .await
        .unwrap();
    assert!(!admission_ready(&f.worker.main, u64::MAX).await.unwrap());
}

#[sqlx::test]
async fn zero_price_accepted_usage_receives_receipt_and_one_aggregate_fold(pool: PgPool) {
    let f = Fixture::new(pool).await;
    sqlx::query("UPDATE model_tariffs SET input_price_per_token=0,output_price_per_token=0 WHERE deployed_model_id=(SELECT id FROM deployed_models WHERE alias='durable-worker-model')")
        .execute(&f.main).await.unwrap();
    f.completed().await;
    assert_eq!(f.worker.tick().await.unwrap(), 1);
    assert_eq!(f.receipts().await, 1);
    assert_eq!(f.balance().await, Decimal::from(100));
    let aggregate: (i64, i64, Decimal) =
        sqlx::query_as("SELECT total_requests,total_tokens,total_amount FROM batch_aggregates WHERE fusillade_batch_id=$1")
            .bind(f.batch)
            .fetch_one(&f.main)
            .await
            .unwrap();
    assert_eq!(aggregate, (1, 15, Decimal::ZERO));
    let debits: i64 = sqlx::query_scalar("SELECT count(*) FROM credits_transactions WHERE source_id=$1")
        .bind(format!("durable-billing:{}", f.request.id.0))
        .fetch_one(&f.main)
        .await
        .unwrap();
    assert_eq!(debits, 0);
}

#[sqlx::test]
async fn expired_worker_lease_is_recovered_without_stealing_a_live_lease(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let event = f.completed().await;
    sqlx::query("UPDATE fusillade_billing_events SET lease_owner=$2,lease_until=now()+interval '1 minute' WHERE event_id=$1")
        .bind(event)
        .bind(Uuid::new_v4())
        .execute(&f.main)
        .await
        .unwrap();
    assert_eq!(f.worker.tick().await.unwrap(), 0);
    assert_eq!(f.receipts().await, 0);
    sqlx::query("UPDATE fusillade_billing_events SET lease_until=now()-interval '1 second' WHERE event_id=$1")
        .bind(event)
        .execute(&f.main)
        .await
        .unwrap();
    assert_eq!(f.worker.tick().await.unwrap(), 1);
    assert_eq!(f.receipts().await, 1);
    assert_eq!(f.balance().await, Decimal::from(85));
}

#[sqlx::test]
async fn heartbeat_refreshes_while_the_work_poll_interval_is_one_minute(pool: PgPool) {
    let f = Fixture::new(pool).await;
    sqlx::raw_sql("CREATE TABLE heartbeat_test_updates (id BIGSERIAL); CREATE FUNCTION record_heartbeat_test_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO heartbeat_test_updates DEFAULT VALUES; RETURN NEW; END $$; CREATE TRIGGER heartbeat_test_update AFTER INSERT OR UPDATE ON billing_worker_heartbeat FOR EACH ROW EXECUTE FUNCTION record_heartbeat_test_update();")
        .execute(&f.main).await.unwrap();
    let mut config = create_test_config();
    config.analytics.durable_billing.poll_interval_ms = 60_000;
    let worker = BillingWorker::new(f.worker.main.clone(), f.worker.fusillade.clone(), config);
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    // SQL uses real I/O. Keep the runtime runnable so Tokio cannot automatically
    // advance its paused clock past a pool/statement timeout while I/O arrives.
    let runnable = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    tokio::time::pause();
    let task = tokio::spawn(async move { worker.run(stop).await });
    let real_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let writes: i64 = sqlx::query_scalar("SELECT count(*) FROM heartbeat_test_updates")
            .fetch_one(&f.main)
            .await
            .unwrap();
        // Both initial heartbeats (health loop and work tick) have committed;
        // a later refresh therefore cannot be a delayed startup write.
        if writes >= 2 {
            break;
        }
        assert!(std::time::Instant::now() < real_deadline, "worker startup did not finish");
        // Drive the initial timer bucket explicitly; the runnable task disables
        // automatic advancement while PostgreSQL I/O is in flight.
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
    }
    sqlx::query("UPDATE billing_worker_heartbeat SET last_seen_at=now()-interval '1 minute'")
        .execute(&f.main)
        .await
        .unwrap();
    assert!(!admission_ready(&f.worker.main, 60).await.unwrap());
    tokio::time::advance(Duration::from_secs(6)).await;
    let real_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if admission_ready(&f.worker.main, 60).await.unwrap() {
            break;
        }
        assert!(
            std::time::Instant::now() < real_deadline,
            "health signal waited for the 60-second work interval"
        );
        tokio::task::yield_now().await;
    }
    shutdown.cancel();
    tokio::time::resume();
    task.await.unwrap().unwrap();
    runnable.abort();
}

#[sqlx::test]
async fn accepted_usage_can_bill_after_request_content_is_erased(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let event = f.completed().await;
    sqlx::query("DELETE FROM requests WHERE id=$1")
        .bind(f.request.id.0)
        .execute(&*f.worker.fusillade.write())
        .await
        .unwrap();
    let accepted: Uuid = sqlx::query_scalar("SELECT accepted_event_id FROM billing_acceptances WHERE request_id=$1")
        .bind(f.request.id.0)
        .fetch_one(&*f.worker.fusillade.write())
        .await
        .unwrap();
    assert_eq!(accepted, event);
    assert_eq!(f.worker.tick().await.unwrap(), 1);
    assert_eq!(f.receipts().await, 1);
    assert_eq!(f.balance().await, Decimal::from(85));
}
