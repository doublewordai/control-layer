//! A manual, read-only verification harness.
//!
//! `#[ignore]`d: it needs a real database and a reachable tokenizer-svc, so it never runs in
//! CI. Run it by hand against a prod-forked branch (or prod, which it only ever `SELECT`s
//! from) to answer the question the recompute exists to answer — *can we reproduce the
//! recorded usage from the inputs, outputs and cache entries alone?*
//!
//! ```sh
//! kubectl -n control-layer port-forward svc/tokenizer-svc 18088:8088 &
//! RECOMPUTE_DB_URL='postgresql://…/clay' \
//! RECOMPUTE_TOKENIZER_URL='http://localhost:18088' \
//! RECOMPUTE_USER_ID='…' RECOMPUTE_MODEL='detail-zai/glm-5.2' RECOMPUTE_LIMIT=200 \
//!   cargo test -p dwctl --lib recompute::verify_harness -- --ignored --nocapture
//! ```
//!
//! Nothing here writes: the corpus load is a `SELECT`, and the cache index it hands the
//! classifier refuses writes outright.

#[cfg(test)]
mod tests {
    use crate::prompt_cache::{Classifier, ModelConfigResolver, PrincipalResolver, TierPolicy, TokenizerClient};
    use crate::recompute::cache_fields::CreationTier;
    use crate::recompute::cache_replay::HistoricalIndex;
    use crate::recompute::source::CorpusFilter;
    use chrono::Utc;
    use std::sync::Arc;

    fn env(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }

    #[tokio::test]
    #[ignore = "manual: needs a real database and a reachable tokenizer-svc"]
    async fn verify_recorded_usage_is_reproducible() {
        let Some(db) = env("RECOMPUTE_DB_URL") else {
            eprintln!("set RECOMPUTE_DB_URL to run this");
            return;
        };
        let tokenizer_url = env("RECOMPUTE_TOKENIZER_URL").unwrap_or_else(|| "http://localhost:18088".into());
        let limit: i64 = env("RECOMPUTE_LIMIT").and_then(|v| v.parse().ok()).unwrap_or(200);
        let days: i64 = env("RECOMPUTE_DAYS").and_then(|v| v.parse().ok()).unwrap_or(2);
        // Optional RFC3339 end bound, so a corpus can target an incident cluster that newer
        // healthy traffic would otherwise push past the row limit.
        let end = env("RECOMPUTE_END")
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(&v).ok())
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&db)
            .await
            .expect("connect");

        let filter = CorpusFilter {
            start: end - chrono::Duration::days(days),
            end,
            user_id: env("RECOMPUTE_USER_ID").and_then(|v| v.parse().ok()),
            uri_pattern: Some(env("RECOMPUTE_URI").unwrap_or_else(|| "%/chat/completions".into())),
            model: env("RECOMPUTE_MODEL"),
            limit,
        };

        let tokenizer_client = TokenizerClient::new(tokenizer_url.clone());
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new(tokenizer_url),
            // Swapped per row by `recompute_corpus`; this is only a placeholder.
            Arc::new(HistoricalIndex::new(pool.clone(), Utc::now())),
            TierPolicy::from_config(&["5m".to_string(), "1h".to_string()], "5m"),
            crate::prompt_cache::TelemetryPolicy::from_config(false, &[]),
            true,
        );

        let report =
            crate::recompute::recompute_corpus(&pool, &filter, CreationTier::FiveMinute, Some(&classifier), Some(&tokenizer_client))
                .await
                .expect("recompute");

        let s = &report.summary;
        println!("\n=== corpus ===");
        println!("rows              {}", s.rows_total);
        println!("changed           {}", s.rows_changed);
        println!("unchanged         {}", s.rows_unchanged);
        println!("not replayable    {}", s.rows_not_replayable);
        println!("columns only      {}", s.rows_columns_only);
        println!("tier inferred     {}", s.rows_tier_inferred);
        println!("\n=== cache split reconstruction ===");
        println!("reconstructed     {}", s.rows_cache_reconstructed);
        println!("disagreed         {}", s.rows_cache_disagreed);
        println!("\n=== tokenizer verification ===");
        println!("render checked    {}", s.rows_render_checked);
        println!("render disagreed  {}", s.rows_render_disagreed);
        println!("rescued           {}", s.rows_tokenizer_rescued);
        println!("\n=== totals ===");
        println!("stored prompt     {}", s.stored_prompt_tokens);
        println!("recomputed prompt {}", s.recomputed_prompt_tokens);
        println!("stored cost       {}", s.stored_cost);
        println!("recomputed cost   {}", s.recomputed_cost);
        println!("net correction    {}", s.net_correction);
        for w in &report.warnings {
            println!("\nWARNING: {w}");
        }

        // Show the first few disagreements, which are the whole point of running this.
        for row in report.rows.iter().filter(|r| r.changed).take(5) {
            println!(
                "\nCHANGED analytics_id={} source={:?}\n  stored     {:?}\n  recomputed {:?}",
                row.analytics_id,
                row.token_source,
                row.stored,
                row.recomputed.as_ref()
            );
        }
        for row in report
            .rows
            .iter()
            .filter(|r| r.reconstructed_cache.as_ref().is_some_and(|c| !c.agrees_with_stored))
            .take(5)
        {
            let c = row.reconstructed_cache.as_ref().unwrap();
            println!(
                "\nCACHE DISAGREE analytics_id={} stored(read={},c5m={}) reconstructed(read={},c5m={}) fidelity={} render={:?}",
                row.analytics_id,
                row.stored.cache_read,
                row.stored.cache_creation_5m,
                c.cache_read,
                c.cache_creation_5m,
                c.fidelity,
                c.render_total
            );
        }
    }
}
