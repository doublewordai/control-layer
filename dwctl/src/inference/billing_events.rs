//! Durable scalar billing events for Fusillade dispatches.
//!
//! The reassembler observes its existing parsed value; cache accounting updates
//! the same typed snapshot during its existing rewrite. This layer never reads
//! or parses a body. PostgreSQL columns are bound directly, without a JSON envelope.
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

pub const EVENT_ID_KEY: &str = "billing-event-id";
pub const OWNER_ID_KEY: &str = "billing-owner-id";
pub const MODEL_KEY: &str = "billing-model";
pub const ATTEMPT_KEY: &str = "billing-retry-attempt";

/// Marker inserted only by the configured queue layer for a marked daemon request.
#[derive(Clone)]
pub(crate) struct CaptureUsage;

/// Body-free snapshot. Deliberately no Serialize/Deserialize implementation.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CapturedUsage {
    pub usage_present: bool,
    pub response_model: Option<String>,
    pub finish_reason: Option<String>,
    pub upstream_response_id: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub engine_cached_tokens: Option<i64>,
    pub cache_read_input_tokens: Option<i64>,
    pub cache_creation_5m_input_tokens: Option<i64>,
    pub cache_creation_1h_input_tokens: Option<i64>,
    pub cache_creation_24h_input_tokens: Option<i64>,
}

fn count(value: &Value, pointer: &str) -> Option<i64> {
    value.pointer(pointer).and_then(Value::as_i64).filter(|n| *n >= 0)
}

impl CapturedUsage {
    /// Borrow the value the reassembler already parsed, without parsing the wire body.
    pub(crate) fn from_response(response: &Value) -> Self {
        let usage = &response["usage"];
        let mut captured = Self {
            usage_present: usage.is_object(),
            response_model: response["model"].as_str().map(str::to_owned),
            finish_reason: response
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            upstream_response_id: response["id"].as_str().map(str::to_owned),
            prompt_tokens: count(usage, "/prompt_tokens").or_else(|| count(usage, "/input_tokens")),
            completion_tokens: count(usage, "/completion_tokens").or_else(|| count(usage, "/output_tokens")),
            total_tokens: count(usage, "/total_tokens"),
            reasoning_tokens: count(usage, "/completion_tokens_details/reasoning_tokens")
                .or_else(|| count(usage, "/output_tokens_details/reasoning_tokens")),
            engine_cached_tokens: count(usage, "/prompt_tokens_details/cached_tokens")
                .or_else(|| count(usage, "/input_tokens_details/cached_tokens")),
            ..Self::default()
        };
        captured.update_cache(response);
        captured
    }

    /// Called from the cache layer's existing parse/rewrite, preserving its exact
    /// clamping and scrubbing behavior without parsing the resulting body again.
    pub(crate) fn update_cache(&mut self, response: &Value) {
        let usage = &response["usage"];
        self.cache_read_input_tokens = count(usage, "/cache_read_input_tokens");
        self.cache_creation_5m_input_tokens = count(usage, "/cache_creation/ephemeral_5m_input_tokens");
        self.cache_creation_1h_input_tokens = count(usage, "/cache_creation/ephemeral_1h_input_tokens");
        self.cache_creation_24h_input_tokens = count(usage, "/cache_creation/ephemeral_24h_input_tokens");
    }
}

#[derive(Debug)]
struct EventContext {
    event_id: Uuid,
    request_id: Uuid,
    retry_attempt: i64,
    owner_id: Uuid,
    batch_id: Option<Uuid>,
    requested_model: String,
    completion_window: Option<String>,
    batch_created_at: Option<DateTime<Utc>>,
    started_at: DateTime<Utc>,
    billing_mode: String,
    request_path: String,
    request_method: String,
    custom_id: Option<String>,
}

impl EventContext {
    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let retry_attempt = get("x-fusillade-batch-billing-retry-attempt")?
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)?;
        Some(Self {
            event_id: get("x-fusillade-batch-billing-event-id")?.parse().ok()?,
            request_id: get("x-fusillade-request-id")?.parse().ok()?,
            retry_attempt,
            owner_id: get("x-fusillade-batch-billing-owner-id")?.parse().ok()?,
            batch_id: get("x-fusillade-batch-id").map(str::parse).transpose().ok()?,
            requested_model: get("x-fusillade-batch-billing-model")?.to_owned(),
            completion_window: get("x-fusillade-batch-completion-window").map(str::to_owned),
            batch_created_at: get("x-fusillade-batch-created-at").map(str::parse).transpose().ok()?,
            started_at: Utc::now(),
            billing_mode: get("x-fusillade-batch-billing-mode").unwrap_or("legacy").to_owned(),
            request_path: String::new(),
            request_method: String::new(),
            custom_id: get("x-fusillade-custom-id").map(str::to_owned),
        })
    }
}

/// Immutable scalar attribution captured before inference starts. It contains
/// no bearer credential and is reused unchanged for every publication retry.
#[derive(Debug, Default, sqlx::FromRow)]
struct BillingAttribution {
    api_key_id: Option<Uuid>,
    api_key_purpose: Option<String>,
    cap_scope_root: Option<Uuid>,
    model_id: Option<Uuid>,
}

#[derive(Clone)]
pub(crate) struct BillingEventQueue {
    pool: sqlx_pool_router::DynPools,
    config: crate::config::AnalyticsConfig,
}

impl BillingEventQueue {
    pub(crate) fn new(pool: impl sqlx_pool_router::PoolProvider) -> Self {
        Self {
            pool: sqlx_pool_router::DynPools::new(pool),
            config: crate::config::AnalyticsConfig {
                capture_fusillade_billing_events: true,
                ..Default::default()
            },
        }
    }

    pub(crate) fn with_config(mut self, config: crate::config::AnalyticsConfig) -> Self {
        self.config = config;
        self
    }

    async fn snapshot_attribution(&self, context: &EventContext, bearer_token: Option<&str>) -> Result<BillingAttribution, sqlx::Error> {
        sqlx::query_as(
            "SELECT attribution.id AS api_key_id, attribution.purpose AS api_key_purpose, attribution.cap_scope_root,
                    (SELECT id FROM deployed_models WHERE alias = $1 LIMIT 1) AS model_id
             FROM (SELECT 1) singleton
             LEFT JOIN LATERAL (
                 SELECT ak.id, ak.purpose, CASE WHEN root.spend_limit IS NOT NULL THEN root.id END AS cap_scope_root
                 FROM api_keys ak JOIN api_keys root ON root.id = COALESCE(ak.parent_api_key_id, ak.id)
                 WHERE ak.secret = $2 AND ak.user_id = $3 AND ak.is_deleted = false
             ) attribution ON TRUE",
        )
        .bind(&context.requested_model)
        .bind(bearer_token)
        .bind(context.owner_id)
        .fetch_one(&*self.pool.write())
        .await
    }

    async fn enqueue(&self, context: &EventContext, usage: &CapturedUsage, attribution: &BillingAttribution) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO fusillade_billing_events (
            event_id, request_id, retry_attempt, owner_id, batch_id, requested_model, response_model, upstream_response_id,
            completion_window, batch_created_at, started_at, completed_at, usage_present,
            prompt_tokens, completion_tokens, total_tokens, reasoning_tokens, engine_cached_tokens,
            cache_read_input_tokens, cache_creation_5m_input_tokens, cache_creation_1h_input_tokens, cache_creation_24h_input_tokens,
            api_key_id, api_key_purpose, cap_scope_root, model_id,
            billing_mode, captured_under_legacy_billing, request_path, request_method, custom_id, finish_reason
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,
                  $23,$24,$25,$26,$27,$27 = 'legacy',$28,$29,$30,$31)
        ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(context.event_id)
        .bind(context.request_id)
        .bind(context.retry_attempt)
        .bind(context.owner_id)
        .bind(context.batch_id)
        .bind(&context.requested_model)
        .bind(&usage.response_model)
        .bind(&usage.upstream_response_id)
        .bind(&context.completion_window)
        .bind(context.batch_created_at)
        .bind(context.started_at)
        .bind(Utc::now())
        .bind(usage.usage_present)
        .bind(usage.prompt_tokens)
        .bind(usage.completion_tokens)
        .bind(usage.total_tokens)
        .bind(usage.reasoning_tokens)
        .bind(usage.engine_cached_tokens)
        .bind(usage.cache_read_input_tokens)
        .bind(usage.cache_creation_5m_input_tokens)
        .bind(usage.cache_creation_1h_input_tokens)
        .bind(usage.cache_creation_24h_input_tokens)
        .bind(attribution.api_key_id)
        .bind(&attribution.api_key_purpose)
        .bind(attribution.cap_scope_root)
        .bind(attribution.model_id)
        .bind(&context.billing_mode)
        .bind(&context.request_path)
        .bind(&context.request_method)
        .bind(&context.custom_id)
        .bind(&usage.finish_reason)
        .execute(&*self.pool.write())
        .await?;
        metrics::counter!("dwctl_fusillade_billing_events_persisted_total").increment(1);
        Ok(())
    }
}

/// Outside cache accounting, inside translation and optional Outlet capture.
/// The completed response cannot reach the daemon until its event is durable.
pub(crate) async fn capture(State(queue): State<BillingEventQueue>, mut request: Request, next: Next) -> Response {
    if request
        .headers()
        .get(super::outbound_request::STREAM_MARKER_HEADER)
        .and_then(|v| v.to_str().ok())
        != Some("1")
    {
        return next.run(request).await;
    }
    let mode = request
        .headers()
        .get("x-fusillade-batch-billing-mode")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("legacy");
    if mode == "legacy" && !queue.config.capture_fusillade_billing_events {
        return next.run(request).await;
    }
    if !matches!(mode, "legacy" | "durable") {
        return (StatusCode::SERVICE_UNAVAILABLE, "Invalid billing mode").into_response();
    }
    let Some(mut context) = EventContext::from_headers(request.headers()) else {
        crate::background_error!(
            crate::metrics::errors::component::ANALYTICS,
            "billing_event_context_missing",
            Error,
            "Fusillade billing event metadata missing; dispatch refused before inference"
        );
        return (StatusCode::SERVICE_UNAVAILABLE, "Billing event metadata unavailable").into_response();
    };
    context.request_path = request.uri().path().to_owned();
    context.request_method = request.method().to_string();
    // Resolve attribution before inference: a key may be revoked or a model
    // alias reassigned while the stream is running. Publication must retain the
    // original billing identity, and retries must never re-read mutable metadata.
    let attribution = {
        let bearer_token = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match queue.snapshot_attribution(&context, bearer_token).await {
            Ok(attribution) => attribution,
            Err(_) => {
                crate::background_error!(crate::metrics::errors::component::ANALYTICS, "billing_attribution_snapshot_failed", Error,
                    request_id = %context.request_id, "Unable to snapshot billing attribution before inference");
                return (StatusCode::SERVICE_UNAVAILABLE, "Billing attribution unavailable").into_response();
            }
        }
    };
    request.extensions_mut().insert(CaptureUsage);
    let mut response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }
    let Some(usage) = response.extensions_mut().remove::<CapturedUsage>() else {
        // No new parsing fallback: an upstream that did not stream is explicitly
        // reported as unsupported, not silently accepted as durable capture.
        crate::background_error!(crate::metrics::errors::component::ANALYTICS, "billing_event_usage_missing", Error,
            request_id = %context.request_id, "No usage snapshot from Fusillade stream reader");
        return (StatusCode::BAD_GATEWAY, "Expected usage snapshot from inference stream").into_response();
    };
    let mut persisted = false;
    for attempt in 0..=queue.config.durable_billing.publish_max_retries {
        if queue.enqueue(&context, &usage, &attribution).await.is_ok() {
            persisted = true;
            break;
        }
        if attempt < queue.config.durable_billing.publish_max_retries {
            let delay = queue
                .config
                .durable_billing
                .publish_retry_delay_ms
                .saturating_mul(1u64 << attempt.min(10));
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
    }
    if !persisted {
        crate::background_error!(crate::metrics::errors::component::ANALYTICS, "billing_event_persist_failed", Error,
            request_id = %context.request_id, event_id = %context.event_id, "Failed to persist Fusillade billing event");
        return (StatusCode::SERVICE_UNAVAILABLE, "Unable to persist billing event").into_response();
    }
    response.headers_mut().insert(
        "x-fusillade-billing-event-id",
        context.event_id.to_string().parse().expect("UUID is a valid header"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::outbound_request::{OutboundConfig, StreamTimeouts, outbound_request_middleware};
    use axum::{Router, body::Body, middleware, routing::post};
    use sqlx::{PgPool, Row};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tower::ServiceExt;

    fn request(event: Uuid, logical: Uuid) -> Request {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("x-fusillade-batch-stream", "1")
            .header("x-fusillade-request-id", logical.to_string())
            .header("x-fusillade-batch-billing-event-id", event.to_string())
            .header("x-fusillade-batch-billing-owner-id", Uuid::nil().to_string())
            .header("x-fusillade-batch-billing-model", "requested-model")
            .header("x-fusillade-batch-billing-retry-attempt", "0")
            .body(Body::from(r#"{"model":"requested-model","messages":[]}"#))
            .unwrap()
    }

    fn app(pool: PgPool, body: &'static str, calls: Arc<AtomicUsize>) -> Router {
        Router::new()
            .route(
                "/v1/chat/completions",
                post(move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        ([("content-type", "text/event-stream")], body)
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(
                OutboundConfig {
                    timeouts: StreamTimeouts {
                        first_chunk: Duration::from_secs(1),
                        chunk: Duration::from_secs(1),
                        body: Duration::from_secs(1),
                    },
                },
                outbound_request_middleware,
            ))
            .layer(middleware::from_fn_with_state(BillingEventQueue::new(pool), capture))
    }

    const SSE: &str = "data: {\"id\":\"response-id\",\"model\":\"served-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":7,\"total_tokens\":17,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\ndata: [DONE]\n\n";

    #[sqlx::test]
    async fn completion_is_durable_before_response_and_survives_writer_drop(pool: PgPool) {
        let event = Uuid::new_v4();
        let options = pool.connect_options();
        let response = app(pool.clone(), SSE, Arc::default())
            .oneshot(request(event, Uuid::new_v4()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // The success headers are not released until the insert has committed.
        pool.close().await;
        let reopened = PgPool::connect_with((*options).clone()).await.unwrap();
        let row = sqlx::query("SELECT completion_tokens, engine_cached_tokens, requested_model, response_model, captured_under_legacy_billing, processed_at FROM fusillade_billing_events WHERE event_id=$1")
            .bind(event)
            .fetch_one(&reopened)
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>("completion_tokens"), 7);
        assert_eq!(row.get::<i64, _>("engine_cached_tokens"), 3);
        assert_eq!(row.get::<String, _>("requested_model"), "requested-model");
        assert_eq!(row.get::<String, _>("response_model"), "served-model");
        assert!(row.get::<bool, _>("captured_under_legacy_billing"));
        assert!(row.get::<Option<DateTime<Utc>>, _>("processed_at").is_none());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "hello");
        reopened.close().await;
    }

    #[sqlx::test]
    async fn publish_replay_is_idempotent_but_physical_attempts_are_distinct(pool: PgPool) {
        let req = request(Uuid::new_v4(), Uuid::new_v4());
        let mut context = EventContext::from_headers(req.headers()).unwrap();
        let usage = CapturedUsage {
            usage_present: true,
            completion_tokens: Some(7),
            ..Default::default()
        };
        let queue = BillingEventQueue::new(pool.clone());
        queue.enqueue(&context, &usage, &BillingAttribution::default()).await.unwrap();
        queue.enqueue(&context, &usage, &BillingAttribution::default()).await.unwrap();
        context.event_id = Uuid::new_v4();
        context.retry_attempt = 1;
        queue.enqueue(&context, &usage, &BillingAttribution::default()).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM fusillade_billing_events WHERE request_id=$1")
            .bind(context.request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[sqlx::test]
    async fn key_attribution_survives_key_deletion_without_retaining_secret(pool: PgPool) {
        use crate::test::utils::{create_test_api_key_for_user, create_test_user};
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        sqlx::query("UPDATE api_keys SET spend_limit=10 WHERE id=$1")
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();
        let req = request(Uuid::new_v4(), Uuid::new_v4());
        let mut context = EventContext::from_headers(req.headers()).unwrap();
        context.owner_id = user.id;
        let queue = BillingEventQueue::new(pool.clone());
        let attribution = queue.snapshot_attribution(&context, Some(&key.secret)).await.unwrap();
        sqlx::query("UPDATE api_keys SET is_deleted=true WHERE id=$1")
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();
        queue.enqueue(&context, &CapturedUsage::default(), &attribution).await.unwrap();
        let row = sqlx::query("SELECT api_key_id, cap_scope_root FROM fusillade_billing_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<Uuid, _>("api_key_id"), key.id);
        assert_eq!(row.get::<Uuid, _>("cap_scope_root"), key.id);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM credits_transactions WHERE user_id=$1 AND transaction_type='usage'")
                .bind(user.id)
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "producer must not charge"
        );
    }

    #[sqlx::test]
    async fn attribution_is_snapshotted_before_stream_even_when_metadata_changes(pool: PgPool) {
        use crate::test::utils::{create_test_api_key_for_user, create_test_deployment, create_test_endpoint, create_test_user};
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        create_test_endpoint(&pool, "test", user.id).await;
        let model = create_test_deployment(&pool, user.id, "original-model", "requested-model").await;
        sqlx::query("UPDATE api_keys SET purpose='batch', spend_limit=10 WHERE id=$1")
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();
        let handler_pool = pool.clone();
        let key_id = key.id;
        let model_id = model.id;
        let router = Router::new()
            .route(
                "/v1/chat/completions",
                post(move || {
                    let pool = handler_pool.clone();
                    async move {
                        // Inference has started: revoke/change the key and reassign its
                        // model alias before the stream's usage is published.
                        sqlx::query("UPDATE api_keys SET is_deleted=true, purpose='realtime', spend_limit=NULL WHERE id=$1")
                            .bind(key_id)
                            .execute(&pool)
                            .await
                            .unwrap();
                        sqlx::query("UPDATE deployed_models SET alias='renamed-model' WHERE id=$1")
                            .bind(model_id)
                            .execute(&pool)
                            .await
                            .unwrap();
                        create_test_deployment(&pool, user.id, "replacement-model", "requested-model").await;
                        ([("content-type", "text/event-stream")], SSE)
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(
                OutboundConfig {
                    timeouts: StreamTimeouts {
                        first_chunk: Duration::from_secs(1),
                        chunk: Duration::from_secs(1),
                        body: Duration::from_secs(1),
                    },
                },
                outbound_request_middleware,
            ))
            .layer(middleware::from_fn_with_state(BillingEventQueue::new(pool.clone()), capture));
        let event_id = Uuid::new_v4();
        let mut req = request(event_id, Uuid::new_v4());
        req.headers_mut()
            .insert("x-fusillade-batch-billing-owner-id", user.id.to_string().parse().unwrap());
        req.headers_mut()
            .insert(axum::http::header::AUTHORIZATION, format!("Bearer {}", key.secret).parse().unwrap());
        let response = router.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let row =
            sqlx::query("SELECT api_key_id, api_key_purpose, cap_scope_root, model_id FROM fusillade_billing_events WHERE event_id=$1")
                .bind(event_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.get::<Uuid, _>("api_key_id"), key_id);
        assert_eq!(row.get::<String, _>("api_key_purpose"), "batch");
        assert_eq!(row.get::<Uuid, _>("cap_scope_root"), key_id);
        assert_eq!(row.get::<Uuid, _>("model_id"), model_id);
    }

    #[sqlx::test]
    async fn failed_attribution_snapshot_refuses_before_inference(pool: PgPool) {
        pool.close().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let response = app(pool, SSE, calls.clone())
            .oneshot(request(Uuid::new_v4(), Uuid::new_v4()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[sqlx::test]
    async fn publication_failure_after_snapshot_cannot_report_success(pool: PgPool) {
        let handler_pool = pool.clone();
        let router = Router::new()
            .route(
                "/v1/chat/completions",
                post(move || {
                    let pool = handler_pool.clone();
                    async move {
                        // Attribution was read successfully before entering inference;
                        // publication now fails after a completed response exists.
                        pool.close().await;
                        let mut response = StatusCode::OK.into_response();
                        response.extensions_mut().insert(CapturedUsage {
                            usage_present: true,
                            prompt_tokens: Some(10),
                            completion_tokens: Some(7),
                            ..Default::default()
                        });
                        response
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(BillingEventQueue::new(pool), capture));
        let response = router.oneshot(request(Uuid::new_v4(), Uuid::new_v4())).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.headers().get("x-fusillade-billing-event-id").is_none());
    }

    #[sqlx::test]
    async fn missing_context_refuses_before_inference_and_realtime_bypasses(pool: PgPool) {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = app(pool.clone(), SSE, calls.clone());
        let mut req = request(Uuid::new_v4(), Uuid::new_v4());
        req.headers_mut().remove("x-fusillade-batch-billing-owner-id");
        let response = router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut req = request(Uuid::new_v4(), Uuid::new_v4());
        req.headers_mut().remove("x-fusillade-batch-stream");
        let response = router.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fusillade_billing_events")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }

    #[sqlx::test]
    async fn missing_usage_is_reconcilable_and_provider_errors_do_not_enqueue(pool: PgPool) {
        const NO_USAGE: &str = "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let response = app(pool.clone(), NO_USAGE, Arc::default())
            .oneshot(request(Uuid::new_v4(), Uuid::new_v4()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let row = sqlx::query("SELECT usage_present, completion_tokens FROM fusillade_billing_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!row.get::<bool, _>("usage_present"));
        assert_eq!(row.get::<Option<i64>, _>("completion_tokens"), None);
        let response = app(pool.clone(), "data: {\"error\":{\"code\":503}}\n\n", Arc::default())
            .oneshot(request(Uuid::new_v4(), Uuid::new_v4()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fusillade_billing_events")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    #[sqlx::test]
    async fn unexpected_nonstreaming_success_is_not_silently_accepted(pool: PgPool) {
        let router = Router::new()
            .route(
                "/v1/chat/completions",
                post(|| async {
                    (
                        [("content-type", "application/json")],
                        r#"{"usage":{"prompt_tokens":10,"completion_tokens":7}}"#,
                    )
                }),
            )
            .layer(middleware::from_fn_with_state(
                OutboundConfig {
                    timeouts: StreamTimeouts {
                        first_chunk: Duration::from_secs(1),
                        chunk: Duration::from_secs(1),
                        body: Duration::from_secs(1),
                    },
                },
                outbound_request_middleware,
            ))
            .layer(middleware::from_fn_with_state(BillingEventQueue::new(pool.clone()), capture));
        let response = router.oneshot(request(Uuid::new_v4(), Uuid::new_v4())).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fusillade_billing_events")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }

    #[test]
    fn captures_responses_usage_without_conflating_missing_and_zero() {
        let usage = CapturedUsage::from_response(&serde_json::json!({"usage": {
            "input_tokens": 10, "output_tokens": 0, "output_tokens_details": {"reasoning_tokens": 0},
            "input_tokens_details": {"cached_tokens": 4}
        }}));
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(0));
        assert_eq!(usage.total_tokens, None);
        assert_eq!(usage.engine_cached_tokens, Some(4));
    }
}
