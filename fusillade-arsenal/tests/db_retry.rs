use std::time::Duration;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::anyhow;
use fusillade_arsenal::{
    DbRetryConfig, PoolProvider, PostgresRequestManager, PostgresResponseStepManager, Storage,
    batch::{BatchInput, RequestTemplateInput},
    response_step::{CreateStepInput, ResponseStepStore, StepKind},
    retry_transient_db_errors,
};
use fusillade_core::FusilladeError;
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

#[derive(Clone)]
struct LazyPools(PgPool);

impl PoolProvider for LazyPools {
    fn read(&self) -> &PgPool {
        &self.0
    }

    fn write(&self) -> &PgPool {
        &self.0
    }
}

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .ok()
        .or_else(|| {
            std::fs::read_to_string(".env").ok().and_then(|contents| {
                contents.lines().find_map(|line| {
                    line.strip_prefix("DATABASE_URL=")
                        .map(|value| value.trim_matches('"').to_string())
                })
            })
        })
        .unwrap_or_else(|| "postgres://postgres:password@localhost:5432/fusillade".to_string())
}

#[test]
fn fixed_retry_config_expands_to_cadence() {
    let config = DbRetryConfig::fixed(3, Duration::from_millis(25));

    assert_eq!(
        config.retry_delays(),
        &[
            Duration::from_millis(25),
            Duration::from_millis(25),
            Duration::from_millis(25),
        ]
    );
}

#[test]
fn custom_retry_config_preserves_cadence() {
    let cadence = vec![
        Duration::from_millis(5),
        Duration::from_millis(25),
        Duration::from_millis(100),
    ];

    let config = DbRetryConfig::new(cadence.clone());

    assert_eq!(config.retry_delays(), cadence.as_slice());
}

#[tokio::test]
async fn retries_pool_timeout_errors_until_success() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let config = DbRetryConfig::fixed(2, Duration::ZERO);

    let result = retry_transient_db_errors(&config, {
        let attempts = Arc::clone(&attempts);
        move || {
            let attempts = Arc::clone(&attempts);
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err(FusilladeError::Other(anyhow!(
                        "pool timed out while waiting for an open connection"
                    )))
                } else {
                    Ok("connected")
                }
            }
        }
    })
    .await;

    assert_eq!(result.unwrap(), "connected");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn does_not_retry_non_transient_errors() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let config = DbRetryConfig::fixed(3, Duration::ZERO);

    let result: Result<(), FusilladeError> = retry_transient_db_errors(&config, {
        let attempts = Arc::clone(&attempts);
        move || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(FusilladeError::ValidationError("bad input".to_string()))
            }
        }
    })
    .await;

    assert!(matches!(result, Err(FusilladeError::ValidationError(_))));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn postgres_manager_stores_configured_db_retry_cadence() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://postgres:password@localhost/fusillade")
        .unwrap();
    let config = DbRetryConfig::new(vec![Duration::from_millis(1), Duration::from_millis(2)]);

    let manager = PostgresRequestManager::new(LazyPools(pool), Default::default())
        .with_db_retry_config(config.clone());

    assert_eq!(manager.db_retry_config(), &config);
}

#[tokio::test]
async fn postgres_manager_retries_pool_acquisition_for_sqlx_queries() {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(50))
        .connect(&database_url())
        .await
        .unwrap();
    let manager = PostgresRequestManager::new(LazyPools(pool.clone()), Default::default())
        .with_db_retry_config(DbRetryConfig::new(vec![
            Duration::from_millis(25),
            Duration::from_millis(50),
            Duration::from_millis(50),
        ]));

    let file_id = manager
        .create_file(
            format!("pool-retry-{}", Uuid::new_v4()),
            None,
            vec![RequestTemplateInput {
                custom_id: Some("pool-retry-template".to_string()),
                endpoint: "/v1/chat/completions".to_string(),
                method: "POST".to_string(),
                path: "/v1/chat/completions".to_string(),
                body: "{}".to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            }],
        )
        .await
        .unwrap();
    let batch = manager
        .create_batch(BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: Some(1),
        })
        .await
        .unwrap();

    let held_connection = pool.acquire().await.unwrap();
    let mark_failed = manager.mark_batch_failed(batch.id, "pool acquisition retry test");
    let release_connection = async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(held_connection);
    };

    let (result, _) = tokio::join!(mark_failed, release_connection);
    result.unwrap();

    let batch = manager.get_batch(batch.id).await.unwrap();
    assert!(batch.failed_at.is_some());
}

#[tokio::test]
async fn response_step_manager_retries_pool_acquisition_for_sqlx_queries() {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(50))
        .connect(&database_url())
        .await
        .unwrap();
    let manager = PostgresResponseStepManager::new(LazyPools(pool.clone())).with_db_retry_config(
        DbRetryConfig::new(vec![
            Duration::from_millis(25),
            Duration::from_millis(50),
            Duration::from_millis(50),
        ]),
    );

    let held_connection = pool.acquire().await.unwrap();
    let create_step = manager.create_step(CreateStepInput {
        id: Some(Uuid::new_v4()),
        request_id: None,
        prev_step_id: None,
        parent_step_id: None,
        step_kind: StepKind::ToolCall,
        step_sequence: 0,
        request_payload: serde_json::json!({ "test": "pool acquisition retry" }),
    });
    let release_connection = async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(held_connection);
    };

    let (result, _) = tokio::join!(create_step, release_connection);
    let step_id = result.unwrap();

    assert!(manager.get_step(step_id).await.unwrap().is_some());
}
