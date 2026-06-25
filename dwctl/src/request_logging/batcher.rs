//! Analytics batching system for efficient database writes.
//!
//! This module provides [`AnalyticsBatcher`] which accumulates analytics records
//! and writes them to the database in batches, significantly reducing per-request
//! database overhead.
//!
//! # Architecture
//!
//! ```text
//! Request → AnalyticsHandler (extract only) → Channel → AnalyticsBatcher
//!                                                            ↓
//!                                                 [Accumulate in buffer]
//!                                                            ↓
//!                                              [Flush immediately (write-through)]
//!                                                            ↓
//!                                              Phase 1: Batch enrich
//!                                                - Token → user_id lookup
//!                                                - Model → pricing lookup
//!                                                            ↓
//!                                              Phase 2: Batch write (transaction)
//!                                                - INSERT http_analytics
//!                                                - INSERT credit_transactions
//!                                                            ↓
//!                                              Phase 3: Record metrics
//! ```
//!
//! # Key Design Decisions
//!
//! - **All DB work in batcher**: The handler sends unenriched `RawAnalyticsRecord`s.
//!   Enrichment (user lookup, pricing lookup) happens in the batcher via batch queries.
//! - **Transactional writes**: Analytics and credit inserts happen in a single transaction.
//!   Either both succeed or both roll back.
//! - **Batch enrichment**: User and pricing lookups are batched using `IN` clauses,
//!   reducing from O(N) queries to O(1) per batch.

use crate::config::{CachePricingConfig, Config, ONWARDS_CONFIG_CHANGED_CHANNEL};
use crate::db::handlers::Credits;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::metrics::MetricsRecorder;
use crate::metrics::errors::component::ANALYTICS_BATCHER;
use crate::request_logging::serializers::HttpAnalyticsRow;
use chrono::{DateTime, Utc};
use metrics::{counter, histogram};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, trace, warn};
use uuid::Uuid;

/// Channel buffer size - how many records can be queued before backpressure
const CHANNEL_BUFFER_SIZE: usize = 10_000;

/// Raw analytics record sent through the channel (unenriched).
///
/// This contains only data that can be extracted from the request/response
/// without any database lookups. Enrichment happens in the batcher.
#[derive(Debug, Clone)]
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
    pub server_address: String,
    pub server_port: u16,

    // === Auth (unresolved - just the token) ===
    /// The bearer token from the Authorization header (not yet resolved to user_id)
    pub bearer_token: Option<String>,

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
#[derive(Debug, Clone)]
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
}

/// A `model_cache_tariffs` row (per model, per tier), with its validity window so batch
/// requests can be priced as of their creation time (mirrors `model_tariffs` handling).
/// One `model_cache_tariffs` version: all three tiers in a single row, plus the validity
/// window so a batch request prices as of its creation time. Completeness is guaranteed by
/// the schema (every multiplier is NOT NULL), so there is no missing-tier case to default.
#[derive(Clone)]
struct CacheTariffRow {
    write_multiplier_5m: Decimal,
    write_multiplier_1h: Decimal,
    write_multiplier_24h: Decimal,
    read_multiplier: Decimal,
    valid_from: DateTime<Utc>,
    valid_until: Option<DateTime<Utc>>,
}

/// The cache multipliers resolved for one request at a point in time.
#[derive(Clone, Copy)]
struct CacheMultipliers {
    read: Decimal,
    write_5m: Decimal,
    write_1h: Decimal,
    write_24h: Decimal,
}

impl CacheMultipliers {
    /// The operator-configured defaults ([`CachePricingConfig`]) — the same values a freshly
    /// enabled tariff would get. Used as the fallback when a request carries cache tokens with
    /// no tariff valid at its time (unreachable in practice — classify gates on an active row,
    /// and the call site emits a `cache_tariff_missing` background error if it ever happens).
    fn from_config(c: &CachePricingConfig) -> Self {
        Self {
            read: c.default_read_multiplier,
            write_5m: c.default_write_multiplier_5m,
            write_1h: c.default_write_multiplier_1h,
            write_24h: c.default_write_multiplier_24h,
        }
    }
}

impl Default for CacheMultipliers {
    /// Mirrors the shipped [`CachePricingConfig`] defaults (read 0.1, writes 1.25/2.0/2.5) so
    /// the hardcoded default and the config defaults can't drift. Production reads the live
    /// config via [`CacheMultipliers::from_config`]; this is for tests/standalone callers.
    fn default() -> Self {
        Self::from_config(&CachePricingConfig::default())
    }
}

/// Resolve the multipliers from the model's cache-tariff row valid at `timestamp` — the
/// most-recently-effective version still in its window. One row carries all tiers, so
/// there is no per-tier resolution or gap. `None` when no version was valid at `timestamp`
/// (so the caller can distinguish "no tariff" from a real row and fall back deliberately).
fn resolve_cache_multipliers(rows: &[CacheTariffRow], timestamp: DateTime<Utc>) -> Option<CacheMultipliers> {
    rows.iter()
        .filter(|r| r.valid_from <= timestamp && r.valid_until.is_none_or(|u| u > timestamp))
        .max_by_key(|r| r.valid_from)
        .map(|r| CacheMultipliers {
            read: r.read_multiplier,
            write_5m: r.write_multiplier_5m,
            write_1h: r.write_multiplier_1h,
            write_24h: r.write_multiplier_24h,
        })
}

/// The charged cost for a record, gating the cache discount on dwctl enablement: when a
/// tariff was valid at inference (`cache_mults` is `Some`) apply the cache-adjusted pricing;
/// otherwise bill the full input at list price. The `None` case deliberately ignores any
/// cache_* tokens in the response — without an active tariff those are the upstream
/// provider's own caching, not dwctl's, and must not earn dwctl's discount.
fn charged_cost(
    raw: &RawAnalyticsRecord,
    input_price: Option<Decimal>,
    output_price: Option<Decimal>,
    cache_mults: Option<CacheMultipliers>,
) -> Option<Decimal> {
    match cache_mults {
        Some(m) => compute_total_cost(raw, input_price, output_price, &m),
        None => compute_list_price(raw, input_price, output_price),
    }
}

/// The cache-adjusted request cost. Reduces to the plain
/// `prompt × input + completion × output` when there are no cache tokens, so non-cache
/// requests are unaffected. `None` when the model has no pricing at all (→ no ledger row),
/// matching the old generated `total_cost`'s NULL.
fn compute_total_cost(
    raw: &RawAnalyticsRecord,
    input_price: Option<Decimal>,
    output_price: Option<Decimal>,
    m: &CacheMultipliers,
) -> Option<Decimal> {
    if input_price.is_none() && output_price.is_none() {
        return None;
    }
    let inp = input_price.unwrap_or(Decimal::ZERO);
    let outp = output_price.unwrap_or(Decimal::ZERO);

    let read = Decimal::from(raw.cache_read_input_tokens.max(0));
    let c5 = Decimal::from(raw.cache_creation_5m_input_tokens.max(0));
    let c1 = Decimal::from(raw.cache_creation_1h_input_tokens.max(0));
    let c24 = Decimal::from(raw.cache_creation_24h_input_tokens.max(0));
    let prompt = Decimal::from(raw.prompt_tokens.max(0));
    let cached_total = read + c5 + c1 + c24;

    // Billing safety: the cached split can never exceed the prompt. If it does, the
    // classifier/provider reported a corrupt count — and since writes bill at a premium,
    // trusting it could massively overcharge. Distrust the split entirely and bill the whole
    // input at the base rate (= the list price), surfacing it so a classifier bug is visible.
    if cached_total > prompt {
        crate::background_error!(
            ANALYTICS_BATCHER,
            "cache_split_exceeds_prompt",
            Warning,
            model = raw.request_model.as_deref().unwrap_or("?"),
            prompt_tokens = raw.prompt_tokens,
            "cached token split exceeds prompt_tokens; ignoring the split and billing at base rate"
        );
        return Some(prompt * inp + Decimal::from(raw.completion_tokens.max(0)) * outp);
    }

    // Uncached = full input minus the cached portion, floored at zero (our tokenizer and
    // the provider's can differ; never let the cached count drive uncached negative).
    let uncached = (prompt - cached_total).max(Decimal::ZERO);

    let input_cost = uncached * inp + read * inp * m.read + c5 * inp * m.write_5m + c1 * inp * m.write_1h + c24 * inp * m.write_24h;
    let output_cost = Decimal::from(raw.completion_tokens.max(0)) * outp;
    Some(input_cost + output_cost)
}

/// The un-discounted list price (`http_analytics.uncached_cost`): the full input + output
/// at base rates, ignoring any cache split. `None` under the same no-pricing condition as
/// [`compute_total_cost`], so the two columns are NULL in lockstep. Equals `total_cost`
/// whenever the request cached nothing — and equals the dropped generated expression, so
/// the backfill can copy `total_cost` into it for historical rows.
fn compute_list_price(raw: &RawAnalyticsRecord, input_price: Option<Decimal>, output_price: Option<Decimal>) -> Option<Decimal> {
    if input_price.is_none() && output_price.is_none() {
        return None;
    }
    let inp = input_price.unwrap_or(Decimal::ZERO);
    let outp = output_price.unwrap_or(Decimal::ZERO);
    Some(Decimal::from(raw.prompt_tokens.max(0)) * inp + Decimal::from(raw.completion_tokens.max(0)) * outp)
}

/// Sender handle for submitting analytics records to the batcher
pub type AnalyticsSender = mpsc::Sender<RawAnalyticsRecord>;

/// Analytics batcher that accumulates records and writes them in batches.
///
/// This significantly reduces database overhead by:
/// 1. Batching enrichment queries (user lookup, pricing lookup)
/// 2. Batching INSERT operations (analytics, credits)
/// 3. Using a single transaction for consistency
/// 4. Retrying failed batches with exponential backoff
pub struct AnalyticsBatcher<M = crate::metrics::GenAiMetrics>
where
    M: MetricsRecorder + Clone + Send + Sync + 'static,
{
    pool: PgPool,
    metrics_recorder: Option<M>,
    receiver: mpsc::Receiver<RawAnalyticsRecord>,
    batch_size: usize,
    max_retries: u32,
    retry_base_delay: std::time::Duration,
    /// Global rate limiter for onwards sync notifications.
    /// Tracks the last time we triggered an onwards sync to prevent storms.
    last_onwards_sync_notification: Arc<RwLock<Instant>>,
    /// Minimum interval between onwards sync notifications (from config).
    onwards_sync_notification_interval: Duration,
}

impl<M> AnalyticsBatcher<M>
where
    M: MetricsRecorder + Clone + Send + Sync + 'static,
{
    /// Creates a new analytics batcher and returns the batcher along with a sender.
    ///
    /// # Arguments
    ///
    /// * `pool` - Database connection pool for batch writes
    /// * `config` - Application configuration (includes batch settings)
    /// * `metrics_recorder` - Optional metrics recorder for Prometheus metrics
    ///
    /// # Returns
    ///
    /// A tuple of (batcher, sender) where the sender is used by AnalyticsHandler
    /// to submit records.
    pub fn new(pool: PgPool, config: Config, metrics_recorder: Option<M>) -> (Self, AnalyticsSender) {
        let (sender, receiver) = mpsc::channel(CHANNEL_BUFFER_SIZE);

        let batch_size = config.analytics.batch_size;
        let max_retries = config.analytics.max_retries;
        let retry_base_delay = std::time::Duration::from_millis(config.analytics.retry_base_delay_ms);
        let onwards_sync_notification_interval = Duration::from_millis(config.analytics.balance_notification_interval_milliseconds);

        let batcher = Self {
            pool,
            metrics_recorder,
            receiver,
            batch_size,
            max_retries,
            retry_base_delay,
            last_onwards_sync_notification: Arc::new(RwLock::new(
                Instant::now()
                    .checked_sub(onwards_sync_notification_interval)
                    .unwrap_or_else(Instant::now),
            )),
            onwards_sync_notification_interval,
        };

        (batcher, sender)
    }

    /// Runs the batcher's background write loop.
    ///
    /// This should be spawned as a tokio task. The strategy is:
    /// 1. Block until at least one record arrives
    /// 2. Non-blocking drain of all available records in the channel
    /// 3. Write the batch immediately
    /// 4. Repeat
    ///
    /// This minimizes latency at low load (single record → immediate write) while
    /// getting batching efficiency at high load (records queue while writing → bigger batch).
    pub async fn run(mut self, shutdown_token: CancellationToken) {
        info!(
            max_batch_size = self.batch_size,
            max_retries = self.max_retries,
            retry_base_delay_ms = self.retry_base_delay.as_millis() as u64,
            "Analytics batcher started (write-through mode with retry)"
        );

        let mut buffer: Vec<RawAnalyticsRecord> = Vec::with_capacity(self.batch_size);

        loop {
            // Step 1: Wait for at least one record OR shutdown
            tokio::select! {
                biased; // Check shutdown first

                _ = shutdown_token.cancelled() => {
                    info!("Shutdown signal received, draining analytics channel");
                    self.receiver.close();
                    // Drain remaining records in batches to avoid OOM with large backlogs
                    while let Some(record) = self.receiver.recv().await {
                        buffer.push(record);
                        if buffer.len() >= self.batch_size {
                            self.flush_batch(&mut buffer).await;
                        }
                    }
                    if !buffer.is_empty() {
                        self.flush_batch(&mut buffer).await;
                    }
                    info!("Analytics batcher shutdown complete");
                    break;
                }

                maybe_record = self.receiver.recv() => {
                    match maybe_record {
                        Some(record) => buffer.push(record),
                        None => {
                            // Channel closed (all senders dropped)
                            info!("Analytics channel closed, shutting down batcher");
                            if !buffer.is_empty() {
                                self.flush_batch(&mut buffer).await;
                            }
                            break;
                        }
                    }
                }
            }

            // Step 2: Non-blocking drain of all available records (up to batch_size)
            while buffer.len() < self.batch_size {
                match self.receiver.try_recv() {
                    Ok(record) => buffer.push(record),
                    Err(_) => break, // Channel empty or closed
                }
            }

            // Step 3: Write immediately
            self.flush_batch(&mut buffer).await;
        }
    }

    /// Flushes the buffer to the database with retry on failure.
    ///
    /// This performs:
    /// 1. Batch enrichment (user lookup, pricing lookup) - no retry, data issues won't fix themselves
    /// 2. Transactional write (analytics + credits) - retried with exponential backoff
    /// 3. Metrics recording
    async fn flush_batch(&self, buffer: &mut Vec<RawAnalyticsRecord>) {
        if buffer.is_empty() {
            return;
        }

        let batch_size = buffer.len();
        let span = info_span!("dwctl.flush_analytics_batch", batch_size = batch_size);

        async {
            let start = std::time::Instant::now();

            // Collect correlation IDs for log correlation
            let correlation_ids: Vec<i64> = buffer.iter().map(|r| r.correlation_id).collect();

            // Phase 1: Batch enrich (no retry - enrichment failures are usually data issues)
            let enriched = match self.enrich_batch(buffer).await {
                Ok(enriched) => enriched,
                Err(e) => {
                    crate::background_error!(ANALYTICS_BATCHER, "enrich", Error, error = %e, batch_size = batch_size, ?correlation_ids, "Failed to enrich analytics batch");
                    buffer.clear();
                    return;
                }
            };

            // Phase 2: Transactional write with retry
            let mut last_error = None;
            for attempt in 0..=self.max_retries {
                match self.write_batch_transactional(&enriched).await {
                    Ok(()) => {
                        if attempt > 0 {
                            debug!(
                                attempt = attempt,
                                batch_size = batch_size,
                                ?correlation_ids,
                                "Batch write succeeded after retry"
                            );
                            counter!("dwctl_analytics_batch_retries_total", "outcome" => "success").increment(1);
                        }
                        last_error = None;
                        break;
                    }
                    Err(e) => {
                        last_error = Some(e);
                        if attempt < self.max_retries {
                            let delay = self.retry_base_delay * 2u32.pow(attempt);
                            warn!(
                                error = %last_error.as_ref().unwrap(),
                                attempt = attempt + 1,
                                max_retries = self.max_retries,
                                delay_ms = delay.as_millis() as u64,
                                batch_size = batch_size,
                                ?correlation_ids,
                                "Batch write failed, retrying"
                            );
                            counter!("dwctl_analytics_batch_retries_total", "outcome" => "retry").increment(1);
                            tokio::time::sleep(delay).await;
                        }
                    }
                }
            }

            if let Some(e) = last_error {
                crate::background_error!(
                    ANALYTICS_BATCHER, "write_drop", Critical,
                    error = %e,
                    batch_size = batch_size,
                    attempts = self.max_retries + 1,
                    ?correlation_ids,
                    "Failed to write analytics batch after all retries, dropping batch"
                );
                buffer.clear();
                return;
            }

            // Phase 3: Record per-record metrics
            let now = chrono::Utc::now();
            for record in &enriched {
                // Record analytics lag (time from response to now)
                let total_ms = now.signed_duration_since(record.raw.timestamp).num_milliseconds();
                let lag_ms = total_ms - record.raw.duration_ms;
                histogram!("dwctl_analytics_lag_seconds").record(lag_ms as f64 / 1000.0);

                // Record GenAI metrics
                if let Some(ref recorder) = self.metrics_recorder {
                    let row = self.enriched_to_row(record);
                    recorder.record_from_analytics(&row).await;
                }
            }

            let duration = start.elapsed();
            histogram!("dwctl_analytics_batch_duration_seconds").record(duration.as_secs_f64());
            counter!("dwctl_analytics_batched_records_total").increment(batch_size as u64);

            debug!(
                batch_size = batch_size,
                duration_ms = duration.as_millis() as u64,
                ?correlation_ids,
                "Flushed analytics batch"
            );

            buffer.clear();
        }
        .instrument(span)
        .await;
    }

    /// Batch enrich raw records with user info and pricing.
    ///
    /// Performs two batch queries:
    /// 1. Token → (user_id, purpose) lookup
    /// 2. Model alias → (model_id, provider, tariffs) lookup
    #[tracing::instrument(skip_all)]
    async fn enrich_batch(&self, buffer: &[RawAnalyticsRecord]) -> Result<Vec<EnrichedRecord>, sqlx::Error> {
        // Collect unique bearer tokens
        let tokens: Vec<&str> = buffer
            .iter()
            .filter_map(|r| r.bearer_token.as_deref())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Collect unique model aliases
        let models: Vec<&str> = buffer
            .iter()
            .filter_map(|r| r.request_model.as_deref())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Batch lookup: token → (user_id, purpose)
        let user_map = if !tokens.is_empty() {
            self.batch_lookup_users(&tokens).await?
        } else {
            HashMap::new()
        };

        // Batch lookup: model alias → (model_id, provider_name, tariffs)
        let model_map = if !models.is_empty() {
            self.batch_lookup_models_with_tariffs(&models).await?
        } else {
            HashMap::new()
        };

        // Batch lookup: model alias → cache tariffs (per tier), for the cache multipliers.
        let cache_tariff_map = if !models.is_empty() {
            self.batch_lookup_cache_tariffs(&models).await?
        } else {
            HashMap::new()
        };

        // Enrich each record
        let mut enriched = Vec::with_capacity(buffer.len());
        for raw in buffer.iter().cloned() {
            let (user_id, api_key_id, access_source, api_key_purpose) = if let Some(ref token) = raw.bearer_token {
                if let Some((uid, akid, purpose)) = user_map.get(token) {
                    (Some(*uid), Some(*akid), "api_key".to_string(), Some(purpose.clone()))
                } else {
                    (None, None, "unknown_api_key".to_string(), None)
                }
            } else {
                (None, None, "unauthenticated".to_string(), None)
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
                    let (input, output) = self.find_best_tariff(
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
            let total_cost = charged_cost(&raw, input_price, output_price, cache_mults_resolved);
            let uncached_cost = compute_list_price(&raw, input_price, output_price);

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
            });
        }

        Ok(enriched)
    }

    /// Batch lookup user info by bearer tokens.
    #[tracing::instrument(skip_all)]
    async fn batch_lookup_users(&self, tokens: &[&str]) -> Result<HashMap<String, (Uuid, Uuid, ApiKeyPurpose)>, sqlx::Error> {
        let tokens_vec: Vec<String> = tokens.iter().map(|s| s.to_string()).collect();

        struct UserRow {
            secret: String,
            user_id: Uuid,
            api_key_id: Uuid,
            purpose: String,
        }

        let rows: Vec<UserRow> = sqlx::query_as!(
            UserRow,
            r#"
            SELECT ak.secret, ak.user_id, ak.id as api_key_id, ak.purpose
            FROM api_keys ak
            WHERE ak.secret = ANY($1) AND ak.is_deleted = false
            "#,
            &tokens_vec
        )
        .fetch_all(&self.pool)
        .await?;

        let mut map = HashMap::with_capacity(rows.len());
        for row in rows {
            let purpose = parse_api_key_purpose(&row.purpose);
            map.insert(row.secret, (row.user_id, row.api_key_id, purpose));
        }

        trace!(count = map.len(), "Batch lookup users completed");
        Ok(map)
    }

    /// Batch lookup model info with tariffs.
    ///
    /// Fetches ALL tariffs (including expired ones) to support historical pricing
    /// for batch requests that may have been created in the past.
    #[tracing::instrument(skip_all)]
    async fn batch_lookup_models_with_tariffs(&self, aliases: &[&str]) -> Result<HashMap<String, ModelInfo>, sqlx::Error> {
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
        .fetch_all(&self.pool)
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
    async fn batch_lookup_cache_tariffs(&self, aliases: &[&str]) -> Result<HashMap<String, Vec<CacheTariffRow>>, sqlx::Error> {
        let aliases_vec: Vec<String> = aliases.iter().map(|s| s.to_string()).collect();

        struct Row {
            alias: String,
            write_multiplier_5m: Decimal,
            write_multiplier_1h: Decimal,
            write_multiplier_24h: Decimal,
            read_multiplier: Decimal,
            valid_from: DateTime<Utc>,
            valid_until: Option<DateTime<Utc>>,
        }

        let rows: Vec<Row> = sqlx::query_as!(
            Row,
            r#"
            SELECT
                dm.alias,
                mct.write_multiplier_5m,
                mct.write_multiplier_1h,
                mct.write_multiplier_24h,
                mct.read_multiplier,
                mct.valid_from,
                mct.valid_until
            FROM deployed_models dm
            JOIN model_cache_tariffs mct ON mct.deployed_model_id = dm.id
            WHERE dm.alias = ANY($1)
            ORDER BY dm.alias, mct.valid_from DESC
            "#,
            &aliases_vec
        )
        .fetch_all(&self.pool)
        .await?;

        let mut map: HashMap<String, Vec<CacheTariffRow>> = HashMap::new();
        for row in rows {
            map.entry(row.alias).or_default().push(CacheTariffRow {
                write_multiplier_5m: row.write_multiplier_5m,
                write_multiplier_1h: row.write_multiplier_1h,
                write_multiplier_24h: row.write_multiplier_24h,
                read_multiplier: row.read_multiplier,
                valid_from: row.valid_from,
                valid_until: row.valid_until,
            });
        }

        trace!(count = map.len(), "Batch lookup cache tariffs completed");
        Ok(map)
    }

    /// Find the best matching tariff for a record.
    ///
    /// Implements fallback logic:
    /// 1. Try exact match (purpose + completion_window + timestamp)
    /// 2. Fall back to generic tariff for that purpose (completion_window = None)
    /// 3. Fall back to realtime purpose (generic)
    fn find_best_tariff(
        &self,
        tariffs: &[TariffInfo],
        api_key_purpose: Option<&ApiKeyPurpose>,
        completion_window: Option<&str>,
        timestamp: DateTime<Utc>,
    ) -> (Option<Decimal>, Option<Decimal>) {
        let purpose = api_key_purpose.unwrap_or(&ApiKeyPurpose::Realtime);

        // Filter tariffs valid at timestamp:
        // effective_from <= timestamp AND (valid_until IS NULL OR valid_until > timestamp)
        let valid_tariffs: Vec<_> = tariffs
            .iter()
            .filter(|t| t.effective_from <= timestamp && t.valid_until.is_none_or(|valid_until| valid_until > timestamp))
            .collect();

        // Try exact match with completion_window (for batch tariffs with specific priority)
        if let Some(cw) = completion_window
            && let Some(tariff) = valid_tariffs
                .iter()
                .find(|t| &t.purpose == purpose && t.completion_window.as_deref() == Some(cw))
        {
            return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
        }

        // Try generic tariff for this purpose (completion_window = None)
        // This ensures we don't accidentally match a different priority tier
        if let Some(tariff) = valid_tariffs
            .iter()
            .find(|t| &t.purpose == purpose && t.completion_window.is_none())
        {
            return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
        }

        // Fall back to generic realtime tariff
        if purpose != &ApiKeyPurpose::Realtime
            && let Some(tariff) = valid_tariffs
                .iter()
                .find(|t| t.purpose == ApiKeyPurpose::Realtime && t.completion_window.is_none())
        {
            return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
        }

        (None, None)
    }

    /// Write enriched records to the database in a single transaction.
    #[tracing::instrument(skip_all)]
    async fn write_batch_transactional(&self, records: &[EnrichedRecord]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Phase 1: Batch INSERT http_analytics
        let analytics_ids = self.batch_insert_analytics(&mut tx, records).await?;

        // Phase 2: Batch INSERT credit_transactions
        let duplicates = self.batch_insert_credits(&mut tx, records, &analytics_ids).await?;
        if duplicates > 0 {
            warn!(duplicates = duplicates, "Some credit transactions were duplicates");
            counter!("dwctl_credits_duplicates_total").increment(duplicates);
        }

        tx.commit().await?;
        Ok(())
    }

    /// Batch INSERT http_analytics records within a transaction.
    async fn batch_insert_analytics(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        records: &[EnrichedRecord],
    ) -> Result<HashMap<(Uuid, i64), i64>, sqlx::Error> {
        if records.is_empty() {
            return Ok(HashMap::new());
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
                total_cost, uncached_cost
            )
            SELECT * FROM UNNEST(
                $1::uuid[], $2::bigint[], $3::timestamptz[], $4::text[], $5::text[], $6::text[],
                $7::int[], $8::bigint[], $9::bigint[], $10::bigint[], $11::bigint[],
                $12::bigint[], $13::bigint[], $14::text[], $15::uuid[], $16::text[],
                $17::numeric[], $18::numeric[], $19::uuid[], $20::uuid[], $21::text[],
                $22::text[], $23::text[], $24::text[], $25::uuid[], $26::text[],
                $27::bigint[], $28::bigint[],
                $29::bigint[], $30::bigint[], $31::bigint[],
                $32::numeric[], $33::numeric[]
            )
            ON CONFLICT (instance_id, correlation_id)
            DO UPDATE SET
                status_code = EXCLUDED.status_code,
                duration_ms = EXCLUDED.duration_ms,
                duration_to_first_byte_ms = EXCLUDED.duration_to_first_byte_ms,
                prompt_tokens = EXCLUDED.prompt_tokens,
                completion_tokens = EXCLUDED.completion_tokens,
                reasoning_tokens = EXCLUDED.reasoning_tokens,
                total_tokens = EXCLUDED.total_tokens,
                response_type = EXCLUDED.response_type,
                user_id = EXCLUDED.user_id,
                access_source = EXCLUDED.access_source,
                input_price_per_token = EXCLUDED.input_price_per_token,
                output_price_per_token = EXCLUDED.output_price_per_token,
                fusillade_batch_id = EXCLUDED.fusillade_batch_id,
                fusillade_request_id = EXCLUDED.fusillade_request_id,
                custom_id = EXCLUDED.custom_id,
                request_origin = EXCLUDED.request_origin,
                batch_sla = EXCLUDED.batch_sla,
                batch_request_source = EXCLUDED.batch_request_source,
                api_key_id = EXCLUDED.api_key_id,
                trace_id = EXCLUDED.trace_id,
                cache_read_input_tokens = EXCLUDED.cache_read_input_tokens,
                cache_creation_input_tokens = EXCLUDED.cache_creation_input_tokens,
                cache_creation_5m_input_tokens = EXCLUDED.cache_creation_5m_input_tokens,
                cache_creation_1h_input_tokens = EXCLUDED.cache_creation_1h_input_tokens,
                cache_creation_24h_input_tokens = EXCLUDED.cache_creation_24h_input_tokens,
                total_cost = EXCLUDED.total_cost,
                uncached_cost = EXCLUDED.uncached_cost
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
        )
        .fetch_all(&mut **tx)
        .await?;

        let mut id_map = HashMap::with_capacity(rows.len());
        for row in rows {
            id_map.insert((row.instance_id, row.correlation_id), row.id);
        }

        trace!(count = id_map.len(), "Batch inserted analytics records");
        Ok(id_map)
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
    ) -> Result<u64, sqlx::Error> {
        // Collect records that need credit transactions
        let mut user_ids: Vec<Uuid> = Vec::new();
        let mut amounts: Vec<Decimal> = Vec::new();
        let mut source_ids: Vec<String> = Vec::new();
        let mut descriptions: Vec<Option<String>> = Vec::new();
        let mut fusillade_batch_ids: Vec<Option<Uuid>> = Vec::new();
        let mut models: Vec<String> = Vec::new();
        let mut api_key_ids_credit: Vec<Option<Uuid>> = Vec::new();

        for record in records {
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

            // Get analytics_id
            let Some(&analytics_id) = analytics_ids.get(&(record.raw.instance_id, record.raw.correlation_id)) else {
                crate::background_error!(
                    ANALYTICS_BATCHER, "analytics_id_missing", Warning,
                    instance_id = %record.raw.instance_id,
                    correlation_id = record.raw.correlation_id,
                    "Analytics ID not found for credit transaction"
                );
                continue;
            };

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
            api_key_ids_credit.push(record.api_key_id);
        }

        if user_ids.is_empty() {
            return Ok(0);
        }

        let expected_count = user_ids.len() as u64;

        // Build a map from source_id to (index, user_id, amount, model) for metric recording
        let source_id_to_record: HashMap<String, (usize, Uuid, Decimal, String)> = source_ids
            .iter()
            .enumerate()
            .map(|(i, sid)| (sid.clone(), (i, user_ids[i], amounts[i], models[i].clone())))
            .collect();

        // Batch INSERT with RETURNING source_id to know exactly which were inserted
        let inserted_rows = sqlx::query_scalar!(
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, fusillade_batch_id, api_key_id)
            SELECT * FROM UNNEST(
                $1::uuid[], $2::text[], $3::numeric[], $4::text[], $5::text[], $6::uuid[], $7::uuid[]
            )
            ON CONFLICT (source_id) DO NOTHING
            RETURNING source_id
            "#,
            &user_ids,
            &vec!["usage".to_string(); user_ids.len()],
            &amounts,
            &source_ids,
            &descriptions as &[Option<String>],
            &fusillade_batch_ids as &[Option<Uuid>],
            &api_key_ids_credit as &[Option<Uuid>],
        )
        .fetch_all(&mut **tx)
        .await?;

        let inserted_count = inserted_rows.len() as u64;
        let duplicates = expected_count.saturating_sub(inserted_count);

        // Collect unique user IDs that had transactions inserted
        let inserted_user_ids: Vec<Uuid> = inserted_rows
            .iter()
            .filter_map(|source_id| source_id_to_record.get(source_id).map(|(_, uid, _, _)| *uid))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Query balances AFTER insert, with probabilistic checkpoint refresh (1 in 1000)
        // Notify onwards for any user with balance <= 0 (rate-limited to prevent storms)
        if !inserted_user_ids.is_empty() {
            let balances = {
                let mut credits = Credits::new(&mut *tx);
                credits
                    .get_users_balances_bulk(&inserted_user_ids, Some(1000))
                    .await
                    .map_err(|e| sqlx::Error::Protocol(format!("Failed to get user balances: {e}")))?
            };

            // Notify onwards if any user has depleted balance (globally rate-limited)
            let depleted_users: Vec<Uuid> = balances
                .iter()
                .filter_map(|(user_id, balance)| if *balance <= Decimal::ZERO { Some(*user_id) } else { None })
                .collect();

            if !depleted_users.is_empty() && self.should_notify_onwards_sync().await {
                self.notify_onwards_sync(&mut *tx, &depleted_users).await?;
            }
        }

        // Record metrics only for successfully inserted credit transactions
        for source_id in &inserted_rows {
            if let Some((_, user_id, amount, model)) = source_id_to_record.get(source_id) {
                let cents = (amount.to_f64().unwrap_or(0.0) * 100.0).round() as u64;
                counter!(
                    "dwctl_credits_deducted_total",
                    "user_id" => user_id.to_string(),
                    "model" => model.clone()
                )
                .increment(cents);
            }
        }

        trace!(
            count = inserted_count,
            duplicates = duplicates,
            "Batch inserted credit transactions"
        );
        Ok(duplicates)
    }

    /// Check if we should trigger an onwards sync notification (globally rate-limited).
    ///
    /// The onwards sync reloads ALL user data, so we rate-limit globally rather than per-user.
    /// When users have depleted balances and continue making requests, we would otherwise
    /// trigger a sync on every batch. This rate limiter ensures we only sync once per interval.
    async fn should_notify_onwards_sync(&self) -> bool {
        let now = Instant::now();
        let mut last_notification = self.last_onwards_sync_notification.write().await;

        if now.duration_since(*last_notification) >= self.onwards_sync_notification_interval {
            *last_notification = now;
            counter!("dwctl_onwards_sync_notifications_total", "action" => "allowed").increment(1);
            true
        } else {
            trace!("Rate limiting onwards sync notification");
            counter!("dwctl_onwards_sync_notifications_total", "action" => "rate_limited").increment(1);
            false
        }
    }

    /// Send pg_notify to trigger onwards sync when users have depleted balances.
    /// Format: "credits_transactions:{epoch_micros}" to match other triggers and enable lag metrics.
    async fn notify_onwards_sync(&self, conn: &mut sqlx::PgConnection, depleted_users: &[Uuid]) -> Result<(), sqlx::Error> {
        debug!(
            depleted_count = depleted_users.len(),
            "Depleted balances detected, notifying onwards sync"
        );

        let epoch_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();

        let payload = format!("credits_transactions:{}", epoch_micros);

        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(ONWARDS_CONFIG_CHANGED_CHANNEL)
            .bind(&payload)
            .execute(conn)
            .await?;

        counter!("dwctl_onwards_sync_notifications_total", "action" => "sent").increment(1);

        Ok(())
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
        }
    }
}

/// Model info with tariffs
#[derive(Debug)]
struct ModelInfo {
    provider_name: String,
    tariffs: Vec<TariffInfo>,
}

/// Tariff info for pricing lookup
#[derive(Debug)]
struct TariffInfo {
    purpose: ApiKeyPurpose,
    effective_from: DateTime<Utc>,
    valid_until: Option<DateTime<Utc>>,
    input_price_per_token: Decimal,
    output_price_per_token: Decimal,
    completion_window: Option<String>,
}

/// Parse API key purpose from string
fn parse_api_key_purpose(s: &str) -> ApiKeyPurpose {
    match s {
        "platform" => ApiKeyPurpose::Platform,
        "batch" => ApiKeyPurpose::Batch,
        "playground" => ApiKeyPurpose::Playground,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_analytics_record_creation() {
        let record = RawAnalyticsRecord {
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
            server_address: "localhost".to_string(),
            server_port: 8080,
            bearer_token: Some("test-token".to_string()),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            batch_completion_window: None,
            batch_created_at: None,
            batch_request_source: "".to_string(),
            trace_id: None,
        };

        assert_eq!(record.correlation_id, 123);
        assert_eq!(record.bearer_token, Some("test-token".to_string()));
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
            server_address: "x".to_string(),
            server_port: 1,
            bearer_token: None,
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

    #[test]
    fn cost_without_cache_is_plain_arithmetic() {
        // No cache tokens → identical to the old prompt×input + completion×output.
        let r = cost_record(1000, 100, 0, 0, 0, 0);
        let cost = compute_total_cost(&r, Some(inp()), Some(outp()), &CacheMultipliers::default()).unwrap();
        assert_eq!(cost, Decimal::new(12, 1)); // 1000*0.001 + 100*0.002 = 1.2
    }

    #[test]
    fn cost_with_cache_applies_per_tier_multipliers() {
        // 2000 input: 1000 read + 500 1h-creation + 500 uncached; completion 100.
        let r = cost_record(2000, 100, 1000, 0, 500, 0);
        let m = CacheMultipliers {
            read: Decimal::new(1, 1), // 0.1
            write_5m: Decimal::ONE,
            write_1h: Decimal::from(2), // 2.0
            write_24h: Decimal::ONE,
        };
        let cost = compute_total_cost(&r, Some(inp()), Some(outp()), &m).unwrap();
        // 500*0.001 (uncached) + 1000*0.001*0.1 (read) + 500*0.001*2.0 (1h write) + 100*0.002 (out)
        // = 0.5 + 0.1 + 1.0 + 0.2 = 1.8
        assert_eq!(cost, Decimal::new(18, 1));
    }

    #[test]
    fn cost_none_when_no_pricing() {
        let r = cost_record(1000, 100, 0, 0, 0, 0);
        assert!(compute_total_cost(&r, None, None, &CacheMultipliers::default()).is_none());
    }

    #[test]
    fn corrupt_split_exceeding_prompt_bills_at_base_rate() {
        // Cached tokens (1000) exceed the prompt (100) — an impossible, corrupt count from
        // the classifier/provider. The split is distrusted and the whole input is billed at
        // base rate (the list price), never at the cache write premium that would massively
        // overcharge on a mistake.
        let r = cost_record(100, 5, 1000, 0, 0, 0);
        let cost = compute_total_cost(&r, Some(inp()), Some(outp()), &CacheMultipliers::default()).unwrap();
        // list price = 100*0.001 + 5*0.002 = 0.11
        assert_eq!(cost, Decimal::new(11, 2));
        // No savings shown for a distrusted split: total == un-discounted list price.
        assert_eq!(cost, compute_list_price(&r, Some(inp()), Some(outp())).unwrap());
    }

    #[test]
    fn charged_cost_gates_discount_on_enablement() {
        // 600 cache-read tokens reported on the response.
        let r = cost_record(1000, 100, 600, 0, 0, 0);

        // Not dwctl-cache-enabled (no tariff → None): the provider's cache tokens are ignored
        // and the full input is billed at list price — no read discount.
        let not_enabled = charged_cost(&r, Some(inp()), Some(outp()), None).unwrap();
        assert_eq!(not_enabled, compute_list_price(&r, Some(inp()), Some(outp())).unwrap());
        assert_eq!(not_enabled, Decimal::new(12, 1)); // 1000*0.001 + 100*0.002

        // Cache-enabled (Some): the read discount applies, so it costs strictly less.
        let m = CacheMultipliers {
            read: Decimal::new(1, 1), // 0.1
            write_5m: Decimal::ONE,
            write_1h: Decimal::ONE,
            write_24h: Decimal::ONE,
        };
        let enabled = charged_cost(&r, Some(inp()), Some(outp()), Some(m)).unwrap();
        // 400 uncached*0.001 + 600 read*0.001*0.1 + 100*0.002 = 0.66
        assert_eq!(enabled, Decimal::new(66, 2));
        assert!(enabled < not_enabled, "the discount must make the enabled case cheaper");
    }

    #[test]
    fn list_price_ignores_cache_split_and_is_none_without_pricing() {
        // Cache tokens present, but the list price is the full input+output at base rates.
        let r = cost_record(1000, 100, 500, 0, 200, 0);
        let list = compute_list_price(&r, Some(inp()), Some(outp())).unwrap();
        assert_eq!(list, Decimal::new(12, 1)); // 1000*0.001 + 100*0.002 = 1.2
        assert!(compute_list_price(&r, None, None).is_none(), "no pricing → NULL list price");
    }

    fn tariff_row(write_1h: Decimal, from_hrs: i64, valid_until: Option<DateTime<Utc>>) -> CacheTariffRow {
        CacheTariffRow {
            write_multiplier_5m: Decimal::new(125, 2), // 1.25
            write_multiplier_1h: write_1h,
            write_multiplier_24h: Decimal::new(25, 1), // 2.5
            read_multiplier: Decimal::new(1, 1),       // 0.1
            valid_from: chrono::Utc::now() - chrono::Duration::hours(from_hrs),
            valid_until,
        }
    }

    #[test]
    fn resolve_multipliers_picks_latest_valid_version() {
        let now = chrono::Utc::now();
        // Two versions; the newer (valid_from 1h ago) wins over the older (5h ago).
        let rows = vec![tariff_row(Decimal::from(2), 1, None), tariff_row(Decimal::from(3), 5, None)];
        let m = resolve_cache_multipliers(&rows, now).expect("a valid version exists");
        assert_eq!(m.write_1h, Decimal::from(2), "latest valid version wins");
        assert_eq!(m.write_5m, Decimal::new(125, 2), "all tiers come from that one row");
        assert_eq!(m.write_24h, Decimal::new(25, 1));
        assert_eq!(m.read, Decimal::new(1, 1));
    }

    #[test]
    fn resolve_multipliers_none_when_empty_or_expired() {
        let now = chrono::Utc::now();
        // empty → no version valid → None (the caller falls back to defaults deliberately).
        assert!(resolve_cache_multipliers(&[], now).is_none(), "no rows → None");
        // expired version ignored → None.
        let expired = vec![tariff_row(Decimal::from(5), 2, Some(now - chrono::Duration::hours(1)))];
        assert!(resolve_cache_multipliers(&expired, now).is_none(), "expired version ignored → None");
    }

    #[test]
    fn default_multipliers_mirror_config_pricing_defaults() {
        // Default delegates to CachePricingConfig::default() so the two can't drift.
        let m = CacheMultipliers::default();
        let c = CachePricingConfig::default();
        assert_eq!(m.read, c.default_read_multiplier, "read = config default (0.1)");
        assert_eq!(m.write_5m, c.default_write_multiplier_5m, "5m = config default (1.25)");
        assert_eq!(m.write_1h, c.default_write_multiplier_1h, "1h = config default (2.0)");
        assert_eq!(m.write_24h, c.default_write_multiplier_24h, "24h = config default (2.5)");
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

    /// Helper to create test tariffs
    fn make_tariff(
        purpose: ApiKeyPurpose,
        effective_from: DateTime<Utc>,
        valid_until: Option<DateTime<Utc>>,
        input_price: &str,
        output_price: &str,
        completion_window: Option<&str>,
    ) -> TariffInfo {
        TariffInfo {
            purpose,
            effective_from,
            valid_until,
            input_price_per_token: Decimal::from_str(input_price).unwrap(),
            output_price_per_token: Decimal::from_str(output_price).unwrap(),
            completion_window: completion_window.map(|s| s.to_string()),
        }
    }

    /// Helper to call find_best_tariff without needing a full batcher
    fn find_tariff(
        tariffs: &[TariffInfo],
        api_key_purpose: Option<&ApiKeyPurpose>,
        completion_window: Option<&str>,
        timestamp: DateTime<Utc>,
    ) -> (Option<Decimal>, Option<Decimal>) {
        let purpose = api_key_purpose.unwrap_or(&ApiKeyPurpose::Realtime);

        let valid_tariffs: Vec<_> = tariffs
            .iter()
            .filter(|t| t.effective_from <= timestamp && t.valid_until.is_none_or(|valid_until| valid_until > timestamp))
            .collect();

        if let Some(cw) = completion_window
            && let Some(tariff) = valid_tariffs
                .iter()
                .find(|t| &t.purpose == purpose && t.completion_window.as_deref() == Some(cw))
        {
            return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
        }

        if let Some(tariff) = valid_tariffs
            .iter()
            .find(|t| &t.purpose == purpose && t.completion_window.is_none())
        {
            return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
        }

        if purpose != &ApiKeyPurpose::Realtime
            && let Some(tariff) = valid_tariffs
                .iter()
                .find(|t| t.purpose == ApiKeyPurpose::Realtime && t.completion_window.is_none())
        {
            return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
        }

        (None, None)
    }

    #[test]
    fn test_find_best_tariff_exact_match() {
        let now = chrono::Utc::now();
        let tariffs = vec![make_tariff(
            ApiKeyPurpose::Realtime,
            now - chrono::Duration::days(1),
            None,
            "0.00010",
            "0.00020",
            None,
        )];

        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(input, Some(Decimal::from_str("0.00010").unwrap()));
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_batch_vs_realtime() {
        let now = chrono::Utc::now();
        let tariffs = vec![
            make_tariff(
                ApiKeyPurpose::Realtime,
                now - chrono::Duration::days(1),
                None,
                "0.00010",
                "0.00020",
                None,
            ),
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00005",
                "0.00010",
                None,
            ),
        ];

        // Batch purpose should get batch pricing
        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), None, now);
        assert_eq!(input, Some(Decimal::from_str("0.00005").unwrap()));
        assert_eq!(output, Some(Decimal::from_str("0.00010").unwrap()));

        // Realtime purpose should get realtime pricing
        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(input, Some(Decimal::from_str("0.00010").unwrap()));
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_fallback_to_realtime() {
        // When batch tariff is missing, should fall back to realtime
        let now = chrono::Utc::now();
        let tariffs = vec![make_tariff(
            ApiKeyPurpose::Realtime,
            now - chrono::Duration::days(1),
            None,
            "0.00015",
            "0.00030",
            None,
        )];

        // Batch purpose with no batch tariff should fall back to realtime
        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), None, now);
        assert_eq!(input, Some(Decimal::from_str("0.00015").unwrap()));
        assert_eq!(output, Some(Decimal::from_str("0.00030").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_historical_pricing() {
        // Test that expired tariffs are not selected for current requests
        // but ARE selected for historical timestamps
        let now = chrono::Utc::now();
        let old_tariff_start = now - chrono::Duration::days(30);
        let old_tariff_end = now - chrono::Duration::days(10);
        let new_tariff_start = now - chrono::Duration::days(10);

        let tariffs = vec![
            // Old tariff: valid from 30 days ago until 10 days ago
            make_tariff(
                ApiKeyPurpose::Realtime,
                old_tariff_start,
                Some(old_tariff_end),
                "0.00020", // Old higher price
                "0.00040",
                None,
            ),
            // New tariff: valid from 10 days ago, still active
            make_tariff(
                ApiKeyPurpose::Realtime,
                new_tariff_start,
                None,
                "0.00010", // New lower price
                "0.00020",
                None,
            ),
        ];

        // Current request should use new pricing
        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00010").unwrap()),
            "Current request should use new pricing"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));

        // Historical request (20 days ago) should use old pricing
        let historical_time = now - chrono::Duration::days(20);
        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, historical_time);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00020").unwrap()),
            "Historical request should use old pricing"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00040").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_completion_window_exact_match() {
        // Test that completion_window-specific tariffs are matched correctly
        let now = chrono::Utc::now();
        let tariffs = vec![
            // Generic batch tariff (no completion_window)
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00010",
                "0.00020",
                None,
            ),
            // Priority-specific batch tariff for 24h window
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00005", // Cheaper for 24h priority
                "0.00010",
                Some("24h"),
            ),
        ];

        // Request with 24h completion window should get the priority-specific pricing
        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), Some("24h"), now);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00005").unwrap()),
            "24h priority should get specific pricing"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00010").unwrap()));

        // Request without completion window should get generic batch pricing
        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), None, now);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00010").unwrap()),
            "No priority should get generic pricing"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_completion_window_fallback_to_generic() {
        // Test that unknown completion_window falls back to generic tariff, not another priority
        let now = chrono::Utc::now();
        let tariffs = vec![
            // Generic batch tariff
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00010",
                "0.00020",
                None,
            ),
            // 24h priority tariff
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00005",
                "0.00010",
                Some("24h"),
            ),
            // 7d priority tariff
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00003",
                "0.00006",
                Some("7d"),
            ),
        ];

        // Request with unknown "1h" priority should fall back to generic, NOT to 24h or 7d
        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), Some("1h"), now);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00010").unwrap()),
            "Unknown priority should fall back to generic, not another priority"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_no_matching_tariff() {
        let now = chrono::Utc::now();
        let tariffs = vec![];

        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(input, None);
        assert_eq!(output, None);
    }

    #[test]
    fn test_find_best_tariff_future_tariff_not_used() {
        // Tariff that starts in the future should not be selected
        let now = chrono::Utc::now();
        let tariffs = vec![make_tariff(
            ApiKeyPurpose::Realtime,
            now + chrono::Duration::days(1), // Starts tomorrow
            None,
            "0.00010",
            "0.00020",
            None,
        )];

        let (input, output) = find_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(input, None, "Future tariff should not be selected");
        assert_eq!(output, None);
    }

    use rust_decimal::prelude::FromStr;
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
    use crate::test::utils::create_test_user;
    use rust_decimal::prelude::FromStr;
    use sqlx::PgPool;

    /// Helper: Create a test model with endpoint
    async fn create_test_model(pool: &PgPool, model_name: &str) -> crate::types::DeploymentId {
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
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                backoff_enabled: false,
                backoff_initial_ms: 100,
                backoff_max_ms: 5_000,
                backoff_factor: 2.0,
                backoff_jitter: "full".to_string(),
                backoff_max_total_ms: None,
                sanitize_responses: true,
                trusted: false,
                open_responses_adapter: true,
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
        pool: &PgPool,
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
    async fn setup_user_with_balance(pool: &PgPool, balance: Decimal) -> Uuid {
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
    async fn create_api_key_for_user(pool: &PgPool, user_id: Uuid, purpose: ApiKeyPurpose) -> String {
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
            })
            .await
            .unwrap();

        api_key.secret
    }

    /// Helper: Create a raw analytics record for testing
    fn create_raw_record(model: &str, bearer_token: Option<String>, prompt_tokens: i64, completion_tokens: i64) -> RawAnalyticsRecord {
        RawAnalyticsRecord {
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
            server_address: "api.test.com".to_string(),
            server_port: 443,
            bearer_token,
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
    async fn run_batcher_with_records(pool: &PgPool, records: Vec<RawAnalyticsRecord>) {
        let config = crate::test::utils::create_test_config();
        let (batcher, sender) = AnalyticsBatcher::<crate::metrics::GenAiMetrics>::new(pool.clone(), config, None);

        // Send all records
        for record in records {
            sender.send(record).await.unwrap();
        }

        // Drop sender to close channel
        drop(sender);

        // Run batcher until channel is drained
        let shutdown = CancellationToken::new();
        batcher.run(shutdown).await;
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_credit_deduction_successful(pool: PgPool) {
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
    async fn test_batcher_cache_discount_applied(pool: PgPool) {
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
    async fn total_cost_fidelity_trigger_fills_when_omitted(pool: PgPool) {
        // Old-release-style insert: omits total_cost (it relied on the dropped generation).
        // The fidelity trigger reconstructs it from the row's own tokens × prices.
        let priced = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO http_analytics (instance_id, correlation_id, timestamp, method, uri,
                 prompt_tokens, completion_tokens, input_price_per_token, output_price_per_token)
               VALUES ($1, 1, now(), 'POST', '/v1/chat/completions', 1000, 100, 0.001, 0.002)"#,
            priced
        )
        .execute(&pool)
        .await
        .unwrap();
        let tc = sqlx::query_scalar!("SELECT total_cost FROM http_analytics WHERE instance_id = $1", priced)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            tc.unwrap(),
            Decimal::from_str("1.2").unwrap(),
            "trigger reconstructs list price (1000*0.001 + 100*0.002)"
        );

        // Free / un-tariffed model (NULL prices) → NULL propagates → total_cost stays NULL,
        // exactly as the old generated CASE expression produced.
        let free = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO http_analytics (instance_id, correlation_id, timestamp, method, uri, prompt_tokens, completion_tokens)
               VALUES ($1, 2, now(), 'POST', '/v1/chat/completions', 1000, 100)"#,
            free
        )
        .execute(&pool)
        .await
        .unwrap();
        let tc_free = sqlx::query_scalar!("SELECT total_cost FROM http_analytics WHERE instance_id = $1", free)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(tc_free.is_none(), "no prices → total_cost NULL (free model)");

        // New-release-style insert: total_cost provided → trigger no-ops (keeps the value,
        // even if it differs from the list price, which is the whole point under caching).
        let provided = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO http_analytics (instance_id, correlation_id, timestamp, method, uri,
                 prompt_tokens, completion_tokens, input_price_per_token, output_price_per_token, total_cost)
               VALUES ($1, 3, now(), 'POST', '/v1/chat/completions', 1000, 100, 0.001, 0.002, 0.5)"#,
            provided
        )
        .execute(&pool)
        .await
        .unwrap();
        let tc_provided = sqlx::query_scalar!("SELECT total_cost FROM http_analytics WHERE instance_id = $1", provided)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            tc_provided.unwrap(),
            Decimal::from_str("0.5").unwrap(),
            "explicit total_cost is preserved (trigger no-ops)"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_different_tariffs_for_batch_and_realtime(pool: PgPool) {
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
    async fn test_batcher_fallback_to_realtime_when_batch_tariff_missing(pool: PgPool) {
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
    async fn test_batcher_skip_deduction_when_no_pricing(pool: PgPool) {
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

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_skip_deduction_for_unauthenticated_requests(pool: PgPool) {
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
    async fn test_batcher_balance_depleted_notification(pool: PgPool) {
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

    #[sqlx::test]
    #[test_log::test]
    async fn test_batcher_rate_limits_balance_notifications(pool: PgPool) {
        use sqlx::postgres::PgListener;
        use std::time::Duration;
        use tokio::time::timeout;

        // Setup: Create model with tariff
        let model_id = create_test_model(&pool, "gpt-4-rate-limit-test").await;
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
        let record1 = create_raw_record("gpt-4-rate-limit-test", Some(api_key1), 1000, 500);
        let record2 = create_raw_record("gpt-4-rate-limit-test", Some(api_key2), 1000, 500);
        let record3 = create_raw_record("gpt-4-rate-limit-test", Some(api_key3), 1000, 500);

        // Create custom config with 100ms rate limiting interval for fast testing
        let mut config = crate::test::utils::create_test_config();
        config.analytics.balance_notification_interval_milliseconds = 100;

        // Run batcher with all 3 records - should trigger 3 depletions but only 1 notification
        // due to rate limiting (interval is 100ms)
        let (batcher, sender) = AnalyticsBatcher::<crate::metrics::GenAiMetrics>::new(pool.clone(), config, None);
        for record in [record1, record2, record3] {
            sender.send(record).await.unwrap();
        }
        drop(sender);
        let shutdown = tokio_util::sync::CancellationToken::new();
        batcher.run(shutdown).await;

        // Should receive ONLY ONE notification despite 3 balance depletions
        let first_notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for first balance depletion notification")
            .expect("Failed to receive notification");

        assert_eq!(first_notification.channel(), "auth_config_changed");
        println!("Received first notification: {}", first_notification.payload());

        // Try to receive a second notification - should timeout because of rate limiting
        let second_notification = timeout(Duration::from_millis(50), listener.recv()).await;
        assert!(
            second_notification.is_err(),
            "Should NOT receive second notification due to rate limiting (interval is 100ms, we only waited 50ms)"
        );

        // Verify all 3 users have negative balances
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);

        let balance1 = credits.get_user_balance(user1_id).await.unwrap();
        let balance2 = credits.get_user_balance(user2_id).await.unwrap();
        let balance3 = credits.get_user_balance(user3_id).await.unwrap();

        assert!(balance1 < Decimal::ZERO, "User 1 balance should be negative, got: {}", balance1);
        assert!(balance2 < Decimal::ZERO, "User 2 balance should be negative, got: {}", balance2);
        assert!(balance3 < Decimal::ZERO, "User 3 balance should be negative, got: {}", balance3);

        println!("✅ Rate limiting working: 3 depletions → 1 notification");
    }
}
