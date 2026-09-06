//! Loading a corpus of already-billed requests, with the payloads needed to replay them.
//!
//! One query, driven off `http_analytics` and joined to the fusillade rows that hold the
//! bodies. Deliberately **not** one lookup per request: probing `http_analytics` per row
//! saturated the Neon pageserver during the July remediation and had to be killed after
//! fourteen minutes. A corpus is bounded and scanned once.
//!
//! Bodies come from fusillade (`request_templates.body`, `requests.response_body`) and never
//! from outlet. Outlet does not store wire bytes — it round-trips the body through typed
//! structs and re-serialises, which silently drops the cache extension fields while
//! `prompt_tokens_details.cached_tokens` survives. A body read from there looks plausibly
//! cache-annotated while the numbers that matter are gone.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use super::replay::StoredExchange;

/// Which requests to recompute.
///
/// Every field narrows the set; the time window is required because an unbounded scan of a
/// 185M-row table is never what anyone means. Anchor a corpus on the deploy window that
/// introduced the bug plus whatever identifies the affected traffic — not on the symptom,
/// or rows that happen to look plausible are missed.
#[derive(Debug, Clone)]
pub struct CorpusFilter {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Restrict to one user. Usually set: incidents are rarely spread evenly.
    pub user_id: Option<Uuid>,
    /// SQL `LIKE` pattern against `http_analytics.uri`, e.g. `/messages%`.
    pub uri_pattern: Option<String>,
    /// Restrict to one model alias.
    pub model: Option<String>,
    /// Hard cap. A recompute reads whole response bodies, so an unbounded corpus is a
    /// memory and IO hazard as much as a correctness one.
    pub limit: i64,
}

/// One already-billed request: what analytics currently says, plus the payload to check it
/// against.
#[derive(Debug, Clone)]
pub struct CorpusRow {
    pub analytics_id: i64,
    pub user_id: Option<Uuid>,
    pub model: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub fusillade_request_id: Option<Uuid>,
    /// Present on batch traffic. Carried because a batch corpus needs its `batch_aggregates`
    /// analytics columns delta-folded, and that is not inferable from the numbers alone.
    pub fusillade_batch_id: Option<Uuid>,
    /// When the customer submitted the work, for anything routed through fusillade — batch,
    /// async, AND flex. Persisted by migration 134 from the same `x-fusillade-batch-created-at`
    /// header the live path prices with, so this reproduces the live `pricing_timestamp`
    /// input verbatim: it equals `batches.created_at` for batch traffic, `requests.created_at`
    /// for flex traffic, and is NULL for realtime (which is priced at dispatch). Falling back
    /// to `timestamp` therefore matches the live path for all three row classes, and only
    /// pre-migration-134 flex rows carry NULL here for a reason other than "realtime" —
    /// see [`Self::cache_pricing_time_is_unresolved`].
    pub submitted_at: Option<DateTime<Utc>>,
    /// The fusillade completion window the live path wrote here ("1h", "24h", ...). Empty for
    /// realtime. Used only to tell flex (no batch id, non-empty SLA) from realtime when
    /// `submitted_at` is NULL, so pre-migration-134 flex rows can be detected and excluded
    /// from cache-multiplier repricing rather than re-priced against the wrong version.
    pub batch_sla: String,

    // What is stored today — the "before" side of every delta.
    pub stored_prompt_tokens: i64,
    pub stored_completion_tokens: i64,
    pub stored_reasoning_tokens: i64,
    pub stored_total_tokens: i64,
    pub stored_cache_read: i64,
    pub stored_cache_creation_5m: i64,
    pub stored_cache_creation_1h: i64,
    pub stored_cache_creation_24h: i64,
    pub stored_total_cost: Option<Decimal>,

    /// Prices as they were resolved at inference time. Re-pricing uses these rather than
    /// re-resolving the tariff, so a correction cannot be silently re-based onto a tariff
    /// that changed after the fact.
    pub input_price_per_token: Option<Decimal>,
    pub output_price_per_token: Option<Decimal>,

    /// The payload, when fusillade still holds it. `None` means not replayable — a ZDR row,
    /// a row with no fusillade link, or one whose bodies have been purged.
    pub exchange: Option<StoredExchange>,
}

impl CorpusRow {
    /// Whether the cache split is all zero, i.e. this request recorded no caching at all.
    pub fn stored_cache_total(&self) -> i64 {
        self.stored_cache_read + self.stored_cache_creation_5m + self.stored_cache_creation_1h + self.stored_cache_creation_24h
    }

    /// The instant tariffs are resolved at — submission time for deferred work (batch or
    /// flex), else the request's own dispatch time. Must match the live batcher's
    /// `pricing_timestamp` exactly, or a recompute across a tariff change would "correct"
    /// rows the live path priced right.
    ///
    /// `submitted_at` is the same value the live path unwraps from the
    /// `x-fusillade-batch-created-at` header (migration 134 persisted it for exactly this
    /// purpose): `batches.created_at` for batch, `requests.created_at` for flex, NULL for
    /// realtime. Falling back to `timestamp` therefore reproduces the live
    /// `raw.batch_created_at.unwrap_or(raw.timestamp)` for all three row classes. The
    /// previous derivation read `batch_created_at` from a `LEFT JOIN fusillade.batches`,
    /// which is NULL for flex (no batch) and fell back to dispatch — mis-resolving the
    /// cache-tariff version whenever a boundary sat in `(submit_time, dispatch_time]`.
    pub fn pricing_timestamp(&self) -> DateTime<Utc> {
        self.submitted_at.unwrap_or(self.timestamp)
    }

    /// A deferred row whose submission instant — and therefore the cache-tariff version the
    /// live path billed with — is unrecoverable, so cache-multiplier repricing cannot be done
    /// honestly and must be skipped (the caller surfaces a warning instead of a confident
    /// wrong number).
    ///
    /// Migration 134 added `http_analytics.submitted_at` with no backfill: flex rows billed
    /// before that deploy stay NULL forever (fusillade purges dispatched requests, so the
    /// value cannot be reconstructed). Only flex rows can hit this — batch rows resolve their
    /// pricing instant from the batch's own `created_at` (which the JOIN held all along), and
    /// realtime rows are correctly priced at dispatch (NULL `submitted_at` is their normal
    /// state, not a gap). The detection therefore keys off a batchless row with a non-empty
    /// `batch_sla` (the definition of flex), which distinguishes flex from realtime when
    /// `submitted_at` is NULL.
    pub fn cache_pricing_time_is_unresolved(&self) -> bool {
        self.fusillade_batch_id.is_none() && !self.batch_sla.is_empty() && self.submitted_at.is_none()
    }
}

/// Load the corpus in one scan.
///
/// `uri` doubles as the replay endpoint: `http_analytics` stores the path the caller used
/// (`/messages?beta=true`, `/chat/completions`) without the proxy prefix, which is exactly
/// the shape response parsing dispatches on.
#[tracing::instrument(skip(pool), fields(limit = filter.limit))]
pub async fn load_corpus(pool: &PgPool, filter: &CorpusFilter) -> Result<Vec<CorpusRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            ha.id                              AS "analytics_id!",
            ha.user_id,
            ha.model,
            ha.timestamp                       AS "timestamp!",
            ha.uri                             AS "uri!",
            ha.status_code,
            ha.fusillade_request_id,
            ha.fusillade_batch_id,
            ha.prompt_tokens                   AS "prompt_tokens!",
            ha.completion_tokens               AS "completion_tokens!",
            ha.reasoning_tokens                AS "reasoning_tokens!",
            ha.total_tokens                    AS "total_tokens!",
            ha.cache_read_input_tokens         AS "cache_read!",
            ha.cache_creation_5m_input_tokens  AS "cache_creation_5m!",
            ha.cache_creation_1h_input_tokens  AS "cache_creation_1h!",
            ha.cache_creation_24h_input_tokens AS "cache_creation_24h!",
            ha.total_cost,
            ha.input_price_per_token,
            ha.output_price_per_token,
            -- When the customer submitted the work, for anything routed through fusillade
            -- (batch, async, flex). NULL for realtime and for rows predating migration 134.
            -- Drives cache-multiplier version resolution at the same instant the live path
            -- priced at, so a recompute across a tariff change does not "correct" rows the
            -- live path priced right. See `CorpusRow::pricing_timestamp`.
            ha.submitted_at                    AS "submitted_at?",
            -- The fusillade completion window ("" for realtime, "1h"/"24h"/... for deferred).
            -- Empty for realtime. Used to tell flex (no batch id, non-empty SLA) from realtime
            -- when `submitted_at` is NULL, so pre-migration-134 flex rows can be detected and
            -- excluded from cache-multiplier repricing rather than re-priced at dispatch.
            ha.batch_sla                       AS "batch_sla!",
            -- `?` overrides sqlx's nullability inference: rt.body is NOT NULL in its own
            -- table, but this is a LEFT JOIN, so it is absent for any row with no fusillade
            -- link. Without the override sqlx types it as String and the None case vanishes.
            rt.body                            AS "request_body?",
            r.response_body                    AS "response_body?",
            r.response_status                  AS "response_status?"
        FROM http_analytics ha
        LEFT JOIN fusillade.requests r          ON r.id  = ha.fusillade_request_id
        LEFT JOIN fusillade.request_templates_all rt ON rt.id = r.template_id
        -- Ordered so the planner drives off a timestamp index. There is NO index on
        -- http_analytics.user_id, and ordering by `id` instead makes Postgres walk the
        -- primary key filtering as it goes — on a 186M-row table that scans most of it
        -- before the LIMIT is satisfied. The time window is the only selective thing here,
        -- so it has to lead: idx_analytics_model_timestamp when a model is given, otherwise
        -- idx_analytics_timestamp.
        WHERE ha.timestamp >= $1
          AND ha.timestamp <= $2
          AND ($3::uuid IS NULL OR ha.user_id = $3)
          AND ($4::text IS NULL OR ha.uri LIKE $4)
          AND ($5::text IS NULL OR ha.model = $5)
          -- Only rows that were billed: attributed, successful, and priced. A NULL
          -- total_cost is a free model, where no charge was ever expected.
          AND ha.user_id IS NOT NULL
          AND ha.status_code BETWEEN 200 AND 299
        ORDER BY ha.timestamp DESC
        LIMIT $6
        "#,
        filter.start,
        filter.end,
        filter.user_id,
        filter.uri_pattern,
        filter.model,
        filter.limit,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            // A replay needs a response body above all; without one there is nothing to
            // re-read. The status comes from fusillade rather than http_analytics because
            // it is the upstream's, which is what the body belongs to.
            let exchange = r.response_body.map(|body| {
                let request_body = r.request_body.map(|b| b.into_bytes());
                StoredExchange {
                    endpoint: r.uri.clone(),
                    streamed: StoredExchange::streamed_from_body(request_body.as_deref()),
                    request_body,
                    response_body: Some(body.into_bytes()),
                    status_code: r.response_status.unwrap_or(r.status_code.unwrap_or(0) as i16) as u16,
                }
            });

            CorpusRow {
                analytics_id: r.analytics_id,
                user_id: r.user_id,
                model: r.model,
                timestamp: r.timestamp,
                fusillade_request_id: r.fusillade_request_id,
                fusillade_batch_id: r.fusillade_batch_id,
                submitted_at: r.submitted_at,
                batch_sla: r.batch_sla,
                stored_prompt_tokens: r.prompt_tokens,
                stored_completion_tokens: r.completion_tokens,
                stored_reasoning_tokens: r.reasoning_tokens,
                stored_total_tokens: r.total_tokens,
                stored_cache_read: r.cache_read,
                stored_cache_creation_5m: r.cache_creation_5m,
                stored_cache_creation_1h: r.cache_creation_1h,
                stored_cache_creation_24h: r.cache_creation_24h,
                stored_total_cost: r.total_cost,
                input_price_per_token: r.input_price_per_token,
                output_price_per_token: r.output_price_per_token,
                exchange,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_cache::TokenizerClient;
    use crate::recompute::cache_fields::CreationTier;
    use crate::recompute::recompute_corpus;
    use crate::test::utils::setup_fusillade_pool;
    use sqlx::PgPool;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A tokenizer-svc double: `/v1/render` answers `render_total` for any request,
    /// `/v1/tokenize` answers `tokenize_total`.
    async fn mock_tokenizer(render_total: u32, tokenize_total: u32) -> (MockServer, TokenizerClient) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": "m", "tokenizer_version": "tok-v1", "template_version": "tpl-v1",
                "total": render_total, "prefix_counts": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": "m", "tokenizer_version": "tok-v1",
                "segment_counts": [tokenize_total], "cumulative": [tokenize_total], "total": tokenize_total
            })))
            .mount(&server)
            .await;
        let client = TokenizerClient::new(server.uri());
        (server, client)
    }

    /// Seed one already-billed request with its stored payload, as the live path would have
    /// left it. Returns the analytics row id.
    #[allow(clippy::too_many_arguments)]
    async fn seed(
        pool: &PgPool,
        uri: &str,
        response_body: &str,
        prompt: i64,
        completion: i64,
        cache_read: i64,
        cache_creation_5m: i64,
        cost: Decimal,
    ) -> (Uuid, i64) {
        let user_id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO users (id, username, email, is_admin, auth_source) VALUES ($1,$2,$3,false,'test')",
            user_id,
            format!("u_{}", user_id.simple()),
            format!("{}@example.com", user_id.simple()),
        )
        .execute(pool)
        .await
        .unwrap();

        let template_id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO fusillade.request_templates (id, endpoint, method, path, model, api_key, body)
             VALUES ($1,'http://x','POST',$2,'m','k',$3)",
            template_id,
            uri,
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .execute(pool)
        .await
        .unwrap();

        let request_id = Uuid::new_v4();
        sqlx::query!(
            // created_by, not batch_id: `requests_attribution_xor` requires exactly one, and
            // these are realtime rows - the same shape as the incident corpus.
            "INSERT INTO fusillade.requests (id, template_id, state, model, response_status, response_body,
                                             claimed_at, started_at, completed_at, created_by)
             VALUES ($1,$2,'completed','m',200,$3,NOW(),NOW(),NOW(),$4)",
            request_id,
            template_id,
            response_body,
            user_id.to_string(),
        )
        .execute(pool)
        .await
        .unwrap();

        let analytics_id = sqlx::query_scalar!(
            "INSERT INTO http_analytics
               (instance_id, correlation_id, timestamp, method, uri, model, status_code, user_id,
                prompt_tokens, completion_tokens, total_tokens, cache_read_input_tokens,
                cache_creation_5m_input_tokens, cache_creation_input_tokens,
                total_cost, input_price_per_token, output_price_per_token, fusillade_request_id)
             VALUES ($1,1,NOW(),'POST',$2,'m',200,$3,$4,$5,$6,$7,$8,$8,$9,0.000001,0.000002,$10)
             RETURNING id",
            Uuid::new_v4(),
            uri,
            user_id,
            prompt,
            completion,
            prompt + completion,
            cache_read,
            cache_creation_5m,
            cost,
            request_id,
        )
        .fetch_one(pool)
        .await
        .unwrap();

        (user_id, analytics_id)
    }

    /// Seed one already-billed **flex** (batchless deferred) request, the shape the live
    /// batcher would have left for a fusillade dispatch with no batch id. Distinct from
    /// [`seed`] because flex carries two timestamps the realtime seed does not: a
    /// `submitted_at` (the request's own `created_at`, carried by the
    /// `x-fusillade-batch-created-at` header on batchless dispatches and persisted by
    /// migration 134) and a non-empty `batch_sla` (the fusillade completion window — "1h"
    /// for flex, "" for realtime). The analytics row's `timestamp` is the dispatch instant,
    /// `submitted_at` is the customer's submission, and a flex row has `submitted_at <
    /// timestamp`; the gap between them is the queue delay fusillade imposed.
    ///
    /// Pass `submitted_at = None` to simulate a flex row that predates migration 134: the
    /// column was never backfilled, so the live path's pricing instant is unrecoverable.
    /// The recompute must detect this and surface a warning rather than re-price against the
    /// wrong cache-tariff version. See
    /// `flex_row_reprices_at_submission_time_not_dispatch` and
    /// `pre_migration_134_flex_row_warns_instead_of_phantom_correcting` for the two cases.
    #[allow(clippy::too_many_arguments)]
    async fn seed_flex_row(
        pool: &PgPool,
        uri: &str,
        response_body: &str,
        prompt: i64,
        completion: i64,
        cache_read: i64,
        cache_creation_5m: i64,
        cost: Decimal,
        submitted_at: Option<DateTime<Utc>>,
        dispatched: DateTime<Utc>,
        batch_sla: &str,
    ) -> (Uuid, i64) {
        let user_id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO users (id, username, email, is_admin, auth_source) VALUES ($1,$2,$3,false,'test')",
            user_id,
            format!("u_{}", user_id.simple()),
            format!("{}@example.com", user_id.simple()),
        )
        .execute(pool)
        .await
        .unwrap();

        let template_id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO fusillade.request_templates (id, endpoint, method, path, model, api_key, body)
             VALUES ($1,'http://x','POST',$2,'m','k',$3)",
            template_id,
            uri,
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .execute(pool)
        .await
        .unwrap();

        let request_id = Uuid::new_v4();
        sqlx::query!(
            // created_by, not batch_id: `requests_attribution_xor` requires exactly one. No
            // batch_id is precisely what makes this row flex rather than batch or async.
            "INSERT INTO fusillade.requests (id, template_id, state, model, response_status, response_body,
                                             claimed_at, started_at, completed_at, created_by)
             VALUES ($1,$2,'completed','m',200,$3,NOW(),NOW(),NOW(),$4)",
            request_id,
            template_id,
            response_body,
            user_id.to_string(),
        )
        .execute(pool)
        .await
        .unwrap();

        let analytics_id = sqlx::query_scalar!(
            r#"INSERT INTO http_analytics
               (instance_id, correlation_id, timestamp, method, uri, model, status_code, user_id,
                prompt_tokens, completion_tokens, total_tokens, cache_read_input_tokens,
                cache_creation_5m_input_tokens, cache_creation_input_tokens,
                total_cost, input_price_per_token, output_price_per_token,
                fusillade_request_id, fusillade_batch_id, batch_sla, submitted_at)
            VALUES ($1,1,$2,'POST',$3,'m',200,$4,$5,$6,$7,$8,$9,$9,$10,0.000001,0.000002,$11,NULL,$12,$13)
            RETURNING id"#,
            Uuid::new_v4(),
            dispatched,
            uri,
            user_id,
            prompt,
            completion,
            prompt + completion,
            cache_read,
            cache_creation_5m,
            cost,
            request_id,
            batch_sla,
            submitted_at,
        )
        .fetch_one(pool)
        .await
        .unwrap();

        (user_id, analytics_id)
    }

    fn filter_for(user_id: Uuid) -> CorpusFilter {
        CorpusFilter {
            start: Utc::now() - chrono::Duration::hours(1),
            end: Utc::now() + chrono::Duration::hours(1),
            user_id: Some(user_id),
            uri_pattern: None,
            model: None,
            limit: 100,
        }
    }

    /// The property the whole feature rests on, end to end against a real database: pointed
    /// at traffic with nothing wrong with it, the recompute proposes no change.
    #[sqlx::test]
    async fn healthy_traffic_recomputes_to_zero_delta(pool: PgPool) {
        // The corpus query joins the fusillade schema, which #[sqlx::test] does not create.
        setup_fusillade_pool(&pool).await;
        // prompt 1000 @ 1e-6 + completion 100 @ 2e-6 = 0.0012, no caching.
        let (user_id, _) = seed(
            &pool,
            "/chat/completions",
            r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":100,"total_tokens":1100}}"#,
            1000,
            100,
            0,
            0,
            Decimal::from_str_exact("0.0012").unwrap(),
        )
        .await;

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, None)
            .await
            .unwrap();

        assert_eq!(report.summary.rows_total, 1);
        assert_eq!(report.summary.rows_changed, 0, "healthy traffic must propose no change");
        assert_eq!(report.summary.rows_unchanged, 1);
        assert_eq!(report.summary.net_correction, Decimal::ZERO);
    }

    /// The August incident, end to end: analytics stored Anthropic's `input_tokens` verbatim
    /// and dropped creation entirely. The recompute must recover the total, recover the
    /// creation from the FLAT field, and flag that the tier was assigned rather than read.
    #[sqlx::test]
    async fn anthropic_incident_row_is_detected_and_corrected(pool: PgPool) {
        setup_fusillade_pool(&pool).await;
        let (user_id, analytics_id) = seed(
            &pool,
            "/messages?beta=true",
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":565,"output_tokens":658,"cache_read_input_tokens":17631,"cache_creation_input_tokens":538}}"#,
            565,   // what the bug stored
            658,   // completion was correct
            17631, // cache_read was correct
            0,     // creation was dropped
            Decimal::from_str_exact("0.000447942276").unwrap(),
        )
        .await;

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, None)
            .await
            .unwrap();

        assert_eq!(report.summary.rows_changed, 1);
        let row = report.rows.iter().find(|r| r.analytics_id == analytics_id).unwrap();
        let rec = row.recomputed.as_ref().expect("row is replayable");

        assert_eq!(rec.prompt_tokens, 18_734, "565 + 17631 + 538");
        assert_eq!(rec.completion_tokens, 658, "completion was already right");
        assert_eq!(rec.cache_read, 17_631, "the split is carried through, not invented");
        assert_eq!(rec.cache_creation_5m, 538, "recovered from the FLAT field");
        assert!(row.cache_tier_inferred, "the body never stated a tier");
        assert!(report.summary.net_correction > Decimal::ZERO, "an undercharge");
    }

    /// A dwctl-cached request re-prices with the tariff version valid at its time — not the
    /// config defaults, and not a version that superseded it. Measured failure this pins:
    /// healthy GLM-5.2 traffic (tariff read ×0.8, writes ×1.0) re-priced with the defaults
    /// (read ×0.1, write ×1.25) and reported a −$0.06 "overcharge" on 25 healthy rows.
    #[sqlx::test]
    async fn cached_row_reprices_with_the_tariff_valid_at_its_time(pool: PgPool) {
        setup_fusillade_pool(&pool).await;

        // The model behind alias 'm', with a superseded tariff version and the live one.
        // Neither matches the config defaults, so resolving wrongly cannot pass by luck.
        let creator = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = crate::test::utils::create_test_endpoint(&pool, "ep-tariff", creator.id).await;
        let model_id = crate::test::utils::create_test_model(&pool, "m", "m", endpoint, creator.id).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, read_multiplier, min_prefix_tokens, valid_from, valid_until)
               VALUES ($1, 2.0, 2.0, 2.5, 0.5, 1024, now() - interval '2 hours', now() - interval '1 hour'),
                      ($1, 1.0, 1.0, 1.0, 0.8, 1024, now() - interval '1 hour', NULL)"#,
            model_id,
        )
        .execute(&pool)
        .await
        .unwrap();

        // The live path's arithmetic for prompt 31840 (read 30723 @ ×0.8, creation 251 @ ×1.0,
        // uncached 866) at 1e-6/2e-6: (866 + 30723·0.8 + 251·1.0)·1e-6 + 709·2e-6.
        let (user_id, _) = seed(
            &pool,
            "/chat/completions",
            r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":31840,"completion_tokens":709,"total_tokens":32549,"cache_read_input_tokens":30723,"cache_creation_input_tokens":251,"cache_creation":{"ephemeral_5m_input_tokens":251,"ephemeral_1h_input_tokens":0,"ephemeral_24h_input_tokens":0},"prompt_tokens_details":{"cached_tokens":30723}}}"#,
            31840,
            709,
            30723,
            251,
            Decimal::from_str_exact("0.0271134").unwrap(),
        )
        .await;

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, None)
            .await
            .unwrap();

        assert_eq!(report.summary.rows_total, 1);
        assert_eq!(
            report.summary.rows_changed, 0,
            "a healthy cached row must re-price to exactly what the live tariff charged"
        );
        assert_eq!(report.summary.net_correction, Decimal::ZERO);
        assert!(report.warnings.is_empty(), "the tariff resolved; nothing to warn about");
    }

    /// `http_analytics.total_cost` is numeric(12,8): the stored cost was rounded to 8dp by
    /// the column on insert. The recompute must compare at that same scale — a tariff whose
    /// arithmetic runs past 8dp (here read ×0.5714 on 1e-6/tok) otherwise shows every
    /// healthy row as "changed" by a sub-cent phantom. Measured: 194 phantom rows on a
    /// healthy 400-row Nemotron corpus before this rounding existed.
    #[sqlx::test]
    async fn cost_is_compared_at_the_stored_column_scale(pool: PgPool) {
        setup_fusillade_pool(&pool).await;
        let creator = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = crate::test::utils::create_test_endpoint(&pool, "ep-scale", creator.id).await;
        let model_id = crate::test::utils::create_test_model(&pool, "m", "m", endpoint, creator.id).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, read_multiplier, min_prefix_tokens)
               VALUES ($1, 1.0, 1.0, 1.0, 0.5714, 1024)"#,
            model_id,
        )
        .execute(&pool)
        .await
        .unwrap();

        // uncached 99·1e-6 + read 101·1e-6·0.5714 + completion 10·2e-6
        //   = 0.000099 + 0.0000577114 + 0.00002 = 0.0001767114 — past 8dp.
        // The column stored round8(that) = 0.00017671.
        let (user_id, _) = seed(
            &pool,
            "/chat/completions",
            r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":200,"completion_tokens":10,"total_tokens":210,"cache_read_input_tokens":101,"cache_creation_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0,"ephemeral_24h_input_tokens":0},"prompt_tokens_details":{"cached_tokens":101}}}"#,
            200,
            10,
            101,
            0,
            Decimal::from_str_exact("0.00017671").unwrap(),
        )
        .await;

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, None)
            .await
            .unwrap();

        assert_eq!(
            report.summary.rows_changed, 0,
            "a sub-8dp arithmetic tail must not read as a correction"
        );
        assert_eq!(report.summary.net_correction, Decimal::ZERO);
    }

    /// The July incident shape, end to end: the backend answered `"usage": null` and the
    /// row was billed nothing. The recompute has nothing to re-read, so the row must surface
    /// as not-replayable — NOT as "unchanged", which would certify a broken row as healthy.
    #[sqlx::test]
    async fn july_null_usage_row_surfaces_as_not_replayable(pool: PgPool) {
        setup_fusillade_pool(&pool).await;
        let (user_id, _) = seed(
            &pool,
            "/chat/completions",
            r#"{"choices":[{"finish_reason":"tool_calls","index":0,"message":{"role":"assistant","content":null}}],"created":1,"id":"c","model":"m","object":"chat.completion","usage":null}"#,
            0,
            0,
            0,
            0,
            Decimal::ZERO,
        )
        .await;

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, None)
            .await
            .unwrap();

        assert_eq!(report.summary.rows_total, 1);
        assert_eq!(report.summary.rows_not_replayable, 1, "no usage to re-read → needs a human");
        assert_eq!(report.summary.rows_unchanged, 0, "must not be certified healthy");
        assert_eq!(report.summary.rows_changed, 0);
        let row = &report.rows[0];
        assert!(
            row.note.as_deref().is_some_and(|n| n.contains("no usage object")),
            "the note must say why: {:?}",
            row.note
        );
    }

    /// The render verifies, it never replaces: a healthy row whose provider count the
    /// tokenizer contradicts stays UNCHANGED — the disagreement is a per-row finding, not a
    /// correction. Adopting "agreeing" renders instead would replace every healthy count
    /// with ours ± template drift and destroy the no-op guarantee.
    #[sqlx::test]
    async fn render_disagreement_is_annotated_but_never_adopted(pool: PgPool) {
        setup_fusillade_pool(&pool).await;
        let (user_id, _) = seed(
            &pool,
            "/chat/completions",
            r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":100,"total_tokens":1100}}"#,
            1000,
            100,
            0,
            0,
            Decimal::from_str_exact("0.0012").unwrap(),
        )
        .await;

        // Render says 2000 against a reported 1000 — a 50% divergence, far beyond tolerance.
        let (_server, tokenizer) = mock_tokenizer(2000, 0).await;
        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, Some(&tokenizer))
            .await
            .unwrap();

        assert_eq!(report.summary.rows_changed, 0, "a contested count is a finding, not a correction");
        assert_eq!(report.summary.rows_render_checked, 1);
        assert_eq!(report.summary.rows_render_disagreed, 1);
        let row = &report.rows[0];
        assert_eq!(row.recomputed.as_ref().unwrap().prompt_tokens, 1000, "provider count kept");
        assert_eq!(row.prompt_render_total, Some(2000));
        assert_eq!(row.prompt_render_agrees, Some(false));
    }

    /// An agreeing render annotates the row and moves nothing.
    #[sqlx::test]
    async fn render_agreement_is_annotated(pool: PgPool) {
        setup_fusillade_pool(&pool).await;
        let (user_id, _) = seed(
            &pool,
            "/chat/completions",
            r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":100,"total_tokens":1100}}"#,
            1000,
            100,
            0,
            0,
            Decimal::from_str_exact("0.0012").unwrap(),
        )
        .await;

        // 1005 vs 1000 = 50 bps, inside the 1% tolerance.
        let (_server, tokenizer) = mock_tokenizer(1005, 0).await;
        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, Some(&tokenizer))
            .await
            .unwrap();

        assert_eq!(report.summary.rows_changed, 0);
        assert_eq!(report.summary.rows_render_checked, 1);
        assert_eq!(report.summary.rows_render_disagreed, 0);
        let row = &report.rows[0];
        assert_eq!(row.recomputed.as_ref().unwrap().prompt_tokens, 1000, "never adopted, even agreeing");
        assert_eq!(row.prompt_render_agrees, Some(true));
    }

    /// The July shape WITH a tokenizer: the row is rescued instead of refused — prompt from
    /// the exact render, completion estimated from the response text, priced, and gated
    /// (`completion_token_source: "estimated"`) so the apply step demands an opt-in.
    #[sqlx::test]
    async fn usage_less_row_is_rescued_by_the_tokenizer(pool: PgPool) {
        setup_fusillade_pool(&pool).await;
        let (user_id, analytics_id) = seed(
            &pool,
            "/chat/completions",
            r#"{"choices":[{"finish_reason":"tool_calls","index":0,"message":{"role":"assistant","content":"partial answer"}}],"created":1,"id":"c","model":"m","object":"chat.completion","usage":null}"#,
            0,
            0,
            0,
            0,
            Decimal::ZERO,
        )
        .await;

        let (_server, tokenizer) = mock_tokenizer(4096, 512).await;
        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, Some(&tokenizer))
            .await
            .unwrap();

        assert_eq!(report.summary.rows_not_replayable, 0, "rescued, not refused");
        assert_eq!(report.summary.rows_changed, 1);
        assert_eq!(report.summary.rows_tokenizer_rescued, 1);
        let row = report.rows.iter().find(|r| r.analytics_id == analytics_id).unwrap();
        let rec = row.recomputed.as_ref().expect("rescued row is replayed");
        assert_eq!(rec.prompt_tokens, 4096, "prompt is the exact render");
        assert_eq!(rec.completion_tokens, 512, "completion is the text estimate");
        assert_eq!(rec.cache_read, 0, "no usage object → no split to read, none invented");
        assert_eq!(row.token_source.as_deref(), Some("rendered"));
        assert_eq!(row.completion_token_source.as_deref(), Some("estimated"));
        // 4096 × 1e-6 + 512 × 2e-6, at the row's stored unit prices.
        assert_eq!(rec.cost, Some(Decimal::from_str_exact("0.00512").unwrap()));
        assert!(report.summary.net_correction > Decimal::ZERO, "recovered usage, undercharge");
        assert_eq!(
            report.summary.net_correction_estimated, report.summary.net_correction,
            "the whole correction rests on an estimate here, and the summary must say so"
        );
    }

    /// A request with no stored body cannot be checked at all. It must be reported as such
    /// rather than dropped, and must not be counted as a change.
    #[sqlx::test]
    async fn a_row_without_a_body_is_reported_as_columns_only(pool: PgPool) {
        setup_fusillade_pool(&pool).await;
        let user_id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO users (id, username, email, is_admin, auth_source) VALUES ($1,$2,$3,false,'test')",
            user_id,
            format!("u_{}", user_id.simple()),
            format!("{}@example.com", user_id.simple()),
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO http_analytics (instance_id, correlation_id, timestamp, method, uri, model,
                status_code, user_id, prompt_tokens, completion_tokens, total_cost)
             VALUES ($1,1,NOW(),'POST','/chat/completions','m',200,$2,10,5,0.0001)",
            Uuid::new_v4(),
            user_id,
        )
        .execute(&pool)
        .await
        .unwrap();

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, None)
            .await
            .unwrap();

        assert_eq!(report.summary.rows_total, 1);
        assert_eq!(report.summary.rows_columns_only, 1);
        assert_eq!(report.summary.rows_changed, 0, "unverifiable is not the same as changed");
        assert_eq!(
            report.summary.net_correction,
            Decimal::ZERO,
            "an unchecked row must not move the net correction"
        );
    }

    /// Build a `CorpusRow` for unit tests of the pricing-timestamp derivation. Only the
    /// fields that derivation reads are parameterised; everything else is zeroed out and the
    /// row is left un-replayable (`exchange: None`), since the unit tests do not exercise the
    /// recompute corpus — only the derivations and the detection helper.
    fn row_for_pricing(
        timestamp: DateTime<Utc>,
        submitted_at: Option<DateTime<Utc>>,
        fusillade_batch_id: Option<Uuid>,
        batch_sla: &str,
    ) -> CorpusRow {
        CorpusRow {
            analytics_id: 0,
            user_id: None,
            model: None,
            timestamp,
            fusillade_request_id: None,
            fusillade_batch_id,
            submitted_at,
            batch_sla: batch_sla.to_string(),
            stored_prompt_tokens: 0,
            stored_completion_tokens: 0,
            stored_reasoning_tokens: 0,
            stored_total_tokens: 0,
            stored_cache_read: 0,
            stored_cache_creation_5m: 0,
            stored_cache_creation_1h: 0,
            stored_cache_creation_24h: 0,
            stored_total_cost: None,
            input_price_per_token: None,
            output_price_per_token: None,
            exchange: None,
        }
    }

    /// `pricing_timestamp` must mirror the live path's
    /// `raw.batch_created_at.unwrap_or(raw.timestamp)` for every row class: the live
    /// submission instant for batch and flex (the value `x-fusillade-batch-created-at`
    /// carried and migration 134 persisted), the dispatch instant for realtime. This is a
    /// no-DB pin of that derivation; the end-to-end flex case — the one the bug was on — is
    /// covered by `flex_row_reprices_at_submission_time_not_dispatch` below and exercises the
    /// resolved cache multipliers, not just the timestamp in isolation.
    #[test]
    fn pricing_timestamp_uses_submission_for_deferred_and_dispatch_for_realtime() {
        let submitted = Utc::now() - chrono::Duration::minutes(90);
        let dispatched = Utc::now() - chrono::Duration::minutes(30);

        // BATCH: submitted_at equals the batch's created_at — the live path's
        // batch_created_at for batch. The pricing instant is the batch's creation, not the
        // dispatcher's processing time.
        let batch = row_for_pricing(dispatched, Some(submitted), Some(Uuid::new_v4()), "24h");
        assert_eq!(batch.pricing_timestamp(), submitted);

        // FLEX: submitted_at is the request's own created_at — the live path's source on
        // batchless dispatches, persisted by migration 134. Priced at submission, not
        // dispatch. This is the row class the bug was on.
        let flex = row_for_pricing(dispatched, Some(submitted), None, "1h");
        assert_eq!(flex.pricing_timestamp(), submitted);
        assert_ne!(
            flex.pricing_timestamp(),
            dispatched,
            "flex must not price at dispatch — that is precisely the bug"
        );

        // REALTIME: no submitted_at (NULL by design — migration 134). Priced at dispatch,
        // which is the live path's `unwrap_or(raw.timestamp)` fallback for realtime.
        let realtime = row_for_pricing(dispatched, None, None, "");
        assert_eq!(realtime.pricing_timestamp(), dispatched);
    }

    /// `cache_pricing_time_is_unresolved` is true ONLY for flex rows predating migration 134
    /// (no batch id, a deferred SLA, no persisted `submitted_at`). It must NOT fire on:
    /// - a post-134 flex row (`submitted_at` was persisted, so the instant IS recoverable),
    /// - a batch row (pre- or post-134): the batch's `created_at` carries the pricing
    ///   instant via the JOIN, regardless of whether migration 134 also persisted it,
    /// - an async row (1h SLA + batch id): same as batch — batch-driven,
    /// - a realtime row: NULL `submitted_at` is the NORMAL state for realtime, not a gap;
    ///   firing here would exclude every realtime row with cache tokens by mistake.
    #[test]
    fn cache_pricing_time_is_unresolved_only_for_pre_migration_134_flex() {
        let submitted = Some(Utc::now() - chrono::Duration::minutes(90));
        let dispatched = Utc::now() - chrono::Duration::minutes(30);
        let batch_id = Some(Uuid::new_v4());

        // The one case that fires: pre-134 flex — batchless, deferred, no recorded submission.
        let pre134_flex = row_for_pricing(dispatched, None, None, "1h");
        assert!(
            pre134_flex.cache_pricing_time_is_unresolved(),
            "pre-134 flex with no submitted_at is the case this is for"
        );

        // Post-134 flex: same shape but submitted_at was persisted. Resolution is honest.
        let post134_flex = row_for_pricing(dispatched, submitted, None, "1h");
        assert!(
            !post134_flex.cache_pricing_time_is_unresolved(),
            "post-134 flex has a real submitted_at — must not warn"
        );

        // Pre-134 BATCH (no submitted_at, batch id, 24h SLA): the batch's created_at carries
        // the pricing instant, so the row is resolvable. Must NOT fire — the fix prices batch
        // rows at their batch's creation, regardless of when migration 134 shipped.
        let pre134_batch = row_for_pricing(dispatched, None, batch_id, "24h");
        assert!(
            !pre134_batch.cache_pricing_time_is_unresolved(),
            "batch rows are always resolvable via the batch's created_at"
        );

        // Pre-134 ASYNC (1h SLA + batch id): same — batch-driven.
        let pre134_async = row_for_pricing(dispatched, None, batch_id, "1h");
        assert!(
            !pre134_async.cache_pricing_time_is_unresolved(),
            "async (1h SLA + batch id) is batch-driven, not flex"
        );

        // REALTIME: NULL submitted_at is its normal state, not a gap. Must NOT fire — or
        // every realtime row with cache tokens would be excluded from repricing by mistake.
        let realtime = row_for_pricing(dispatched, None, None, "");
        assert!(
            !realtime.cache_pricing_time_is_unresolved(),
            "realtime rows are priced at dispatch by design, not by gap"
        );

        // A batch row with an EMPTY batch_sla (unusual but possible if the SLA header was
        // missing): still has a batch id, so still resolvable via the batch's created_at.
        let batch_empty_sla = row_for_pricing(dispatched, None, batch_id, "");
        assert!(
            !batch_empty_sla.cache_pricing_time_is_unresolved(),
            "a batch id always implies a known batches.created_at, even with an empty SLA"
        );
    }

    /// THE BUG, END TO END. A flex row submitted before a `model_cache_tariffs` version
    /// boundary and dispatched after it carries a stored cost the live path computed with
    /// the OLD multipliers (the version valid at submission). The recompute must price at
    /// the row's submission instant — not its dispatch instant — so it picks the same OLD
    /// version the live path billed with and the row recomputes to a zero delta.
    ///
    /// Before the fix, the recompute read `batch_created_at` from a `LEFT JOIN
    /// fusillade.batches` (NULL on flex rows) and fell back to `timestamp` (dispatch), so it
    /// resolved the NEW multipliers and reported a phantom "correction" on a row whose
    /// cache pricing was already correct. The same seed under the old code yielded
    /// `rows_changed == 1` with
    /// `net_correction == 0.02711340 - 0.01814750 == +0.00896590`.
    #[sqlx::test]
    async fn flex_row_reprices_at_submission_time_not_dispatch(pool: PgPool) {
        setup_fusillade_pool(&pool).await;

        // The model behind alias 'm', with two cache-tariff versions split by a boundary that
        // sits in the queue-delay window (between submission and dispatch). Neither matches
        // the config defaults, so resolving wrongly cannot pass by luck — mirroring
        // `cached_row_reprices_with_the_tariff_valid_at_its_time` for the flex case.
        let creator = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = crate::test::utils::create_test_endpoint(&pool, "ep-flex-boundary", creator.id).await;
        let model_id = crate::test::utils::create_test_model(&pool, "m", "m", endpoint, creator.id).await;
        let now = Utc::now();
        let submitted = now - chrono::Duration::minutes(90);
        let boundary = now - chrono::Duration::minutes(60);
        let dispatched = now - chrono::Duration::minutes(30);
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, read_multiplier, min_prefix_tokens, valid_from, valid_until)
               VALUES ($1, 2.0, 2.0, 2.5, 0.5, 1024, $2, $3),
                      ($1, 1.0, 1.0, 1.0, 0.8, 1024, $3, NULL)"#,
            model_id,
            submitted - chrono::Duration::hours(1), // OLD: valid before submission
            boundary,                                // OLD valid_until == NEW valid_from, between submit and dispatch
        )
        .execute(&pool)
        .await
        .unwrap();

        // The live path's arithmetic at the OLD multipliers (read ×0.5, write_5m ×2.0) for
        // prompt 31840 (read 30723, creation_5m 251, uncached 866), completion 709, at
        // 1e-6 / 2e-6:
        //   866·1e-6 + 30723·1e-6·0.5 + 251·1e-6·2.0 + 709·2e-6
        //   = 0.000866 + 0.0153615 + 0.000502 + 0.001418 = 0.0181475
        // round8 → 0.01814750.
        let (user_id, _) = seed_flex_row(
            &pool,
            "/chat/completions",
            r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":31840,"completion_tokens":709,"total_tokens":32549,"cache_read_input_tokens":30723,"cache_creation_input_tokens":251,"cache_creation":{"ephemeral_5m_input_tokens":251,"ephemeral_1h_input_tokens":0,"ephemeral_24h_input_tokens":0},"prompt_tokens_details":{"cached_tokens":30723}}}"#,
            31840,
            709,
            30723,
            251,
            Decimal::from_str_exact("0.01814750").unwrap(),
            Some(submitted),
            dispatched,
            "1h",
        )
        .await;

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, None)
            .await
            .unwrap();

        assert_eq!(report.summary.rows_total, 1);
        assert_eq!(
            report.summary.rows_changed, 0,
            "a healthy flex row submitted before the boundary and dispatched after it must \
             re-price at submission (the live path's pricing instant), not at dispatch"
        );
        assert_eq!(report.summary.rows_unchanged, 1);
        assert_eq!(report.summary.net_correction, Decimal::ZERO);
        assert!(
            report.warnings.is_empty(),
            "post-migration-134 flex rows carry a real submitted_at, so the fix resolves their \
             cache pricing honestly and nothing should warn: {:?}",
            report.warnings
        );
    }

    /// THE SCOPE LIMITATION, END TO END. A flex row that predates migration 134 carries
    /// `submitted_at = NULL`: the live path's pricing instant was never recorded and cannot
    /// be recovered (fusillade purges dispatched requests). Resolving at dispatch instead
    /// would re-introduce the bug — the wrong cache-tariff version whenever a boundary sits
    /// in `(submit_time, dispatch_time]`. The fix therefore EXCLUDES such rows from
    /// cache-multiplier repricing (re-prices at list rate, with a discount of zero) and
    /// surfaces a warning identifying them, so an operator sees "submission time unknown,
    /// cache pricing not re-verified" instead of a confident wrong number.
    #[sqlx::test]
    async fn pre_migration_134_flex_row_warns_instead_of_phantom_correcting(pool: PgPool) {
        setup_fusillade_pool(&pool).await;

        let creator = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = crate::test::utils::create_test_endpoint(&pool, "ep-flex-pre134", creator.id).await;
        let model_id = crate::test::utils::create_test_model(&pool, "m", "m", endpoint, creator.id).await;
        let now = Utc::now();
        let boundary = now - chrono::Duration::minutes(60);
        let dispatched = now - chrono::Duration::minutes(30);
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, read_multiplier, min_prefix_tokens, valid_from, valid_until)
               VALUES ($1, 2.0, 2.0, 2.5, 0.5, 1024, now() - interval '3 hours', $2),
                      ($1, 1.0, 1.0, 1.0, 0.8, 1024, $2, NULL)"#,
            model_id,
            boundary,
        )
        .execute(&pool)
        .await
        .unwrap();

        // A pre-134 flex row: submitted_at was never persisted (NULL), batch_sla "1h",
        // no batch id, cache tokens > 0 so cache-multiplier repricing IS attempted — which
        // is exactly the situation the warning has to defuse. The stored cost is the live
        // discount (same arithmetic as the post-134 seed above): 0.01814750.
        let (user_id, _) = seed_flex_row(
            &pool,
            "/chat/completions",
            r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":31840,"completion_tokens":709,"total_tokens":32549,"cache_read_input_tokens":30723,"cache_creation_input_tokens":251,"cache_creation":{"ephemeral_5m_input_tokens":251,"ephemeral_1h_input_tokens":0,"ephemeral_24h_input_tokens":0},"prompt_tokens_details":{"cached_tokens":30723}}}"#,
            31840,
            709,
            30723,
            251,
            Decimal::from_str_exact("0.01814750").unwrap(),
            None, // pre-migration-134: no submitted_at, never persisted
            dispatched,
            "1h",
        )
        .await;

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None, None)
            .await
            .unwrap();

        assert_eq!(report.summary.rows_total, 1);
        // The warning must call out the pre-migration-134 flex flag and must NOT be the
        // generic `rows_tariff_unresolvable` warning, since the model's tariff history is
        // intact — only the pricing instant is unrecoverable.
        assert!(
            report.warnings.iter().any(|w| w.contains("migration 134") && w.contains("flex")),
            "expected a pre-migration-134 flex warning, got: {:?}",
            report.warnings
        );
        assert!(
            !report.warnings.iter().any(|w| w.contains("tariff history deleted")),
            "the model's tariff history is intact, so the generic tariff-unresolvable warning must not fire: {:?}",
            report.warnings
        );
        // The row re-prices at LIST rate (cache_mults stays None) because the instant is
        // unrecoverable, so the row appears changed — list = 31840·1e-6 + 709·2e-6 = 0.033258,
        // vs the discounted stored 0.01814750. The warning defuses the correction.
        assert_eq!(
            report.summary.rows_changed, 1,
            "row re-prices at list rate (no discount) because cache multipliers were deliberately not resolved"
        );
        assert!(
            report.summary.net_correction > Decimal::ZERO,
            "list rate (no discount) exceeds the discounted stored cost, so the row reads as undercharged until the warning defuses it"
        );
    }
}
