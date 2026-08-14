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
    /// When the batch was created, for batch traffic. Carried because the live path prices
    /// a batch request as of its batch's creation — tariffs included — not as of processing.
    pub batch_created_at: Option<DateTime<Utc>>,

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

    /// The instant tariffs are resolved at — batch creation for batch traffic, else the
    /// request's own time. Must match the live batcher's `pricing_timestamp` exactly, or a
    /// recompute across a tariff change would "correct" rows the live path priced right.
    pub fn pricing_timestamp(&self) -> DateTime<Utc> {
        self.batch_created_at.unwrap_or(self.timestamp)
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
            -- `?` overrides sqlx's nullability inference: rt.body is NOT NULL in its own
            -- table, but this is a LEFT JOIN, so it is absent for any row with no fusillade
            -- link. Without the override sqlx types it as String and the None case vanishes.
            rt.body                            AS "request_body?",
            r.response_body                    AS "response_body?",
            r.response_status                  AS "response_status?",
            b.created_at                       AS "batch_created_at?"
        FROM http_analytics ha
        LEFT JOIN fusillade.requests r          ON r.id  = ha.fusillade_request_id
        LEFT JOIN fusillade.request_templates rt ON rt.id = r.template_id
        LEFT JOIN fusillade.batches b            ON b.id  = ha.fusillade_batch_id
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
                batch_created_at: r.batch_created_at,
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
    use crate::recompute::cache_fields::CreationTier;
    use crate::recompute::recompute_corpus;
    use crate::test::utils::setup_fusillade_pool;
    use sqlx::PgPool;

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

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None)
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

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None)
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

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None)
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

        let report = recompute_corpus(&pool, &filter_for(user_id), CreationTier::FiveMinute, None)
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
}
