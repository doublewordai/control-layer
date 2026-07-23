//! Integration tests for the [`RequestProcessor`] daemon hook.
//!
//! Covers:
//! - The default processor preserves today's batch path (a request claimed by
//!   the daemon completes via the same mock HTTP fixture that drove the
//!   pre-hook integration tests).
//! - A custom processor is invoked exactly once per claimed request, with
//!   the daemon's storage/http/cancellation parameters threaded through.
//!
//! These tests guard against accidental regressions in the spawn-task wiring.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use fusillade::FusilladeError;
use fusillade::PostgresDaemon;
use fusillade::batch::{BatchInput, RequestTemplateInput};
use fusillade::daemon::{DaemonConfig, default_should_retry};
use fusillade::http::{HttpClient, HttpResponse, MockHttpClient};
use fusillade::manager::{ModelFilter, ModelFilterState, Storage};
use fusillade::processor::{
    CancellationFuture, DefaultRequestProcessor, RequestProcessor, ShouldRetry,
};
use fusillade::request::{
    AnyRequest, Claimed, Completed, DaemonId, Failed, Request, RequestCompletionResult, RequestData,
};
use fusillade_arsenal::{PostgresRequestManager as PostgresStore, TestDbPools};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type TestStore = PostgresStore<TestDbPools>;
type TestDaemon = PostgresDaemon<TestDbPools, MockHttpClient>;

const PROCESSING_LOCK_KEY: i64 = 7_204_411_063;

fn fast_test_config() -> DaemonConfig {
    let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
    model_concurrency_limits.insert("test-model".to_string(), 10);
    DaemonConfig {
        claim_batch_size: 10,
        claim_interval_ms: 10,
        model_concurrency_limits,
        max_retries: Some(3),
        backoff_ms: 100,
        backoff_factor: 2,
        max_backoff_ms: 1000,
        status_log_interval_ms: None,
        heartbeat_interval_ms: 10000,
        should_retry: Arc::new(default_should_retry),
        claim_timeout_ms: 60000,
        processing_timeout_ms: 600000,
        cancellation_poll_interval_ms: 100,
        ..Default::default()
    }
}

async fn submit_one_request(manager: &Arc<TestStore>) -> fusillade::request::RequestId {
    let template = RequestTemplateInput {
        custom_id: None,
        endpoint: "http://upstream".into(),
        method: "POST".into(),
        path: "/v1/test".into(),
        body: r#"{"hello":"world"}"#.into(),
        model: "test-model".into(),
        api_key: "test-key".into(),
    };
    let file_id = manager
        .create_file("test_file".into(), None, vec![template])
        .await
        .unwrap();
    let batch = manager
        .create_batch(BatchInput {
            file_id,
            endpoint: "/v1/test".into(),
            completion_window: "24h".into(),
            metadata: None,
            created_by: Some("tester".into()),
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .unwrap();
    manager
        .append_model_filter_event(&ModelFilter {
            model: "test-model".to_string(),
            state: ModelFilterState::Live,
            expected_ready_at: None,
        })
        .await
        .unwrap();
    let requests = manager.get_batch_requests(batch.id).await.unwrap();
    requests[0].id()
}

async fn wait_until_completed(manager: &Arc<TestStore>, request_id: fusillade::request::RequestId) {
    let start = std::time::Instant::now();
    loop {
        let req = fetch_any_request(manager, request_id).await;
        let variant = req.variant();
        if variant == "Completed" || variant == "Failed" {
            return;
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!(
                "Timeout waiting for request to reach terminal state, last={:?}",
                variant
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_until_retry_pending(
    manager: &Arc<TestStore>,
    request_id: fusillade::request::RequestId,
) {
    let start = std::time::Instant::now();
    loop {
        if matches!(
            fetch_any_request(manager, request_id).await,
            AnyRequest::Pending(ref request) if request.state.retry_attempt == 1
        ) {
            return;
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!("timeout waiting for retry to return to pending");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn fetch_any_request(
    manager: &Arc<TestStore>,
    request_id: fusillade::request::RequestId,
) -> AnyRequest {
    manager
        .get_requests(vec![request_id])
        .await
        .expect("get_requests")
        .into_iter()
        .next()
        .expect("one result")
        .expect("request found")
}

async fn postgres_store(pool: sqlx::PgPool, config: &DaemonConfig) -> Arc<TestStore> {
    Arc::new(PostgresStore::new(
        TestDbPools::new(pool).await.unwrap(),
        config.into(),
    ))
}

async fn install_processing_transition_blocker(pool: &sqlx::PgPool) {
    sqlx::query(
        r#"
        CREATE FUNCTION block_processing_transition() RETURNS trigger AS $$
        BEGIN
            IF NEW.state = 'processing' AND OLD.state = 'claimed' THEN
                PERFORM pg_advisory_xact_lock(7204411063);
            END IF;
            RETURN NEW;
        END;
        $$ LANGUAGE plpgsql;
        "#,
    )
    .execute(pool)
    .await
    .expect("install processing transition blocker function");
    sqlx::query(
        r#"
        CREATE TRIGGER block_processing_transition
        BEFORE UPDATE ON requests
        FOR EACH ROW EXECUTE FUNCTION block_processing_transition()
        "#,
    )
    .execute(pool)
    .await
    .expect("install processing transition blocker trigger");
}

async fn wait_for_processing_transition_blocked(pool: &sqlx::PgPool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM pg_stat_activity
                    WHERE datname = current_database()
                      AND state = 'active'
                      AND wait_event_type = 'Lock'
                      AND wait_event = 'advisory'
                )
                "#,
            )
            .fetch_one(pool)
            .await
            .expect("inspect blocked processing transition");
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("processing transition never reached the database lock");
}

async fn install_transient_terminal_write_fault(pool: &sqlx::PgPool, failures: u32) {
    sqlx::query("CREATE SEQUENCE terminal_write_fault_seq")
        .execute(pool)
        .await
        .expect("create terminal fault sequence");
    let function = format!(
        r#"
        CREATE FUNCTION fail_terminal_write() RETURNS trigger AS $$
        BEGIN
            IF NEW.state = 'completed'
               AND OLD.state = 'processing'
               AND nextval('terminal_write_fault_seq') <= {failures}
            THEN
                RAISE EXCEPTION USING
                    ERRCODE = '40001',
                    MESSAGE = 'synthetic transient terminal write fault';
            END IF;
            RETURN NEW;
        END;
        $$ LANGUAGE plpgsql
        "#
    );
    sqlx::query(&function)
        .execute(pool)
        .await
        .expect("install terminal fault function");
    sqlx::query(
        r#"
        CREATE TRIGGER fail_terminal_write
        BEFORE UPDATE ON requests
        FOR EACH ROW EXECUTE FUNCTION fail_terminal_write()
        "#,
    )
    .execute(pool)
    .await
    .expect("install terminal fault trigger");
}

async fn install_definitive_completed_write_fault(pool: &sqlx::PgPool) {
    sqlx::query(
        r#"
        CREATE FUNCTION reject_completed_write() RETURNS trigger AS $$
        BEGIN
            IF NEW.state = 'completed' AND OLD.state = 'processing' THEN
                RAISE EXCEPTION USING
                    ERRCODE = '23514',
                    MESSAGE = 'synthetic definitive completed-write fault';
            END IF;
            RETURN NEW;
        END;
        $$ LANGUAGE plpgsql
        "#,
    )
    .execute(pool)
    .await
    .expect("install definitive completed-write fault function");
    sqlx::query(
        r#"
        CREATE TRIGGER reject_completed_write
        BEFORE UPDATE ON requests
        FOR EACH ROW EXECUTE FUNCTION reject_completed_write()
        "#,
    )
    .execute(pool)
    .await
    .expect("install definitive completed-write fault trigger");
}

struct DispatchProbe {
    polls: Arc<AtomicUsize>,
    dropped: Option<oneshot::Sender<()>>,
}

impl Future for DispatchProbe {
    type Output = fusillade::Result<HttpResponse>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Ok(HttpResponse {
            status: 200,
            body: "{}".into(),
        }))
    }
}

impl Drop for DispatchProbe {
    fn drop(&mut self) {
        if let Some(dropped) = self.dropped.take() {
            let _ = dropped.send(());
        }
    }
}

fn postgres_daemon(
    store: Arc<TestStore>,
    http_client: Arc<MockHttpClient>,
    config: DaemonConfig,
) -> TestDaemon {
    PostgresDaemon::new(store, http_client, config)
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn default_processor_preserves_batch_path(pool: sqlx::PgPool) {
    let http_client = Arc::new(MockHttpClient::new());
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"ok":true}"#.into(),
        }),
    );

    let shutdown_token = CancellationToken::new();
    let config = fast_test_config();
    let manager = postgres_store(pool, &config).await;
    let daemon = Arc::new(postgres_daemon(
        manager.clone(),
        http_client.clone(),
        config,
    ));
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let request_id = submit_one_request(&manager).await;
    wait_until_completed(&manager, request_id).await;

    let AnyRequest::Completed(req) = fetch_any_request(&manager, request_id).await else {
        panic!("expected Completed variant");
    };
    assert!(req.state.response_body.contains("\"ok\":true"));

    shutdown_token.cancel();
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn terminal_write_retry_retains_outcome_without_reinvoking_provider(pool: sqlx::PgPool) {
    install_transient_terminal_write_fault(&pool, 3).await;

    let http_client = Arc::new(MockHttpClient::new());
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"provider_call":1}"#.into(),
        }),
    );

    let shutdown_token = CancellationToken::new();
    let config = fast_test_config();
    let manager = postgres_store(pool.clone(), &config).await;
    let daemon = Arc::new(postgres_daemon(
        manager.clone(),
        http_client.clone(),
        config,
    ));
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let request_id = submit_one_request(&manager).await;
    wait_until_completed(&manager, request_id).await;

    assert_eq!(
        http_client.call_count(),
        1,
        "daemon durability retries must retain the provider outcome"
    );
    let fault_calls: i64 = sqlx::query_scalar("SELECT last_value FROM terminal_write_fault_seq")
        .fetch_one(&pool)
        .await
        .expect("read terminal fault count");
    assert_eq!(
        fault_calls, 4,
        "three inner SQL failures must force one outer daemon durability retry"
    );
    let AnyRequest::Completed(completed) = fetch_any_request(&manager, request_id).await else {
        panic!("expected completed request");
    };
    assert_eq!(completed.state.response_body, r#"{"provider_call":1}"#);

    shutdown_token.cancel();
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn definitive_terminal_preparation_failure_uses_redacted_fallback(pool: sqlx::PgPool) {
    install_definitive_completed_write_fault(&pool).await;

    let http_client = Arc::new(MockHttpClient::new());
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: "must not be redispatched".into(),
        }),
    );
    let shutdown_token = CancellationToken::new();
    let config = fast_test_config();
    let manager = postgres_store(pool, &config).await;
    let daemon = Arc::new(postgres_daemon(
        manager.clone(),
        http_client.clone(),
        config,
    ));
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let request_id = submit_one_request(&manager).await;
    wait_until_completed(&manager, request_id).await;

    assert_eq!(http_client.call_count(), 1);
    let AnyRequest::Failed(failed) = fetch_any_request(&manager, request_id).await else {
        panic!("expected redacted terminal fallback");
    };
    assert!(matches!(
        failed.state.reason,
        fusillade::request::FailureReason::RequestBuilderError { .. }
    ));
    assert!(
        !failed
            .state
            .reason
            .to_error_message()
            .contains("must not be redispatched")
    );
    shutdown_token.cancel();
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn retried_requests_reacquire_model_admission(pool: sqlx::PgPool) {
    let http_client = Arc::new(MockHttpClient::new());
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 500,
            body: "retry".into(),
        }),
    );
    let blocking_second_call = http_client.add_response_with_trigger(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: "second request".into(),
        }),
    );
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: "retried request".into(),
        }),
    );

    let shutdown_token = CancellationToken::new();
    let mut config = fast_test_config();
    config
        .model_concurrency_limits
        .insert("test-model".to_string(), 1);
    config.backoff_ms = 50;
    config.max_backoff_ms = 50;
    let manager = postgres_store(pool, &config).await;
    let daemon = Arc::new(postgres_daemon(
        manager.clone(),
        http_client.clone(),
        config,
    ));
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let first = submit_one_request(&manager).await;
    wait_until_retry_pending(&manager, first).await;
    let second = submit_one_request(&manager).await;

    tokio::time::timeout(Duration::from_secs(5), async {
        while http_client.call_count() < 2 || http_client.in_flight_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second request never occupied the sole model slot");

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        http_client.call_count(),
        2,
        "eligible retry must not dispatch while another request holds model admission"
    );
    assert_eq!(http_client.in_flight_count(), 1);

    blocking_second_call
        .send(())
        .expect("release second provider call");
    wait_until_completed(&manager, second).await;
    wait_until_completed(&manager, first).await;
    assert_eq!(http_client.call_count(), 3);

    shutdown_token.cancel();
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn processing_state_is_durable_before_http_dispatch(pool: sqlx::PgPool) {
    let config = fast_test_config();
    let manager = postgres_store(pool.clone(), &config).await;
    let request_id = submit_one_request(&manager).await;

    let AnyRequest::Pending(pending) = fetch_any_request(&manager, request_id).await else {
        panic!("expected Pending variant");
    };
    let claimed = pending
        .claim(DaemonId::from(Uuid::new_v4()), &*manager)
        .await
        .expect("claim request");

    install_processing_transition_blocker(&pool).await;

    let mut blocker = pool.acquire().await.expect("acquire blocker connection");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(PROCESSING_LOCK_KEY)
        .execute(&mut *blocker)
        .await
        .expect("hold processing transition lock");

    let http_polls = Arc::new(AtomicUsize::new(0));
    let process_handle = {
        let manager = manager.clone();
        let http_polls = http_polls.clone();
        tokio::spawn(async move {
            claimed
                .process(&*manager, async move {
                    http_polls.fetch_add(1, Ordering::SeqCst);
                    Ok(HttpResponse {
                        status: 200,
                        body: "{}".into(),
                    })
                })
                .await
        })
    };

    wait_for_processing_transition_blocked(&pool).await;

    assert_eq!(
        http_polls.load(Ordering::SeqCst),
        0,
        "downstream HTTP must not start before Processing is durable"
    );

    let admitted_after = Utc::now();
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(PROCESSING_LOCK_KEY)
        .execute(&mut *blocker)
        .await
        .expect("release processing transition lock");

    let processing = process_handle
        .await
        .expect("processing task joined")
        .expect("processing transition succeeded");
    assert!(
        processing.state.started_at >= admitted_after,
        "in-memory processing timeout must start after storage admission"
    );
    let durable_started_at: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT started_at FROM requests WHERE id = $1")
            .bind(*processing.data.id)
            .fetch_one(&pool)
            .await
            .expect("read durable processing timestamp");
    assert!(
        durable_started_at.expect("processing timestamp is set") >= admitted_after,
        "durable processing timeout must start after the database wait"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while http_polls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("HTTP task was not dispatched after Processing became durable");
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn canceling_queued_processing_transition_does_not_dispatch_http(pool: sqlx::PgPool) {
    let config = fast_test_config();
    let manager = postgres_store(pool.clone(), &config).await;
    let request_id = submit_one_request(&manager).await;

    let AnyRequest::Pending(pending) = fetch_any_request(&manager, request_id).await else {
        panic!("expected Pending variant");
    };
    let claimed = pending
        .claim(DaemonId::from(Uuid::new_v4()), &*manager)
        .await
        .expect("claim request");

    install_processing_transition_blocker(&pool).await;
    let mut blocker = pool.acquire().await.expect("acquire blocker connection");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(PROCESSING_LOCK_KEY)
        .execute(&mut *blocker)
        .await
        .expect("hold processing transition lock");

    let http_polls = Arc::new(AtomicUsize::new(0));
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let process_handle = {
        let manager = manager.clone();
        let response_fut = DispatchProbe {
            polls: http_polls.clone(),
            dropped: Some(dropped_tx),
        };
        tokio::spawn(async move { claimed.process(&*manager, response_fut).await })
    };

    wait_for_processing_transition_blocked(&pool).await;
    process_handle.abort();
    assert!(
        process_handle
            .await
            .expect_err("processing task should be canceled")
            .is_cancelled()
    );
    tokio::time::timeout(Duration::from_secs(2), dropped_rx)
        .await
        .expect("gated HTTP task did not terminate")
        .expect("dispatch probe drop signal was canceled");

    assert_eq!(
        http_polls.load(Ordering::SeqCst),
        0,
        "canceling a queued transition must leave the HTTP future unpolled"
    );

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(PROCESSING_LOCK_KEY)
        .execute(&mut *blocker)
        .await
        .expect("release processing transition lock");
}

/// Counting processor: wraps DefaultRequestProcessor and records how many
/// times it was invoked. Validates that the daemon's spawn task actually
/// dispatches through the trait object.
struct CountingProcessor {
    invocations: Arc<AtomicUsize>,
    inner: DefaultRequestProcessor,
}

#[async_trait]
impl<S, H> RequestProcessor<S, H> for CountingProcessor
where
    S: Storage + Sync,
    H: HttpClient + 'static,
{
    async fn process(
        &self,
        request: Request<Claimed>,
        http: H,
        storage: &S,
        should_retry: ShouldRetry,
        cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        self.inner
            .process(request, http, storage, should_retry, cancellation)
            .await
    }
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn custom_processor_is_invoked(pool: sqlx::PgPool) {
    let http_client = Arc::new(MockHttpClient::new());
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: "{}".into(),
        }),
    );

    let invocations = Arc::new(AtomicUsize::new(0));
    let processor = Arc::new(CountingProcessor {
        invocations: invocations.clone(),
        inner: DefaultRequestProcessor,
    });

    let shutdown_token = CancellationToken::new();
    let config = fast_test_config();
    let manager = postgres_store(pool, &config).await;
    let daemon = Arc::new(
        postgres_daemon(manager.clone(), http_client.clone(), config).with_processor(processor),
    );
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let request_id = submit_one_request(&manager).await;
    wait_until_completed(&manager, request_id).await;

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "processor must be invoked exactly once for a single claimed request"
    );
    assert!(matches!(
        fetch_any_request(&manager, request_id).await,
        AnyRequest::Completed(_)
    ));

    shutdown_token.cancel();
}

struct PanicOnceProcessor {
    invocations: Arc<AtomicUsize>,
    inner: DefaultRequestProcessor,
}

#[async_trait]
impl<S, H> RequestProcessor<S, H> for PanicOnceProcessor
where
    S: Storage + Sync,
    H: HttpClient + 'static,
{
    async fn process(
        &self,
        request: Request<Claimed>,
        http: H,
        storage: &S,
        should_retry: ShouldRetry,
        cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        if self.invocations.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("synthetic processor panic");
        }
        self.inner
            .process(request, http, storage, should_retry, cancellation)
            .await
    }
}

struct ErrorOnceProcessor {
    invocations: Arc<AtomicUsize>,
    inner: DefaultRequestProcessor,
}

#[async_trait]
impl<S, H> RequestProcessor<S, H> for ErrorOnceProcessor
where
    S: Storage + Sync,
    H: HttpClient + 'static,
{
    async fn process(
        &self,
        request: Request<Claimed>,
        http: H,
        storage: &S,
        should_retry: ShouldRetry,
        cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        if self.invocations.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(FusilladeError::Other(anyhow::anyhow!(
                "synthetic processor error"
            )));
        }
        self.inner
            .process(request, http, storage, should_retry, cancellation)
            .await
    }
}

struct ValidationErrorProcessor {
    invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl<S, H> RequestProcessor<S, H> for ValidationErrorProcessor
where
    S: Storage + Sync,
    H: HttpClient + 'static,
{
    async fn process(
        &self,
        _request: Request<Claimed>,
        _http: H,
        _storage: &S,
        _should_retry: ShouldRetry,
        _cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Err(FusilladeError::ValidationError(
            "sensitive validation detail".to_string(),
        ))
    }
}

struct InvalidStateProcessor {
    invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl<S, H> RequestProcessor<S, H> for InvalidStateProcessor
where
    S: Storage + Sync,
    H: HttpClient + 'static,
{
    async fn process(
        &self,
        request: Request<Claimed>,
        _http: H,
        _storage: &S,
        _should_retry: ShouldRetry,
        _cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Err(FusilladeError::InvalidState(
            request.data.id,
            "claimed".to_string(),
            "processing".to_string(),
        ))
    }
}

struct PendingProviderWork {
    started: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl Future for PendingProviderWork {
    type Output = fusillade::Result<HttpResponse>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.started.store(1, Ordering::SeqCst);
        Poll::Pending
    }
}

impl Drop for PendingProviderWork {
    fn drop(&mut self) {
        self.dropped.store(1, Ordering::SeqCst);
    }
}

struct ErrorAfterProcessingOnceProcessor {
    invocations: Arc<AtomicUsize>,
    provider_started: Arc<AtomicUsize>,
    provider_dropped: Arc<AtomicUsize>,
    inner: DefaultRequestProcessor,
}

#[async_trait]
impl<S, H> RequestProcessor<S, H> for ErrorAfterProcessingOnceProcessor
where
    S: Storage + Sync,
    H: HttpClient + 'static,
{
    async fn process(
        &self,
        request: Request<Claimed>,
        http: H,
        storage: &S,
        should_retry: ShouldRetry,
        cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        if self.invocations.fetch_add(1, Ordering::SeqCst) == 0 {
            let _processing = request
                .process(
                    storage,
                    PendingProviderWork {
                        started: self.provider_started.clone(),
                        dropped: self.provider_dropped.clone(),
                    },
                )
                .await?;
            tokio::time::timeout(Duration::from_secs(2), async {
                while self.provider_started.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("provider work was never dispatched");
            return Err(FusilladeError::Other(anyhow::anyhow!(
                "synthetic processor error after durable processing admission"
            )));
        }
        self.inner
            .process(request, http, storage, should_retry, cancellation)
            .await
    }
}

#[derive(Clone)]
struct ProviderTaskPanicOnce {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl HttpClient for ProviderTaskPanicOnce {
    async fn execute(
        &self,
        _request: &RequestData,
        _api_key: &str,
    ) -> fusillade::Result<HttpResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("synthetic provider task panic");
        }
        Ok(HttpResponse {
            status: 200,
            body: "recovered".to_string(),
        })
    }
}

async fn assert_recovered_processor_completes(
    pool: sqlx::PgPool,
    processor: Arc<dyn RequestProcessor<TestStore, MockHttpClient>>,
    invocations: Arc<AtomicUsize>,
) {
    let http_client = Arc::new(MockHttpClient::new());
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: "recovered".into(),
        }),
    );
    let shutdown_token = CancellationToken::new();
    let mut config = fast_test_config();
    config.backoff_ms = 10;
    config.max_backoff_ms = 10;
    let manager = postgres_store(pool, &config).await;
    let daemon =
        Arc::new(postgres_daemon(manager.clone(), http_client, config).with_processor(processor));
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let request_id = submit_one_request(&manager).await;
    wait_until_completed(&manager, request_id).await;
    assert!(matches!(
        fetch_any_request(&manager, request_id).await,
        AnyRequest::Completed(_)
    ));
    assert_eq!(invocations.load(Ordering::SeqCst), 2);
    shutdown_token.cancel();
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn processor_panic_is_recovered_as_a_new_admitted_attempt(pool: sqlx::PgPool) {
    let invocations = Arc::new(AtomicUsize::new(0));
    assert_recovered_processor_completes(
        pool,
        Arc::new(PanicOnceProcessor {
            invocations: invocations.clone(),
            inner: DefaultRequestProcessor,
        }),
        invocations,
    )
    .await;
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn processor_error_is_recovered_as_a_new_admitted_attempt(pool: sqlx::PgPool) {
    let invocations = Arc::new(AtomicUsize::new(0));
    assert_recovered_processor_completes(
        pool,
        Arc::new(ErrorOnceProcessor {
            invocations: invocations.clone(),
            inner: DefaultRequestProcessor,
        }),
        invocations,
    )
    .await;
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn processor_error_after_processing_cancels_provider_before_retry(pool: sqlx::PgPool) {
    let invocations = Arc::new(AtomicUsize::new(0));
    let provider_started = Arc::new(AtomicUsize::new(0));
    let provider_dropped = Arc::new(AtomicUsize::new(0));
    assert_recovered_processor_completes(
        pool,
        Arc::new(ErrorAfterProcessingOnceProcessor {
            invocations: invocations.clone(),
            provider_started: provider_started.clone(),
            provider_dropped: provider_dropped.clone(),
            inner: DefaultRequestProcessor,
        }),
        invocations,
    )
    .await;
    assert_eq!(provider_started.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider_dropped.load(Ordering::SeqCst),
        1,
        "dropping the processor's Processing value must cancel live provider work"
    );
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn processor_validation_error_terminalizes_without_retry(pool: sqlx::PgPool) {
    let invocations = Arc::new(AtomicUsize::new(0));
    let http_client = Arc::new(MockHttpClient::new());
    let shutdown_token = CancellationToken::new();
    let config = fast_test_config();
    let manager = postgres_store(pool, &config).await;
    let daemon = Arc::new(
        postgres_daemon(manager.clone(), http_client.clone(), config).with_processor(Arc::new(
            ValidationErrorProcessor {
                invocations: invocations.clone(),
            },
        )),
    );
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let request_id = submit_one_request(&manager).await;
    wait_until_completed(&manager, request_id).await;
    let AnyRequest::Failed(failed) = fetch_any_request(&manager, request_id).await else {
        panic!("expected failed request");
    };
    assert!(matches!(
        failed.state.reason,
        fusillade::request::FailureReason::RequestBuilderError { .. }
    ));
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert_eq!(http_client.call_count(), 0);
    shutdown_token.cancel();
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn processor_invalid_state_terminalizes_without_retry(pool: sqlx::PgPool) {
    let invocations = Arc::new(AtomicUsize::new(0));
    let http_client = Arc::new(MockHttpClient::new());
    let shutdown_token = CancellationToken::new();
    let config = fast_test_config();
    let manager = postgres_store(pool, &config).await;
    let daemon = Arc::new(
        postgres_daemon(manager.clone(), http_client.clone(), config).with_processor(Arc::new(
            InvalidStateProcessor {
                invocations: invocations.clone(),
            },
        )),
    );
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let request_id = submit_one_request(&manager).await;
    wait_until_completed(&manager, request_id).await;
    let AnyRequest::Failed(failed) = fetch_any_request(&manager, request_id).await else {
        panic!("expected failed request");
    };
    assert!(matches!(
        failed.state.reason,
        fusillade::request::FailureReason::RequestBuilderError { .. }
    ));
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert_eq!(http_client.call_count(), 0);
    shutdown_token.cancel();
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn terminated_provider_task_stays_unpersisted_until_retry_policy_runs(pool: sqlx::PgPool) {
    let calls = Arc::new(AtomicUsize::new(0));
    let http_client = Arc::new(ProviderTaskPanicOnce {
        calls: calls.clone(),
    });
    let shutdown_token = CancellationToken::new();
    let mut config = fast_test_config();
    config.backoff_ms = 10;
    config.max_backoff_ms = 10;
    let manager = postgres_store(pool, &config).await;
    let daemon = Arc::new(PostgresDaemon::new(manager.clone(), http_client, config));
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let request_id = submit_one_request(&manager).await;
    wait_until_completed(&manager, request_id).await;
    assert!(matches!(
        fetch_any_request(&manager, request_id).await,
        AnyRequest::Completed(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    shutdown_token.cancel();
}

/// Synthesizes a hard failure: validates that the processor's terminal-state
/// outcome is honored by the daemon's downstream metric/retry logic.
struct AlwaysFailProcessor;

#[async_trait]
impl<S, H> RequestProcessor<S, H> for AlwaysFailProcessor
where
    S: Storage + Sync,
    H: HttpClient + 'static,
{
    async fn process(
        &self,
        request: Request<Claimed>,
        _http: H,
        _storage: &S,
        _should_retry: ShouldRetry,
        _cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        // Synthesize a non-retriable failure so the daemon persists the
        // terminal state and we can observe it via the manager.
        let model = request.data.model.clone();
        let retry_attempt = request.state.retry_attempt;
        let batch_expires_at = request.state.batch_expires_at;
        let failed = Request {
            data: request.data,
            state: Failed {
                reason: fusillade::request::FailureReason::NonRetriableHttpStatus {
                    status: 400,
                    body: "synthetic test failure".into(),
                },
                failed_at: chrono::Utc::now(),
                retry_attempt,
                batch_expires_at,
                routed_model: model,
            },
        };
        Ok(RequestCompletionResult::Failed(failed))
    }
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn custom_processor_can_synthesize_terminal_failure(pool: sqlx::PgPool) {
    // No HTTP fixture needed — the processor never calls upstream.
    let http_client = Arc::new(MockHttpClient::new());

    let shutdown_token = CancellationToken::new();
    let config = fast_test_config();
    let manager = postgres_store(pool, &config).await;
    let daemon = Arc::new(
        postgres_daemon(manager.clone(), http_client.clone(), config)
            .with_processor(Arc::new(AlwaysFailProcessor)),
    );
    let _handle = daemon.run(shutdown_token.clone()).unwrap();

    let request_id = submit_one_request(&manager).await;
    wait_until_completed(&manager, request_id).await;

    let AnyRequest::Failed(req) = fetch_any_request(&manager, request_id).await else {
        panic!("expected Failed variant");
    };

    // The synthesized failure reason still carries the upstream body (it is
    // persisted), proving the custom processor's terminal outcome propagated.
    match &req.state.reason {
        fusillade::request::FailureReason::NonRetriableHttpStatus { status, body } => {
            assert_eq!(*status, 400, "synthesized status should propagate");
            assert_eq!(
                body, "synthetic test failure",
                "synthesized body should propagate"
            );
        }
        other => panic!("expected NonRetriableHttpStatus, got {other:?}"),
    }

    // ZDR: to_error_message() is scrubbed — it reports the status but must never
    // echo the provider response body (see COR-498).
    let err = req.state.reason.to_error_message();
    assert!(
        err.contains("400"),
        "status should appear in error message: {err}"
    );
    assert!(
        !err.contains("synthetic test failure"),
        "ZDR: provider body must not appear in to_error_message(): {err}"
    );

    shutdown_token.cancel();
}

// Compile-time check: the typestate Completed variant of RequestCompletionResult
// is reachable from our trait, ensuring downstream consumers can construct any
// terminal state if they need to.
#[allow(dead_code)]
fn _smoke_check_completed_variant(c: Request<Completed>) -> RequestCompletionResult {
    RequestCompletionResult::Completed(c)
}
