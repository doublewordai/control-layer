//! End-to-end test: the fusillade daemon picks up a `/v1/responses` row
//! and runs it through the multi-step orchestration loop.
//!
//! Differs from `multi_step_executor` by going through the *daemon claim
//! path* — `PostgresRequestManager` is started with the
//! `DwctlRequestProcessor` attached via `set_processor`, the test inserts
//! a `pending` request row, and the test polls until the row reaches
//! `completed` with the assembled response body.
//!
//! This exercises the wiring that `Application::new_with_pool` does in
//! production. A regression in the processor attachment, the loop
//! invocation, or the assembly path will surface here as a stuck or
//! mis-completed request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fusillade::{
    BatchInput, PostgresRequestManager, PostgresResponseStepManager, RequestId,
    RequestTemplateInput, ReqwestHttpClient, TestDbPools,
};
use fusillade::manager::{DaemonExecutor, Storage};
use onwards::client::HttpClient;
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::responses::processor::DwctlRequestProcessor;
use crate::responses::store::FusilladeResponseStore;
use crate::tool_executor::{HttpToolExecutor, ResolvedToolSet, ResolvedTools, ToolDefinition};

use crate::test::utils::setup_fusillade_pool;

#[sqlx::test]
async fn daemon_claim_runs_multi_step_loop_end_to_end(pool: PgPool) {
    // Wiremocks for upstream model + tool.
    let model_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "echo_args", "arguments": "{\"x\":42}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&model_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "the answer is 42"},
                "finish_reason": "stop"
            }]
        })))
        .mount(&model_server)
        .await;

    let tool_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"echoed": {"x": 42}})))
        .mount(&tool_server)
        .await;

    let pool = setup_fusillade_pool(&pool).await;
    let test_pools = TestDbPools::new(pool.clone()).await.unwrap();

    // Build the request_manager exactly the way Application::new_with_pool
    // does. Aggressive claim interval so the test doesn't sit waiting.
    let mut model_concurrency = HashMap::new();
    model_concurrency.insert("gpt-4o".to_string(), 5);
    let model_concurrency: dashmap::DashMap<String, usize> = model_concurrency.into_iter().collect();

    let config = fusillade::DaemonConfig {
        claim_batch_size: 5,
        claim_interval_ms: 50,
        model_concurrency_limits: Arc::new(model_concurrency),
        max_retries: Some(0),
        backoff_ms: 100,
        backoff_factor: 2,
        max_backoff_ms: 1000,
        status_log_interval_ms: None,
        heartbeat_interval_ms: 10000,
        should_retry: Arc::new(fusillade::daemon::default_should_retry),
        claim_timeout_ms: 60000,
        processing_timeout_ms: 600000,
        cancellation_poll_interval_ms: 100,
        ..Default::default()
    };

    let request_manager = Arc::new(PostgresRequestManager::<_, ReqwestHttpClient>::new(
        test_pools.clone(),
        config,
    ));
    let step_manager = Arc::new(PostgresResponseStepManager::new(test_pools));
    let response_store = Arc::new(
        FusilladeResponseStore::new(request_manager.clone()).with_step_manager(step_manager),
    );

    // Tool registry with a single HTTP tool pointing at our wiremock.
    let mut tools = HashMap::new();
    tools.insert(
        "echo_args".to_string(),
        ToolDefinition {
            kind: "http".to_string(),
            url: format!("{}/echo", tool_server.uri()),
            api_key: None,
            timeout_secs: 5,
            tool_source_id: Uuid::new_v4(),
        },
    );
    let resolved_tools = Arc::new(ResolvedToolSet::new(tools, HashMap::new()));

    // The HttpToolExecutor reads ResolvedTools from RequestContext
    // extensions. The daemon path doesn't have middleware to inject
    // that, so we wrap the executor with a "default tools" shim that
    // injects ResolvedTools at the trait-call boundary.
    struct InjectingExecutor {
        inner: Arc<HttpToolExecutor>,
        tools: Arc<ResolvedToolSet>,
    }

    #[async_trait::async_trait]
    impl onwards::traits::ToolExecutor for InjectingExecutor {
        async fn tools(
            &self,
            _ctx: &onwards::traits::RequestContext,
        ) -> Vec<onwards::traits::ToolSchema> {
            // Inject tools into a fresh context that delegates to the inner
            // executor's discovery.
            let ctx = onwards::traits::RequestContext::new()
                .with_extension(ResolvedTools(self.tools.clone()));
            self.inner.tools(&ctx).await
        }
        async fn execute(
            &self,
            tool_name: &str,
            tool_call_id: &str,
            arguments: &serde_json::Value,
            _ctx: &onwards::traits::RequestContext,
        ) -> Result<serde_json::Value, onwards::traits::ToolError> {
            let ctx = onwards::traits::RequestContext::new()
                .with_extension(ResolvedTools(self.tools.clone()));
            self.inner.execute(tool_name, tool_call_id, arguments, &ctx).await
        }
    }

    let injecting_executor = Arc::new(InjectingExecutor {
        inner: Arc::new(HttpToolExecutor::new(reqwest::Client::new(), None)),
        tools: resolved_tools,
    });
    let http_client: Arc<dyn HttpClient + Send + Sync> =
        Arc::new(onwards::client::create_hyper_client(10, 30));

    let processor = Arc::new(DwctlRequestProcessor::new(
        response_store.clone(),
        injecting_executor,
        http_client,
        onwards::LoopConfig::default(),
    ));
    request_manager
        .set_processor(processor)
        .expect("attach multi-step processor");

    // Start the daemon — from this point on it'll claim any /v1/responses
    // row in pending state and run it through the multi-step loop.
    let shutdown_token = tokio_util::sync::CancellationToken::new();
    let _daemon_handle = request_manager
        .clone()
        .run(shutdown_token.clone())
        .expect("start daemon");

    // Insert a pending request through the manager's normal create_batch
    // path (single-row batch wrapping the responses request).
    let template = RequestTemplateInput {
        custom_id: None,
        endpoint: model_server.uri(),
        method: "POST".into(),
        path: "/v1/responses".into(),
        body: serde_json::json!({
            "model": "gpt-4o",
            "input": "what is 2+2?"
        })
        .to_string(),
        model: "gpt-4o".into(),
        api_key: "test-key".into(),
    };
    let file_id = request_manager
        .create_file("multi_step_daemon_test".into(), None, vec![template])
        .await
        .unwrap();
    let batch = request_manager
        .create_batch(BatchInput {
            file_id,
            endpoint: "/v1/responses".into(),
            completion_window: "24h".into(),
            metadata: None,
            created_by: Some("multi_step_daemon_test".into()),
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .unwrap();
    let requests = request_manager.get_batch_requests(batch.id).await.unwrap();
    let request_id = match &requests[0] {
        fusillade::request::AnyRequest::Pending(r) => r.data.id,
        other => panic!("expected pending, got {:?}", other.variant()),
    };

    // Poll until the daemon completes the request.
    let start = Instant::now();
    let detail = loop {
        let detail = request_manager
            .get_request_detail(RequestId(*request_id))
            .await
            .unwrap();
        if detail.status == "completed" || detail.status == "failed" {
            break detail;
        }
        if start.elapsed() > Duration::from_secs(15) {
            panic!(
                "Timed out waiting for daemon-driven multi-step completion. \
                 last status={}, body={:?}",
                detail.status, detail.response_body
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    shutdown_token.cancel();

    assert_eq!(detail.status, "completed", "request should complete");

    // Both wiremocks were hit through the daemon path.
    assert_eq!(
        model_server.received_requests().await.unwrap().len(),
        2,
        "model wiremock should have received initial + summarize POSTs"
    );
    assert_eq!(
        tool_server.received_requests().await.unwrap().len(),
        1,
        "tool wiremock should have received one POST"
    );

    // Walk the response_steps table directly and confirm the chain shape.
    // Use the runtime sqlx::query (not the macro) so compile-time check
    // doesn't need the fusillade schema in the DATABASE_URL connection —
    // fusillade migrations are applied programmatically by the dwctl
    // binary, not by the dwctl/migrations/ folder that sqlx prepare sees.
    let steps: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT step_kind, state
        FROM fusillade.response_steps
        WHERE request_id = $1
        ORDER BY step_sequence
        "#,
    )
    .bind(*request_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(steps.len(), 3, "expected 3 steps, got {steps:?}");
    assert_eq!(steps[0].0, "model_call");
    assert_eq!(steps[1].0, "tool_call");
    assert_eq!(steps[2].0, "model_call");
    assert!(steps.iter().all(|s| s.1 == "completed"));

    // The parent row's response_body holds the assembled OpenAI Response
    // JSON. Spot-check the shape.
    let body = detail.response_body.expect("response_body set");
    let response: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(response["object"], "response");
    assert_eq!(response["status"], "completed");
    let output = response["output"].as_array().unwrap();
    // function_call (from step 1) + function_call_output (from step 2) +
    // assistant message (from step 3) = 3 items
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["type"], "function_call");
    assert_eq!(output[1]["type"], "function_call_output");
    assert_eq!(output[2]["type"], "message");
    assert_eq!(output[2]["content"][0]["text"], "the answer is 42");
}
