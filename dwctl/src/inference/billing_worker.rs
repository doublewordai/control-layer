//! Durable billing of accepted Fusillade completions. Queue acknowledgement,
//! receipts, ledger and authoritative aggregates share one primary transaction.
use crate::{
    config::Config,
    db::models::api_keys::ApiKeyPurpose,
    request_logging::batcher::{AnalyticsBatcher, DurableBillingInput, RawAnalyticsRecord},
};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, Row};
use sqlx_pool_router::DynPools;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Debug, FromRow)]
struct Event {
    event_id: Uuid,
    request_id: Uuid,
    owner_id: Uuid,
    billing_mode: String,
    event_version: i16,
    model_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    api_key_purpose: Option<String>,
    cap_scope_root: Option<Uuid>,
    batch_id: Option<Uuid>,
    requested_model: String,
    response_model: Option<String>,
    completion_window: Option<String>,
    batch_created_at: Option<DateTime<Utc>>,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    usage_present: bool,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    total_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    engine_cached_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_creation_5m_input_tokens: Option<i64>,
    cache_creation_1h_input_tokens: Option<i64>,
    cache_creation_24h_input_tokens: Option<i64>,
    request_path: String,
    request_method: String,
    custom_id: Option<String>,
    finish_reason: Option<String>,
    served_by: Option<String>,
}

impl Event {
    fn billing_input(&self) -> Result<DurableBillingInput, &'static str> {
        if self.event_version != 1 {
            return Err("unsupported_version");
        }
        if !self.usage_present {
            return Err("missing_usage");
        }
        let prompt = self.prompt_tokens.ok_or("missing_prompt_tokens")?;
        let completion = self.completion_tokens.ok_or("missing_completion_tokens")?;
        let purpose = match self.api_key_purpose.as_deref() {
            Some("batch") => ApiKeyPurpose::Batch,
            Some("realtime") => ApiKeyPurpose::Realtime,
            Some("playground") => ApiKeyPurpose::Playground,
            Some("continuation") => ApiKeyPurpose::Continuation,
            _ => return Err("missing_key_purpose"),
        };
        let raw = RawAnalyticsRecord {
            // Stable analytics identity, even if publication is replayed. The
            // permanent receipt additionally fences replay after analytics expiry.
            instance_id: self.event_id,
            correlation_id: 0,
            timestamp: self.started_at,
            method: self.request_method.clone(),
            uri: self.request_path.clone(),
            request_model: Some(self.requested_model.clone()),
            response_model: self.response_model.clone(),
            status_code: 200,
            duration_ms: (self.completed_at - self.started_at).num_milliseconds().max(0),
            duration_to_first_byte_ms: None,
            prompt_tokens: prompt,
            completion_tokens: completion,
            reasoning_tokens: self.reasoning_tokens.unwrap_or(0),
            total_tokens: self.total_tokens.unwrap_or_else(|| prompt.saturating_add(completion)),
            cache_read_input_tokens: self.cache_read_input_tokens.unwrap_or(0),
            cache_creation_5m_input_tokens: self.cache_creation_5m_input_tokens.unwrap_or(0),
            cache_creation_1h_input_tokens: self.cache_creation_1h_input_tokens.unwrap_or(0),
            cache_creation_24h_input_tokens: self.cache_creation_24h_input_tokens.unwrap_or(0),
            response_type: "chat_completion".into(),
            finish_reason: self.finish_reason.clone(),
            user_agent: None,
            engine_cached_tokens: self.engine_cached_tokens,
            request_params: Default::default(),
            server_address: String::new(),
            server_port: 0,
            served_by: self.served_by.clone(),
            bearer_token: None,
            fusillade_batch_id: self.batch_id,
            fusillade_request_id: Some(self.request_id),
            custom_id: self.custom_id.clone(),
            batch_completion_window: self.completion_window.clone(),
            batch_created_at: self.batch_created_at,
            batch_request_source: "durable_billing".into(),
            trace_id: None,
        };
        Ok(DurableBillingInput {
            raw,
            owner_id: self.owner_id,
            api_key_id: self.api_key_id.ok_or("missing_key")?,
            api_key_purpose: purpose,
            cap_scope_root: self.cap_scope_root,
            model_id: self.model_id.ok_or("missing_model")?,
        })
    }
}

#[derive(FromRow)]
struct Acceptance {
    state: String,
    billing_mode: Option<String>,
    accepted_event_id: Option<Uuid>,
}

pub(crate) struct BillingWorker {
    main: DynPools,
    fusillade: DynPools,
    batcher: AnalyticsBatcher,
    config: Config,
    worker_id: Uuid,
}

impl BillingWorker {
    pub(crate) fn new(main: DynPools, fusillade: DynPools, config: Config) -> Self {
        let (batcher, _) = AnalyticsBatcher::new(main.clone(), config.clone(), None);
        Self {
            main,
            fusillade,
            batcher,
            config,
            worker_id: Uuid::new_v4(),
        }
    }

    pub(crate) fn with_metrics(mut self, metrics: Option<crate::metrics::GenAiMetrics>) -> Self {
        let (batcher, _) = AnalyticsBatcher::new(self.main.clone(), self.config.clone(), metrics);
        self.batcher = batcher;
        self
    }

    pub(crate) fn with_usage_refresh_notify(mut self, notify: std::sync::Arc<tokio::sync::Notify>) -> Self {
        self.batcher = self.batcher.with_usage_refresh_notify(notify);
        self
    }

    async fn heartbeat(&self) -> anyhow::Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(5),
            sqlx::query("INSERT INTO billing_worker_heartbeat (singleton,last_seen_at) VALUES (true,now()) ON CONFLICT(singleton) DO UPDATE SET last_seen_at=EXCLUDED.last_seen_at")
                .execute(&*self.main.write())
        ).await??;
        Ok(())
    }

    pub(crate) async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        // Polling and per-event work must not delay the health signal. Both loops
        // share supervision: failure stops this worker and its heartbeat together.
        let health = async {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return Ok::<(), anyhow::Error>(()),
                    _ = interval.tick() => self.heartbeat().await?,
                }
            }
        };
        let work = async {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(
                self.config.analytics.durable_billing.poll_interval_ms,
            ));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return Ok::<(), anyhow::Error>(()),
                    _ = interval.tick() => {
                        self.tick().await?;
                        self.cleanup().await?;
                    }
                }
            }
        };
        tokio::try_join!(health, work)?;
        Ok(())
    }

    async fn tick(&self) -> Result<usize, sqlx::Error> {
        sqlx::query("INSERT INTO billing_worker_heartbeat (singleton,last_seen_at) VALUES (true,now()) ON CONFLICT(singleton) DO UPDATE SET last_seen_at=EXCLUDED.last_seen_at")
            .execute(&*self.main.write()).await?;
        let events = sqlx::query_as::<_, Event>("UPDATE fusillade_billing_events e SET lease_owner=$2,lease_until=now()+interval '60 seconds' FROM (SELECT event_id FROM fusillade_billing_events WHERE processing_state='pending' AND next_attempt_at<=now() AND (lease_until IS NULL OR lease_until<now()) ORDER BY next_attempt_at,event_id LIMIT $1 FOR UPDATE SKIP LOCKED) due WHERE e.event_id=due.event_id RETURNING e.event_id, e.request_id, e.owner_id, e.billing_mode, e.event_version, e.model_id, e.api_key_id, e.api_key_purpose, e.cap_scope_root, e.batch_id, e.requested_model, e.response_model, e.completion_window, e.batch_created_at, e.started_at, e.completed_at, e.usage_present, e.prompt_tokens, e.completion_tokens, e.total_tokens, e.reasoning_tokens, e.engine_cached_tokens, e.cache_read_input_tokens, e.cache_creation_5m_input_tokens, e.cache_creation_1h_input_tokens, e.cache_creation_24h_input_tokens, e.request_path, e.request_method, e.custom_id, e.finish_reason, e.served_by")
            .bind(self.config.analytics.durable_billing.batch_size as i64).bind(self.worker_id).fetch_all(&*self.main.write()).await?;
        let mut processed = 0;
        for event in events {
            let renewed = sqlx::query("UPDATE fusillade_billing_events SET lease_until=now()+interval '60 seconds' WHERE event_id=$1 AND lease_owner=$2 AND processing_state='pending'")
                .bind(event.event_id).bind(self.worker_id).execute(&*self.main.write()).await?.rows_affected();
            if renewed == 0 {
                continue;
            }
            match self.process(&event).await {
                Ok(done) => processed += usize::from(done),
                Err(_) => {
                    // Never include SQL errors/bind data in telemetry (credentials
                    // are absent, but identifiers and internal details remain private).
                    metrics::counter!("dwctl_billing_worker_retries_total").increment(1);
                    self.retry(event.event_id, "transaction_failed").await?;
                }
            }
        }
        let row = sqlx::query("SELECT count(*) FILTER (WHERE processing_state='pending') AS pending, count(*) FILTER (WHERE processing_state='unresolved') AS unresolved, COALESCE(EXTRACT(EPOCH FROM now()-MIN(created_at) FILTER (WHERE billing_mode='durable' AND processing_state='pending')),0)::float8 AS age FROM fusillade_billing_events WHERE processed_at IS NULL")
            .fetch_one(&*self.main.write()).await?;
        metrics::gauge!("dwctl_billing_events_pending").set(row.get::<i64, _>("pending") as f64);
        metrics::gauge!("dwctl_billing_events_unresolved").set(row.get::<i64, _>("unresolved") as f64);
        metrics::gauge!("dwctl_billing_events_oldest_pending_seconds").set(row.get::<f64, _>("age").max(0.0));
        super::billing_reconciliation::reconcile_with_retention(
            &self.main,
            &self.fusillade,
            self.config.analytics.durable_billing.batch_size,
            self.config.analytics.durable_billing.analytics_retention_days,
        )
        .await?;
        Ok(processed)
    }

    async fn process(&self, event: &Event) -> Result<bool, sqlx::Error> {
        if event.billing_mode == "legacy" {
            // Shadow data cannot authorize a debit. Retain mismatches for review;
            // allow the asynchronous legacy writer time to finish before comparing.
            if (Utc::now() - event.completed_at).num_seconds() < 120 {
                self.retry(event.event_id, "awaiting_legacy_comparison").await?;
                return Ok(false);
            }
            let usage = sqlx::query("SELECT COALESCE(sum(prompt_tokens),0)::bigint AS prompt, COALESCE(sum(completion_tokens),0)::bigint AS completion, count(*) AS records, sum(total_cost) AS cost FROM http_analytics WHERE fusillade_request_id=$1")
                .bind(event.request_id).fetch_one(&*self.main.write()).await?;
            let input = match event.billing_input() {
                Ok(input) => input,
                Err(code) => {
                    self.unresolved(event.event_id, code).await?;
                    return Ok(false);
                }
            };
            let prepared = match self.batcher.prepare_durable(input).await {
                Ok(prepared) => prepared,
                Err(sqlx::Error::Protocol(_)) => {
                    self.unresolved(event.event_id, "shadow_pricing_unresolved").await?;
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
            if usage.get::<Option<rust_decimal::Decimal>, _>("cost") == Some(prepared.total_cost())
                && usage.get::<i64, _>("records") == 1
                && event.prompt_tokens == Some(usage.get("prompt"))
                && event.completion_tokens == Some(usage.get("completion"))
            {
                self.finish_without_charge(event.event_id, "shadow_billing_matches").await?;
                return Ok(true);
            }
            self.unresolved(event.event_id, "shadow_billing_mismatch").await?;
            return Ok(false);
        }
        // Acceptance evidence outlives response content and request retention.
        // Live/archive rows remain useful while an execution is not accepted.
        // Do not hold the billing transaction while consulting another database.
        let accepted = sqlx::query_as::<_, Acceptance>("SELECT state,billing_mode,accepted_event_id FROM (SELECT 'completed'::text AS state,'durable'::text AS billing_mode,accepted_event_id,0 AS priority FROM billing_acceptances WHERE request_id=$1 UNION ALL SELECT state,billing_mode,accepted_event_id,1 FROM requests WHERE id=$1 UNION ALL SELECT state,billing_mode,accepted_event_id,2 FROM batch_requests_archive WHERE id=$1) evidence ORDER BY priority LIMIT 1")
            .bind(event.request_id).fetch_optional(&*self.fusillade.write()).await?;
        let Some(accepted) = accepted else {
            self.unresolved(event.event_id, "missing_request").await?;
            return Ok(false);
        };
        if accepted.billing_mode.as_deref() != Some("durable") {
            self.unresolved(event.event_id, "billing_mode_mismatch").await?;
            return Ok(false);
        }
        if accepted.state != "completed" {
            if matches!(accepted.state.as_str(), "failed" | "canceled") {
                self.unresolved(event.event_id, "request_not_accepted").await?;
            } else {
                self.retry(event.event_id, "awaiting_acceptance").await?;
            }
            return Ok(false);
        }
        if accepted.accepted_event_id != Some(event.event_id) {
            if accepted.accepted_event_id.is_none() {
                self.unresolved(event.event_id, "missing_accepted_event").await?;
                return Ok(false);
            }
            self.finish_without_charge(event.event_id, "unaccepted_attempt").await?;
            return Ok(true);
        }
        let input = match event.billing_input() {
            Ok(input) => input,
            Err(code) => {
                self.unresolved(event.event_id, code).await?;
                return Ok(false);
            }
        };
        let prepared = match self.batcher.prepare_durable(input).await {
            Ok(prepared) => prepared,
            Err(sqlx::Error::Protocol(_)) => {
                self.unresolved(event.event_id, "invalid_billing_inputs").await?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let mut tx = self.main.write().begin().await?;
        let locked = sqlx::query_scalar::<_, Uuid>(
            "SELECT event_id FROM fusillade_billing_events WHERE event_id=$1 AND processing_state='pending' AND lease_owner=$2 FOR UPDATE SKIP LOCKED",
        )
        .bind(event.event_id)
        .bind(self.worker_id)
        .fetch_optional(&mut *tx)
        .await?;
        if locked.is_none() {
            return Ok(false);
        }
        let source = format!("durable-billing:{}", event.request_id);
        let inserted = sqlx::query("INSERT INTO billing_receipts(request_id,owner_id,event_id,total_cost,ledger_source_id) VALUES($1,$2,$3,0,$4) ON CONFLICT(request_id) DO NOTHING")
            .bind(event.request_id).bind(event.owner_id).bind(event.event_id).bind(&source).execute(&mut *tx).await?.rows_affected();
        if inserted == 0 {
            let receipt = sqlx::query("SELECT owner_id,event_id FROM billing_receipts WHERE request_id=$1")
                .bind(event.request_id)
                .fetch_one(&mut *tx)
                .await?;
            if receipt.get::<Uuid, _>("owner_id") != event.owner_id || receipt.get::<Uuid, _>("event_id") != event.event_id {
                tx.rollback().await?;
                self.unresolved(event.event_id, "receipt_identity_mismatch").await?;
                return Ok(false);
            }
        } else {
            if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM credits_transactions WHERE source_id=$1)")
                .bind(&source)
                .fetch_one(&mut *tx)
                .await?
            {
                tx.rollback().await?;
                self.unresolved(event.event_id, "receipt_history_missing").await?;
                return Ok(false);
            }
            let result = self.batcher.write_durable(&mut tx, prepared.clone()).await?;
            sqlx::query("UPDATE billing_receipts SET total_cost=$2,analytics_id=$3,input_price_per_token=$4,output_price_per_token=$5,uncached_cost=$6,analytics_timestamp=$7 WHERE request_id=$1")
                .bind(event.request_id).bind(result.total_cost).bind(result.analytics_id)
                .bind(result.input_price_per_token).bind(result.output_price_per_token).bind(result.uncached_cost).bind(event.started_at)
                .execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE fusillade_billing_events SET lease_owner=NULL,lease_until=NULL,processing_state='processed',processed_at=now(),disposition='billed',error_code=NULL WHERE event_id=$1")
            .bind(event.event_id).execute(&mut *tx).await?;
        tx.commit().await?;
        if inserted > 0 {
            self.batcher.record_durable_metrics(&prepared).await;
        }
        metrics::counter!("dwctl_billing_events_processed_total", "disposition" => "billed").increment(1);
        Ok(true)
    }

    async fn retry(&self, event_id: Uuid, code: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE fusillade_billing_events SET lease_owner=NULL,lease_until=NULL,attempt_count=attempt_count+1,error_code=$2,next_attempt_at=now()+make_interval(secs => LEAST(300,POWER(2,LEAST(attempt_count,8)))::double precision) WHERE event_id=$1 AND processing_state='pending' AND lease_owner=$3")
            .bind(event_id).bind(code).bind(self.worker_id).execute(&*self.main.write()).await?;
        Ok(())
    }

    async fn unresolved(&self, event_id: Uuid, code: &str) -> Result<(), sqlx::Error> {
        let changed = sqlx::query("UPDATE fusillade_billing_events SET lease_owner=NULL,lease_until=NULL,processing_state='unresolved',error_code=$2,attempt_count=attempt_count+1 WHERE event_id=$1 AND processing_state='pending' AND lease_owner=$3")
            .bind(event_id).bind(code).bind(self.worker_id).execute(&*self.main.write()).await?.rows_affected();
        if changed > 0 {
            crate::background_error!(crate::metrics::errors::component::ANALYTICS, "billing_event_unresolved", Error,
                event_id = %event_id, reason = code, "Billing event requires reconciliation");
        }
        Ok(())
    }

    async fn finish_without_charge(&self, event_id: Uuid, disposition: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE fusillade_billing_events SET lease_owner=NULL,lease_until=NULL,processing_state='processed',processed_at=now(),disposition=$2,error_code=NULL WHERE event_id=$1 AND processing_state='pending' AND lease_owner=$3")
            .bind(event_id).bind(disposition).bind(self.worker_id).execute(&*self.main.write()).await?;
        metrics::counter!("dwctl_billing_events_processed_total", "disposition" => disposition.to_owned()).increment(1);
        Ok(())
    }

    async fn cleanup(&self) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM fusillade_billing_events WHERE event_id IN (SELECT event_id FROM fusillade_billing_events WHERE processing_state='processed' AND processed_at < now()-make_interval(hours=>$1) ORDER BY processed_at,event_id LIMIT $2 FOR UPDATE SKIP LOCKED)")
            .bind(self.config.analytics.durable_billing.retention_hours.min(i32::MAX as u64) as i32)
            .bind(self.config.analytics.durable_billing.batch_size as i64).execute(&*self.main.write()).await?;
        Ok(())
    }
}

/// A live consumer and bounded queue lag are required before another durable dispatch.
pub(crate) async fn admission_ready(main: &DynPools, max_age_secs: u64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM billing_worker_heartbeat WHERE singleton AND last_seen_at>now()-interval '30 seconds') AND NOT EXISTS(SELECT 1 FROM fusillade_billing_events WHERE billing_mode='durable' AND processing_state='pending' AND created_at<now()-make_interval(secs=>$1))")
        .bind(max_age_secs.min(i32::MAX as u64) as f64).fetch_one(&*main.write()).await
}

#[cfg(test)]
mod tests;
