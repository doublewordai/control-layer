use fusillade::PostgresDaemon;
use fusillade::batch::{BatchInput, RequestTemplateInput};
use fusillade::daemon::{DaemonConfig, DaemonMode, ModelEscalationConfig, default_should_retry};
use fusillade::http::{HttpResponse, MockHttpClient};
use fusillade::manager::{ModelFilter, ModelFilterState, Storage};
use fusillade::request::{CreateFlexInput, Failed, ListRequestsFilter, Request, ServiceTierFilter};
use fusillade_arsenal::{PostgresRequestManager as PostgresStore, TestDbPools};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

type TestStore = PostgresStore<TestDbPools>;
type TestDaemon = PostgresDaemon<TestDbPools, MockHttpClient>;

async fn postgres_store(pool: sqlx::PgPool, config: &DaemonConfig) -> Arc<TestStore> {
    Arc::new(PostgresStore::new(
        TestDbPools::new(pool).await.unwrap(),
        config.into(),
    ))
}

fn postgres_daemon(
    store: Arc<TestStore>,
    http_client: Arc<MockHttpClient>,
    config: DaemonConfig,
) -> Arc<TestDaemon> {
    Arc::new(PostgresDaemon::new(store, http_client, config))
}

async fn mark_models_live_for_test(manager: &PostgresStore<TestDbPools>, models: &[&str]) {
    let filters: Vec<ModelFilter> = models
        .iter()
        .map(|model| ModelFilter {
            model: (*model).to_string(),
            state: ModelFilterState::Live,
            expected_ready_at: None,
        })
        .collect();

    manager.append_model_filter_events(&filters).await.unwrap();
}

fn retry_backoff_ms(config: &DaemonConfig, retry_attempt: u32) -> i64 {
    config
        .backoff_ms
        .saturating_mul(config.backoff_factor.saturating_pow(retry_attempt))
        .min(config.max_backoff_ms) as i64
}

fn assert_next_retry_would_cross_effective_deadline(
    failed: &Request<Failed>,
    config: &DaemonConfig,
) {
    let effective_deadline = match config.stop_before_deadline_ms {
        Some(stop_before_deadline_ms) => {
            failed.state.batch_expires_at - chrono::Duration::milliseconds(stop_before_deadline_ms)
        }
        None => failed.state.batch_expires_at,
    };
    let next_retry_at = failed.state.failed_at
        + chrono::Duration::milliseconds(retry_backoff_ms(config, failed.state.retry_attempt));

    assert!(
        next_retry_at >= effective_deadline,
        "expected terminal failure because the next retry at {next_retry_at} would cross effective deadline {effective_deadline}; failed_at={}, retry_attempt={}",
        failed.state.failed_at,
        failed.state.retry_attempt
    );
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn test_batch_only_daemon_reclaims_with_zero_local_capacity(pool: sqlx::PgPool) {
    let config = DaemonConfig {
        mode: DaemonMode::BatchOnly,
        claim_interval_ms: 10,
        batch_claim_interval_ms: 10,
        claim_query_timeout_ms: 1_000,
        heartbeat_interval_ms: 10_000,
        cancellation_poll_interval_ms: 10_000,
        status_log_interval_ms: None,
        model_concurrency_limits: Arc::new(dashmap::DashMap::new()),
        ..Default::default()
    };
    let manager = postgres_store(pool.clone(), &config).await;
    let request_id = manager
        .create_flex(CreateFlexInput {
            request_id: uuid::Uuid::new_v4(),
            body: r#"{"model":"test-model","service_tier":"flex"}"#.to_string(),
            model: "test-model".to_string(),
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/v1/test".to_string(),
            api_key: "test-key".to_string(),
            created_by: "test-user".to_string(),
        })
        .await
        .unwrap();
    sqlx::query(
        "UPDATE requests
         SET state = 'claimed',
             daemon_id = $2,
             attempt_id = $3,
             claimed_at = NOW()
         WHERE id = $1",
    )
    .bind(*request_id)
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    let before: (String, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT state, attempt_id FROM requests WHERE id = $1")
            .bind(*request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before.0, "claimed");
    assert!(before.1.is_some());

    let shutdown = CancellationToken::new();
    let handle = postgres_daemon(manager, Arc::new(MockHttpClient::new()), config)
        .run_with_mode(shutdown.clone(), DaemonMode::BatchOnly)
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row: (String, Option<uuid::Uuid>, Option<uuid::Uuid>) =
                sqlx::query_as("SELECT state, daemon_id, attempt_id FROM requests WHERE id = $1")
                    .bind(*request_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if row == ("pending".to_string(), None, None) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("BatchOnly maintenance did not reclaim without local capacity");

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("daemon did not stop")
        .expect("daemon task panicked")
        .expect("daemon returned an error");
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
#[test_log::test]
async fn test_daemon_claims_and_completes_request(pool: sqlx::PgPool) {
    // Setup: Create HTTP client with mock response
    let http_client = Arc::new(MockHttpClient::new());
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"result":"success"}"#.to_string(),
        }),
    );

    // Setup: Create manager with fast claim interval (no sleeping)
    let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
    model_concurrency_limits.insert("test-model".to_string(), 10);

    let config = DaemonConfig {
        claim_batch_size: 10,
        claim_interval_ms: 10, // Very fast for testing
        model_concurrency_limits,

        max_retries: Some(3),
        stop_before_deadline_ms: None,
        backoff_ms: 100,
        backoff_factor: 2,
        max_backoff_ms: 1000,
        status_log_interval_ms: None, // Disable status logging in tests
        heartbeat_interval_ms: 10000, // 10 seconds
        should_retry: Arc::new(default_should_retry),
        claim_timeout_ms: 60000,
        processing_timeout_ms: 600000,
        cancellation_poll_interval_ms: 100, // Fast polling for tests
        ..Default::default()
    };

    let manager = postgres_store(pool.clone(), &config).await;

    // Setup: Create a file and batch to associate with our request
    let file_id = manager
        .create_file(
            "test-file".to_string(),
            Some("Test file".to_string()),
            vec![fusillade::RequestTemplateInput {
                custom_id: None,
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            }],
        )
        .await
        .expect("Failed to create file");

    let batch = manager
        .create_batch(fusillade::batch::BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .expect("Failed to create batch");
    mark_models_live_for_test(manager.as_ref(), &["test-model"]).await;

    // Get the created request from the batch
    let requests = manager
        .get_batch_requests(batch.id)
        .await
        .expect("Failed to get batch requests");
    assert_eq!(requests.len(), 1);
    let request_id = requests[0].id();

    // Start the daemon
    let shutdown_token = CancellationToken::new();
    postgres_daemon(manager.clone(), http_client.clone(), config)
        .run(shutdown_token.clone())
        .expect("Failed to start daemon");

    // Poll for completion (with timeout)
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(5);
    let mut completed = false;

    while start.elapsed() < timeout {
        let results = manager
            .get_requests(vec![request_id])
            .await
            .expect("Failed to get request");

        if let Some(Ok(any_request)) = results.first()
            && any_request.is_terminal()
        {
            if let fusillade::AnyRequest::Completed(req) = any_request {
                // Verify the request was completed successfully
                assert_eq!(req.state.response_status, 200);
                assert_eq!(req.state.response_body, r#"{"result":"success"}"#);
                completed = true;
                break;
            } else {
                panic!(
                    "Request reached terminal state but was not completed: {:?}",
                    any_request
                );
            }
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Stop the daemon
    shutdown_token.cancel();

    // Assert that the request completed
    assert!(
        completed,
        "Request did not complete within timeout. Check daemon processing."
    );

    // Verify HTTP client was called exactly once
    assert_eq!(http_client.call_count(), 1);
    let calls = http_client.get_calls();
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].path, "/v1/test");
    assert_eq!(calls[0].api_key, "test-key");
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
async fn test_daemon_respects_per_model_concurrency_limits(pool: sqlx::PgPool) {
    // Setup: Create HTTP client with triggered responses
    let http_client = Arc::new(MockHttpClient::new());

    // Add 5 triggered responses for our 5 requests
    let trigger1 = http_client.add_response_with_trigger(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"result":"1"}"#.to_string(),
        }),
    );
    let trigger2 = http_client.add_response_with_trigger(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"result":"2"}"#.to_string(),
        }),
    );
    let trigger3 = http_client.add_response_with_trigger(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"result":"3"}"#.to_string(),
        }),
    );
    let trigger4 = http_client.add_response_with_trigger(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"result":"4"}"#.to_string(),
        }),
    );
    let trigger5 = http_client.add_response_with_trigger(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"result":"5"}"#.to_string(),
        }),
    );

    // Setup: Create manager with concurrency limit of 2 for "gpt-4"
    let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
    model_concurrency_limits.insert("gpt-4".to_string(), 2);

    let config = DaemonConfig {
        claim_batch_size: 10,
        claim_interval_ms: 10,
        model_concurrency_limits,

        max_retries: Some(3),
        stop_before_deadline_ms: None,
        backoff_ms: 100,
        backoff_factor: 2,
        max_backoff_ms: 1000,
        status_log_interval_ms: None,
        heartbeat_interval_ms: 10000,
        should_retry: Arc::new(default_should_retry),
        claim_timeout_ms: 60000,
        processing_timeout_ms: 600000,
        cancellation_poll_interval_ms: 100, // Fast polling for tests
        ..Default::default()
    };

    let manager = postgres_store(pool.clone(), &config).await;

    // Setup: Create a file with 5 templates, all using "gpt-4"
    let file_id = manager
        .create_file(
            "test-file".to_string(),
            Some("Test concurrency limits".to_string()),
            vec![
                fusillade::RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: r#"{"prompt":"test1"}"#.to_string(),
                    model: "gpt-4".to_string(),
                    api_key: "test-key".to_string(),
                },
                fusillade::RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: r#"{"prompt":"test2"}"#.to_string(),
                    model: "gpt-4".to_string(),
                    api_key: "test-key".to_string(),
                },
                fusillade::RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: r#"{"prompt":"test3"}"#.to_string(),
                    model: "gpt-4".to_string(),
                    api_key: "test-key".to_string(),
                },
                fusillade::RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: r#"{"prompt":"test4"}"#.to_string(),
                    model: "gpt-4".to_string(),
                    api_key: "test-key".to_string(),
                },
                fusillade::RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: r#"{"prompt":"test5"}"#.to_string(),
                    model: "gpt-4".to_string(),
                    api_key: "test-key".to_string(),
                },
            ],
        )
        .await
        .expect("Failed to create file");

    let batch = manager
        .create_batch(fusillade::batch::BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .expect("Failed to create batch");
    mark_models_live_for_test(manager.as_ref(), &["gpt-4"]).await;

    // Start the daemon
    let shutdown_token = CancellationToken::new();
    postgres_daemon(manager.clone(), http_client.clone(), config)
        .run(shutdown_token.clone())
        .expect("Failed to start daemon");

    // Wait for exactly 2 requests to be in-flight (respecting concurrency limit)
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(2);
    let mut reached_limit = false;

    while start.elapsed() < timeout {
        let in_flight = http_client.in_flight_count();
        if in_flight == 2 {
            reached_limit = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        reached_limit,
        "Expected exactly 2 requests in-flight, got {}",
        http_client.in_flight_count()
    );

    // Verify exactly 2 are in-flight (not more) by polling for a bit
    let start = tokio::time::Instant::now();
    let stable_duration = Duration::from_millis(100);
    while start.elapsed() < stable_duration {
        let in_flight = http_client.in_flight_count();
        assert!(
            in_flight <= 2,
            "Concurrency limit violated: {} requests in-flight (expected max 2)",
            in_flight
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        http_client.in_flight_count(),
        2,
        "Expected exactly 2 requests in-flight after stability check"
    );

    // Trigger completion of first request
    trigger1.send(()).unwrap();

    // Wait for the third request to start
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(2);
    let mut third_started = false;

    while start.elapsed() < timeout {
        if http_client.call_count() >= 3 {
            third_started = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        third_started,
        "Third request should have started after first completed"
    );

    // Verify still only 2 in-flight
    assert_eq!(
        http_client.in_flight_count(),
        2,
        "Should maintain concurrency limit of 2"
    );

    // Complete remaining requests to clean up
    trigger2.send(()).unwrap();
    trigger3.send(()).unwrap();
    trigger4.send(()).unwrap();
    trigger5.send(()).unwrap();

    // Wait for all requests to complete
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(5);
    let mut all_completed = false;

    while start.elapsed() < timeout {
        let status = manager
            .get_batch_status(batch.id)
            .await
            .expect("Failed to get batch status");

        if status.completed_requests == 5 {
            all_completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Stop the daemon
    shutdown_token.cancel();

    assert!(all_completed, "All 5 requests should have completed");

    // Verify all 5 HTTP calls were made
    assert_eq!(http_client.call_count(), 5);
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
async fn test_daemon_retries_configured_and_default_statuses(pool: sqlx::PgPool) {
    // Setup: Create HTTP client with failing responses, then success
    let http_client = Arc::new(MockHttpClient::new());

    // First attempt: fails with the additionally configured default status
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 499,
            body: r#"{"error":"arbitrary upstream cancellation"}"#.to_string(),
        }),
    );

    // Second attempt: fails with a built-in default status
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 503,
            body: r#"{"error":"service unavailable"}"#.to_string(),
        }),
    );

    // Third attempt: succeeds
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"result":"success after retries"}"#.to_string(),
        }),
    );

    // Setup: Create manager with fast backoff for testing
    let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
    model_concurrency_limits.insert("test-model".to_string(), 10);

    let config = DaemonConfig {
        claim_batch_size: 10,
        claim_interval_ms: 10,
        model_concurrency_limits,

        max_retries: Some(5),
        stop_before_deadline_ms: None,
        backoff_ms: 10, // Very fast backoff for testing
        backoff_factor: 2,
        max_backoff_ms: 100,
        status_log_interval_ms: None,
        heartbeat_interval_ms: 10000,
        should_retry: Arc::new(default_should_retry),
        claim_timeout_ms: 60000,
        processing_timeout_ms: 600000,
        cancellation_poll_interval_ms: 100,
        ..Default::default()
    };

    let manager = postgres_store(pool.clone(), &config).await;

    // Setup: Create a file and batch
    let file_id = manager
        .create_file(
            "test-file".to_string(),
            Some("Test retry logic".to_string()),
            vec![fusillade::RequestTemplateInput {
                custom_id: None,
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            }],
        )
        .await
        .expect("Failed to create file");

    let batch = manager
        .create_batch(fusillade::batch::BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .expect("Failed to create batch");
    mark_models_live_for_test(manager.as_ref(), &["test-model"]).await;

    let requests = manager
        .get_batch_requests(batch.id)
        .await
        .expect("Failed to get batch requests");
    assert_eq!(requests.len(), 1);
    let request_id = requests[0].id();

    // Start the daemon
    let shutdown_token = CancellationToken::new();
    postgres_daemon(manager.clone(), http_client.clone(), config)
        .run(shutdown_token.clone())
        .expect("Failed to start daemon");

    // Poll for completion (with timeout)
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(5);
    let mut completed = false;

    while start.elapsed() < timeout {
        let results = manager
            .get_requests(vec![request_id])
            .await
            .expect("Failed to get request");

        if let Some(Ok(any_request)) = results.first()
            && let fusillade::AnyRequest::Completed(req) = any_request
        {
            // Verify the request eventually completed successfully
            assert_eq!(req.state.response_status, 200);
            assert_eq!(
                req.state.response_body,
                r#"{"result":"success after retries"}"#
            );
            completed = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Stop the daemon
    shutdown_token.cancel();

    assert!(completed, "Request should have completed after retries");

    // Verify the request was attempted 3 times (2 failures + 1 success)
    assert_eq!(
        http_client.call_count(),
        3,
        "Expected 3 HTTP calls (2 failed attempts + 1 success)"
    );
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
async fn test_daemon_dynamically_updates_concurrency_limits(pool: sqlx::PgPool) {
    // Setup: Create HTTP client with triggered responses
    let http_client = Arc::new(MockHttpClient::new());

    // Add 10 triggered responses
    let mut triggers = vec![];
    for i in 1..=10 {
        let trigger = http_client.add_response_with_trigger(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: format!(r#"{{"result":"{}"}}"#, i),
            }),
        );
        triggers.push(trigger);
    }

    // Setup: Start with concurrency limit of 2 for "gpt-4"
    let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
    model_concurrency_limits.insert("gpt-4".to_string(), 2);

    let config = DaemonConfig {
        claim_batch_size: 10,
        claim_interval_ms: 10,
        model_concurrency_limits: model_concurrency_limits.clone(),
        max_retries: Some(3),
        stop_before_deadline_ms: None,
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
    };

    let manager = postgres_store(pool.clone(), &config).await;

    // Setup: Create a file with 10 requests, all using "gpt-4"
    let templates: Vec<_> = (1..=10)
        .map(|i| fusillade::RequestTemplateInput {
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/v1/test".to_string(),
            body: format!(r#"{{"prompt":"test{}"}}"#, i),
            model: "gpt-4".to_string(),
            api_key: "test-key".to_string(),
        })
        .collect();

    let file_id = manager
        .create_file(
            "test-file".to_string(),
            Some("Test dynamic limits".to_string()),
            templates,
        )
        .await
        .expect("Failed to create file");

    let batch = manager
        .create_batch(fusillade::batch::BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .expect("Failed to create batch");
    mark_models_live_for_test(manager.as_ref(), &["gpt-4"]).await;

    // Start the daemon
    let shutdown_token = CancellationToken::new();
    postgres_daemon(manager.clone(), http_client.clone(), config)
        .run(shutdown_token.clone())
        .expect("Failed to start daemon");

    // Wait for exactly 2 requests to be in-flight (initial limit)
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(2);
    let mut reached_initial_limit = false;

    while start.elapsed() < timeout {
        let in_flight = http_client.in_flight_count();
        if in_flight == 2 {
            reached_initial_limit = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        reached_initial_limit,
        "Expected exactly 2 requests in-flight with initial limit"
    );

    // Increase the limit to 5
    model_concurrency_limits.insert("gpt-4".to_string(), 5);

    // Complete one request to free up a permit and trigger daemon to check limits
    triggers.remove(0).send(()).unwrap();

    // Now we should see up to 5 requests in flight (daemon picks up new limit)
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(2);
    let mut reached_new_limit = false;

    while start.elapsed() < timeout {
        let in_flight = http_client.in_flight_count();
        if in_flight >= 4 {
            // Should see at least 4-5 in flight with new limit
            reached_new_limit = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        reached_new_limit,
        "Expected more requests in-flight after limit increase, got {}",
        http_client.in_flight_count()
    );

    // Now decrease the limit to 3
    model_concurrency_limits.insert("gpt-4".to_string(), 3);

    // Complete remaining requests
    for trigger in triggers {
        trigger.send(()).unwrap();
    }

    // Wait for all requests to complete
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(5);
    let mut all_completed = false;

    while start.elapsed() < timeout {
        let status = manager
            .get_batch_status(batch.id)
            .await
            .expect("Failed to get batch status");

        if status.completed_requests == 10 {
            all_completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Stop the daemon
    shutdown_token.cancel();

    assert!(all_completed, "All 10 requests should have completed");
    assert_eq!(http_client.call_count(), 10);
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
async fn test_deadline_aware_retry_stops_before_deadline(pool: sqlx::PgPool) {
    // Test that retries stop when approaching the deadline
    let http_client = Arc::new(MockHttpClient::new());

    // All requests will fail
    for _ in 0..20 {
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 500,
                body: r#"{"error":"server error"}"#.to_string(),
            }),
        );
    }

    // Use deadline-aware retry with a short completion window and short buffer
    let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
    model_concurrency_limits.insert("test-model".to_string(), 10);

    let config = DaemonConfig {
        claim_batch_size: 10,
        claim_interval_ms: 10,
        model_concurrency_limits,

        max_retries: Some(10_000),
        stop_before_deadline_ms: Some(500), // 500ms buffer before deadline
        backoff_ms: 50,
        backoff_factor: 2,
        max_backoff_ms: 200,
        status_log_interval_ms: None,
        heartbeat_interval_ms: 10000,
        should_retry: Arc::new(default_should_retry),
        claim_timeout_ms: 60000,
        processing_timeout_ms: 600000,
        cancellation_poll_interval_ms: 100,
        ..Default::default()
    };

    let manager = postgres_store(pool.clone(), &config).await;

    // Create a batch with a very short completion window (2 seconds)
    let file_id = manager
        .create_file(
            "test-file".to_string(),
            Some("Test deadline cutoff".to_string()),
            vec![fusillade::RequestTemplateInput {
                custom_id: None,
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            }],
        )
        .await
        .expect("Failed to create file");

    let batch = manager
        .create_batch(fusillade::batch::BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "2s".to_string(), // Very short window
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .expect("Failed to create batch");
    mark_models_live_for_test(manager.as_ref(), &["test-model"]).await;

    let requests = manager
        .get_batch_requests(batch.id)
        .await
        .expect("Failed to get batch requests");
    let request_id = requests[0].id();

    // Start the daemon
    let shutdown_token = CancellationToken::new();
    postgres_daemon(manager.clone(), http_client.clone(), config.clone())
        .run(shutdown_token.clone())
        .expect("Failed to start daemon");

    // Poll until request reaches Failed state (due to deadline)
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(5);
    let mut results = None;

    while start.elapsed() < timeout {
        let res = manager
            .get_requests(vec![request_id])
            .await
            .expect("Failed to get request");

        if let Some(Ok(req)) = res.first()
            && req.is_terminal()
        {
            results = Some(res);
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    shutdown_token.cancel();

    let results = results.expect("Request should have reached terminal state within timeout");

    if let Some(Ok(fusillade::AnyRequest::Failed(failed))) = results.first() {
        let retry_count = failed.state.retry_attempt;
        let call_count = http_client.call_count();

        // Verify the daemon stopped because another retry would cross the
        // configured effective deadline. Absolute attempt counts vary under CI
        // load because DB and task scheduling overhead consume the same window.
        assert_next_retry_would_cross_effective_deadline(failed, &config);
        assert!(
            retry_count > 0,
            "Expected at least one retry attempt before the buffered deadline, got {}",
            retry_count
        );

        // Verify HTTP call count matches retry attempts (1 initial + N retries)
        assert_eq!(
            call_count,
            (retry_count + 1) as usize,
            "Expected call count to match retry attempts + 1 initial attempt, got {} calls for {} retry attempts",
            call_count,
            retry_count
        );

        // Verify the request actually has error details from the last attempt
        assert!(
            !failed.state.reason.to_error_message().is_empty(),
            "Expected failed request to have failure reason"
        );
    } else {
        panic!(
            "Expected request to be in Failed state, got {:?}",
            results.first()
        );
    }
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
async fn test_retry_stops_at_deadline_when_no_limits_set(pool: sqlx::PgPool) {
    // Test that when neither max_retries nor stop_before_deadline_ms is set,
    // retries stop exactly at the deadline (no buffer)
    let http_client = Arc::new(MockHttpClient::new());

    // All requests will fail
    for _ in 0..20 {
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 500,
                body: r#"{"error":"server error"}"#.to_string(),
            }),
        );
    }

    // No max_retries, no stop_before_deadline_ms
    let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
    model_concurrency_limits.insert("test-model".to_string(), 10);

    let config = DaemonConfig {
        claim_batch_size: 10,
        claim_interval_ms: 10,
        model_concurrency_limits,

        max_retries: None,             // No retry limit
        stop_before_deadline_ms: None, // No buffer - should retry until deadline
        backoff_ms: 50,
        backoff_factor: 2,
        max_backoff_ms: 200,
        status_log_interval_ms: None,
        heartbeat_interval_ms: 10000,
        should_retry: Arc::new(default_should_retry),
        claim_timeout_ms: 60000,
        processing_timeout_ms: 600000,
        cancellation_poll_interval_ms: 100,
        ..Default::default()
    };

    let manager = postgres_store(pool.clone(), &config).await;

    // Create a batch with a 2 second completion window
    let file_id = manager
        .create_file(
            "test-file".to_string(),
            Some("Test no limits retry".to_string()),
            vec![fusillade::RequestTemplateInput {
                custom_id: None,
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            }],
        )
        .await
        .expect("Failed to create file");

    let batch = manager
        .create_batch(fusillade::batch::BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "2s".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .expect("Failed to create batch");
    mark_models_live_for_test(manager.as_ref(), &["test-model"]).await;

    let requests = manager
        .get_batch_requests(batch.id)
        .await
        .expect("Failed to get batch requests");
    let request_id = requests[0].id();

    // Start the daemon
    let shutdown_token = CancellationToken::new();
    postgres_daemon(manager.clone(), http_client.clone(), config.clone())
        .run(shutdown_token.clone())
        .expect("Failed to start daemon");

    // Poll until request reaches Failed state (due to deadline)
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(5);
    let mut results = None;

    while start.elapsed() < timeout {
        let res = manager
            .get_requests(vec![request_id])
            .await
            .expect("Failed to get request");

        if let Some(Ok(req)) = res.first()
            && req.is_terminal()
        {
            results = Some(res);
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    shutdown_token.cancel();

    let results = results.expect("Request should have reached terminal state within timeout");

    if let Some(Ok(fusillade::AnyRequest::Failed(failed))) = results.first() {
        let retry_count = failed.state.retry_attempt;
        let call_count = http_client.call_count();

        // Verify retries continue until the actual deadline when no buffer is
        // configured. Avoid asserting an exact attempt count; CI overhead changes
        // how many backoff cycles fit inside a two-second completion window.
        assert_next_retry_would_cross_effective_deadline(failed, &config);
        assert!(
            retry_count > 0,
            "Expected at least one retry attempt before the deadline, got {}",
            retry_count
        );

        // Verify HTTP call count matches retry attempts (1 initial + N retries)
        assert_eq!(
            call_count,
            (retry_count + 1) as usize,
            "Expected call count to match retry attempts + 1 initial attempt, got {} calls for {} retry attempts",
            call_count,
            retry_count
        );

        // Verify the request has error details from the last attempt
        assert!(
            !failed.state.reason.to_error_message().is_empty(),
            "Expected failed request to have failure reason"
        );
    } else {
        panic!(
            "Expected request to be in Failed state, got {:?}",
            results.first()
        );
    }
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
async fn test_route_at_claim_time_escalation(pool: sqlx::PgPool) {
    // Test: When time remaining before batch expiry is below the escalation threshold,
    // requests are routed to the escalation model at claim time.

    let http_client = Arc::new(MockHttpClient::new());

    // Add response for the escalation model endpoint
    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"result":"escalated response"}"#.to_string(),
        }),
    );

    // Configure model escalation: gpt-4 -> gpt-4-turbo when under time pressure
    let model_escalations = Arc::new(dashmap::DashMap::new());
    model_escalations.insert(
        "gpt-4".to_string(),
        ModelEscalationConfig {
            escalation_model: "gpt-4-turbo".to_string(),
            // Use a very high threshold (2 hours) so it always triggers with our short window
            escalation_threshold_seconds: 7200,
        },
    );

    let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
    model_concurrency_limits.insert("gpt-4".to_string(), 10);
    model_concurrency_limits.insert("gpt-4-turbo".to_string(), 10);

    let config = DaemonConfig {
        claim_batch_size: 10,
        claim_interval_ms: 10,
        model_concurrency_limits,

        model_escalations,
        max_retries: Some(3),
        stop_before_deadline_ms: None,
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
    };

    let manager = postgres_store(pool.clone(), &config).await;

    // Create a file with a request using gpt-4
    let file_id = manager
        .create_file(
            "test-escalation".to_string(),
            Some("Test route-at-claim-time escalation".to_string()),
            vec![fusillade::RequestTemplateInput {
                custom_id: Some("escalation-test".to_string()),
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "gpt-4".to_string(),
                api_key: "original-key".to_string(),
            }],
        )
        .await
        .expect("Failed to create file");

    // Create batch with a 1-hour completion window (less than 2-hour threshold)
    let batch = manager
        .create_batch(fusillade::batch::BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "1h".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .expect("Failed to create batch");
    mark_models_live_for_test(manager.as_ref(), &["gpt-4"]).await;

    let requests = manager
        .get_batch_requests(batch.id)
        .await
        .expect("Failed to get batch requests");
    assert_eq!(requests.len(), 1);
    let request_id = requests[0].id();

    // Start the daemon
    let shutdown_token = CancellationToken::new();
    postgres_daemon(manager.clone(), http_client.clone(), config)
        .run(shutdown_token.clone())
        .expect("Failed to start daemon");

    // Poll for completion
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(5);
    let mut completed = false;

    while start.elapsed() < timeout {
        let results = manager
            .get_requests(vec![request_id])
            .await
            .expect("Failed to get request");

        if let Some(Ok(any_request)) = results.first()
            && any_request.is_terminal()
        {
            if let fusillade::AnyRequest::Completed(req) = any_request {
                assert_eq!(req.state.response_status, 200);
                assert_eq!(
                    req.state.response_body,
                    r#"{"result":"escalated response"}"#
                );
                completed = true;
                break;
            } else {
                panic!(
                    "Request reached terminal state but was not completed: {:?}",
                    any_request
                );
            }
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    shutdown_token.cancel();

    assert!(completed, "Request did not complete within timeout");

    // Verify the HTTP call was made with the ESCALATION model (routed at claim time)
    // Note: With route-at-claim-time escalation, the original API key is used
    // (batch API keys automatically have access to escalation models in the onwards cache)
    assert_eq!(http_client.call_count(), 1);
    let calls = http_client.get_calls();
    assert_eq!(
        calls[0].api_key, "original-key",
        "Should use original API key (escalation only changes model routing)"
    );
    // The escalation is verified by the request completing successfully with the
    // escalated model's response. The model change happens at claim time in the daemon.
}

#[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
async fn test_route_at_claim_time_no_escalation_when_enough_time(pool: sqlx::PgPool) {
    // Test: When there's enough time remaining (above threshold), requests use original model

    let http_client = Arc::new(MockHttpClient::new());

    http_client.add_response(
        "POST /v1/test",
        Ok(HttpResponse {
            status: 200,
            body: r#"{"result":"normal response"}"#.to_string(),
        }),
    );

    // Configure escalation with a 1-minute threshold
    let model_escalations = Arc::new(dashmap::DashMap::new());
    model_escalations.insert(
        "gpt-4".to_string(),
        ModelEscalationConfig {
            escalation_model: "gpt-4-turbo".to_string(),
            escalation_threshold_seconds: 60, // 1 minute threshold
        },
    );

    let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
    model_concurrency_limits.insert("gpt-4".to_string(), 10);
    model_concurrency_limits.insert("gpt-4-turbo".to_string(), 10);

    let config = DaemonConfig {
        claim_batch_size: 10,
        claim_interval_ms: 10,
        model_concurrency_limits,

        model_escalations,
        max_retries: Some(3),
        stop_before_deadline_ms: None,
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
    };

    let manager = postgres_store(pool.clone(), &config).await;

    let file_id = manager
        .create_file(
            "test-no-escalation".to_string(),
            Some("Test no escalation when enough time".to_string()),
            vec![fusillade::RequestTemplateInput {
                custom_id: Some("no-escalation-test".to_string()),
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "gpt-4".to_string(),
                api_key: "original-key".to_string(),
            }],
        )
        .await
        .expect("Failed to create file");

    // Create batch with 24-hour window (well above 1-minute threshold)
    let batch = manager
        .create_batch(fusillade::batch::BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .expect("Failed to create batch");
    mark_models_live_for_test(manager.as_ref(), &["gpt-4"]).await;

    let requests = manager
        .get_batch_requests(batch.id)
        .await
        .expect("Failed to get batch requests");
    let request_id = requests[0].id();

    let shutdown_token = CancellationToken::new();
    postgres_daemon(manager.clone(), http_client.clone(), config)
        .run(shutdown_token.clone())
        .expect("Failed to start daemon");

    // Poll for completion
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(5);
    let mut completed = false;

    while start.elapsed() < timeout {
        let results = manager
            .get_requests(vec![request_id])
            .await
            .expect("Failed to get request");

        if let Some(Ok(any_request)) = results.first()
            && any_request.is_terminal()
        {
            completed = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    shutdown_token.cancel();

    assert!(completed, "Request did not complete within timeout");

    // Verify the HTTP call used the ORIGINAL API key (no escalation)
    assert_eq!(http_client.call_count(), 1);
    let calls = http_client.get_calls();
    assert_eq!(
        calls[0].api_key, "original-key",
        "Should use original API key when time remaining is above threshold"
    );
}

/// Tests for `get_batch_results_stream` and `stream_batch_results`.
///
/// These tests verify that batch results streaming works correctly,
/// including pagination, filtering, and error handling.
mod batch_results_stream {
    use super::*;
    use futures::StreamExt;

    /// Helper to collect all results from a batch results stream
    async fn collect_batch_results(
        manager: &PostgresStore<TestDbPools>,
        batch_id: fusillade::batch::BatchId,
    ) -> Vec<fusillade::batch::BatchResultItem> {
        let stream = manager.get_batch_results_stream(batch_id, 0, None, None);
        stream
            .filter_map(|r| async { r.ok() })
            .collect::<Vec<_>>()
            .await
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    async fn test_batch_results_basic(pool: sqlx::PgPool) {
        // Test: Basic batch returns all results correctly
        let http_client = Arc::new(MockHttpClient::new());
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"success1"}"#.to_string(),
            }),
        );
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"success2"}"#.to_string(),
            }),
        );

        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("gpt-4".to_string(), 10);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            model_concurrency_limits,
            ..Default::default()
        };

        let manager = postgres_store(pool.clone(), &config).await;

        // Create file with 2 templates
        let file_id = manager
            .create_file(
                "test-results".to_string(),
                None,
                vec![
                    RequestTemplateInput {
                        custom_id: Some("req-1".to_string()),
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test1"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: Some("req-2".to_string()),
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test2"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                ],
            )
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .expect("Failed to create batch");
        mark_models_live_for_test(manager.as_ref(), &["gpt-4"]).await;

        // Run daemon to complete requests
        let shutdown_token = CancellationToken::new();
        postgres_daemon(manager.clone(), http_client.clone(), config)
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Wait for requests to complete
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let requests = manager
                .get_batch_requests(batch.id)
                .await
                .expect("Failed to get requests");
            if requests.iter().all(|r| r.is_terminal()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        shutdown_token.cancel();

        // Get batch results stream
        let results = collect_batch_results(&manager, batch.id).await;

        // Should have exactly 2 results (one per template)
        assert_eq!(results.len(), 2, "Should have 2 results");

        // Verify custom IDs
        let custom_ids: Vec<_> = results
            .iter()
            .filter_map(|r| r.custom_id.as_ref())
            .collect();
        assert!(custom_ids.contains(&&"req-1".to_string()));
        assert!(custom_ids.contains(&&"req-2".to_string()));

        // All should be completed
        for result in &results {
            assert_eq!(
                result.status,
                fusillade::batch::BatchResultStatus::Completed
            );
            assert!(result.response_body.is_some());
        }
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    async fn test_batch_results_deleted_file_returns_error(pool: sqlx::PgPool) {
        // Test: When batch's file is deleted, stream returns error
        let http_client = Arc::new(MockHttpClient::new());

        let manager = Arc::new(PostgresStore::with_client(
            TestDbPools::new(pool.clone()).await.unwrap(),
            http_client,
        ));

        let file_id = manager
            .create_file(
                "to-delete".to_string(),
                None,
                vec![RequestTemplateInput {
                    custom_id: Some("test".to_string()),
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: r#"{"prompt":"test"}"#.to_string(),
                    model: "gpt-4".to_string(),
                    api_key: "test-key".to_string(),
                }],
            )
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .expect("Failed to create batch");

        // Delete the file (set file_id to NULL on batch)
        sqlx::query!(
            "UPDATE batches SET file_id = NULL WHERE id = $1",
            *batch.id as uuid::Uuid
        )
        .execute(&pool)
        .await
        .expect("Failed to clear file_id");

        // Try to get results - should get an error
        let stream = manager.get_batch_results_stream(batch.id, 0, None, None);
        let results: Vec<_> = stream.collect().await;

        // Should have one error result
        assert_eq!(results.len(), 1, "Should have one result (the error)");
        assert!(
            results[0].is_err(),
            "Result should be an error when file_id is NULL"
        );

        let err = results[0].as_ref().unwrap_err();
        assert!(
            err.to_string().contains("file_id"),
            "Error should mention file_id: {}",
            err
        );
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    async fn test_output_file_streamable_after_batch_deleted(pool: sqlx::PgPool) {
        // Test: Output files can still be streamed after the batch is soft-deleted
        // This ensures users can download completed results even if the batch is deleted
        let http_client = Arc::new(MockHttpClient::new());
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"success"}"#.to_string(),
            }),
        );

        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("gpt-4".to_string(), 10);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            model_concurrency_limits,
            ..Default::default()
        };

        let manager = postgres_store(pool.clone(), &config).await;

        let file_id = manager
            .create_file(
                "output-after-delete".to_string(),
                None,
                vec![RequestTemplateInput {
                    custom_id: Some("test".to_string()),
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: r#"{"prompt":"test"}"#.to_string(),
                    model: "gpt-4".to_string(),
                    api_key: "test-key".to_string(),
                }],
            )
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .expect("Failed to create batch");
        mark_models_live_for_test(manager.as_ref(), &["gpt-4"]).await;

        // Run daemon to process the request
        let shutdown_token = CancellationToken::new();
        postgres_daemon(manager.clone(), http_client.clone(), config)
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Wait for completion
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let b = manager.get_batch(batch.id).await.expect("get_batch failed");
            if b.completed_requests == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        shutdown_token.cancel();

        // Get the output file ID
        let batch = manager.get_batch(batch.id).await.expect("get_batch failed");
        let output_file_id = batch
            .output_file_id
            .expect("Batch should have output_file_id");

        // Verify we can get output file content before deletion
        let results_before = manager
            .get_file_content(output_file_id)
            .await
            .expect("Should be able to get output file content before deletion");
        assert!(
            !results_before.is_empty(),
            "Output file should have content before deletion"
        );

        // Soft-delete the batch
        manager
            .delete_batch(batch.id)
            .await
            .expect("delete_batch failed");

        // Verify batch is no longer accessible
        let batch_result = manager.get_batch(batch.id).await;
        assert!(
            batch_result.is_err(),
            "Batch should not be found after deletion"
        );

        // Verify we can STILL get the output file content after batch deletion
        let results_after = manager
            .get_file_content(output_file_id)
            .await
            .expect("Should still be able to get output file content after batch deletion");
        assert!(
            !results_after.is_empty(),
            "Output file should still have content after batch deletion"
        );
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    async fn test_batch_results_pagination(pool: sqlx::PgPool) {
        // Test: Pagination works correctly with offset
        let http_client = Arc::new(MockHttpClient::new());

        // Add responses for 5 requests
        for i in 0..5 {
            http_client.add_response(
                "POST /v1/test",
                Ok(HttpResponse {
                    status: 200,
                    body: format!(r#"{{"result":"success{}"}}"#, i),
                }),
            );
        }

        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("gpt-4".to_string(), 10);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            model_concurrency_limits,
            ..Default::default()
        };

        let manager = postgres_store(pool.clone(), &config).await;

        // Create file with 5 templates
        let templates: Vec<_> = (0..5)
            .map(|i| RequestTemplateInput {
                custom_id: Some(format!("req-{}", i)),
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: format!(r#"{{"prompt":"test{}"}}"#, i),
                model: "gpt-4".to_string(),
                api_key: "test-key".to_string(),
            })
            .collect();

        let file_id = manager
            .create_file("pagination-test".to_string(), None, templates)
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .expect("Failed to create batch");
        mark_models_live_for_test(manager.as_ref(), &["gpt-4"]).await;

        // Run daemon
        let shutdown_token = CancellationToken::new();
        postgres_daemon(manager.clone(), http_client.clone(), config)
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Wait for completion
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let requests = manager
                .get_batch_requests(batch.id)
                .await
                .expect("Failed to get requests");
            if requests.iter().all(|r| r.is_terminal()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        shutdown_token.cancel();

        // Get all results
        let all_results = collect_batch_results(&manager, batch.id).await;
        assert_eq!(all_results.len(), 5, "Should have 5 results");

        // Get with offset 2
        let stream = manager.get_batch_results_stream(batch.id, 2, None, None);
        let offset_results: Vec<_> = stream.filter_map(|r| async { r.ok() }).collect().await;

        assert_eq!(
            offset_results.len(),
            3,
            "Should have 3 results with offset 2"
        );

        // Verify the offset results are the last 3
        let offset_ids: Vec<_> = offset_results
            .iter()
            .filter_map(|r| r.custom_id.as_ref())
            .collect();
        assert!(offset_ids.contains(&&"req-2".to_string()));
        assert!(offset_ids.contains(&&"req-3".to_string()));
        assert!(offset_ids.contains(&&"req-4".to_string()));
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    async fn test_batch_results_status_filter(pool: sqlx::PgPool) {
        // Test: Status filter works correctly
        let http_client = Arc::new(MockHttpClient::new());

        // First 2 succeed, third fails
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"success1"}"#.to_string(),
            }),
        );
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"success2"}"#.to_string(),
            }),
        );
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 500,
                body: r#"{"error":"server error"}"#.to_string(),
            }),
        );

        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("gpt-4".to_string(), 10);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            model_concurrency_limits,
            max_retries: Some(0), // No retries so it fails immediately
            ..Default::default()
        };

        let manager = postgres_store(pool.clone(), &config).await;

        let templates: Vec<_> = (0..3)
            .map(|i| RequestTemplateInput {
                custom_id: Some(format!("req-{}", i)),
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: format!(r#"{{"prompt":"test{}"}}"#, i),
                model: "gpt-4".to_string(),
                api_key: "test-key".to_string(),
            })
            .collect();

        let file_id = manager
            .create_file("filter-test".to_string(), None, templates)
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .expect("Failed to create batch");
        mark_models_live_for_test(manager.as_ref(), &["gpt-4"]).await;

        let shutdown_token = CancellationToken::new();
        postgres_daemon(manager.clone(), http_client.clone(), config)
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Wait for completion
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let requests = manager
                .get_batch_requests(batch.id)
                .await
                .expect("Failed to get requests");
            if requests.iter().all(|r| r.is_terminal()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        shutdown_token.cancel();

        // Filter by completed
        let stream =
            manager.get_batch_results_stream(batch.id, 0, None, Some("completed".to_string()));
        let completed_results: Vec<_> = stream.filter_map(|r| async { r.ok() }).collect().await;

        assert_eq!(
            completed_results.len(),
            2,
            "Should have 2 completed results"
        );
        for r in &completed_results {
            assert_eq!(r.status, fusillade::batch::BatchResultStatus::Completed);
        }

        // Filter by failed
        let stream =
            manager.get_batch_results_stream(batch.id, 0, None, Some("failed".to_string()));
        let failed_results: Vec<_> = stream.filter_map(|r| async { r.ok() }).collect().await;

        assert_eq!(failed_results.len(), 1, "Should have 1 failed result");
        assert_eq!(
            failed_results[0].status,
            fusillade::batch::BatchResultStatus::Failed
        );
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    async fn test_batch_results_search_filter(pool: sqlx::PgPool) {
        // Test: Search filter works correctly (case-insensitive)
        let http_client = Arc::new(MockHttpClient::new());

        for _ in 0..3 {
            http_client.add_response(
                "POST /v1/test",
                Ok(HttpResponse {
                    status: 200,
                    body: r#"{"result":"success"}"#.to_string(),
                }),
            );
        }

        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("gpt-4".to_string(), 10);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            model_concurrency_limits,
            ..Default::default()
        };

        let manager = postgres_store(pool.clone(), &config).await;

        let file_id = manager
            .create_file(
                "search-test".to_string(),
                None,
                vec![
                    RequestTemplateInput {
                        custom_id: Some("Alpha-Request".to_string()),
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: Some("Beta-Request".to_string()),
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: Some("Gamma-Item".to_string()),
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                ],
            )
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .expect("Failed to create batch");
        mark_models_live_for_test(manager.as_ref(), &["gpt-4"]).await;

        let shutdown_token = CancellationToken::new();
        postgres_daemon(manager.clone(), http_client.clone(), config)
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Wait for completion
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let requests = manager
                .get_batch_requests(batch.id)
                .await
                .expect("Failed to get requests");
            if requests.iter().all(|r| r.is_terminal()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        shutdown_token.cancel();

        // Search for "request" (case-insensitive)
        let stream =
            manager.get_batch_results_stream(batch.id, 0, Some("request".to_string()), None);
        let search_results: Vec<_> = stream.filter_map(|r| async { r.ok() }).collect().await;

        assert_eq!(
            search_results.len(),
            2,
            "Should find 2 results containing 'request'"
        );

        let custom_ids: Vec<_> = search_results
            .iter()
            .filter_map(|r| r.custom_id.as_ref())
            .collect();
        assert!(custom_ids.contains(&&"Alpha-Request".to_string()));
        assert!(custom_ids.contains(&&"Beta-Request".to_string()));

        // Search for "ALPHA" (case-insensitive)
        let stream = manager.get_batch_results_stream(batch.id, 0, Some("ALPHA".to_string()), None);
        let alpha_results: Vec<_> = stream.filter_map(|r| async { r.ok() }).collect().await;

        assert_eq!(alpha_results.len(), 1, "Should find 1 result for 'ALPHA'");
        assert_eq!(
            alpha_results[0].custom_id,
            Some("Alpha-Request".to_string())
        );
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    #[test_log::test]
    async fn test_retry_failed_requests_for_batch_retries_all(pool: sqlx::PgPool) {
        // Test that retry_failed_requests_for_batch retries both retriable and non-retriable errors

        let http_client = Arc::new(MockHttpClient::new());

        // First round: 2 retriable failures (429) and 2 non-retriable failures (400)
        for _ in 0..2 {
            http_client.add_response(
                "POST /v1/test",
                Ok(HttpResponse {
                    status: 429,
                    body: "rate limited".to_string(),
                }),
            );
        }
        for _ in 0..2 {
            http_client.add_response(
                "POST /v1/test",
                Ok(HttpResponse {
                    status: 400,
                    body: "bad request".to_string(),
                }),
            );
        }

        // Second round after retry: all succeed
        for _ in 0..4 {
            http_client.add_response(
                "POST /v1/test",
                Ok(HttpResponse {
                    status: 200,
                    body: r#"{"result":"success"}"#.to_string(),
                }),
            );
        }

        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("test-model".to_string(), 10);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            model_concurrency_limits,

            max_retries: Some(0), // No automatic retries
            stop_before_deadline_ms: None,
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
        };

        let manager = postgres_store(pool.clone(), &config).await;

        let templates = (0..4)
            .map(|i| fusillade::RequestTemplateInput {
                custom_id: Some(format!("req-{}", i)),
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: r#"{"test":"data"}"#.to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            })
            .collect();

        let file_id = manager
            .create_file("test-file".to_string(), None, templates)
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(fusillade::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "1h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .expect("Failed to create batch");
        mark_models_live_for_test(manager.as_ref(), &["test-model"]).await;

        let shutdown_token = CancellationToken::new();
        postgres_daemon(manager.clone(), http_client.clone(), config)
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Wait for all to fail
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let status = manager
                .get_batch_status(batch.id)
                .await
                .expect("Failed to get status");
            if status.failed_requests == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Verify we have 4 failed requests (2 retriable + 2 non-retriable)
        let status_before = manager
            .get_batch_status(batch.id)
            .await
            .expect("Failed to get status");
        assert_eq!(status_before.failed_requests, 4);

        // Retry all failed requests using bulk method
        let retried_count = manager
            .retry_failed_requests_for_batch(batch.id)
            .await
            .expect("Failed to retry batch");
        assert_eq!(retried_count, 4, "Should retry all 4 failed requests");

        // Wait for retries to complete successfully
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let status = manager
                .get_batch_status(batch.id)
                .await
                .expect("Failed to get status");
            if status.completed_requests == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        shutdown_token.cancel();

        // Verify all requests completed successfully after retry
        let status_after = manager
            .get_batch_status(batch.id)
            .await
            .expect("Failed to get status");
        assert_eq!(
            status_after.completed_requests, 4,
            "All 4 requests should complete after retry"
        );
        assert_eq!(status_after.failed_requests, 0, "No failed requests remain");
    }
}

mod queue_counts {
    use super::*;
    use fusillade::request::DaemonId;
    use uuid::Uuid;

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    async fn test_pending_queue_counts_by_model_and_completion_window(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = Arc::new(PostgresStore::with_client(
            TestDbPools::new(pool.clone()).await.unwrap(),
            http_client,
        ));

        // Batch 1: completion_window=24h, models: gpt-4 x2, gpt-3.5 x1
        let file_24h = manager
            .create_file(
                "file-24h".to_string(),
                None,
                vec![
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: "{}".to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "k".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: "{}".to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "k".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: "{}".to_string(),
                        model: "gpt-3.5".to_string(),
                        api_key: "k".to_string(),
                    },
                ],
            )
            .await
            .unwrap();

        let batch_24h = manager
            .create_batch(BatchInput {
                file_id: file_24h,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .unwrap();

        // Batch 2: completion_window=1h, models: gpt-4 x1, gpt-3.5 x2
        let file_1h = manager
            .create_file(
                "file-1h".to_string(),
                None,
                vec![
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: "{}".to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "k".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: "{}".to_string(),
                        model: "gpt-3.5".to_string(),
                        api_key: "k".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: "{}".to_string(),
                        model: "gpt-3.5".to_string(),
                        api_key: "k".to_string(),
                    },
                ],
            )
            .await
            .unwrap();

        let _batch_1h = manager
            .create_batch(BatchInput {
                file_id: file_1h,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "1h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .unwrap();

        // Move one 24h gpt-4 request out of pending
        let reqs_24h = manager.get_batch_requests(batch_24h.id).await.unwrap();
        let to_claim = reqs_24h
            .iter()
            .find(|r| r.data().model == "gpt-4")
            .expect("Expected a gpt-4 request")
            .id();
        let daemon_id = DaemonId(Uuid::new_v4());
        sqlx::query!(
            "UPDATE requests SET state = 'claimed', daemon_id = $2, claimed_at = NOW() WHERE id = $1",
            *to_claim as Uuid,
            *daemon_id as Uuid,
        )
        .execute(&pool)
        .await
        .unwrap();

        let counts = manager
            .get_pending_request_counts_by_model_and_window(
                &[
                    ("1h".to_string(), None, 3600),
                    ("24h".to_string(), None, 24 * 3600),
                ],
                &["pending".to_string()],
                &[],
                &ServiceTierFilter::Any,
                None,
                false,
            )
            .await
            .unwrap();

        let mut expected: std::collections::HashMap<
            String,
            std::collections::HashMap<String, i64>,
        > = std::collections::HashMap::new();
        expected
            .entry("gpt-3.5".to_string())
            .or_default()
            .insert("1h".to_string(), 2);
        // the 2x1hr requests and the 1x24h request will finish in the next 24h
        expected
            .entry("gpt-3.5".to_string())
            .or_default()
            .insert("24h".to_string(), 3);
        expected
            .entry("gpt-4".to_string())
            .or_default()
            .insert("1h".to_string(), 1);
        // both the 1hr and 24hr request will finish in the next 24h
        expected
            .entry("gpt-4".to_string())
            .or_default()
            .insert("24h".to_string(), 2);

        assert_eq!(counts, expected);
    }
}

mod service_tier {
    use super::*;

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    #[test_log::test]
    async fn test_populate_batch_sets_service_tier(pool: sqlx::PgPool) {
        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("test-model".to_string(), 10);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            model_concurrency_limits,
            ..Default::default()
        };

        let manager = postgres_store(pool.clone(), &config).await;

        let template = RequestTemplateInput {
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/v1/test".to_string(),
            body: r#"{"prompt":"test"}"#.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
        };

        // Create a batch with completion_window = "1h" → service_tier = "flex"
        let file_id_1h = manager
            .create_file("file-1h".to_string(), None, vec![template.clone()])
            .await
            .unwrap();
        let batch_1h = manager
            .create_batch(BatchInput {
                file_id: file_id_1h,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "1h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .unwrap();
        let requests_1h = manager.get_batch_requests(batch_1h.id).await.unwrap();
        assert_eq!(requests_1h.len(), 1);
        // Query the batched row's service_tier directly — get_batch_requests
        // doesn't surface service_tier, and we only need the one field here.
        let tier_1h: Option<String> =
            sqlx::query_scalar("SELECT service_tier FROM requests WHERE batch_id = $1")
                .bind(*batch_1h.id as uuid::Uuid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(tier_1h, Some("flex".to_string()));

        // Create a batch with completion_window = "24h" → service_tier = NULL
        let file_id_24h = manager
            .create_file("file-24h".to_string(), None, vec![template.clone()])
            .await
            .unwrap();
        let batch_24h = manager
            .create_batch(BatchInput {
                file_id: file_id_24h,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .unwrap();
        let requests_24h = manager.get_batch_requests(batch_24h.id).await.unwrap();
        assert_eq!(requests_24h.len(), 1);
        let tier_24h: Option<String> =
            sqlx::query_scalar("SELECT service_tier FROM requests WHERE batch_id = $1")
                .bind(*batch_24h.id as uuid::Uuid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(tier_24h, None);
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    #[test_log::test]
    async fn test_list_requests_filters_by_service_tier(pool: sqlx::PgPool) {
        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("test-model".to_string(), 10);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            model_concurrency_limits,
            ..Default::default()
        };

        let manager = postgres_store(pool.clone(), &config).await;

        // list_requests is scoped to batchless responses, so create one of
        // each tier via the batchless insert methods.
        manager
            .create_flex(fusillade::CreateFlexInput {
                request_id: uuid::Uuid::new_v4(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "test-model".to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "tier-user".to_string(),
            })
            .await
            .unwrap();
        manager
            .create_realtime(fusillade::CreateRealtimeInput {
                request_id: uuid::Uuid::new_v4(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "test-model".to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "tier-user".to_string(),
            })
            .await
            .unwrap();

        // Filter by service_tier = "flex" — only the flex row
        let flex_result = manager
            .list_requests(ListRequestsFilter {
                service_tiers: Some(vec!["flex".to_string()]),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(flex_result.data.len(), 1);
        assert_eq!(flex_result.data[0].service_tier, Some("flex".to_string()));

        // No filter — both requests
        let all_result = manager
            .list_requests(ListRequestsFilter::default())
            .await
            .unwrap();
        assert_eq!(all_result.data.len(), 2);
    }
}

/// Tests for the per-creditor rolling-window count queries that back the
/// control layer's unverified upload-volume cap (COR-481).
mod unverified_volume_counts {
    use super::*;

    /// Create a batch attributed to `creditor` with `completion_window` and `n`
    /// templated requests. The batch-status trigger sets `total_requests` to `n`
    /// as the requests are populated.
    async fn seed_batch(
        manager: &PostgresStore<TestDbPools>,
        creditor: &str,
        completion_window: &str,
        n: usize,
    ) {
        let templates: Vec<RequestTemplateInput> = (0..n)
            .map(|_| RequestTemplateInput {
                custom_id: None,
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: "{}".to_string(),
                model: "test-model".to_string(),
                api_key: "k".to_string(),
            })
            .collect();
        let file_id = manager
            .create_file(
                format!("f-{creditor}-{completion_window}-{n}"),
                None,
                templates,
            )
            .await
            .unwrap();
        manager
            .create_batch(BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: completion_window.to_string(),
                metadata: None,
                created_by: Some(creditor.to_string()),
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .unwrap();
    }

    async fn seed_flex(manager: &PostgresStore<TestDbPools>, creditor: &str) {
        manager
            .create_flex(fusillade::CreateFlexInput {
                request_id: uuid::Uuid::new_v4(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "test-model".to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: creditor.to_string(),
            })
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    #[test_log::test]
    async fn test_sum_owner_batch_requests_in_window(pool: sqlx::PgPool) {
        let manager = PostgresStore::with_client(
            TestDbPools::new(pool.clone()).await.unwrap(),
            Arc::new(MockHttpClient::new()),
        );

        // user-a: 3 requests in 24h, 2 in 1h. user-b: 5 in 24h (isolation).
        seed_batch(&manager, "user-a", "24h", 3).await;
        seed_batch(&manager, "user-a", "1h", 2).await;
        seed_batch(&manager, "user-b", "24h", 5).await;

        let past = chrono::Utc::now() - chrono::Duration::hours(48);
        let future = chrono::Utc::now() + chrono::Duration::hours(1);

        // Scoped by creditor AND completion_window.
        assert_eq!(
            manager
                .sum_owner_batch_requests_in_window("user-a", "24h", past, true)
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            manager
                .sum_owner_batch_requests_in_window("user-a", "1h", past, true)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            manager
                .sum_owner_batch_requests_in_window("user-b", "24h", past, true)
                .await
                .unwrap(),
            5,
            "other creditors must not bleed into the count"
        );

        // A future cutoff excludes the just-created batches.
        assert_eq!(
            manager
                .sum_owner_batch_requests_in_window("user-a", "24h", future, true)
                .await
                .unwrap(),
            0,
            "created_at < cutoff must be excluded"
        );

        // Unknown creditor / window → 0.
        assert_eq!(
            manager
                .sum_owner_batch_requests_in_window("nobody", "24h", past, true)
                .await
                .unwrap(),
            0
        );
    }

    #[sqlx::test(migrator = "fusillade_arsenal::MIGRATOR")]
    #[test_log::test]
    async fn test_count_owner_flex_requests_since(pool: sqlx::PgPool) {
        let manager = PostgresStore::with_client(
            TestDbPools::new(pool.clone()).await.unwrap(),
            Arc::new(MockHttpClient::new()),
        );

        // user-a: two flex requests + one realtime (must be excluded). Plus a 1h
        // BATCH (its requests are batched, not batchless flex) and a flex request
        // for user-b — both must be excluded from user-a's flex count.
        seed_flex(&manager, "user-a").await;
        seed_flex(&manager, "user-a").await;
        manager
            .create_realtime(fusillade::CreateRealtimeInput {
                request_id: uuid::Uuid::new_v4(),
                body: r#"{"prompt":"test"}"#.to_string(),
                model: "test-model".to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "user-a".to_string(),
            })
            .await
            .unwrap();
        seed_batch(&manager, "user-a", "1h", 4).await;
        seed_flex(&manager, "user-b").await;

        let past = chrono::Utc::now() - chrono::Duration::hours(48);
        let future = chrono::Utc::now() + chrono::Duration::hours(1);

        assert_eq!(
            manager
                .count_owner_flex_requests_since("user-a", past, true)
                .await
                .unwrap(),
            2,
            "counts only batchless flex rows: excludes realtime, batched 1h, and other creditors"
        );
        assert_eq!(
            manager
                .count_owner_flex_requests_since("user-b", past, true)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            manager
                .count_owner_flex_requests_since("user-a", future, true)
                .await
                .unwrap(),
            0,
            "created_at < cutoff must be excluded"
        );
    }
}
