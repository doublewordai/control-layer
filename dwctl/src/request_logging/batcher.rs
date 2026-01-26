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
//!                                              [Flush on size/time threshold]
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

use crate::config::Config;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::metrics::MetricsRecorder;
use crate::request_logging::serializers::HttpAnalyticsRow;
use chrono::{DateTime, Utc};
use metrics::{counter, histogram};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use sqlx::PgPool;
use std::collections::HashMap;
use tokio::sync::mpsc;
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
    pub total_tokens: i64,
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
    /// The completion window SLA (e.g., "24h") - used for batch pricing lookup
    pub batch_completion_window: Option<String>,
    /// Request origin: "api", "frontend", or "fusillade"
    pub request_origin: String,
    /// The request_source from batch metadata
    pub batch_request_source: String,
}

/// Enriched data resolved during batch processing
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields stored for future metrics recording
struct EnrichedRecord {
    raw: RawAnalyticsRecord,
    user_id: Option<Uuid>,
    access_source: String,
    api_key_purpose: Option<ApiKeyPurpose>,
    model_id: Option<Uuid>,
    provider_name: Option<String>,
    input_price_per_token: Option<Decimal>,
    output_price_per_token: Option<Decimal>,
}

/// Sender handle for submitting analytics records to the batcher
pub type AnalyticsSender = mpsc::Sender<RawAnalyticsRecord>;

/// Analytics batcher that accumulates records and writes them in batches.
///
/// This significantly reduces database overhead by:
/// 1. Batching enrichment queries (user lookup, pricing lookup)
/// 2. Batching INSERT operations (analytics, credits)
/// 3. Using a single transaction for consistency
pub struct AnalyticsBatcher<M = crate::metrics::GenAiMetrics>
where
    M: MetricsRecorder + Clone + Send + Sync + 'static,
{
    pool: PgPool,
    #[allow(dead_code)]
    config: Config,
    metrics_recorder: Option<M>,
    receiver: mpsc::Receiver<RawAnalyticsRecord>,
    batch_size: usize,
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

        let batcher = Self {
            pool,
            config,
            metrics_recorder,
            receiver,
            batch_size,
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
            "Analytics batcher started (write-through mode)"
        );

        let mut buffer: Vec<RawAnalyticsRecord> = Vec::with_capacity(self.batch_size);

        loop {
            // Step 1: Wait for at least one record OR shutdown
            tokio::select! {
                biased; // Check shutdown first

                _ = shutdown_token.cancelled() => {
                    info!("Shutdown signal received, draining analytics channel");
                    self.receiver.close();
                    // Drain any remaining records
                    while let Some(record) = self.receiver.recv().await {
                        buffer.push(record);
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

    /// Flushes the buffer to the database.
    ///
    /// This performs:
    /// 1. Batch enrichment (user lookup, pricing lookup)
    /// 2. Transactional write (analytics + credits)
    /// 3. Metrics recording
    async fn flush_batch(&self, buffer: &mut Vec<RawAnalyticsRecord>) {
        if buffer.is_empty() {
            return;
        }

        let batch_size = buffer.len();
        let span = info_span!("flush_analytics_batch", batch_size = batch_size);

        async {
            let start = std::time::Instant::now();

            // Phase 1: Batch enrich
            let enriched = match self.enrich_batch(buffer).await {
                Ok(enriched) => enriched,
                Err(e) => {
                    error!(error = %e, batch_size = batch_size, "Failed to enrich analytics batch");
                    counter!("dwctl_analytics_batch_errors_total", "phase" => "enrich").increment(1);
                    buffer.clear();
                    return;
                }
            };

            // Phase 2: Transactional write (analytics + credits)
            if let Err(e) = self.write_batch_transactional(&enriched).await {
                error!(error = %e, batch_size = batch_size, "Failed to write analytics batch");
                counter!("dwctl_analytics_batch_errors_total", "phase" => "write").increment(1);
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

        // Enrich each record
        let mut enriched = Vec::with_capacity(buffer.len());
        for raw in buffer.iter().cloned() {
            let (user_id, access_source, api_key_purpose) = if let Some(ref token) = raw.bearer_token {
                if let Some((uid, purpose)) = user_map.get(token) {
                    (Some(*uid), "api_key".to_string(), Some(purpose.clone()))
                } else {
                    (None, "unknown_api_key".to_string(), None)
                }
            } else {
                (None, "unauthenticated".to_string(), None)
            };

            let (model_id, provider_name, input_price, output_price) = if let Some(ref model_alias) = raw.request_model {
                if let Some(model_info) = model_map.get(model_alias) {
                    // Find best matching tariff
                    let (input, output) = self.find_best_tariff(
                        &model_info.tariffs,
                        api_key_purpose.as_ref(),
                        raw.batch_completion_window.as_deref(),
                        raw.timestamp,
                    );
                    (Some(model_info.model_id), Some(model_info.provider_name.clone()), input, output)
                } else {
                    (None, None, None, None)
                }
            } else {
                (None, None, None, None)
            };

            enriched.push(EnrichedRecord {
                raw,
                user_id,
                access_source,
                api_key_purpose,
                model_id,
                provider_name,
                input_price_per_token: input_price,
                output_price_per_token: output_price,
            });
        }

        Ok(enriched)
    }

    /// Batch lookup user info by bearer tokens.
    async fn batch_lookup_users(&self, tokens: &[&str]) -> Result<HashMap<String, (Uuid, ApiKeyPurpose)>, sqlx::Error> {
        let tokens_vec: Vec<String> = tokens.iter().map(|s| s.to_string()).collect();

        struct UserRow {
            secret: String,
            user_id: Uuid,
            purpose: String,
        }

        let rows: Vec<UserRow> = sqlx::query_as!(
            UserRow,
            r#"
            SELECT ak.secret, ak.user_id, ak.purpose
            FROM api_keys ak
            WHERE ak.secret = ANY($1)
            "#,
            &tokens_vec
        )
        .fetch_all(&self.pool)
        .await?;

        let mut map = HashMap::with_capacity(rows.len());
        for row in rows {
            let purpose = parse_api_key_purpose(&row.purpose);
            map.insert(row.secret, (row.user_id, purpose));
        }

        trace!(count = map.len(), "Batch lookup users completed");
        Ok(map)
    }

    /// Batch lookup model info with tariffs.
    async fn batch_lookup_models_with_tariffs(&self, aliases: &[&str]) -> Result<HashMap<String, ModelInfo>, sqlx::Error> {
        let aliases_vec: Vec<String> = aliases.iter().map(|s| s.to_string()).collect();

        struct ModelRow {
            alias: String,
            model_id: Uuid,
            provider_name: Option<String>,
            tariff_purpose: Option<String>,
            tariff_valid_from: Option<DateTime<Utc>>,
            tariff_input_price: Option<Decimal>,
            tariff_output_price: Option<Decimal>,
            tariff_completion_window: Option<String>,
        }

        // Query models with their tariffs in a single query
        let rows: Vec<ModelRow> = sqlx::query_as!(
            ModelRow,
            r#"
            SELECT
                dm.alias,
                dm.id as model_id,
                ie.name as provider_name,
                mt.api_key_purpose as tariff_purpose,
                mt.valid_from as tariff_valid_from,
                mt.input_price_per_token as tariff_input_price,
                mt.output_price_per_token as tariff_output_price,
                mt.completion_window as tariff_completion_window
            FROM deployed_models dm
            LEFT JOIN inference_endpoints ie ON dm.hosted_on = ie.id
            LEFT JOIN model_tariffs mt ON mt.deployed_model_id = dm.id AND mt.valid_until IS NULL
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
                model_id: row.model_id,
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
                    input_price_per_token: input_price,
                    output_price_per_token: output_price,
                    completion_window: row.tariff_completion_window,
                });
            }
        }

        trace!(count = map.len(), "Batch lookup models completed");
        Ok(map)
    }

    /// Find the best matching tariff for a record.
    ///
    /// Implements fallback logic:
    /// 1. Try exact match (purpose + completion_window + timestamp)
    /// 2. Fall back to purpose match without completion_window
    /// 3. Fall back to realtime purpose
    fn find_best_tariff(
        &self,
        tariffs: &[TariffInfo],
        api_key_purpose: Option<&ApiKeyPurpose>,
        completion_window: Option<&str>,
        timestamp: DateTime<Utc>,
    ) -> (Option<Decimal>, Option<Decimal>) {
        let purpose = api_key_purpose.unwrap_or(&ApiKeyPurpose::Realtime);

        // Filter tariffs valid at timestamp
        let valid_tariffs: Vec<_> = tariffs.iter().filter(|t| t.effective_from <= timestamp).collect();

        // Try exact match with completion_window (for batch tariffs)
        if let Some(cw) = completion_window
            && let Some(tariff) = valid_tariffs
                .iter()
                .find(|t| &t.purpose == purpose && t.completion_window.as_deref() == Some(cw))
        {
            return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
        }

        // Try purpose match without completion_window
        if let Some(tariff) = valid_tariffs.iter().find(|t| &t.purpose == purpose) {
            return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
        }

        // Fall back to realtime
        if purpose != &ApiKeyPurpose::Realtime
            && let Some(tariff) = valid_tariffs
                .iter()
                .find(|t| t.purpose == ApiKeyPurpose::Realtime)
        {
            return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
        }

        (None, None)
    }

    /// Write enriched records to the database in a single transaction.
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
            total_tokens_vec.push(record.raw.total_tokens);
            response_types.push(record.raw.response_type.clone());
            user_ids.push(record.user_id);
            access_sources.push(record.access_source.clone());
            input_prices.push(record.input_price_per_token);
            output_prices.push(record.output_price_per_token);
            fusillade_batch_ids.push(record.raw.fusillade_batch_id);
            fusillade_request_ids.push(record.raw.fusillade_request_id);
            custom_ids.push(record.raw.custom_id.clone());
            request_origins.push(record.raw.request_origin.clone());
            batch_slas.push(record.raw.batch_completion_window.clone().unwrap_or_default());
            batch_request_sources.push(record.raw.batch_request_source.clone());
        }

        let rows = sqlx::query!(
            r#"
            INSERT INTO http_analytics (
                instance_id, correlation_id, timestamp, method, uri, model,
                status_code, duration_ms, duration_to_first_byte_ms, prompt_tokens, completion_tokens,
                total_tokens, response_type, user_id, access_source,
                input_price_per_token, output_price_per_token, fusillade_batch_id, fusillade_request_id, custom_id,
                request_origin, batch_sla, batch_request_source
            )
            SELECT * FROM UNNEST(
                $1::uuid[], $2::bigint[], $3::timestamptz[], $4::text[], $5::text[], $6::text[],
                $7::int[], $8::bigint[], $9::bigint[], $10::bigint[], $11::bigint[],
                $12::bigint[], $13::text[], $14::uuid[], $15::text[],
                $16::numeric[], $17::numeric[], $18::uuid[], $19::uuid[], $20::text[],
                $21::text[], $22::text[], $23::text[]
            )
            ON CONFLICT (instance_id, correlation_id)
            DO UPDATE SET
                status_code = EXCLUDED.status_code,
                duration_ms = EXCLUDED.duration_ms,
                duration_to_first_byte_ms = EXCLUDED.duration_to_first_byte_ms,
                prompt_tokens = EXCLUDED.prompt_tokens,
                completion_tokens = EXCLUDED.completion_tokens,
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
                batch_request_source = EXCLUDED.batch_request_source
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

        for record in records {
            // Skip if no user or no pricing
            let Some(user_id) = record.user_id else { continue };

            // Skip system user
            if user_id == Uuid::nil() {
                continue;
            }

            // Skip if no pricing configured
            if record.input_price_per_token.is_none() && record.output_price_per_token.is_none() {
                continue;
            }

            // Calculate cost
            let input_cost = Decimal::from(record.raw.prompt_tokens) * record.input_price_per_token.unwrap_or(Decimal::ZERO);
            let output_cost = Decimal::from(record.raw.completion_tokens) * record.output_price_per_token.unwrap_or(Decimal::ZERO);
            let total_cost = input_cost + output_cost;

            if total_cost <= Decimal::ZERO {
                continue;
            }

            // Get analytics_id
            let Some(&analytics_id) = analytics_ids.get(&(record.raw.instance_id, record.raw.correlation_id)) else {
                warn!(
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
        }

        if user_ids.is_empty() {
            return Ok(0);
        }

        let expected_count = user_ids.len() as u64;

        // Batch INSERT with RETURNING to count actual inserts
        let result = sqlx::query!(
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, fusillade_batch_id)
            SELECT * FROM UNNEST(
                $1::uuid[], $2::text[], $3::numeric[], $4::text[], $5::text[], $6::uuid[]
            )
            ON CONFLICT (source_id) DO NOTHING
            "#,
            &user_ids,
            &vec!["usage".to_string(); user_ids.len()],
            &amounts,
            &source_ids,
            &descriptions as &[Option<String>],
            &fusillade_batch_ids as &[Option<Uuid>],
        )
        .execute(&mut **tx)
        .await?;

        let inserted_count = result.rows_affected();
        let duplicates = expected_count.saturating_sub(inserted_count);

        // Record metrics for deducted credits
        for (i, amount) in amounts.iter().enumerate() {
            let cents = (amount.to_f64().unwrap_or(0.0) * 100.0).round() as u64;
            counter!(
                "dwctl_credits_deducted_total",
                "user_id" => user_ids[i].to_string(),
                "model" => models[i].clone()
            )
            .increment(cents);
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
            request_origin: record.raw.request_origin.clone(),
            batch_sla: record.raw.batch_completion_window.clone().unwrap_or_default(),
            batch_request_source: record.raw.batch_request_source.clone(),
        }
    }
}

/// Model info with tariffs
#[derive(Debug)]
struct ModelInfo {
    model_id: Uuid,
    provider_name: String,
    tariffs: Vec<TariffInfo>,
}

/// Tariff info for pricing lookup
#[derive(Debug)]
struct TariffInfo {
    purpose: ApiKeyPurpose,
    effective_from: DateTime<Utc>,
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
            total_tokens: 30,
            response_type: "chat_completion".to_string(),
            server_address: "localhost".to_string(),
            server_port: 8080,
            bearer_token: Some("test-token".to_string()),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            batch_completion_window: None,
            request_origin: "api".to_string(),
            batch_request_source: "".to_string(),
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
}
