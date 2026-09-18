//! Durable analytics outbox writer and batched projector.
//!
//! Completed captures are inserted into PostgreSQL immediately. [`AnalyticsBatcher`]
//! then enriches and projects already-durable rows in batches.
//!
//! # Architecture
//!
//! ```text
//! Request → AnalyticsHandler → INSERT analytics_outbox
//!                                      ↓
//!                              claim + project (transaction)
//!                                - Key ID → user lookup
//!                                - Model → pricing lookup
//!                                                - INSERT http_analytics
//!                                                - INSERT credit_transactions
//!                                                - DELETE analytics_outbox
//! ```
//!
//! # Key Design Decisions
//!
//! - **Durable first hand-off**: The handler writes an unenriched, content-free
//!   `RawAnalyticsRecord` directly to PostgreSQL. Bearer tokens are excluded.
//!   The projector also accepts the enriched payload written by release 11.12.0
//!   so rolling upgrades can drain rows created by either version.
//! - **Batch enrichment**: Key and pricing lookups happen only after the row is durable.
//! - **Transactional projection**: Analytics, credit inserts and outbox deletion happen in
//!   a single transaction. Either all succeed or the durable row remains for retry.

use crate::config::{Config, ONWARDS_CONFIG_CHANGED_CHANNEL};

use crate::db::models::api_keys::ApiKeyPurpose;
use crate::metrics::MetricsRecorder;
use crate::metrics::errors::component::ANALYTICS_BATCHER;
use crate::pricing::{
    CacheTariffRow, ModelInfo, TariffInfo, TokenCounts, charged_cost, clamp_implicit_read_multiplier, find_best_tariff, list_price,
    resolve_cache_multipliers,
};
use crate::request_logging::serializers::{HttpAnalyticsRow, RequestParams};
use chrono::{DateTime, Utc};
use metrics::{counter, histogram};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

/// Raw analytics record durably written to the outbox before enrichment.
///
/// This contains only data that can be extracted from the request/response
/// without any database lookups. Enrichment happens in the batcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawAnalyticsRecord {
    // === Core metrics (from request/response) ===
    pub instance_id: Uuid,
    pub correlation_id: i64,
    pub timestamp: DateTime<Utc>,
    pub method: String,
    pub uri: String,
    pub request_model: Option<String>,
    pub response_model: Option<String>,
    pub status_code: i32,
    pub duration_ms: i64,
    pub duration_to_first_byte_ms: Option<i64>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
    // Cached-input split. `prompt_tokens` stays the full input count; these break
    // out the cached portion so the batcher can apply the cache multipliers. The split usually
    // reconciles to `prompt_tokens` but isn't guaranteed to (tokenizer drift; cost logic floors
    // uncached at 0 — see compute_cost).
    pub cache_read_input_tokens: i64,
    pub cache_creation_5m_input_tokens: i64,
    pub cache_creation_1h_input_tokens: i64,
    pub cache_creation_24h_input_tokens: i64,
    pub response_type: String,
    /// Why the model stopped — `stop`, `length`, `tool_calls`, ... `None` when the
    /// response shape has no such concept (embeddings, the Responses API) or it could not
    /// be read. `tool_calls` is what makes CLIENT-side tool loops visible.
    ///
    /// Extracted by `serializers::extract_finish_reason`, like every other field on this
    /// struct that comes off the payload. The batcher deliberately never sees a request or
    /// response body: `AnalyticsHandler::handle_response` parses once via
    /// `parse_ai_response` (which also does SSE reassembly and decompression) and sends
    /// only flat scalars down the channel. Re-deriving anything payload-shaped here would
    /// mean a second parse on the write path and would put prompt/response bodies into the
    /// queue — see the module docs on `serializers`.
    pub finish_reason: Option<String>,
    /// Inbound `User-Agent`, truncated to 256 chars — which CLIENT the caller used (SDK,
    /// CLI, curl, own code), as opposed to `uri` (which protocol) and `request_origin`
    /// (which dispatch path). Read straight off the request headers, not the payload.
    pub user_agent: Option<String>,
    /// The upstream's own cached-prompt count (`usage.prompt_tokens_details.cached_tokens`).
    /// Observational: distinct from `cache_read_input_tokens`, which is dwctl's cache layer
    /// and is what gets priced. See `serializers::extract_engine_cached_tokens`.
    pub engine_cached_tokens: Option<i64>,
    /// Provenance of the billed cache read: "module" (dwctl's classifier) or "engine"
    /// (upstream hit passed through as an implicit discount). `None` when the billed read
    /// is zero. See `serializers::extract_cache_read_source`.
    pub cache_read_source: Option<String>,
    /// Content-free request parameters (stream, max_tokens, sampling, message and tool
    /// counts). See `serializers::RequestParams`.
    pub request_params: RequestParams,
    pub server_address: String,
    pub server_port: u16,
    /// URL of the upstream that served the request (onwards `ServedBy`
    /// extension) — per-component attribution for composite models.
    pub served_by: Option<String>,

    // === Auth ===
    /// Stable key identity attached by Onwards after successful authentication.
    pub api_key_id: Option<Uuid>,

    // === Fusillade batch metadata (from headers) ===
    pub fusillade_batch_id: Option<Uuid>,
    pub fusillade_request_id: Option<Uuid>,
    pub custom_id: Option<String>,
    /// The completion window (e.g., "24h") - used for batch pricing lookup
    pub batch_completion_window: Option<String>,
    /// The batch creation timestamp (from x-fusillade-batch-created-at header)
    /// Used to look up tariff pricing as of batch creation time, not processing time
    pub batch_created_at: Option<DateTime<Utc>>,
    /// The request_source from batch metadata
    pub batch_request_source: String,

    // === Tracing ===
    /// OpenTelemetry trace ID for correlation with Tempo
    pub trace_id: Option<String>,
}

/// Enriched data resolved during batch processing
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnrichedRecord {
    raw: RawAnalyticsRecord,
    user_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    access_source: String,
    api_key_purpose: Option<ApiKeyPurpose>,
    provider_name: Option<String>,
    input_price_per_token: Option<Decimal>,
    output_price_per_token: Option<Decimal>,
    /// The cache-adjusted request cost: uncached tokens at list price, cache
    /// reads at the read multiplier, per-tier creation at its write multiplier, plus output.
    /// `None` when the model has no pricing (→ no analytics cost, no ledger row). Written to
    /// `http_analytics.total_cost` AND used as the billed `credits_transactions.amount`.
    total_cost: Option<Decimal>,
    /// The un-discounted list price = `prompt·input + completion·output` (no cache
    /// adjustment) — `http_analytics.uncached_cost`, for savings = `uncached_cost − total_cost`.
    /// `None` under the same no-pricing condition as `total_cost`, so the two are NULL in
    /// lockstep. Equals `total_cost` whenever the request cached nothing.
    uncached_cost: Option<Decimal>,
    /// The spending-cap scope this request's cost folds into: the id of the
    /// capped root key — `COALESCE(parent_api_key_id, id)` of the billing key,
    /// but only when that root currently has `spend_limit` set. `None` for the
    /// uncapped 99% (no checkpoint row is ever written for them). Removing a
    /// cap therefore stops folding; when a cap is later (re-)enabled, the
    /// cap-set path MUST reset the scope's checkpoint window (zero
    /// window_spend, fresh window_started_at) or the scope inherits old spend
    /// and can exhaust immediately — this includes caps set via manual SQL
    /// while no API path exists yet.
    cap_scope_root: Option<Uuid>,
}

/// Per-key lookup result used during enrichment.
struct KeyLookup {
    user_id: Uuid,
    api_key_id: Uuid,
    purpose: ApiKeyPurpose,
    /// See `EnrichedRecord::cap_scope_root`.
    cap_scope_root: Option<Uuid>,
}

/// The price-relevant token counts for a record, in the shape [`crate::pricing`] works in.
///
/// `prompt_tokens` is the TOTAL input including the cached share — see
/// [`crate::pricing::TokenCounts`] for why that invariant matters.
impl From<&RawAnalyticsRecord> for TokenCounts {
    fn from(raw: &RawAnalyticsRecord) -> Self {
        Self {
            prompt: raw.prompt_tokens,
            completion: raw.completion_tokens,
            cache_read: raw.cache_read_input_tokens,
            cache_creation_5m: raw.cache_creation_5m_input_tokens,
            cache_creation_1h: raw.cache_creation_1h_input_tokens,
            cache_creation_24h: raw.cache_creation_24h_input_tokens,
        }
    }
}

/// The un-discounted list price (`http_analytics.uncached_cost`): the full input + output
/// at base rates, ignoring any cache split. `None` under the same no-pricing condition as
/// [`crate::pricing::compute_total_cost`], so the two columns are NULL in lockstep. Equals
/// `total_cost` whenever the request cached nothing.
fn compute_list_price(raw: &RawAnalyticsRecord, input_price: Option<Decimal>, output_price: Option<Decimal>) -> Option<Decimal> {
    list_price(raw.prompt_tokens, raw.completion_tokens, input_price, output_price)
}

/// Direct writer for the durable analytics handoff. Each completed capture is
/// inserted immediately; enrichment and final writes remain batched later.
#[derive(Clone)]
pub struct AnalyticsOutboxWriter {
    destination: AnalyticsOutboxDestination,
}

#[derive(Clone)]
enum AnalyticsOutboxDestination {
    Database {
        pool: sqlx_pool_router::DynPools,
        projector_notify: Arc<Notify>,
    },
    #[cfg(test)]
    Channel(tokio::sync::mpsc::Sender<RawAnalyticsRecord>),
}

impl AnalyticsOutboxWriter {
    pub async fn publish(&self, record: RawAnalyticsRecord) -> anyhow::Result<()> {
        #[cfg(test)]
        if let AnalyticsOutboxDestination::Channel(sender) = &self.destination {
            sender.send(record).await?;
            return Ok(());
        }

        let (pool, projector_notify) = match &self.destination {
            AnalyticsOutboxDestination::Database { pool, projector_notify } => (pool, projector_notify),
            #[cfg(test)]
            AnalyticsOutboxDestination::Channel(_) => unreachable!("test channel is handled above"),
        };
        let payload = serde_json::to_value(&record)?;
        sqlx::query(
            r#"
            INSERT INTO analytics_outbox (instance_id, correlation_id, payload)
            VALUES ($1, $2, $3)
            ON CONFLICT (instance_id, correlation_id)
            DO UPDATE SET payload = EXCLUDED.payload
            "#,
        )
        .bind(record.instance_id)
        .bind(record.correlation_id)
        .bind(payload)
        .execute(&pool.write())
        .await?;

        counter!("dwctl_analytics_outbox_published_total").increment(1);
        projector_notify.notify_one();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(sender: tokio::sync::mpsc::Sender<RawAnalyticsRecord>) -> Self {
        Self {
            destination: AnalyticsOutboxDestination::Channel(sender),
        }
    }
}

/// Background projector for already-durable analytics rows.
pub struct AnalyticsBatcher<M = crate::metrics::GenAiMetrics>
where
    M: MetricsRecorder + Clone + Send + Sync + 'static,
{
    /// Live provider (not a pinned pool): survives runtime pool swaps.
    pool: sqlx_pool_router::DynPools,
    metrics_recorder: Option<M>,
    batch_size: usize,
    projector_notify: Arc<Notify>,
    /// In-process wake signal for the usage-refresh daemon. Nudged after every
    /// successful batch write so the daemon folds the just-written rows into
    /// `user_model_usage_daily`. `None` disables the nudge (tests, refresh daemon off).
    usage_refresh_notify: Option<Arc<Notify>>,
}

impl<M> AnalyticsBatcher<M>
where
    M: MetricsRecorder + Clone + Send + Sync + 'static,
{
    /// Creates the durable writer and its background projector.
    ///
    /// # Arguments
    ///
    /// * `pool` - Database connection pool for batch writes
    /// * `config` - Application configuration (includes batch settings)
    /// * `metrics_recorder` - Optional metrics recorder for Prometheus metrics
    ///
    /// # Returns
    ///
    /// A tuple of (projector, writer) where the writer is used by AnalyticsHandler.
    pub fn new(pool: impl sqlx_pool_router::PoolProvider, config: Config, metrics_recorder: Option<M>) -> (Self, AnalyticsOutboxWriter) {
        let pool = sqlx_pool_router::DynPools::new(pool);
        // A zero limit would leave every durable row unprojected forever.
        let batch_size = config.analytics.batch_size.max(1);
        let projector_notify = Arc::new(Notify::new());

        let batcher = Self {
            pool: pool.clone(),
            metrics_recorder,
            batch_size,
            projector_notify: projector_notify.clone(),
            usage_refresh_notify: None,
        };

        let writer = AnalyticsOutboxWriter {
            destination: AnalyticsOutboxDestination::Database { pool, projector_notify },
        };

        (batcher, writer)
    }

    /// Attach the usage-refresh daemon's wake signal. After each successful batch
    /// write the batcher calls `notify_one()` on it, so the daemon coalesces the
    /// nudges and drains the `user_model_usage_daily` cursor. Off by default.
    #[must_use]
    pub fn with_usage_refresh_notify(mut self, notify: Arc<Notify>) -> Self {
        self.usage_refresh_notify = Some(notify);
        self
    }

    /// Projects durable rows immediately after publication. The periodic wake
    /// also recovers rows left behind by a process restart or another instance.
    pub async fn run(self, shutdown_token: CancellationToken) {
        info!(max_batch_size = self.batch_size, "Analytics outbox projector started");
        let mut outbox_tick = tokio::time::interval(std::time::Duration::from_secs(30));
        outbox_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            let observe = tokio::select! {
                _ = shutdown_token.cancelled() => {
                    let drained = loop {
                        match self.project_outbox_batch().await {
                            Ok(0) => break true,
                            Ok(_) => {}
                            Err(error) => {
                                crate::background_error!(
                                    ANALYTICS_BATCHER,
                                    "outbox_project",
                                    Error,
                                    error = %error,
                                    "Failed to drain analytics outbox during shutdown; rows remain durable"
                                );
                                counter!("dwctl_analytics_outbox_projection_total", "result" => "error").increment(1);
                                break false;
                            }
                        }
                    };
                    info!(drained, "Analytics outbox projector shutdown complete");
                    break;
                }
                _ = outbox_tick.tick() => {
                    true
                }
                _ = self.projector_notify.notified() => {
                    false
                }
            };

            loop {
                match self.project_outbox_batch().await {
                    Ok(0) => break,
                    Ok(projected) if projected < self.batch_size => break,
                    Ok(_) => tokio::task::yield_now().await,
                    Err(error) => {
                        crate::background_error!(
                            ANALYTICS_BATCHER,
                            "outbox_project",
                            Error,
                            error = %error,
                            "Failed to project analytics outbox batch; rows remain durable"
                        );
                        counter!("dwctl_analytics_outbox_projection_total", "result" => "error").increment(1);
                        break;
                    }
                }
            }

            if observe {
                self.observe_outbox().await;
            }
        }
    }

    /// Claim, apply and delete one durable batch. The row locks, analytics and
    /// credit writes, and deletion all share the same transaction; a crash or
    /// error rolls everything back and makes the outbox rows claimable again.
    async fn project_outbox_batch(&self) -> anyhow::Result<usize> {
        let mut tx = self.pool.write().begin().await?;
        let rows = sqlx::query(
            r#"
            SELECT id, payload, created_at
            FROM analytics_outbox
            ORDER BY id
            LIMIT $1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(i64::try_from(self.batch_size).unwrap_or(i64::MAX))
        .fetch_all(&mut *tx)
        .await?;

        if rows.is_empty() {
            tx.commit().await?;
            return Ok(0);
        }

        let mut ids = Vec::with_capacity(rows.len());
        let mut raw_records = Vec::with_capacity(rows.len());
        let mut records = Vec::with_capacity(rows.len());
        let mut oldest = Utc::now();
        for row in rows {
            ids.push(row.try_get::<i64, _>("id")?);
            let mut payload = row.try_get::<serde_json::Value, _>("payload")?;
            if payload.get("raw").is_some() {
                // Release 11.12.0 wrote EnrichedRecord. Its nested raw record
                // predates api_key_id, so copy the already-resolved top-level
                // value into the new field before deserializing it.
                let api_key_id = payload.get("api_key_id").cloned().unwrap_or(serde_json::Value::Null);
                if let Some(raw) = payload.get_mut("raw").and_then(serde_json::Value::as_object_mut) {
                    raw.entry("api_key_id").or_insert(api_key_id);
                }
                records.push(serde_json::from_value::<EnrichedRecord>(payload)?);
            } else {
                raw_records.push(serde_json::from_value::<RawAnalyticsRecord>(payload)?);
            }
            oldest = oldest.min(row.try_get::<DateTime<Utc>, _>("created_at")?);
        }

        records.extend(self.enrich_batch(&mut tx, &raw_records).await?);

        self.write_batch_in_transaction(&mut tx, &records).await?;
        sqlx::query("DELETE FROM analytics_outbox WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        if let Some(notify) = &self.usage_refresh_notify {
            notify.notify_one();
        }

        let now = Utc::now();
        for record in &records {
            let total_ms = now.signed_duration_since(record.raw.timestamp).num_milliseconds();
            let lag_ms = total_ms - record.raw.duration_ms;
            histogram!("dwctl_analytics_lag_seconds").record(lag_ms as f64 / 1000.0);
            if let Some(ref recorder) = self.metrics_recorder {
                let row = self.enriched_to_row(record);
                recorder.record_from_analytics(&row).await;
            }
        }

        let projected = records.len();
        counter!("dwctl_analytics_outbox_projection_total", "result" => "success").increment(projected as u64);
        histogram!("dwctl_analytics_outbox_oldest_projected_age_seconds")
            .record(now.signed_duration_since(oldest).num_milliseconds().max(0) as f64 / 1000.0);
        Ok(projected)
    }

    async fn observe_outbox(&self) {
        let observation = sqlx::query(
            r#"
            SELECT COUNT(*)::bigint AS depth,
                   COALESCE(EXTRACT(EPOCH FROM (NOW() - MIN(created_at))), 0)::float8 AS oldest_age_seconds
            FROM analytics_outbox
            "#,
        )
        // The outbox is writer-owned mutable state. Reading it from a replica
        // could report stale depth/age and create a false stalled-outbox alert.
        .fetch_one(&self.pool.write())
        .await;

        match observation {
            Ok(row) => {
                let depth = row.try_get::<i64, _>("depth").unwrap_or_default();
                let age = row.try_get::<f64, _>("oldest_age_seconds").unwrap_or_default();
                metrics::gauge!("dwctl_analytics_outbox_depth").set(depth as f64);
                metrics::gauge!("dwctl_analytics_outbox_oldest_age_seconds").set(age);
            }
            Err(error) => {
                debug!(%error, "Failed to observe analytics outbox");
            }
        }
    }

    /// Batch enrich raw records with user info and pricing.
    ///
    /// Performs batched queries inside the projector transaction:
    /// 1. API-key ID → (user_id, purpose) lookup
    /// 2. Model alias → (model_id, provider, tariffs) lookup
    #[tracing::instrument(skip_all)]
    async fn enrich_batch(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        buffer: &[RawAnalyticsRecord],
    ) -> Result<Vec<EnrichedRecord>, sqlx::Error> {
        let key_ids: Vec<Uuid> = buffer
            .iter()
            .filter_map(|r| r.api_key_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        // Collect unique model aliases
        let models: Vec<&str> = buffer
            .iter()
            .filter_map(|r| r.request_model.as_deref())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let key_map = if !key_ids.is_empty() {
            self.batch_lookup_users_by_ids(tx, &key_ids).await?
        } else {
            HashMap::new()
        };

        // Batch lookup: model alias → (model_id, provider_name, tariffs)
        let model_map = if !models.is_empty() {
            self.batch_lookup_models_with_tariffs(tx, &models).await?
        } else {
            HashMap::new()
        };

        // Batch lookup: model alias → cache tariffs (per tier), for the cache multipliers.
        let cache_tariff_map = if !models.is_empty() {
            self.batch_lookup_cache_tariffs(tx, &models).await?
        } else {
            HashMap::new()
        };

        // Enrich each record
        let mut enriched = Vec::with_capacity(buffer.len());
        for raw in buffer.iter().cloned() {
            let key = raw.api_key_id.and_then(|id| key_map.get(&id));
            let (user_id, api_key_id, access_source, api_key_purpose, cap_scope_root) = if let Some(key) = key {
                (
                    Some(key.user_id),
                    Some(key.api_key_id),
                    "api_key".to_string(),
                    Some(key.purpose.clone()),
                    key.cap_scope_root,
                )
            } else if raw.api_key_id.is_some() {
                (None, raw.api_key_id, "unknown_api_key".to_string(), None, None)
            } else {
                (None, None, "unauthenticated".to_string(), None, None)
            };

            if raw.request_model.is_none() && (raw.completion_tokens > 0 || raw.prompt_tokens > 0) {
                error!(
                    correlation_id = raw.correlation_id,
                    response_model = ?raw.response_model,
                    completion_tokens = raw.completion_tokens,
                    prompt_tokens = raw.prompt_tokens,
                    uri = %raw.uri,
                    "request_model is None but response has token usage — record will not be billed"
                );
            }

            // Price batch requests as of batch creation, not processing time.
            let pricing_timestamp = raw.batch_created_at.unwrap_or(raw.timestamp);

            let (provider_name, input_price, output_price) = if let Some(ref model_alias) = raw.request_model {
                if let Some(model_info) = model_map.get(model_alias) {
                    // Find best matching tariff
                    let (input, output) = find_best_tariff(
                        &model_info.tariffs,
                        api_key_purpose.as_ref(),
                        raw.batch_completion_window.as_deref(),
                        pricing_timestamp,
                    );

                    (Some(model_info.provider_name.clone()), input, output)
                } else {
                    (None, None, None)
                }
            } else {
                (None, None, None)
            };

            // Resolve cache multipliers from the tariff row valid at inference time. `None`
            // for the normal non-cache model (no tariff) and for the dead anomaly path below.
            // Resolve the cache multipliers from the tariff row valid at inference time. `None`
            // means this model was NOT dwctl-cache-enabled then (the lookup is as-of inference
            // against an append-only ledger, so a tariff that was active then always resolves).
            let cache_mults_resolved = raw
                .request_model
                .as_deref()
                .and_then(|alias| cache_tariff_map.get(alias))
                .and_then(|rows| resolve_cache_multipliers(rows, pricing_timestamp));

            // dwctl only injects cache tokens when a tariff is active, so if no tariff was valid
            // at inference yet the response still carries cache_* tokens, those are the upstream
            // provider's own (e.g. Anthropic's native caching) — surface that we're ignoring them.
            if cache_mults_resolved.is_none()
                && (raw.cache_read_input_tokens > 0
                    || raw.cache_creation_5m_input_tokens > 0
                    || raw.cache_creation_1h_input_tokens > 0
                    || raw.cache_creation_24h_input_tokens > 0)
            {
                crate::background_error!(
                    ANALYTICS_BATCHER,
                    "provider_cache_tokens_ignored",
                    Warning,
                    model = raw.request_model.as_deref().unwrap_or("?"),
                    "response carried cache tokens but the model is not dwctl-cache-enabled; ignoring them and billing at list price"
                );
            }
            let cache_mults_resolved = clamp_implicit_read_multiplier(cache_mults_resolved, raw.cache_read_source.as_deref());
            let billable = (200..=299).contains(&raw.status_code);
            let (total_cost, uncached_cost) = if billable {
                (
                    charged_cost(
                        &TokenCounts::from(&raw),
                        raw.request_model.as_deref(),
                        input_price,
                        output_price,
                        cache_mults_resolved,
                        ANALYTICS_BATCHER,
                    ),
                    compute_list_price(&raw, input_price, output_price),
                )
            } else {
                (None, None)
            };

            enriched.push(EnrichedRecord {
                raw,
                user_id,
                api_key_id,
                access_source,
                api_key_purpose,
                provider_name,
                input_price_per_token: input_price,
                output_price_per_token: output_price,
                total_cost,
                uncached_cost,
                cap_scope_root,
            });
        }

        Ok(enriched)
    }

    #[tracing::instrument(skip_all)]
    async fn batch_lookup_users_by_ids(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        key_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, KeyLookup>, sqlx::Error> {
        #[derive(sqlx::FromRow)]
        struct UserRow {
            user_id: Uuid,
            api_key_id: Uuid,
            purpose: String,
            cap_scope_root: Option<Uuid>,
        }

        let rows: Vec<UserRow> = sqlx::query_as!(
            UserRow,
            r#"
            SELECT ak.user_id, ak.id as api_key_id, ak.purpose,
                   CASE WHEN root.spend_limit IS NOT NULL THEN root.id END AS "cap_scope_root?"
            FROM api_keys ak
            JOIN api_keys root ON root.id = COALESCE(ak.parent_api_key_id, ak.id)
            WHERE ak.id = ANY($1)
            "#,
            key_ids
        )
        .fetch_all(&mut **tx)
        .await?;

        let mut map = HashMap::with_capacity(rows.len());
        for row in rows {
            map.insert(
                row.api_key_id,
                KeyLookup {
                    user_id: row.user_id,
                    api_key_id: row.api_key_id,
                    purpose: parse_api_key_purpose(&row.purpose),
                    cap_scope_root: row.cap_scope_root,
                },
            );
        }
        Ok(map)
    }

    /// Batch lookup model info with tariffs.
    ///
    /// Fetches ALL tariffs (including expired ones) to support historical pricing
    /// for batch requests that may have been created in the past.
    #[tracing::instrument(skip_all)]
    async fn batch_lookup_models_with_tariffs(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        aliases: &[&str],
    ) -> Result<HashMap<String, ModelInfo>, sqlx::Error> {
        let aliases_vec: Vec<String> = aliases.iter().map(|s| s.to_string()).collect();

        struct ModelRow {
            alias: String,
            provider_name: Option<String>,
            tariff_purpose: Option<String>,
            tariff_valid_from: Option<DateTime<Utc>>,
            tariff_valid_until: Option<DateTime<Utc>>,
            tariff_input_price: Option<Decimal>,
            tariff_output_price: Option<Decimal>,
            tariff_completion_window: Option<String>,
        }

        // Query models with ALL their tariffs (including expired) for historical pricing
        // Note: Column aliases use "?" suffix to force nullable for LEFT JOIN columns
        let rows: Vec<ModelRow> = sqlx::query_as!(
            ModelRow,
            r#"
            SELECT
                dm.alias,
                ie.name as "provider_name?",
                mt.api_key_purpose as "tariff_purpose?",
                mt.valid_from as "tariff_valid_from?",
                mt.valid_until as "tariff_valid_until?",
                mt.input_price_per_token as "tariff_input_price?",
                mt.output_price_per_token as "tariff_output_price?",
                mt.completion_window as "tariff_completion_window?"
            FROM deployed_models dm
            LEFT JOIN inference_endpoints ie ON dm.hosted_on = ie.id
            LEFT JOIN model_tariffs mt ON mt.deployed_model_id = dm.id
            WHERE dm.alias = ANY($1)
            ORDER BY dm.alias, mt.valid_from DESC
            "#,
            &aliases_vec
        )
        .fetch_all(&mut **tx)
        .await?;

        // Group by alias
        let mut map: HashMap<String, ModelInfo> = HashMap::new();
        for row in rows {
            let entry = map.entry(row.alias.clone()).or_insert_with(|| ModelInfo {
                provider_name: row.provider_name.unwrap_or_default(),
                tariffs: Vec::new(),
            });

            // Add tariff if present
            if let (Some(purpose), Some(valid_from), Some(input_price), Some(output_price)) = (
                row.tariff_purpose,
                row.tariff_valid_from,
                row.tariff_input_price,
                row.tariff_output_price,
            ) {
                entry.tariffs.push(TariffInfo {
                    purpose: parse_api_key_purpose(&purpose),
                    effective_from: valid_from,
                    valid_until: row.tariff_valid_until,
                    input_price_per_token: input_price,
                    output_price_per_token: output_price,
                    completion_window: row.tariff_completion_window,
                });
            }
        }

        trace!(count = map.len(), "Batch lookup models completed");
        Ok(map)
    }

    /// Batch lookup cache tariffs (per model, per tier) for the given aliases.
    ///
    /// Fetches ALL rows (including expired) so batch requests price as of their creation
    /// time, exactly like `batch_lookup_models_with_tariffs`. Models without cache tariffs
    /// simply don't appear (the resolver then falls back to safe defaults).
    #[tracing::instrument(skip_all)]
    async fn batch_lookup_cache_tariffs(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        aliases: &[&str],
    ) -> Result<HashMap<String, Vec<CacheTariffRow>>, sqlx::Error> {
        let aliases_vec: Vec<String> = aliases.iter().map(|s| s.to_string()).collect();
        let map = crate::pricing::lookup_cache_tariffs(&mut **tx, &aliases_vec).await?;
        trace!(count = map.len(), "Batch lookup cache tariffs completed");
        Ok(map)
    }

    /// Write enriched records using the caller's outbox transaction.
    #[tracing::instrument(skip_all)]
    async fn write_batch_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        records: &[EnrichedRecord],
    ) -> Result<(), sqlx::Error> {
        // Phase 1: Batch INSERT http_analytics
        let (analytics_ids, newly_inserted) = self.batch_insert_analytics(tx, records).await?;

        // Phase 2: Batch INSERT credit_transactions (+ fold batch_aggregates)
        let duplicates = self.batch_insert_credits(tx, records, &analytics_ids, &newly_inserted).await?;
        if duplicates > 0 {
            warn!(duplicates = duplicates, "Some credit transactions were duplicates");
            counter!("dwctl_credits_duplicates_total").increment(duplicates);
        }
        Ok(())
    }

    /// Batch INSERT http_analytics records within a transaction.
    ///
    /// Returns the analytics ids inserted by this call. A repeated physical
    /// receipt, and (once the logical-request unique index is deployed) a
    /// second successful attempt for the same Fusillade request, returns no
    /// row and therefore cannot proceed to billing or aggregation. The
    /// batch-analytics fold uses the same returned ids as its idempotency
    /// anchor, including for free requests that have no credit row.
    async fn batch_insert_analytics(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        records: &[EnrichedRecord],
    ) -> Result<(HashMap<(Uuid, i64), i64>, HashSet<i64>), sqlx::Error> {
        if records.is_empty() {
            return Ok((HashMap::new(), HashSet::new()));
        }

        // Build arrays for UNNEST
        let mut instance_ids: Vec<Uuid> = Vec::with_capacity(records.len());
        let mut correlation_ids: Vec<i64> = Vec::with_capacity(records.len());
        let mut timestamps: Vec<DateTime<Utc>> = Vec::with_capacity(records.len());
        let mut methods: Vec<String> = Vec::with_capacity(records.len());
        let mut uris: Vec<String> = Vec::with_capacity(records.len());
        let mut request_models: Vec<Option<String>> = Vec::with_capacity(records.len());
        let mut status_codes: Vec<i32> = Vec::with_capacity(records.len());
        let mut duration_ms_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut duration_to_first_byte_ms_vec: Vec<Option<i64>> = Vec::with_capacity(records.len());
        let mut prompt_tokens_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut completion_tokens_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut reasoning_tokens_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut total_tokens_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut response_types: Vec<String> = Vec::with_capacity(records.len());
        let mut user_ids: Vec<Option<Uuid>> = Vec::with_capacity(records.len());
        let mut access_sources: Vec<String> = Vec::with_capacity(records.len());
        let mut input_prices: Vec<Option<Decimal>> = Vec::with_capacity(records.len());
        let mut output_prices: Vec<Option<Decimal>> = Vec::with_capacity(records.len());
        let mut fusillade_batch_ids: Vec<Option<Uuid>> = Vec::with_capacity(records.len());
        let mut fusillade_request_ids: Vec<Option<Uuid>> = Vec::with_capacity(records.len());
        let mut custom_ids: Vec<Option<String>> = Vec::with_capacity(records.len());
        let mut request_origins: Vec<String> = Vec::with_capacity(records.len());
        let mut batch_slas: Vec<String> = Vec::with_capacity(records.len());
        let mut batch_request_sources: Vec<String> = Vec::with_capacity(records.len());

        let mut api_key_ids: Vec<Option<Uuid>> = Vec::with_capacity(records.len());
        let mut trace_ids: Vec<Option<String>> = Vec::with_capacity(records.len());
        let mut cache_read_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut cache_creation_total_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut cache_5m_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut cache_1h_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut cache_24h_vec: Vec<i64> = Vec::with_capacity(records.len());
        let mut total_cost_vec: Vec<Option<Decimal>> = Vec::with_capacity(records.len());
        let mut uncached_cost_vec: Vec<Option<Decimal>> = Vec::with_capacity(records.len());
        let mut served_by_vec: Vec<Option<String>> = Vec::with_capacity(records.len());
        let mut finish_reason_vec: Vec<Option<String>> = Vec::with_capacity(records.len());
        let mut user_agent_vec: Vec<Option<String>> = Vec::with_capacity(records.len());
        let mut submitted_at_vec: Vec<Option<DateTime<Utc>>> = Vec::with_capacity(records.len());
        let mut engine_cached_vec: Vec<Option<i64>> = Vec::with_capacity(records.len());
        let mut cache_read_source_vec: Vec<Option<String>> = Vec::with_capacity(records.len());
        let mut stream_vec: Vec<Option<bool>> = Vec::with_capacity(records.len());
        let mut max_tokens_vec: Vec<Option<i64>> = Vec::with_capacity(records.len());
        let mut temperature_vec: Vec<Option<f32>> = Vec::with_capacity(records.len());
        let mut top_p_vec: Vec<Option<f32>> = Vec::with_capacity(records.len());
        let mut n_vec: Vec<Option<i32>> = Vec::with_capacity(records.len());
        let mut tool_count_vec: Vec<Option<i32>> = Vec::with_capacity(records.len());
        let mut message_count_vec: Vec<Option<i32>> = Vec::with_capacity(records.len());

        for record in records {
            instance_ids.push(record.raw.instance_id);
            correlation_ids.push(record.raw.correlation_id);
            timestamps.push(record.raw.timestamp);
            methods.push(record.raw.method.clone());
            uris.push(record.raw.uri.clone());
            request_models.push(record.raw.request_model.clone());
            status_codes.push(record.raw.status_code);
            duration_ms_vec.push(record.raw.duration_ms);
            duration_to_first_byte_ms_vec.push(record.raw.duration_to_first_byte_ms);
            prompt_tokens_vec.push(record.raw.prompt_tokens);
            completion_tokens_vec.push(record.raw.completion_tokens);
            reasoning_tokens_vec.push(record.raw.reasoning_tokens);
            total_tokens_vec.push(record.raw.total_tokens);
            response_types.push(record.raw.response_type.clone());
            user_ids.push(record.user_id);
            access_sources.push(record.access_source.clone());
            input_prices.push(record.input_price_per_token);
            output_prices.push(record.output_price_per_token);
            fusillade_batch_ids.push(record.raw.fusillade_batch_id);
            fusillade_request_ids.push(record.raw.fusillade_request_id);
            custom_ids.push(record.raw.custom_id.clone());

            let request_origin = compute_request_origin(record.api_key_purpose.as_ref(), record.raw.fusillade_batch_id);
            request_origins.push(request_origin.to_string());

            batch_slas.push(record.raw.batch_completion_window.clone().unwrap_or_default());
            batch_request_sources.push(record.raw.batch_request_source.clone());

            api_key_ids.push(record.api_key_id);
            trace_ids.push(record.raw.trace_id.clone());

            let c5 = record.raw.cache_creation_5m_input_tokens;
            let c1 = record.raw.cache_creation_1h_input_tokens;
            let c24 = record.raw.cache_creation_24h_input_tokens;
            cache_read_vec.push(record.raw.cache_read_input_tokens);
            // Saturating: corrupt/huge counts must never wrap into a negative total.
            cache_creation_total_vec.push(c5.saturating_add(c1).saturating_add(c24));
            cache_5m_vec.push(c5);
            cache_1h_vec.push(c1);
            cache_24h_vec.push(c24);
            total_cost_vec.push(record.total_cost);
            uncached_cost_vec.push(record.uncached_cost);
            served_by_vec.push(record.raw.served_by.clone());
            finish_reason_vec.push(record.raw.finish_reason.clone());
            user_agent_vec.push(record.raw.user_agent.clone());
            // Already carried for batch-creation pricing; now also persisted, so the
            // queue delay (timestamp - submitted_at) survives past this process.
            submitted_at_vec.push(record.raw.batch_created_at);
            engine_cached_vec.push(record.raw.engine_cached_tokens);
            cache_read_source_vec.push(record.raw.cache_read_source.clone());
            let p = &record.raw.request_params;
            stream_vec.push(p.stream);
            max_tokens_vec.push(p.max_tokens);
            temperature_vec.push(p.temperature);
            top_p_vec.push(p.top_p);
            n_vec.push(p.n);
            tool_count_vec.push(p.tool_count);
            message_count_vec.push(p.message_count);
        }

        let rows = sqlx::query!(
            r#"
            INSERT INTO http_analytics (
                instance_id, correlation_id, timestamp, method, uri, model,
                status_code, duration_ms, duration_to_first_byte_ms, prompt_tokens, completion_tokens,
                reasoning_tokens, total_tokens, response_type, user_id, access_source,
                input_price_per_token, output_price_per_token, fusillade_batch_id, fusillade_request_id, custom_id,
                request_origin, batch_sla, batch_request_source, api_key_id, trace_id,
                cache_read_input_tokens, cache_creation_input_tokens,
                cache_creation_5m_input_tokens, cache_creation_1h_input_tokens, cache_creation_24h_input_tokens,
                total_cost, uncached_cost, served_by, finish_reason, user_agent, submitted_at,
                engine_cached_tokens, stream, max_tokens, temperature, top_p, n, tool_count, message_count,
                cache_read_source
            )
            SELECT * FROM UNNEST(
                $1::uuid[], $2::bigint[], $3::timestamptz[], $4::text[], $5::text[], $6::text[],
                $7::int[], $8::bigint[], $9::bigint[], $10::bigint[], $11::bigint[],
                $12::bigint[], $13::bigint[], $14::text[], $15::uuid[], $16::text[],
                $17::numeric[], $18::numeric[], $19::uuid[], $20::uuid[], $21::text[],
                $22::text[], $23::text[], $24::text[], $25::uuid[], $26::text[],
                $27::bigint[], $28::bigint[],
                $29::bigint[], $30::bigint[], $31::bigint[],
                $32::numeric[], $33::numeric[], $34::text[], $35::text[], $36::text[],
                $37::timestamptz[],
                $38::bigint[], $39::boolean[], $40::bigint[], $41::real[], $42::real[], $43::int[], $44::int[], $45::int[],
                $46::text[]
            )
            ON CONFLICT DO NOTHING
            RETURNING id, instance_id, correlation_id
            "#,
            &instance_ids,
            &correlation_ids,
            &timestamps,
            &methods,
            &uris,
            &request_models as &[Option<String>],
            &status_codes,
            &duration_ms_vec,
            &duration_to_first_byte_ms_vec as &[Option<i64>],
            &prompt_tokens_vec,
            &completion_tokens_vec,
            &reasoning_tokens_vec,
            &total_tokens_vec,
            &response_types,
            &user_ids as &[Option<Uuid>],
            &access_sources,
            &input_prices as &[Option<Decimal>],
            &output_prices as &[Option<Decimal>],
            &fusillade_batch_ids as &[Option<Uuid>],
            &fusillade_request_ids as &[Option<Uuid>],
            &custom_ids as &[Option<String>],
            &request_origins,
            &batch_slas,
            &batch_request_sources,
            &api_key_ids as &[Option<Uuid>],
            &trace_ids as &[Option<String>],
            &cache_read_vec,
            &cache_creation_total_vec,
            &cache_5m_vec,
            &cache_1h_vec,
            &cache_24h_vec,
            &total_cost_vec as &[Option<Decimal>],
            &uncached_cost_vec as &[Option<Decimal>],
            &served_by_vec as &[Option<String>],
            &finish_reason_vec as &[Option<String>],
            &user_agent_vec as &[Option<String>],
            &submitted_at_vec as &[Option<DateTime<Utc>>],
            &engine_cached_vec as &[Option<i64>],
            &stream_vec as &[Option<bool>],
            &max_tokens_vec as &[Option<i64>],
            &temperature_vec as &[Option<f32>],
            &top_p_vec as &[Option<f32>],
            &n_vec as &[Option<i32>],
            &tool_count_vec as &[Option<i32>],
            &message_count_vec as &[Option<i32>],
            &cache_read_source_vec as &[Option<String>],
        )
        .fetch_all(&mut **tx)
        .await?;

        let mut id_map = HashMap::with_capacity(rows.len());
        let mut newly_inserted: HashSet<i64> = HashSet::with_capacity(rows.len());
        for row in rows {
            id_map.insert((row.instance_id, row.correlation_id), row.id);
            newly_inserted.insert(row.id);
        }

        let duplicate_receipts = records.len().saturating_sub(id_map.len());
        if duplicate_receipts > 0 {
            counter!("dwctl_analytics_duplicate_receipts_total").increment(duplicate_receipts as u64);
        }

        trace!(
            count = id_map.len(),
            inserted = newly_inserted.len(),
            duplicates = duplicate_receipts,
            "Batch inserted analytics records"
        );
        Ok((id_map, newly_inserted))
    }

    /// Batch INSERT credit_transactions within a transaction.
    ///
    /// Returns the number of duplicate transactions that were skipped.
    ///
    /// Also handles balance threshold notifications (when a user's balance crosses zero).
    /// This replaces the database trigger approach for better performance - instead of
    /// running a SUM query per row, we query balances once before insert and check
    /// threshold crossings after.
    async fn batch_insert_credits(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        records: &[EnrichedRecord],
        analytics_ids: &HashMap<(Uuid, i64), i64>,
        newly_inserted: &HashSet<i64>,
    ) -> Result<u64, sqlx::Error> {
        // Collect records that need credit transactions
        let mut user_ids: Vec<Uuid> = Vec::new();
        let mut amounts: Vec<Decimal> = Vec::new();
        let mut source_ids: Vec<String> = Vec::new();
        let mut descriptions: Vec<Option<String>> = Vec::new();
        let mut fusillade_batch_ids: Vec<Option<Uuid>> = Vec::new();
        let mut models: Vec<String> = Vec::new();
        let mut served_bys: Vec<String> = Vec::new();
        let mut api_key_ids_credit: Vec<Option<Uuid>> = Vec::new();
        // Service tier, computed in memory from the fusillade metadata, so the
        // transactions list can label each row (realtime / flex / async / batch)
        // without joining http_analytics.
        let mut service_tiers: Vec<String> = Vec::new();
        // Denormalized per-request id so the responses view can read cost off the ledger
        // durably (by fusillade_request_id) instead of joining http_analytics, which ages
        // out of retention. NULL for non-fusillade (realtime) usage.
        let mut fusillade_request_ids_credit: Vec<Option<Uuid>> = Vec::new();
        // Spending-cap scope root per billed row (None for the uncapped
        // majority); parallel to the vecs above, consumed by the cap fold below.
        let mut cap_scope_roots: Vec<Option<Uuid>> = Vec::new();
        let mut processed_analytics_ids: HashSet<i64> = HashSet::new();

        for record in records {
            // Only a row returned by the analytics INSERT may have downstream
            // effects. If the same physical identity appeared twice in one
            // flush, process the first input row (the one INSERT considered
            // first) exactly once.
            let Some(&analytics_id) = analytics_ids.get(&(record.raw.instance_id, record.raw.correlation_id)) else {
                trace!(
                    instance_id = %record.raw.instance_id,
                    correlation_id = record.raw.correlation_id,
                    fusillade_request_id = ?record.raw.fusillade_request_id,
                    "Skipping billing for an analytics receipt rejected by a uniqueness constraint"
                );
                continue;
            };
            if !processed_analytics_ids.insert(analytics_id) {
                continue;
            }

            // Billing eligibility is the shared effective HTTP outcome. Failed
            // requests remain in analytics, but never create a debit even when
            // the provider reported usage and a non-zero price.
            if !(200..=299).contains(&record.raw.status_code) {
                continue;
            }

            // Skip if no user or no pricing
            let Some(user_id) = record.user_id else { continue };

            // Skip system user
            if user_id == Uuid::nil() {
                continue;
            }

            // The cache-adjusted cost was computed during enrichment and is the same
            // value written to http_analytics.total_cost. `None` = no pricing configured.
            let Some(total_cost) = record.total_cost else { continue };

            if total_cost <= Decimal::ZERO {
                continue;
            }

            let model = record.raw.request_model.clone().unwrap_or_default();

            user_ids.push(user_id);
            amounts.push(total_cost);
            source_ids.push(analytics_id.to_string());
            descriptions.push(Some(format!(
                "API usage: {} ({} input + {} output tokens)",
                model, record.raw.prompt_tokens, record.raw.completion_tokens
            )));
            fusillade_batch_ids.push(record.raw.fusillade_batch_id);
            models.push(model);
            served_bys.push(crate::metrics::served_by_host(record.raw.served_by.as_deref()));
            api_key_ids_credit.push(record.api_key_id);
            service_tiers
                .push(compute_billing_tier(record.raw.fusillade_batch_id, record.raw.batch_completion_window.as_deref()).to_string());
            fusillade_request_ids_credit.push(record.raw.fusillade_request_id);
            cap_scope_roots.push(record.cap_scope_root);
        }

        if user_ids.is_empty() {
            return Ok(0);
        }

        let expected_count = user_ids.len() as u64;

        // Build a map from source_id to (index, user_id, amount, model, served_by) for metric recording
        let source_id_to_record: HashMap<String, (usize, Uuid, Decimal, String, String)> = source_ids
            .iter()
            .enumerate()
            .map(|(i, sid)| (sid.clone(), (i, user_ids[i], amounts[i], models[i].clone(), served_bys[i].clone())))
            .collect();

        // Batch INSERT, folding into the read model in the same transaction.
        // Batched rows are born is_aggregated = true (aggregated below). The
        // RETURNING clause reports exactly which rows were inserted, so under
        // retries (ON CONFLICT DO NOTHING) already-inserted rows are neither
        // re-folded nor re-aggregated.
        let inserted_rows = sqlx::query!(
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, fusillade_batch_id, api_key_id, is_aggregated, service_tier, fusillade_request_id)
            SELECT u.user_id, u.transaction_type, u.amount, u.source_id, u.description, u.fusillade_batch_id, u.api_key_id,
                   u.fusillade_batch_id IS NOT NULL, u.service_tier, u.fusillade_request_id
            FROM UNNEST(
                $1::uuid[], $2::text[], $3::numeric[], $4::text[], $5::text[], $6::uuid[], $7::uuid[], $8::text[], $9::uuid[]
            ) AS u(user_id, transaction_type, amount, source_id, description, fusillade_batch_id, api_key_id, service_tier, fusillade_request_id)
            ON CONFLICT (source_id) DO NOTHING
            RETURNING source_id, user_id, amount, seq, created_at, fusillade_batch_id, service_tier
            "#,
            &user_ids,
            &vec!["usage".to_string(); user_ids.len()],
            &amounts,
            &source_ids,
            &descriptions as &[Option<String>],
            &fusillade_batch_ids as &[Option<Uuid>],
            &api_key_ids_credit as &[Option<Uuid>],
            &service_tiers,
            &fusillade_request_ids_credit as &[Option<Uuid>],
        )
        .fetch_all(&mut **tx)
        .await?;

        let inserted_count = inserted_rows.len() as u64;
        let duplicates = expected_count.saturating_sub(inserted_count);

        // Fold the inserted usage amounts into the user_balance_checkpoints
        // read model: one grouped update per distinct user per flush (NOT per
        // request). Usage is always a debit. checkpoint_seq advances so old
        // binaries reading checkpoint + delta stay exact during rolling
        // deploys.
        struct UserFold {
            delta: Decimal,
            max_seq: i64,
        }
        let mut folds: HashMap<Uuid, UserFold> = HashMap::new();

        // Billing fold: total_amount / transaction_count per batch, over the *billed* rows
        // (the credit set). The per-batch analytics aggregates (tokens/latency/cost) are folded
        // separately below over ALL newly inserted 2xx requests, so free-model batches are counted too.
        struct BatchFold {
            user_id: Uuid,
            total: Decimal,
            count: i32,
            max_seq: i64,
            min_created_at: chrono::DateTime<chrono::Utc>,
            // Constant per batch (async or batch); captured from the first folded row
            // and denormalized onto batch_aggregates so the list needn't join http_analytics.
            service_tier: String,
        }
        let mut batch_folds: HashMap<Uuid, BatchFold> = HashMap::new();

        for row in &inserted_rows {
            let fold = folds.entry(row.user_id).or_insert(UserFold {
                delta: Decimal::ZERO,
                max_seq: 0,
            });
            fold.delta -= row.amount;
            fold.max_seq = fold.max_seq.max(row.seq);

            if let Some(batch_id) = row.fusillade_batch_id {
                let bf = batch_folds.entry(batch_id).or_insert(BatchFold {
                    user_id: row.user_id,
                    total: Decimal::ZERO,
                    count: 0,
                    max_seq: 0,
                    min_created_at: row.created_at,
                    service_tier: row.service_tier.clone().unwrap_or_default(),
                });
                bf.total += row.amount;
                bf.count += 1;
                bf.max_seq = bf.max_seq.max(row.seq);
                bf.min_created_at = bf.min_created_at.min(row.created_at);
            }
        }

        let mut crossed_down: Vec<Uuid> = Vec::new();
        if !folds.is_empty() {
            // Sorted by user id so concurrent flushes from other replicas
            // lock overlapping user sets in the same order (deadlock
            // avoidance).
            let mut fold_users: Vec<Uuid> = folds.keys().copied().collect();
            fold_users.sort_unstable();
            let fold_seqs: Vec<i64> = fold_users.iter().map(|u| folds[u].max_seq).collect();
            let fold_deltas: Vec<Decimal> = fold_users.iter().map(|u| folds[u].delta).collect();

            let updated = sqlx::query!(
                r#"
                INSERT INTO user_balance_checkpoints (user_id, checkpoint_seq, balance)
                SELECT u.user_id, u.checkpoint_seq, u.delta
                FROM UNNEST($1::uuid[], $2::bigint[], $3::numeric[]) AS u(user_id, checkpoint_seq, delta)
                ON CONFLICT (user_id) DO UPDATE SET
                    balance = user_balance_checkpoints.balance + EXCLUDED.balance,
                    checkpoint_seq = GREATEST(user_balance_checkpoints.checkpoint_seq, EXCLUDED.checkpoint_seq),
                    updated_at = NOW()
                RETURNING user_id, balance
                "#,
                &fold_users,
                &fold_seqs,
                &fold_deltas,
            )
            .fetch_all(&mut **tx)
            .await?;

            // Usage only debits, so the only crossing possible here is downward.
            for row in &updated {
                let old_balance = row.balance - folds[&row.user_id].delta;
                if old_balance > Decimal::ZERO && row.balance <= Decimal::ZERO {
                    crossed_down.push(row.user_id);
                }
            }
        }

        // Notify onwards to re-evaluate key eligibility. Edge-triggered (a
        // user fires this once per depletion, not on every flush while
        // negative), so the resulting full reloads are rare - one notify
        // covers all crossings in this flush. pg_notify is transactional, so
        // nothing fires if the flush aborts.
        if !crossed_down.is_empty() {
            let epoch_micros = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros();
            let payload = format!("credits_transactions:{}", epoch_micros);
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(ONWARDS_CONFIG_CHANGED_CHANNEL)
                .bind(&payload)
                .execute(&mut **tx)
                .await?;
            counter!("dwctl_balance_crossings_total", "direction" => "down").increment(crossed_down.len() as u64);
        }

        // Fold this flush's billed amounts into the per-cap-scope spend
        // checkpoints, mirroring the user-balance fold above: grouped per
        // scope per flush, riding the RETURNING'd (inserted) rows so retries
        // never double-fold. Only rows whose key belongs to a currently-capped
        // scope carry a `cap_scope_root`, so this is a no-op for the uncapped
        // majority. Cap-crossing NOTIFYs are edge-triggered like balance
        // zero-crossings.
        let mut scope_folds: HashMap<Uuid, Decimal> = HashMap::new();
        for row in &inserted_rows {
            if let Some(&(idx, ..)) = source_id_to_record.get(&row.source_id)
                && let Some(scope_root) = cap_scope_roots[idx]
            {
                *scope_folds.entry(scope_root).or_insert(Decimal::ZERO) += row.amount;
            }
        }

        if !scope_folds.is_empty() {
            // Sorted for the same cross-replica deadlock avoidance as the
            // balance fold.
            let mut scope_ids: Vec<Uuid> = scope_folds.keys().copied().collect();
            scope_ids.sort_unstable();
            let scope_deltas: Vec<Decimal> = scope_ids.iter().map(|s| scope_folds[s]).collect();

            // Main path: the checkpoint row exists (created at cap-set time).
            // Windows are CALENDAR-ALIGNED (UTC) — `api_key_cap_window_current`
            // (migration 123, shared with the sync eligibility predicate) says
            // whether window_started_at falls in the same calendar day/week/
            // month as now(); a stale window means this is the first billed
            // request past the boundary, so the fold REPLACES window_spend
            // with this delta instead of accumulating (lazy rollover — no
            // scheduled job exists).
            let updated = sqlx::query!(
                r#"
                UPDATE api_key_spend_checkpoints ck SET
                    total_spend = ck.total_spend + i.delta,
                    window_spend = CASE
                        WHEN api_key_cap_window_current(ck.window_started_at, ak.spend_limit_interval)
                        THEN ck.window_spend + i.delta
                        ELSE i.delta
                    END,
                    window_started_at = CASE
                        WHEN api_key_cap_window_current(ck.window_started_at, ak.spend_limit_interval)
                        THEN ck.window_started_at
                        ELSE NOW()
                    END,
                    updated_at = NOW()
                FROM UNNEST($1::uuid[], $2::numeric[]) AS i(api_key_id, delta)
                JOIN api_keys ak ON ak.id = i.api_key_id
                WHERE ck.api_key_id = i.api_key_id
                RETURNING ck.api_key_id, ck.window_spend AS "window_spend!", i.delta AS "delta!", ak.spend_limit
                "#,
                &scope_ids,
                &scope_deltas,
            )
            .fetch_all(&mut **tx)
            .await?;

            let mut crossed_caps: u64 = 0;
            for row in &updated {
                // Edge-trigger: crossed iff this delta moved the window total
                // from below the limit to at/above it. Holds for rolled-over
                // windows too (there window_spend == delta, so the "before"
                // side is 0 < limit).
                if let Some(limit) = row.spend_limit
                    && row.window_spend >= limit
                    && row.window_spend - row.delta < limit
                {
                    crossed_caps += 1;
                }
            }

            // Fallback: checkpoint row missing (cap set without the PR-3 API,
            // e.g. manual SQL). Accumulate-only upsert — on the conflict arm
            // (a concurrent flush just created the row milliseconds ago) the
            // window cannot have rolled, so plain accumulation is correct and
            // no delta is ever lost.
            let missing: Vec<usize> = scope_ids
                .iter()
                .enumerate()
                .filter(|(_, id)| !updated.iter().any(|u| u.api_key_id == **id))
                .map(|(i, _)| i)
                .collect();
            if !missing.is_empty() {
                let missing_ids: Vec<Uuid> = missing.iter().map(|&i| scope_ids[i]).collect();
                let missing_deltas: Vec<Decimal> = missing.iter().map(|&i| scope_deltas[i]).collect();

                let inserted = sqlx::query!(
                    r#"
                    INSERT INTO api_key_spend_checkpoints (api_key_id, total_spend, window_spend)
                    SELECT i.api_key_id, i.delta, i.delta
                    FROM UNNEST($1::uuid[], $2::numeric[]) AS i(api_key_id, delta)
                    ON CONFLICT (api_key_id) DO UPDATE SET
                        total_spend = api_key_spend_checkpoints.total_spend + EXCLUDED.total_spend,
                        window_spend = api_key_spend_checkpoints.window_spend + EXCLUDED.window_spend,
                        updated_at = NOW()
                    RETURNING api_key_id, window_spend AS "window_spend!"
                    "#,
                    &missing_ids,
                    &missing_deltas,
                )
                .fetch_all(&mut **tx)
                .await?;

                // Limits for the fresh rows (rare path; usually empty).
                let fresh_ids: Vec<Uuid> = inserted.iter().map(|r| r.api_key_id).collect();
                let limits = sqlx::query!(r#"SELECT id, spend_limit FROM api_keys WHERE id = ANY($1)"#, &fresh_ids)
                    .fetch_all(&mut **tx)
                    .await?;
                let limit_map: HashMap<Uuid, Option<Decimal>> = limits.into_iter().map(|r| (r.id, r.spend_limit)).collect();
                let delta_map: HashMap<Uuid, Decimal> = missing_ids.iter().copied().zip(missing_deltas.iter().copied()).collect();
                for row in &inserted {
                    // Total lookup: a scope id absent from the map (cannot
                    // happen — `inserted` is a subset of `missing_ids`) counts
                    // as zero delta, which can never mis-fire a crossing.
                    let delta = delta_map.get(&row.api_key_id).copied().unwrap_or(Decimal::ZERO);
                    if let Some(Some(limit)) = limit_map.get(&row.api_key_id)
                        && row.window_spend >= *limit
                        && row.window_spend - delta < *limit
                    {
                        crossed_caps += 1;
                    }
                }
            }

            if crossed_caps > 0 {
                let epoch_micros = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros();
                let payload = format!("api_key_spend_cap:{}", epoch_micros);
                sqlx::query("SELECT pg_notify($1, $2)")
                    .bind(ONWARDS_CONFIG_CHANGED_CHANNEL)
                    .bind(&payload)
                    .execute(&mut **tx)
                    .await?;
                counter!("dwctl_spend_cap_crossings_total").increment(crossed_caps);
            }
        }

        // Aggregate this flush's batched rows into batch_aggregates (the
        // grouped view the transactions UI reads). Sorted for the same
        // deadlock-avoidance reason as above.
        if !batch_folds.is_empty() {
            let mut batch_ids: Vec<Uuid> = batch_folds.keys().copied().collect();
            batch_ids.sort_unstable();
            let batch_users: Vec<Uuid> = batch_ids.iter().map(|b| batch_folds[b].user_id).collect();
            let totals: Vec<Decimal> = batch_ids.iter().map(|b| batch_folds[b].total).collect();
            let counts: Vec<i32> = batch_ids.iter().map(|b| batch_folds[b].count).collect();
            let batch_seqs: Vec<i64> = batch_ids.iter().map(|b| batch_folds[b].max_seq).collect();
            let created_ats: Vec<chrono::DateTime<chrono::Utc>> = batch_ids.iter().map(|b| batch_folds[b].min_created_at).collect();
            let service_tiers_agg: Vec<String> = batch_ids.iter().map(|b| batch_folds[b].service_tier.clone()).collect();

            sqlx::query!(
                r#"
                INSERT INTO batch_aggregates (
                    fusillade_batch_id, user_id, total_amount, transaction_count, max_seq, created_at, updated_at, service_tier
                )
                SELECT b.batch_id, b.user_id, b.total, b.cnt, b.max_seq, b.created_at, NOW(), b.service_tier
                FROM UNNEST($1::uuid[], $2::uuid[], $3::numeric[], $4::int[], $5::bigint[], $6::timestamptz[], $7::text[])
                    AS b(batch_id, user_id, total, cnt, max_seq, created_at, service_tier)
                ON CONFLICT (fusillade_batch_id) DO UPDATE SET
                    total_amount = batch_aggregates.total_amount + EXCLUDED.total_amount,
                    transaction_count = batch_aggregates.transaction_count + EXCLUDED.transaction_count,
                    max_seq = GREATEST(batch_aggregates.max_seq, EXCLUDED.max_seq),
                    service_tier = COALESCE(batch_aggregates.service_tier, EXCLUDED.service_tier),
                    updated_at = NOW()
                "#,
                &batch_ids,
                &batch_users,
                &totals,
                &counts,
                &batch_seqs,
                &created_ats,
                &service_tiers_agg,
            )
            .execute(&mut **tx)
            .await?;
        }

        // Fold this flush's newly-inserted, successful (2xx) batched requests into the
        // batch_aggregates *analytics* columns (COR-524). Unlike the billing fold above — which
        // rides the credit set and so excludes free / zero-priced requests — this covers ALL
        // 2xx batched requests, matching get_batch_analytics's historical "status 2xx" set, so
        // free-model batches still report tokens/latency/cost. Idempotency rides the
        // http_analytics INSERT's returned rows: a conflicting receipt returns nothing and
        // is not folded. count_duration_ms / count_ttfb_ms count only requests that reported the
        // metric so the endpoint's AVG (which ignores NULLs) is reproduced; total_requests is
        // the plain 2xx count (distinct from transaction_count, the billed-row count).
        struct AnalyticsFold {
            user_id: Uuid,
            service_tier: String,
            min_created_at: chrono::DateTime<chrono::Utc>,
            total_requests: i64,
            prompt_tokens: i64,
            completion_tokens: i64,
            reasoning_tokens: i64,
            total_tokens: i64,
            sum_duration_ms: i64,
            count_duration_ms: i64,
            sum_ttfb_ms: i64,
            count_ttfb_ms: i64,
            list_cost: Decimal,
        }
        let mut analytics_folds: HashMap<Uuid, AnalyticsFold> = HashMap::new();
        let mut folded_analytics_ids: HashSet<i64> = HashSet::new();
        for record in records {
            let Some(&analytics_id) = analytics_ids.get(&(record.raw.instance_id, record.raw.correlation_id)) else {
                continue;
            };
            if !newly_inserted.contains(&analytics_id) || !folded_analytics_ids.insert(analytics_id) {
                continue;
            }

            let Some(batch_id) = record.raw.fusillade_batch_id else { continue };
            if !(200..=299).contains(&record.raw.status_code) {
                continue;
            }
            let Some(user_id) = record.user_id else { continue };
            if user_id == Uuid::nil() {
                continue;
            }
            let af = analytics_folds.entry(batch_id).or_insert_with(|| AnalyticsFold {
                user_id,
                service_tier: compute_billing_tier(record.raw.fusillade_batch_id, record.raw.batch_completion_window.as_deref())
                    .to_string(),
                min_created_at: record.raw.timestamp,
                total_requests: 0,
                prompt_tokens: 0,
                completion_tokens: 0,
                reasoning_tokens: 0,
                total_tokens: 0,
                sum_duration_ms: 0,
                count_duration_ms: 0,
                sum_ttfb_ms: 0,
                count_ttfb_ms: 0,
                list_cost: Decimal::ZERO,
            });
            af.total_requests += 1;
            af.prompt_tokens += record.raw.prompt_tokens;
            af.completion_tokens += record.raw.completion_tokens;
            af.reasoning_tokens += record.raw.reasoning_tokens;
            af.total_tokens += record.raw.total_tokens;
            af.sum_duration_ms += record.raw.duration_ms;
            af.count_duration_ms += 1;
            if let Some(ttfb) = record.raw.duration_to_first_byte_ms {
                af.sum_ttfb_ms += ttfb;
                af.count_ttfb_ms += 1;
            }
            // List price the analytics endpoint reports (uncached_cost); 0 for free models.
            af.list_cost += record.uncached_cost.unwrap_or(Decimal::ZERO);
            af.min_created_at = af.min_created_at.min(record.raw.timestamp);
        }

        if !analytics_folds.is_empty() {
            let mut a_ids: Vec<Uuid> = analytics_folds.keys().copied().collect();
            a_ids.sort_unstable();
            let a_users: Vec<Uuid> = a_ids.iter().map(|b| analytics_folds[b].user_id).collect();
            let a_created: Vec<chrono::DateTime<chrono::Utc>> = a_ids.iter().map(|b| analytics_folds[b].min_created_at).collect();
            let a_tiers: Vec<String> = a_ids.iter().map(|b| analytics_folds[b].service_tier.clone()).collect();
            let a_total_requests: Vec<i64> = a_ids.iter().map(|b| analytics_folds[b].total_requests).collect();
            let a_prompt: Vec<i64> = a_ids.iter().map(|b| analytics_folds[b].prompt_tokens).collect();
            let a_completion: Vec<i64> = a_ids.iter().map(|b| analytics_folds[b].completion_tokens).collect();
            let a_reasoning: Vec<i64> = a_ids.iter().map(|b| analytics_folds[b].reasoning_tokens).collect();
            let a_total_tokens: Vec<i64> = a_ids.iter().map(|b| analytics_folds[b].total_tokens).collect();
            let a_sum_duration: Vec<i64> = a_ids.iter().map(|b| analytics_folds[b].sum_duration_ms).collect();
            let a_count_duration: Vec<i64> = a_ids.iter().map(|b| analytics_folds[b].count_duration_ms).collect();
            let a_sum_ttfb: Vec<i64> = a_ids.iter().map(|b| analytics_folds[b].sum_ttfb_ms).collect();
            let a_count_ttfb: Vec<i64> = a_ids.iter().map(|b| analytics_folds[b].count_ttfb_ms).collect();
            let a_list_cost: Vec<Decimal> = a_ids.iter().map(|b| analytics_folds[b].list_cost).collect();

            // total_amount / transaction_count are left at 0 here (the billing fold owns them);
            // for a free-only batch this INSERTs a row with billing 0 but real analytics, which
            // is correct — get_batch_analytics reads total_requests, not transaction_count.
            sqlx::query!(
                r#"
                INSERT INTO batch_aggregates (
                    fusillade_batch_id, user_id, total_amount, transaction_count, max_seq, created_at, updated_at, service_tier,
                    total_requests, total_prompt_tokens, total_completion_tokens, total_reasoning_tokens, total_tokens,
                    sum_duration_ms, count_duration_ms, sum_ttfb_ms, count_ttfb_ms, total_list_cost
                )
                SELECT b.batch_id, b.user_id, 0, 0, 0, b.created_at, NOW(), b.service_tier,
                    b.total_requests, b.prompt_tokens, b.completion_tokens, b.reasoning_tokens, b.total_tokens,
                    b.sum_duration, b.count_duration, b.sum_ttfb, b.count_ttfb, b.list_cost
                FROM UNNEST(
                    $1::uuid[], $2::uuid[], $3::timestamptz[], $4::text[], $5::bigint[],
                    $6::bigint[], $7::bigint[], $8::bigint[], $9::bigint[],
                    $10::bigint[], $11::bigint[], $12::bigint[], $13::bigint[], $14::numeric[]
                )
                    AS b(batch_id, user_id, created_at, service_tier, total_requests,
                         prompt_tokens, completion_tokens, reasoning_tokens, total_tokens,
                         sum_duration, count_duration, sum_ttfb, count_ttfb, list_cost)
                ON CONFLICT (fusillade_batch_id) DO UPDATE SET
                    total_requests = batch_aggregates.total_requests + EXCLUDED.total_requests,
                    total_prompt_tokens = batch_aggregates.total_prompt_tokens + EXCLUDED.total_prompt_tokens,
                    total_completion_tokens = batch_aggregates.total_completion_tokens + EXCLUDED.total_completion_tokens,
                    total_reasoning_tokens = batch_aggregates.total_reasoning_tokens + EXCLUDED.total_reasoning_tokens,
                    total_tokens = batch_aggregates.total_tokens + EXCLUDED.total_tokens,
                    sum_duration_ms = batch_aggregates.sum_duration_ms + EXCLUDED.sum_duration_ms,
                    count_duration_ms = batch_aggregates.count_duration_ms + EXCLUDED.count_duration_ms,
                    sum_ttfb_ms = batch_aggregates.sum_ttfb_ms + EXCLUDED.sum_ttfb_ms,
                    count_ttfb_ms = batch_aggregates.count_ttfb_ms + EXCLUDED.count_ttfb_ms,
                    total_list_cost = batch_aggregates.total_list_cost + EXCLUDED.total_list_cost,
                    service_tier = COALESCE(batch_aggregates.service_tier, EXCLUDED.service_tier),
                    updated_at = NOW()
                "#,
                &a_ids,
                &a_users,
                &a_created,
                &a_tiers,
                &a_total_requests,
                &a_prompt,
                &a_completion,
                &a_reasoning,
                &a_total_tokens,
                &a_sum_duration,
                &a_count_duration,
                &a_sum_ttfb,
                &a_count_ttfb,
                &a_list_cost,
            )
            .execute(&mut **tx)
            .await?;
        }

        // Record metrics only for successfully inserted credit transactions
        for row in &inserted_rows {
            if let Some((_, user_id, amount, model, served_by)) = source_id_to_record.get(&row.source_id) {
                // Nanocredits (1 credit = 1e9). Tariffs price tokens at
                // DECIMAL(12,8), so a single token can cost 1e-8 credits; nano
                // is the coarsest power of ten that represents every
                // token-priced amount exactly. The predecessor counted whole
                // rounded cents per transaction, which floored the (typical)
                // sub-cent flex/batch deductions to zero and hid entire
                // revenue tiers from the metric.
                let nanocredits = (amount.to_f64().unwrap_or(0.0) * 1_000_000_000.0).round() as u64;
                counter!(
                    "dwctl_credits_deducted_nanocredits_total",
                    "user_id" => user_id.to_string(),
                    "model" => model.clone(),
                    "served_by" => served_by.clone()
                )
                .increment(nanocredits);
            }
        }

        trace!(
            count = inserted_count,
            duplicates = duplicates,
            "Batch inserted credit transactions"
        );
        Ok(duplicates)
    }

    /// Convert enriched record back to HttpAnalyticsRow for metrics recording.
    fn enriched_to_row(&self, record: &EnrichedRecord) -> HttpAnalyticsRow {
        HttpAnalyticsRow {
            instance_id: record.raw.instance_id,
            correlation_id: record.raw.correlation_id,
            timestamp: record.raw.timestamp,
            method: record.raw.method.clone(),
            uri: record.raw.uri.clone(),
            request_model: record.raw.request_model.clone(),
            response_model: record.raw.response_model.clone(),
            status_code: record.raw.status_code,
            duration_ms: record.raw.duration_ms,
            duration_to_first_byte_ms: record.raw.duration_to_first_byte_ms,
            prompt_tokens: record.raw.prompt_tokens,
            completion_tokens: record.raw.completion_tokens,
            reasoning_tokens: record.raw.reasoning_tokens,
            total_tokens: record.raw.total_tokens,
            response_type: record.raw.response_type.clone(),
            user_id: record.user_id,
            access_source: record.access_source.clone(),
            input_price_per_token: record.input_price_per_token,
            output_price_per_token: record.output_price_per_token,
            server_address: record.raw.server_address.clone(),
            server_port: record.raw.server_port,
            provider_name: record.provider_name.clone(),
            fusillade_batch_id: record.raw.fusillade_batch_id,
            fusillade_request_id: record.raw.fusillade_request_id,
            custom_id: record.raw.custom_id.clone(),
            request_origin: compute_request_origin(record.api_key_purpose.as_ref(), record.raw.fusillade_batch_id).to_string(),
            batch_sla: record.raw.batch_completion_window.clone().unwrap_or_default(),
            batch_request_source: record.raw.batch_request_source.clone(),
            served_by: record.raw.served_by.clone(),
        }
    }
}

/// Parse API key purpose from string
fn parse_api_key_purpose(s: &str) -> ApiKeyPurpose {
    match s {
        "platform" => ApiKeyPurpose::Platform,
        "batch" => ApiKeyPurpose::Batch,
        "playground" => ApiKeyPurpose::Playground,
        "continuation" => ApiKeyPurpose::Continuation,
        _ => ApiKeyPurpose::Realtime,
    }
}

/// Compute request origin from API key purpose and fusillade batch ID.
///
/// Returns:
/// - "fusillade" for any request with a fusillade_batch_id, or batch API keys
/// - "frontend" for playground API keys
/// - "api" for everything else
fn compute_request_origin(api_key_purpose: Option<&ApiKeyPurpose>, fusillade_batch_id: Option<Uuid>) -> &'static str {
    match (api_key_purpose, fusillade_batch_id) {
        // Any record with fusillade_batch_id is "fusillade"
        (_, Some(_)) => "fusillade",
        // Batch API keys without fusillade_batch_id are still "fusillade"
        (Some(ApiKeyPurpose::Batch), None) => "fusillade",
        // Playground keys are "frontend"
        (Some(ApiKeyPurpose::Playground), _) => "frontend",
        // Everything else is "api"
        _ => "api",
    }
}

/// Compute the Doubleword billing tier from the fusillade metadata.
///
/// This is our product/billing classification, deliberately named apart from
/// the OpenAI-compatible `service_tier` request parameter (whose values —
/// "auto", "priority", "flex" — don't map 1:1 onto it). The ledger persists it
/// in columns historically named `service_tier`.
///
/// Distinguished by whether the request was queued through fusillade (has a
/// `fusillade_batch_id`) and its SLA (`completion_window`):
/// - `realtime` — synchronous: no batch id, no SLA window
/// - `flex`     — 1h SLA, no batch id (async single request)
/// - `async`    — 1h SLA with a batch id
/// - `batch`    — 24h SLA with a batch id (the /v1/batches API)
pub(crate) fn compute_billing_tier(fusillade_batch_id: Option<Uuid>, completion_window: Option<&str>) -> &'static str {
    let window = completion_window.filter(|w| !w.is_empty());
    match (fusillade_batch_id.is_some(), window) {
        (false, None) => "realtime",
        (false, _) => "flex",
        (true, Some("24h")) => "batch",
        (true, _) => "async",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_billing_tier() {
        let batch_id = Uuid::new_v4();
        assert_eq!(compute_billing_tier(None, None), "realtime");
        assert_eq!(compute_billing_tier(None, Some("")), "realtime");
        assert_eq!(compute_billing_tier(None, Some("1h")), "flex");
        assert_eq!(compute_billing_tier(Some(batch_id), Some("1h")), "async");
        assert_eq!(compute_billing_tier(Some(batch_id), Some("24h")), "batch");
    }

    #[test]
    fn test_raw_analytics_record_creation() {
        let api_key_id = Uuid::new_v4();
        let record = RawAnalyticsRecord {
            served_by: None,
            instance_id: Uuid::new_v4(),
            correlation_id: 123,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/ai/v1/chat/completions".to_string(),
            request_model: Some("gpt-4".to_string()),
            response_model: Some("gpt-4".to_string()),
            status_code: 200,
            duration_ms: 100,
            duration_to_first_byte_ms: Some(50),
            prompt_tokens: 10,
            completion_tokens: 20,
            reasoning_tokens: 0,
            total_tokens: 30,
            cache_read_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_creation_24h_input_tokens: 0,
            response_type: "chat_completion".to_string(),
            finish_reason: None,
            user_agent: None,
            engine_cached_tokens: None,
            cache_read_source: None,
            request_params: RequestParams::default(),
            server_address: "localhost".to_string(),
            server_port: 8080,
            api_key_id: Some(api_key_id),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            batch_completion_window: None,
            batch_created_at: None,
            batch_request_source: "".to_string(),
            trace_id: None,
        };

        assert_eq!(record.correlation_id, 123);

        let payload = serde_json::to_value(&record).unwrap();
        let restored: RawAnalyticsRecord = serde_json::from_value(payload).unwrap();
        assert_eq!(restored.api_key_id, Some(api_key_id));
    }

    #[test]
    fn test_parse_api_key_purpose() {
        assert_eq!(parse_api_key_purpose("platform"), ApiKeyPurpose::Platform);
        assert_eq!(parse_api_key_purpose("batch"), ApiKeyPurpose::Batch);
        assert_eq!(parse_api_key_purpose("playground"), ApiKeyPurpose::Playground);
        assert_eq!(parse_api_key_purpose("realtime"), ApiKeyPurpose::Realtime);
        assert_eq!(parse_api_key_purpose("unknown"), ApiKeyPurpose::Realtime);
    }

    /// A minimal record carrying just the token fields the cost arithmetic reads.
    fn cost_record(prompt: i64, completion: i64, read: i64, c5: i64, c1: i64, c24: i64) -> RawAnalyticsRecord {
        RawAnalyticsRecord {
            served_by: None,
            instance_id: Uuid::new_v4(),
            correlation_id: 1,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/ai/v1/chat/completions".to_string(),
            request_model: Some("m".to_string()),
            response_model: Some("m".to_string()),
            status_code: 200,
            duration_ms: 1,
            duration_to_first_byte_ms: None,
            prompt_tokens: prompt,
            completion_tokens: completion,
            reasoning_tokens: 0,
            total_tokens: prompt + completion,
            cache_read_input_tokens: read,
            cache_creation_5m_input_tokens: c5,
            cache_creation_1h_input_tokens: c1,
            cache_creation_24h_input_tokens: c24,
            response_type: "chat_completion".to_string(),
            finish_reason: None,
            user_agent: None,
            engine_cached_tokens: None,
            cache_read_source: None,
            request_params: RequestParams::default(),
            server_address: "x".to_string(),
            server_port: 1,
            api_key_id: None,
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            batch_completion_window: None,
            batch_created_at: None,
            batch_request_source: String::new(),
            trace_id: None,
        }
    }

    // input price 0.001, output price 0.002.
    fn inp() -> Decimal {
        Decimal::new(1, 3)
    }
    fn outp() -> Decimal {
        Decimal::new(2, 3)
    }

    /// The mapping the batcher owns: a `RawAnalyticsRecord`'s token fields into the shape
    /// `crate::pricing` prices. The arithmetic itself is tested in `crate::pricing`; what
    /// matters here is that every field lands in the right slot — a transposed cache tier
    /// would silently bill reads at a write multiplier.
    #[test]
    fn token_counts_from_raw_record_maps_every_field() {
        let r = cost_record(1000, 100, 500, 1, 2, 3);
        let c = TokenCounts::from(&r);
        assert_eq!(c.prompt, 1000);
        assert_eq!(c.completion, 100);
        assert_eq!(c.cache_read, 500);
        assert_eq!(c.cache_creation_5m, 1);
        assert_eq!(c.cache_creation_1h, 2);
        assert_eq!(c.cache_creation_24h, 3);
    }

    #[test]
    fn list_price_ignores_cache_split_and_is_none_without_pricing() {
        // Cache tokens present, but the list price is the full input+output at base rates.
        let r = cost_record(1000, 100, 500, 0, 200, 0);
        let list = compute_list_price(&r, Some(inp()), Some(outp())).unwrap();
        assert_eq!(list, Decimal::new(12, 1)); // 1000*0.001 + 100*0.002 = 1.2
        assert!(compute_list_price(&r, None, None).is_none(), "no pricing → NULL list price");
    }

    #[test]
    fn test_compute_request_origin() {
        let batch_id = Uuid::new_v4();

        // Any request with fusillade_batch_id is "fusillade"
        assert_eq!(compute_request_origin(None, Some(batch_id)), "fusillade");
        assert_eq!(compute_request_origin(Some(&ApiKeyPurpose::Realtime), Some(batch_id)), "fusillade");
        assert_eq!(
            compute_request_origin(Some(&ApiKeyPurpose::Playground), Some(batch_id)),
            "fusillade"
        );

        // Batch API keys without fusillade_batch_id are still "fusillade"
        assert_eq!(compute_request_origin(Some(&ApiKeyPurpose::Batch), None), "fusillade");

        // Playground keys are "frontend"
        assert_eq!(compute_request_origin(Some(&ApiKeyPurpose::Playground), None), "frontend");

        // Everything else is "api"
        assert_eq!(compute_request_origin(None, None), "api");
        assert_eq!(compute_request_origin(Some(&ApiKeyPurpose::Realtime), None), "api");
        assert_eq!(compute_request_origin(Some(&ApiKeyPurpose::Platform), None), "api");
    }
}

/// Integration tests for the batcher that require database access
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::api::models::transactions::TransactionFilters;
    use crate::api::models::users::Role;
    use crate::db::handlers::Repository;
    use crate::db::handlers::credits::Credits;
    use crate::db::models::credits::CreditTransactionType;
    use crate::pricing::CacheMultipliers;
    use crate::test::utils::create_test_user;
    use rust_decimal::prelude::FromStr;

    /// Helper: Create a test model with endpoint
    async fn create_test_model(pool: &sqlx::PgPool, model_name: &str) -> crate::types::DeploymentId {
        use crate::db::handlers::{Deployments, InferenceEndpoints};
        use crate::db::models::{deployments::DeploymentCreateDBRequest, inference_endpoints::InferenceEndpointCreateDBRequest};
        use std::str::FromStr as _;

        let user = create_test_user(pool, Role::StandardUser).await;

        // Create endpoint
        let mut conn = pool.acquire().await.unwrap();
        let mut endpoints_repo = InferenceEndpoints::new(&mut conn);
        let endpoint = endpoints_repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: user.id,
                name: format!("test-endpoint-{}", Uuid::new_v4()),
                description: None,
                url: url::Url::from_str("https://api.test.com").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
                reasoning_translation: None,
                accepts_scheduling_priority: false,
            })
            .await
            .unwrap();

        // Create deployment
        let mut conn = pool.acquire().await.unwrap();
        let mut deployments_repo = Deployments::new(&mut conn);
        let deployment = deployments_repo
            .create(&DeploymentCreateDBRequest {
                created_by: user.id,
                model_name: model_name.to_string(),
                alias: model_name.to_string(),
                display_name: None,
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: Some(endpoint.id),
                requests_per_second: None,
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: false,
                lb_strategy: None,
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_realtime_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                backoff_enabled: false,
                backoff_initial_ms: 100,
                backoff_max_ms: 5_000,
                backoff_factor: 2.0,
                backoff_jitter: "full".to_string(),
                backoff_max_total_ms: None,
                first_token_timeout_ms: None,
                aimd: None,
                sanitize_responses: true,
                trusted: false,
                reasoning_translation_overrides: None,
                allowed_batch_completion_windows: None,
                metadata: None,
            })
            .await
            .unwrap();

        deployment.id
    }

    /// Helper: Setup a tariff for a model
    /// Note: Batch tariffs require a completion_window per database constraint
    async fn setup_tariff(
        pool: &sqlx::PgPool,
        deployed_model_id: crate::types::DeploymentId,
        input_price: Decimal,
        output_price: Decimal,
        api_key_purpose: ApiKeyPurpose,
    ) {
        use crate::db::handlers::Tariffs;
        use crate::db::models::tariffs::TariffCreateDBRequest;

        let mut conn = pool.acquire().await.unwrap();
        let mut tariffs_repo = Tariffs::new(&mut conn);

        // Batch tariffs require a completion_window
        let completion_window = if api_key_purpose == ApiKeyPurpose::Batch {
            Some("24h".to_string())
        } else {
            None
        };

        tariffs_repo
            .create(&TariffCreateDBRequest {
                deployed_model_id,
                name: format!("{:?}_tariff", api_key_purpose),
                api_key_purpose: Some(api_key_purpose),
                input_price_per_token: input_price,
                output_price_per_token: output_price,
                valid_from: None,
                completion_window,
            })
            .await
            .unwrap();
    }

    /// Helper: Create a user with initial balance
    async fn setup_user_with_balance(pool: &sqlx::PgPool, balance: Decimal) -> Uuid {
        use crate::db::handlers::credits::Credits;
        use crate::db::models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType};

        let user = create_test_user(pool, Role::StandardUser).await;

        if balance > Decimal::ZERO {
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::Purchase,
                    amount: balance,
                    source_id: format!("test-topup-{}", Uuid::new_v4()),
                    description: Some("Test topup".to_string()),
                    fusillade_batch_id: None,
                    api_key_id: None,
                })
                .await
                .unwrap();
        }

        user.id
    }

    /// Helper: Create an API key for a user
    async fn create_api_key_for_user(pool: &sqlx::PgPool, user_id: Uuid, purpose: ApiKeyPurpose) -> Uuid {
        use crate::db::handlers::api_keys::ApiKeys;
        use crate::db::models::api_keys::ApiKeyCreateDBRequest;

        let mut conn = pool.acquire().await.unwrap();
        let mut api_keys = ApiKeys::new(&mut conn);
        let api_key = api_keys
            .create(&ApiKeyCreateDBRequest {
                user_id,
                name: format!("test-key-{}", Uuid::new_v4()),
                description: None,
                purpose,
                requests_per_second: None,
                burst_size: None,
                created_by: user_id,
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();

        api_key.id
    }

    /// Helper: Create a raw analytics record for testing
    fn create_raw_record(model: &str, api_key_id: Option<Uuid>, prompt_tokens: i64, completion_tokens: i64) -> RawAnalyticsRecord {
        RawAnalyticsRecord {
            served_by: None,
            instance_id: Uuid::new_v4(),
            correlation_id: rand::random::<i64>().abs(),
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/ai/v1/chat/completions".to_string(),
            request_model: Some(model.to_string()),
            response_model: Some(model.to_string()),
            status_code: 200,
            duration_ms: 100,
            duration_to_first_byte_ms: Some(50),
            prompt_tokens,
            completion_tokens,
            reasoning_tokens: 0,
            total_tokens: prompt_tokens + completion_tokens,
            cache_read_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_creation_24h_input_tokens: 0,
            response_type: "chat_completion".to_string(),
            finish_reason: None,
            user_agent: None,
            engine_cached_tokens: None,
            cache_read_source: None,
            request_params: RequestParams::default(),
            server_address: "api.test.com".to_string(),
            server_port: 443,
            api_key_id,
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            batch_completion_window: None,
            batch_created_at: None,
            batch_request_source: String::new(),
            trace_id: None,
        }
    }

    /// Run the batcher with given records and wait for completion
    async fn run_batcher_with_records(pool: &sqlx::PgPool, records: Vec<RawAnalyticsRecord>) {
        let config = crate::test::utils::create_test_config();
        let (batcher, writer) = AnalyticsBatcher::<crate::metrics::GenAiMetrics>::new(pool.clone(), config, None);

        for record in records {
            writer.publish(record).await.unwrap();
        }

        while batcher.project_outbox_batch().await.unwrap() > 0 {}
    }

    #[sqlx::test]
    async fn test_writer_checkpoints_raw_record_before_enrichment(pool: sqlx::PgPool) {
        let config = crate::test::utils::create_test_config();
        let (_projector, writer) = AnalyticsBatcher::<crate::metrics::GenAiMetrics>::new(pool.clone(), config, None);
        let api_key_id = Uuid::new_v4();
        let record = create_raw_record("checkpoint-test", Some(api_key_id), 10, 5);
        let instance_id = record.instance_id;
        let correlation_id = record.correlation_id;

        writer.publish(record).await.unwrap();

        let payload: serde_json::Value =
            sqlx::query_scalar("SELECT payload FROM analytics_outbox WHERE instance_id = $1 AND correlation_id = $2")
                .bind(instance_id)
                .bind(correlation_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(payload["api_key_id"], api_key_id.to_string());
        assert!(payload.get("user_id").is_none());
        assert!(payload.get("total_cost").is_none());
    }

    #[sqlx::test]
    async fn test_projector_accepts_deployed_and_raw_outbox_payloads(pool: sqlx::PgPool) {
        let model_id = create_test_model(&pool, "mixed-outbox-payload-test").await;
        setup_tariff(
            &pool,
            model_id,
            Decimal::from_str("0.00001").unwrap(),
            Decimal::from_str("0.00003").unwrap(),
            ApiKeyPurpose::Realtime,
        )
        .await;
        let user_id = setup_user_with_balance(&pool, Decimal::from(10)).await;
        let api_key_id = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;

        let config = crate::test::utils::create_test_config();
        let (batcher, writer) = AnalyticsBatcher::<crate::metrics::GenAiMetrics>::new(pool.clone(), config, None);

        let deployed_raw = create_raw_record("mixed-outbox-payload-test", Some(api_key_id), 10, 5);
        let mut tx = pool.begin().await.unwrap();
        let deployed_enriched = batcher
            .enrich_batch(&mut tx, std::slice::from_ref(&deployed_raw))
            .await
            .unwrap()
            .remove(0);
        tx.rollback().await.unwrap();

        // Match the payload emitted by release 11.12.0: EnrichedRecord at the
        // top level and no api_key_id field inside its nested raw record.
        let mut deployed_payload = serde_json::to_value(deployed_enriched).unwrap();
        deployed_payload
            .get_mut("raw")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("api_key_id");
        sqlx::query("INSERT INTO analytics_outbox (instance_id, correlation_id, payload) VALUES ($1, $2, $3)")
            .bind(deployed_raw.instance_id)
            .bind(deployed_raw.correlation_id)
            .bind(deployed_payload)
            .execute(&pool)
            .await
            .unwrap();

        let raw = create_raw_record("mixed-outbox-payload-test", Some(api_key_id), 20, 10);
        writer.publish(raw).await.unwrap();

        assert_eq!(batcher.project_outbox_batch().await.unwrap(), 2);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM analytics_outbox")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM http_analytics WHERE model = 'mixed-outbox-payload-test'",)
                .fetch_one(&pool)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM credits_transactions WHERE transaction_type = 'usage' AND api_key_id = $1",)
                .bind(api_key_id)
                .fetch_one(&pool)
                .await
                .unwrap(),
            2
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_credit_deduction_successful(pool: sqlx::PgPool) {
        // Setup: Create model with tariff
        let model_id = create_test_model(&pool, "gpt-4-test").await;
        let input_price = Decimal::from_str("0.00001").unwrap();
        let output_price = Decimal::from_str("0.00003").unwrap();
        setup_tariff(&pool, model_id, input_price, output_price, ApiKeyPurpose::Realtime).await;

        // Setup: User with $10.00 balance
        let initial_balance = Decimal::from_str("10.00").unwrap();
        let user_id = setup_user_with_balance(&pool, initial_balance).await;
        let api_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;

        // Create record: 1000 input tokens, 500 output tokens
        // Expected cost: (1000 * 0.00001) + (500 * 0.00003) = 0.01 + 0.015 = 0.025
        let record = create_raw_record("gpt-4-test", Some(api_key), 1000, 500);

        // Run batcher
        run_batcher_with_records(&pool, vec![record]).await;

        // Verify: Balance should be deducted
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let final_balance = credits.get_user_balance(user_id).await.unwrap();

        let expected_cost = Decimal::from_str("0.025").unwrap();
        let expected_balance = initial_balance - expected_cost;
        assert_eq!(final_balance, expected_balance, "Balance should be deducted correctly");

        // Verify: Transaction was created
        let transactions = credits
            .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
            .await
            .unwrap();
        let usage_tx = transactions.iter().find(|tx| tx.transaction_type == CreditTransactionType::Usage);
        assert!(usage_tx.is_some(), "Usage transaction should be created");
        assert_eq!(usage_tx.unwrap().amount, expected_cost);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn failed_response_is_analytic_but_not_billed(pool: sqlx::PgPool) {
        let model_id = create_test_model(&pool, "failed-response-test").await;
        setup_tariff(
            &pool,
            model_id,
            Decimal::from_str("0.00001").unwrap(),
            Decimal::from_str("0.00003").unwrap(),
            ApiKeyPurpose::Realtime,
        )
        .await;

        let initial_balance = Decimal::from_str("10.00").unwrap();
        let user_id = setup_user_with_balance(&pool, initial_balance).await;
        let api_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;
        let mut record = create_raw_record("failed-response-test", Some(api_key), 1000, 500);
        record.status_code = 502;
        run_batcher_with_records(&pool, vec![record]).await;

        let (analytics_status, total_cost): (i32, Option<Decimal>) =
            sqlx::query_as("SELECT status_code, total_cost FROM http_analytics WHERE model = 'failed-response-test'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(analytics_status, 502);
        assert_eq!(total_cost, None);

        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        assert_eq!(credits.get_user_balance(user_id).await.unwrap(), initial_balance);
        let transactions = credits
            .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
            .await
            .unwrap();
        assert!(transactions.iter().all(|tx| tx.transaction_type != CreditTransactionType::Usage));

        let outbox_depth: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM analytics_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(outbox_depth, 0, "projection must delete the durable row after committing analytics");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_cache_discount_applied(pool: sqlx::PgPool) {
        // Model with a base tariff + a cache tariff (presence = enabled): 1h write ×2.0,
        // read ×0.1. The other tiers are set but unused by this request.
        let model_id = create_test_model(&pool, "cache-bill-test").await;
        let input_price = Decimal::from_str("0.00001").unwrap();
        let output_price = Decimal::from_str("0.00003").unwrap();
        setup_tariff(&pool, model_id, input_price, output_price, ApiKeyPurpose::Realtime).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, read_multiplier, min_prefix_tokens)
               VALUES ($1, 1.25, 2.0, 2.5, 0.1, 1024)"#,
            model_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let initial_balance = Decimal::from_str("10.00").unwrap();
        let user_id = setup_user_with_balance(&pool, initial_balance).await;
        let api_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;

        // 2000 input = 1000 read + 500 1h-creation + 500 uncached; 500 output.
        let mut record = create_raw_record("cache-bill-test", Some(api_key), 2000, 500);
        record.cache_read_input_tokens = 1000;
        record.cache_creation_1h_input_tokens = 500;

        run_batcher_with_records(&pool, vec![record]).await;

        // input = 500*1e-5 (uncached) + 1000*1e-5*0.1 (read) + 500*1e-5*2.0 (1h write)
        //       = 0.005 + 0.001 + 0.010 = 0.016 ; output = 500*3e-5 = 0.015 → 0.031
        let expected_cost = Decimal::from_str("0.031").unwrap();
        // List price (no caching): 2000*1e-5 + 500*3e-5 = 0.035 → savings 0.004.
        let expected_list = Decimal::from_str("0.035").unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let final_balance = credits.get_user_balance(user_id).await.unwrap();
        assert_eq!(
            final_balance,
            initial_balance - expected_cost,
            "the cache-discounted amount is billed, not the list price"
        );

        // http_analytics carries the split, the cache-adjusted total_cost, AND the
        // batcher-written list-price uncached_cost (savings = uncached_cost − total_cost).
        let row = sqlx::query!(
            r#"SELECT cache_read_input_tokens, cache_creation_input_tokens, cache_creation_1h_input_tokens,
                      total_cost, uncached_cost
               FROM http_analytics WHERE model = 'cache-bill-test'"#
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.cache_read_input_tokens, 1000);
        assert_eq!(row.cache_creation_input_tokens, 500);
        assert_eq!(row.cache_creation_1h_input_tokens, 500);
        assert_eq!(row.total_cost.unwrap(), expected_cost, "total_cost = cache-adjusted");
        assert_eq!(row.uncached_cost.unwrap(), expected_list, "uncached_cost = list price");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_different_tariffs_for_batch_and_realtime(pool: sqlx::PgPool) {
        // Setup: Create model with different tariffs for batch and realtime
        let model_id = create_test_model(&pool, "gpt-4-turbo-test").await;

        // Batch pricing: cheaper
        setup_tariff(
            &pool,
            model_id,
            Decimal::from_str("0.00005").unwrap(),
            Decimal::from_str("0.00010").unwrap(),
            ApiKeyPurpose::Batch,
        )
        .await;

        // Realtime pricing: more expensive (2x)
        setup_tariff(
            &pool,
            model_id,
            Decimal::from_str("0.00010").unwrap(),
            Decimal::from_str("0.00020").unwrap(),
            ApiKeyPurpose::Realtime,
        )
        .await;

        // Setup: User with balance
        let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
        let batch_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Batch).await;
        let realtime_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;

        // Create records: same tokens, different API keys
        // Batch record needs completion_window to match the batch tariff
        let mut batch_record = create_raw_record("gpt-4-turbo-test", Some(batch_key), 1000, 500);
        batch_record.batch_completion_window = Some("24h".to_string());
        let realtime_record = create_raw_record("gpt-4-turbo-test", Some(realtime_key), 1000, 500);

        // Run batcher
        run_batcher_with_records(&pool, vec![batch_record, realtime_record]).await;

        // Expected costs:
        // Batch: (1000 * 0.00005) + (500 * 0.00010) = 0.05 + 0.05 = 0.10
        // Realtime: (1000 * 0.00010) + (500 * 0.00020) = 0.10 + 0.10 = 0.20
        let expected_batch_cost = Decimal::from_str("0.10").unwrap();
        let expected_realtime_cost = Decimal::from_str("0.20").unwrap();
        let total_cost = expected_batch_cost + expected_realtime_cost;

        // Verify balance
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let final_balance = credits.get_user_balance(user_id).await.unwrap();
        let expected_balance = Decimal::from_str("100.00").unwrap() - total_cost;
        assert_eq!(final_balance, expected_balance, "Balance should reflect both charges");

        // Verify transactions
        let transactions = credits
            .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
            .await
            .unwrap();
        let usage_txs: Vec<_> = transactions
            .iter()
            .filter(|tx| tx.transaction_type == CreditTransactionType::Usage)
            .collect();
        assert_eq!(usage_txs.len(), 2, "Should have 2 usage transactions");

        // Check that we have both amounts (order may vary)
        let amounts: Vec<_> = usage_txs.iter().map(|tx| tx.amount).collect();
        assert!(amounts.contains(&expected_batch_cost), "Should have batch cost transaction");
        assert!(amounts.contains(&expected_realtime_cost), "Should have realtime cost transaction");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_folds_batch_analytics_into_aggregates(pool: sqlx::PgPool) {
        // Three requests of one batch fold their tokens / latency / list-cost into
        // batch_aggregates (COR-524), so get_batch_analytics can read the row instead of
        // scanning http_analytics. The third is on an UNPRICED model (free): it is not billed
        // (no credit row) but must still be counted in the analytics aggregates — proving the
        // fold rides the newly-inserted http_analytics rows, not the billed set.
        let model_id = create_test_model(&pool, "batch-analytics-test").await;
        let input_price = Decimal::from_str("0.00005").unwrap();
        let output_price = Decimal::from_str("0.00010").unwrap();
        setup_tariff(&pool, model_id, input_price, output_price, ApiKeyPurpose::Batch).await;
        // Free model — no tariff, so its requests get total_cost = None and are skipped by billing.
        create_test_model(&pool, "batch-free-test").await;

        let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
        let batch_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Batch).await;
        let batch_id = Uuid::new_v4();

        // Request A: 1000/500 tokens, 0 reasoning, duration 100, ttfb 50.
        let mut a = create_raw_record("batch-analytics-test", Some(batch_key.clone()), 1000, 500);
        a.batch_completion_window = Some("24h".to_string());
        a.fusillade_batch_id = Some(batch_id);
        a.duration_ms = 100;
        a.duration_to_first_byte_ms = Some(50);

        // Request B: 2000/800 tokens, 100 reasoning, duration 200, NO ttfb (streaming
        // metric absent) — exercises count_ttfb_ms counting only reported values.
        let mut b = create_raw_record("batch-analytics-test", Some(batch_key.clone()), 2000, 800);
        b.batch_completion_window = Some("24h".to_string());
        b.fusillade_batch_id = Some(batch_id);
        b.reasoning_tokens = 100;
        b.total_tokens = 2900;
        b.duration_ms = 200;
        b.duration_to_first_byte_ms = None;

        // Request C: FREE model (no tariff) → not billed, but must still fold into analytics.
        // 500/100 tokens (total 600), duration 50, no ttfb.
        let mut c = create_raw_record("batch-free-test", Some(batch_key), 500, 100);
        c.batch_completion_window = Some("24h".to_string());
        c.fusillade_batch_id = Some(batch_id);
        c.duration_ms = 50;
        c.duration_to_first_byte_ms = None;

        run_batcher_with_records(&pool, vec![a, b, c]).await;

        // List cost = list price (no cache) = billed cost here. C is free (no tariff → 0).
        // A: 1000*5e-5 + 500*1e-4 = 0.05 + 0.05 = 0.10
        // B: 2000*5e-5 + 800*1e-4 = 0.10 + 0.08 = 0.18  → 0.28 total (C adds 0)
        let agg = sqlx::query!(
            r#"
            SELECT transaction_count, total_requests, total_amount,
                   total_prompt_tokens, total_completion_tokens, total_reasoning_tokens, total_tokens,
                   sum_duration_ms, count_duration_ms, sum_ttfb_ms, count_ttfb_ms,
                   total_list_cost, analytics_backfilled_at
            FROM batch_aggregates WHERE fusillade_batch_id = $1
            "#,
            batch_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // Billing counts only the two PRICED requests; analytics counts all three 2xx requests.
        assert_eq!(agg.transaction_count, 2, "only the two priced requests are billed");
        assert_eq!(agg.total_requests, 3, "all three 2xx requests counted in analytics (incl. free C)");
        assert_eq!(agg.total_prompt_tokens, 3500, "1000 (A) + 2000 (B) + 500 (C)");
        assert_eq!(agg.total_completion_tokens, 1400, "500 (A) + 800 (B) + 100 (C)");
        assert_eq!(agg.total_reasoning_tokens, 100);
        assert_eq!(agg.total_tokens, 5000, "1500 (A) + 2900 (B) + 600 (C)");
        assert_eq!(agg.sum_duration_ms, 350, "100 + 200 + 50");
        assert_eq!(agg.count_duration_ms, 3, "all three reported duration");
        assert_eq!(agg.sum_ttfb_ms, 50, "only A reported ttfb");
        assert_eq!(agg.count_ttfb_ms, 1, "only A counted for the AVG denominator");
        assert_eq!(agg.total_list_cost, Decimal::from_str("0.28").unwrap(), "free C contributes 0 cost");
        assert_eq!(
            agg.total_amount,
            Decimal::from_str("0.28").unwrap(),
            "no cache → billed == list; C not billed"
        );
        assert!(agg.analytics_backfilled_at.is_none(), "live fold leaves the backfill marker null");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_denormalizes_fusillade_request_id_onto_credits(pool: sqlx::PgPool) {
        // A batched request's credit row carries its fusillade_request_id (migration 120), so the
        // responses view can read per-request cost off the ledger durably instead of joining
        // http_analytics (COR-524 follow-up / Usage E).
        let model_id = create_test_model(&pool, "req-id-credits-test").await;
        setup_tariff(
            &pool,
            model_id,
            Decimal::from_str("0.00005").unwrap(),
            Decimal::from_str("0.00010").unwrap(),
            ApiKeyPurpose::Batch,
        )
        .await;
        let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
        let batch_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Batch).await;

        let request_id = Uuid::new_v4();
        let mut rec = create_raw_record("req-id-credits-test", Some(batch_key), 1000, 500);
        rec.batch_completion_window = Some("24h".to_string());
        rec.fusillade_batch_id = Some(Uuid::new_v4());
        rec.fusillade_request_id = Some(request_id);

        run_batcher_with_records(&pool, vec![rec]).await;

        // The credit is keyed to the request durably (no http_analytics needed).
        let row = sqlx::query!(
            "SELECT fusillade_request_id, amount FROM credits_transactions \
             WHERE fusillade_request_id = $1 AND transaction_type = 'usage'",
            request_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.fusillade_request_id, Some(request_id));
        assert_eq!(row.amount, Decimal::from_str("0.10").unwrap(), "1000*5e-5 + 500*1e-4");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_future_fusillade_success_index_gates_billing_and_aggregation(pool: sqlx::PgPool) {
        // This preparation release does not ship the index: production history
        // must be reconciled first. Install its intended definition in this
        // isolated test database to prove the application is ready for it.
        sqlx::query(
            "CREATE UNIQUE INDEX test_http_analytics_fusillade_success_unique \
             ON http_analytics (fusillade_request_id) \
             WHERE fusillade_request_id IS NOT NULL AND status_code BETWEEN 200 AND 299",
        )
        .execute(&pool)
        .await
        .unwrap();

        let model_id = create_test_model(&pool, "fusillade-billing-idempotency").await;
        setup_tariff(
            &pool,
            model_id,
            Decimal::from_str("0.00005").unwrap(),
            Decimal::from_str("0.00010").unwrap(),
            ApiKeyPurpose::Batch,
        )
        .await;
        let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
        let batch_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Batch).await;
        let batch_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        // Failed attempts remain observable, but must not become the canonical
        // usage row or prevent a later success from being billed.
        let mut failed = create_raw_record("fusillade-billing-idempotency", Some(batch_key.clone()), 1000, 500);
        failed.status_code = 500;
        failed.batch_completion_window = Some("24h".to_string());
        failed.fusillade_batch_id = Some(batch_id);
        failed.fusillade_request_id = Some(request_id);

        let mut first = create_raw_record("fusillade-billing-idempotency", Some(batch_key.clone()), 1000, 500);
        first.batch_completion_window = Some("24h".to_string());
        first.fusillade_batch_id = Some(batch_id);
        first.fusillade_request_id = Some(request_id);

        // A second successful physical attempt has a different gateway
        // identity but the same logical Fusillade request identity.
        let mut duplicate = create_raw_record("fusillade-billing-idempotency", Some(batch_key), 1000, 500);
        duplicate.batch_completion_window = Some("24h".to_string());
        duplicate.fusillade_batch_id = Some(batch_id);
        duplicate.fusillade_request_id = Some(request_id);

        run_batcher_with_records(&pool, vec![failed]).await;
        run_batcher_with_records(&pool, vec![first, duplicate]).await;

        let analytics_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM http_analytics WHERE fusillade_request_id = $1")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let charge_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM credits_transactions \
             WHERE fusillade_request_id = $1 AND transaction_type = 'usage'",
        )
        .bind(request_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let successful_analytics_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM http_analytics \
             WHERE fusillade_request_id = $1 AND status_code BETWEEN 200 AND 299",
        )
        .bind(request_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(analytics_count, 2, "the failed attempt and one canonical success remain observable");
        assert_eq!(successful_analytics_count, 1, "only one success may contribute usage");
        assert_eq!(charge_count, 1, "one Fusillade request must produce one debit");
        let aggregate = sqlx::query!(
            "SELECT total_requests, total_prompt_tokens, total_completion_tokens, total_amount \
             FROM batch_aggregates WHERE fusillade_batch_id = $1",
            batch_id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            aggregate.total_requests, 1,
            "customer-visible usage must count the logical request once"
        );
        assert_eq!(aggregate.total_prompt_tokens, 1000);
        assert_eq!(aggregate.total_completion_tokens, 500);
        assert_eq!(aggregate.total_amount, Decimal::from_str("0.10").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_legacy_fusillade_duplicates_remain_reconcilable_during_rollout(pool: sqlx::PgPool) {
        let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
        let batch_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Batch).await;
        let batch_key_id: Uuid = sqlx::query_scalar("SELECT id FROM api_keys WHERE secret = $1")
            .bind(batch_key)
            .fetch_one(&pool)
            .await
            .unwrap();
        let batch_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        // A pre-change binary supplies fusillade_request_id but uses a
        // per-attempt analytics source_id. Both writes remain visible during
        // the rollout window so the separate reconciliation can find them.
        let insert_legacy = |source_id: &'static str| {
            sqlx::query(
                r#"
                INSERT INTO credits_transactions (
                    user_id, transaction_type, amount, source_id, description,
                    fusillade_batch_id, api_key_id, service_tier, fusillade_request_id
                )
                VALUES ($1, 'usage', $2, $3, 'legacy Fusillade usage', $4, $5, 'batch', $6)
                ON CONFLICT (source_id) DO NOTHING
                "#,
            )
            .bind(user_id)
            .bind(Decimal::from_str("0.10").unwrap())
            .bind(source_id)
            .bind(batch_id)
            .bind(batch_key_id)
            .bind(request_id)
            .execute(&pool)
        };

        let (first, second) = tokio::join!(insert_legacy("legacy-attempt-a"), insert_legacy("legacy-attempt-b"));
        first.unwrap();
        second.unwrap();

        let rows = sqlx::query!(
            "SELECT source_id, fusillade_request_id \
             FROM credits_transactions WHERE fusillade_request_id = $1",
            request_id,
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "rollout-window duplicates must remain available for reconciliation");
        assert!(rows.iter().all(|row| row.fusillade_request_id == Some(request_id)));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_shared_fusillade_id_does_not_suppress_realtime_billing(pool: sqlx::PgPool) {
        let model_id = create_test_model(&pool, "spoofed-fusillade-id").await;
        setup_tariff(
            &pool,
            model_id,
            Decimal::from_str("0.00005").unwrap(),
            Decimal::from_str("0.00010").unwrap(),
            ApiKeyPurpose::Realtime,
        )
        .await;
        let first_user = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
        let second_user = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
        let first_key = create_api_key_for_user(&pool, first_user, ApiKeyPurpose::Realtime).await;
        let second_key = create_api_key_for_user(&pool, second_user, ApiKeyPurpose::Realtime).await;
        let spoofed_request_id = Uuid::new_v4();

        let mut first = create_raw_record("spoofed-fusillade-id", Some(first_key), 1000, 500);
        first.fusillade_request_id = Some(spoofed_request_id);
        let mut second = create_raw_record("spoofed-fusillade-id", Some(second_key), 1000, 500);
        second.fusillade_request_id = Some(spoofed_request_id);

        run_batcher_with_records(&pool, vec![first, second]).await;

        let rows = sqlx::query!(
            "SELECT user_id, fusillade_request_id FROM credits_transactions \
             WHERE user_id = ANY($1) AND transaction_type = 'usage'",
            &[first_user, second_user],
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "an untrusted header must not deduplicate billing");
        assert!(
            rows.iter().all(|row| row.fusillade_request_id == Some(spoofed_request_id)),
            "without the future analytics index, the shared correlation id is retained for reconciliation"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_fallback_to_realtime_when_batch_tariff_missing(pool: sqlx::PgPool) {
        // Setup: Create model with ONLY realtime tariff
        let model_id = create_test_model(&pool, "gpt-4-fallback-test").await;
        let realtime_input = Decimal::from_str("0.00015").unwrap();
        let realtime_output = Decimal::from_str("0.00030").unwrap();
        setup_tariff(&pool, model_id, realtime_input, realtime_output, ApiKeyPurpose::Realtime).await;

        // Setup: User with batch API key (no batch tariff exists)
        let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
        let batch_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Batch).await;

        // Create batch record
        let record = create_raw_record("gpt-4-fallback-test", Some(batch_key), 1000, 500);

        // Run batcher
        run_batcher_with_records(&pool, vec![record]).await;

        // Expected: Should fall back to realtime pricing
        // Cost: (1000 * 0.00015) + (500 * 0.00030) = 0.15 + 0.15 = 0.30
        let expected_cost = Decimal::from_str("0.30").unwrap();

        // Verify
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let final_balance = credits.get_user_balance(user_id).await.unwrap();
        let expected_balance = Decimal::from_str("100.00").unwrap() - expected_cost;
        assert_eq!(
            final_balance, expected_balance,
            "Batch request should fall back to realtime pricing"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_skip_deduction_when_no_pricing(pool: sqlx::PgPool) {
        // Setup: Create model WITHOUT any tariff
        let _model_id = create_test_model(&pool, "gpt-4-no-tariff").await;

        // Setup: User with balance
        let initial_balance = Decimal::from_str("100.00").unwrap();
        let user_id = setup_user_with_balance(&pool, initial_balance).await;
        let api_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;

        // Create record
        let record = create_raw_record("gpt-4-no-tariff", Some(api_key), 1000, 500);

        // Run batcher
        run_batcher_with_records(&pool, vec![record]).await;

        // Verify: Balance should NOT be deducted (no pricing)
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let final_balance = credits.get_user_balance(user_id).await.unwrap();
        assert_eq!(
            final_balance, initial_balance,
            "Balance should not change when no pricing configured"
        );

        // Verify: No usage transaction created
        let transactions = credits
            .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
            .await
            .unwrap();
        let usage_txs: Vec<_> = transactions
            .iter()
            .filter(|tx| tx.transaction_type == CreditTransactionType::Usage)
            .collect();
        assert_eq!(usage_txs.len(), 0, "Should have no usage transactions");
    }

    /// The last hop, against a real table: a record carrying a client must persist it to
    /// `http_analytics.user_agent`. Everything upstream of here — stamping the batch,
    /// forwarding the key on claim, emitting the header, reading it back off the request —
    /// is worth nothing if the column ends up empty, and this is the column every dashboard
    /// reads.
    #[sqlx::test]
    #[test_log::test]
    async fn batcher_persists_the_user_agent_to_http_analytics(pool: sqlx::PgPool) {
        create_test_model(&pool, "ua-persist-test").await;

        let mut record = create_raw_record("ua-persist-test", None, 10, 5);
        record.user_agent = Some("claude-cli/1.2.3".to_string());
        run_batcher_with_records(&pool, vec![record]).await;

        let stored: Option<String> = sqlx::query_scalar("SELECT user_agent FROM http_analytics WHERE model = 'ua-persist-test'")
            .fetch_one(&pool)
            .await
            .expect("the analytics row should exist");
        assert_eq!(stored.as_deref(), Some("claude-cli/1.2.3"));
    }

    /// The same last hop for `submitted_at`. The value already reached this struct — it
    /// has been read for batch-creation pricing for as long as the header has existed —
    /// so the only thing that can break is the write, and a dropped bind would leave the
    /// column silently NULL on every row.
    ///
    /// Worth testing rather than assuming, because NULL here does not necessarily stay
    /// NULL for downstream consumers — a missing value can surface as the Unix epoch, and
    /// any duration computed from it then reads as decades of queue delay.
    #[sqlx::test]
    #[test_log::test]
    async fn batcher_persists_the_submitted_at_to_http_analytics(pool: sqlx::PgPool) {
        create_test_model(&pool, "submitted-at-test").await;

        let submitted = Utc::now() - chrono::Duration::minutes(90);
        let mut record = create_raw_record("submitted-at-test", None, 10, 5);
        record.batch_created_at = Some(submitted);
        run_batcher_with_records(&pool, vec![record]).await;

        let stored: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT submitted_at FROM http_analytics WHERE model = 'submitted-at-test'")
            .fetch_one(&pool)
            .await
            .expect("the analytics row should exist");

        let stored = stored.expect("submitted_at should be persisted, not NULL");
        assert!(
            (stored - submitted).num_seconds().abs() < 1,
            "expected {submitted}, stored {stored}"
        );
    }

    #[test]
    fn implicit_reads_clamp_the_read_multiplier_to_list_price() {
        let mults = CacheMultipliers {
            read: Decimal::new(15, 1), // 1.5 — misconfigured surcharge
            write_5m: Decimal::new(125, 2),
            write_1h: Decimal::TWO,
            write_24h: Decimal::new(25, 1),
        };
        // Engine-sourced read: clamped to 1 (never above list price).
        let clamped = clamp_implicit_read_multiplier(Some(mults), Some("engine")).unwrap();
        assert_eq!(clamped.read, Decimal::ONE);
        assert_eq!(clamped.write_1h, Decimal::TWO, "write premiums untouched");
        // Module-sourced (explicit) read: configured multiplier stands.
        assert_eq!(
            clamp_implicit_read_multiplier(Some(mults), Some("module")).unwrap().read,
            Decimal::new(15, 1)
        );
        // Sane multipliers pass through unchanged for both sources.
        let sane = CacheMultipliers {
            read: Decimal::new(1, 1),
            ..mults
        };
        assert_eq!(
            clamp_implicit_read_multiplier(Some(sane), Some("engine")).unwrap().read,
            Decimal::new(1, 1)
        );
        assert!(clamp_implicit_read_multiplier(None, Some("engine")).is_none());
    }

    /// The same last hop for `cache_read_source`: the value rides the `CacheBilling`
    /// extension into this struct upstream, so the only thing that can break is the
    /// write — a dropped bind or a misordered UNNEST array would leave the column
    /// silently NULL (or misaligned) on every row.
    #[sqlx::test]
    #[test_log::test]
    async fn batcher_persists_the_cache_read_source_to_http_analytics(pool: sqlx::PgPool) {
        create_test_model(&pool, "cache-source-test").await;

        let mut record = create_raw_record("cache-source-test", None, 10, 5);
        record.cache_read_source = Some("engine".to_string());
        run_batcher_with_records(&pool, vec![record.clone()]).await;

        let stored: Option<String> = sqlx::query_scalar("SELECT cache_read_source FROM http_analytics WHERE model = 'cache-source-test'")
            .fetch_one(&pool)
            .await
            .expect("the analytics row should exist");
        assert_eq!(stored.as_deref(), Some("engine"));

        // A second receipt for the same physical attempt is ignored. Analytics
        // records are complete when enqueued, so a replay must not rewrite the
        // canonical row or repeat any downstream usage effects.
        record.cache_read_source = Some("module".to_string());
        run_batcher_with_records(&pool, vec![record]).await;
        let rows: Vec<Option<String>> =
            sqlx::query_scalar("SELECT cache_read_source FROM http_analytics WHERE model = 'cache-source-test'")
                .fetch_all(&pool)
                .await
                .expect("query should succeed");
        assert_eq!(rows.len(), 1, "the duplicate receipt must not create another row");
        assert_eq!(rows[0].as_deref(), Some("engine"), "the first complete receipt remains canonical");
    }

    /// Realtime work is not submitted ahead of time, so there is no distinct submission
    /// moment and the column must stay NULL rather than being backfilled from `timestamp`.
    /// Writing one would invent a zero-length queue for a request that never queued, and
    /// "no submitted_at" is how a consumer tells deferred work from immediate.
    #[sqlx::test]
    #[test_log::test]
    async fn realtime_rows_have_no_submitted_at(pool: sqlx::PgPool) {
        create_test_model(&pool, "realtime-no-submit").await;

        let record = create_raw_record("realtime-no-submit", None, 10, 5);
        assert!(record.batch_created_at.is_none(), "fixture should be realtime");
        run_batcher_with_records(&pool, vec![record]).await;

        let stored: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT submitted_at FROM http_analytics WHERE model = 'realtime-no-submit'")
                .fetch_one(&pool)
                .await
                .expect("the analytics row should exist");
        assert!(stored.is_none(), "realtime should leave submitted_at NULL, got {stored:?}");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_skip_deduction_for_unauthenticated_requests(pool: sqlx::PgPool) {
        // Setup: Create model with tariff
        let model_id = create_test_model(&pool, "gpt-4-unauth-test").await;
        setup_tariff(
            &pool,
            model_id,
            Decimal::from_str("0.00010").unwrap(),
            Decimal::from_str("0.00020").unwrap(),
            ApiKeyPurpose::Realtime,
        )
        .await;

        // Create record without bearer token
        let record = create_raw_record("gpt-4-unauth-test", None, 1000, 500);

        // Run batcher - should not panic or create transactions
        run_batcher_with_records(&pool, vec![record]).await;

        // Verify: Analytics record was created
        let count = sqlx::query_scalar!("SELECT COUNT(*) FROM http_analytics WHERE model = 'gpt-4-unauth-test'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, Some(1), "Analytics record should be created");

        // Verify: No credit transaction (no user to charge)
        let tx_count = sqlx::query_scalar!("SELECT COUNT(*) FROM credits_transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tx_count, Some(0), "No credit transactions for unauthenticated requests");
    }

    /// Test that the batcher sends pg_notify when a user's balance is depleted (crosses zero downward)
    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_balance_depleted_notification(pool: sqlx::PgPool) {
        use sqlx::postgres::PgListener;
        use std::time::Duration;
        use tokio::time::timeout;

        // Setup: Create model with tariff that will cost $0.025 per request (1000 input + 500 output tokens)
        let model_id = create_test_model(&pool, "gpt-4-depletion-test").await;
        let input_price = Decimal::from_str("0.00001").unwrap();
        let output_price = Decimal::from_str("0.00003").unwrap();
        setup_tariff(&pool, model_id, input_price, output_price, ApiKeyPurpose::Realtime).await;

        // Setup: User with small balance that will be depleted by usage
        // Balance: $0.01, Cost per request: $0.025 → will go negative
        let initial_balance = Decimal::from_str("0.01").unwrap();
        let user_id = setup_user_with_balance(&pool, initial_balance).await;
        let api_key = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;

        // Set up listener for auth_config_changed notifications BEFORE running batcher
        let mut listener = PgListener::connect_with(&pool).await.expect("Failed to create listener");
        listener.listen("auth_config_changed").await.expect("Failed to listen");

        // Drain any notifications from setup (user went from 0 to positive during setup)
        while timeout(Duration::from_millis(10), listener.try_recv()).await.is_ok() {
            // Keep draining while notifications available
        }

        // Create record that will deplete balance: cost = (1000 * 0.00001) + (500 * 0.00003) = $0.025
        let record = create_raw_record("gpt-4-depletion-test", Some(api_key), 1000, 500);

        // Run batcher - this should trigger balance depletion notification
        run_batcher_with_records(&pool, vec![record]).await;

        // Should receive notification for balance depletion
        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for balance depletion notification")
            .expect("Failed to receive notification");

        assert_eq!(notification.channel(), "auth_config_changed");

        // Verify payload format: "credits_transactions:{epoch_micros}"
        let payload = notification.payload();
        assert!(
            payload.starts_with("credits_transactions:"),
            "Expected payload to start with 'credits_transactions:', got: {}",
            payload
        );

        // Verify balance is actually negative
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let final_balance = credits.get_user_balance(user_id).await.unwrap();
        assert!(
            final_balance < Decimal::ZERO,
            "Balance should be negative after depletion, got: {}",
            final_balance
        );
    }

    /// Drain the listener asserting no `api_key_spend_cap:` notification arrives.
    async fn assert_no_cap_notification(listener: &mut sqlx::postgres::PgListener) {
        use std::time::Duration;
        use tokio::time::timeout;
        while let Ok(Ok(n)) = timeout(Duration::from_millis(500), listener.try_recv()).await {
            if let Some(n) = n {
                assert!(
                    !n.payload().starts_with("api_key_spend_cap:"),
                    "unexpected cap notification: {}",
                    n.payload()
                );
            } else {
                break;
            }
        }
    }

    /// Mixed flush across a cap scope: the parent's realtime row and the
    /// child's batch row fold into ONE checkpoint row keyed by the scope root,
    /// uncapped keys produce no row, the crossing NOTIFY fires exactly once
    /// (edge-triggered), and further over-cap flushes fold silently.
    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_folds_cap_scope_and_notifies_on_crossing(pool: sqlx::PgPool) {
        use crate::db::handlers::api_keys::ApiKeys;
        use sqlx::postgres::PgListener;
        use std::time::Duration;
        use tokio::time::timeout;

        let model_id = create_test_model(&pool, "gpt-4-cap-fold-test").await;
        let input_price = Decimal::from_str("0.00001").unwrap();
        let output_price = Decimal::from_str("0.00003").unwrap();
        setup_tariff(&pool, model_id, input_price, output_price, ApiKeyPurpose::Realtime).await;
        setup_tariff(&pool, model_id, input_price, output_price, ApiKeyPurpose::Batch).await;

        // Wealthy user so no balance crossing interferes with the assertions.
        let user_id = setup_user_with_balance(&pool, Decimal::from_str("100").unwrap()).await;
        let parent_id = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;
        let uncapped_id = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;

        // Cap the parent at $0.04 (each request below costs $0.025) and mint its child.
        sqlx::query("UPDATE api_keys SET spend_limit = 0.04 WHERE id = $1")
            .bind(parent_id)
            .execute(&pool)
            .await
            .unwrap();
        let child_id = {
            let mut conn = pool.acquire().await.unwrap();
            let (_, id) = ApiKeys::new(&mut conn).get_or_create_child_hidden_key(parent_id).await.unwrap();
            id
        };

        let mut listener = PgListener::connect_with(&pool).await.expect("Failed to create listener");
        listener.listen(ONWARDS_CONFIG_CHANGED_CHANNEL).await.expect("Failed to listen");
        while timeout(Duration::from_millis(10), listener.try_recv()).await.is_ok() {}

        // Parent realtime + child batch + uncapped key, one flush. Each row
        // costs $0.025; the scope total 0.05 crosses the 0.04 cap.
        let mut child_record = create_raw_record("gpt-4-cap-fold-test", Some(child_id), 1000, 500);
        child_record.batch_completion_window = Some("24h".to_string());
        let records = vec![
            create_raw_record("gpt-4-cap-fold-test", Some(parent_id), 1000, 500),
            child_record,
            create_raw_record("gpt-4-cap-fold-test", Some(uncapped_id), 1000, 500),
        ];
        run_batcher_with_records(&pool, records).await;

        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for cap crossing notification")
            .expect("Failed to receive notification");
        assert!(
            notification.payload().starts_with("api_key_spend_cap:"),
            "Expected cap payload, got: {}",
            notification.payload()
        );

        // One checkpoint row, keyed by the scope ROOT, summing parent + child.
        let rows = sqlx::query!("SELECT api_key_id, total_spend, window_spend FROM api_key_spend_checkpoints")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "only the cap scope folds; uncapped keys get no row");
        assert_eq!(rows[0].api_key_id, parent_id);
        assert_eq!(rows[0].total_spend, Decimal::from_str("0.05").unwrap());
        assert_eq!(rows[0].window_spend, Decimal::from_str("0.05").unwrap());

        // Edge-trigger: a further over-cap flush folds but does not re-notify.
        run_batcher_with_records(&pool, vec![create_raw_record("gpt-4-cap-fold-test", Some(parent_id), 1000, 500)]).await;
        assert_no_cap_notification(&mut listener).await;
        let window_spend: Decimal = sqlx::query_scalar!(
            r#"SELECT window_spend AS "window_spend!" FROM api_key_spend_checkpoints WHERE api_key_id = $1"#,
            parent_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(window_spend, Decimal::from_str("0.075").unwrap());
    }

    /// Lazy calendar rollover: the first billed request past the boundary
    /// REPLACES window_spend instead of accumulating, advances
    /// window_started_at, keeps total_spend monotonic, and does not fire a
    /// crossing NOTIFY when the fresh window is under the cap.
    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_cap_window_rollover(pool: sqlx::PgPool) {
        use sqlx::postgres::PgListener;
        use std::time::Duration;
        use tokio::time::timeout;

        let model_id = create_test_model(&pool, "gpt-4-cap-rollover-test").await;
        setup_tariff(
            &pool,
            model_id,
            Decimal::from_str("0.00001").unwrap(),
            Decimal::from_str("0.00003").unwrap(),
            ApiKeyPurpose::Realtime,
        )
        .await;
        let user_id = setup_user_with_balance(&pool, Decimal::from_str("100").unwrap()).await;
        let key_id = create_api_key_for_user(&pool, user_id, ApiKeyPurpose::Realtime).await;

        sqlx::query("UPDATE api_keys SET spend_limit = 10, spend_limit_interval = 'daily' WHERE id = $1")
            .bind(key_id)
            .execute(&pool)
            .await
            .unwrap();

        // Exhausted checkpoint from a previous calendar day.
        sqlx::query(
            "INSERT INTO api_key_spend_checkpoints (api_key_id, total_spend, window_spend, window_started_at)
             VALUES ($1, 999, 999, now() - interval '2 days')",
        )
        .bind(key_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut listener = PgListener::connect_with(&pool).await.expect("Failed to create listener");
        listener.listen(ONWARDS_CONFIG_CHANGED_CHANNEL).await.expect("Failed to listen");
        while timeout(Duration::from_millis(10), listener.try_recv()).await.is_ok() {}

        run_batcher_with_records(&pool, vec![create_raw_record("gpt-4-cap-rollover-test", Some(key_id), 1000, 500)]).await;

        let row = sqlx::query!(
            r#"SELECT total_spend AS "total_spend!", window_spend AS "window_spend!",
                      window_started_at AS "window_started_at!"
               FROM api_key_spend_checkpoints WHERE api_key_id = $1"#,
            key_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.window_spend,
            Decimal::from_str("0.025").unwrap(),
            "rollover replaces, not accumulates"
        );
        assert_eq!(
            row.total_spend,
            Decimal::from_str("999.025").unwrap(),
            "lifetime total stays monotonic"
        );
        assert!(
            row.window_started_at > chrono::Utc::now() - chrono::Duration::hours(1),
            "window_started_at advances at rollover"
        );

        // Fresh window is far under the cap: no crossing NOTIFY.
        assert_no_cap_notification(&mut listener).await;
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_flush_emits_single_notification_for_multiple_depletions(pool: sqlx::PgPool) {
        use sqlx::postgres::PgListener;
        use std::time::Duration;
        use tokio::time::timeout;

        // Setup: Create model with tariff
        let model_id = create_test_model(&pool, "gpt-4-multi-notify-test").await;
        let input_price = Decimal::from_str("0.00001").unwrap();
        let output_price = Decimal::from_str("0.00003").unwrap();
        setup_tariff(&pool, model_id, input_price, output_price, ApiKeyPurpose::Realtime).await;

        // Setup: Create 3 users with small balances that will all be depleted
        let initial_balance = Decimal::from_str("0.01").unwrap();
        let user1_id = setup_user_with_balance(&pool, initial_balance).await;
        let user2_id = setup_user_with_balance(&pool, initial_balance).await;
        let user3_id = setup_user_with_balance(&pool, initial_balance).await;

        let api_key1 = create_api_key_for_user(&pool, user1_id, ApiKeyPurpose::Realtime).await;
        let api_key2 = create_api_key_for_user(&pool, user2_id, ApiKeyPurpose::Realtime).await;
        let api_key3 = create_api_key_for_user(&pool, user3_id, ApiKeyPurpose::Realtime).await;

        // Set up listener BEFORE running batcher
        let mut listener = PgListener::connect_with(&pool).await.expect("Failed to create listener");
        listener.listen("auth_config_changed").await.expect("Failed to listen");

        // Drain any notifications from setup (poll with timeout, no sleep needed)
        while timeout(Duration::from_millis(10), listener.try_recv()).await.is_ok() {
            // Keep draining while notifications available
        }

        // Create 3 records that will all deplete balances (cost = $0.025 each)
        let record1 = create_raw_record("gpt-4-multi-notify-test", Some(api_key1), 1000, 500);
        let record2 = create_raw_record("gpt-4-multi-notify-test", Some(api_key2), 1000, 500);
        let record3 = create_raw_record("gpt-4-multi-notify-test", Some(api_key3), 1000, 500);

        // One flush folds all three depletions: one reload notification
        // covers them all (edge-triggered crossings, so no storm to limit).
        run_batcher_with_records(&pool, vec![record1, record2, record3]).await;

        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for depletion notification")
            .expect("Failed to receive notification");
        assert_eq!(notification.channel(), "auth_config_changed");
        assert!(
            notification.payload().starts_with("credits_transactions:"),
            "Expected payload to start with 'credits_transactions:', got: {}",
            notification.payload()
        );

        // No further notifications: the three crossings shared one notify.
        let second = timeout(Duration::from_millis(100), listener.recv()).await;
        assert!(second.is_err(), "Expected exactly one notification for the flush");

        // Verify all 3 users have negative balances
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);

        let balance1 = credits.get_user_balance(user1_id).await.unwrap();
        let balance2 = credits.get_user_balance(user2_id).await.unwrap();
        let balance3 = credits.get_user_balance(user3_id).await.unwrap();

        assert!(balance1 < Decimal::ZERO, "User 1 balance should be negative, got: {}", balance1);
        assert!(balance2 < Decimal::ZERO, "User 2 balance should be negative, got: {}", balance2);
        assert!(balance3 < Decimal::ZERO, "User 3 balance should be negative, got: {}", balance3);
    }
}
