-- no-transaction
--
-- COR-673: one logical queued request may have multiple physical attempts,
-- including failed attempts retained for observability, but only one successful
-- attempt may contribute analytics, usage, batch aggregates, or billing.
--
-- The application uses INSERT ... ON CONFLICT DO NOTHING and only performs
-- downstream billing/aggregation for rows returned by that INSERT. This partial
-- unique index is therefore the database fence that makes a second successful
-- physical attempt return no row. Non-2xx attempts remain unconstrained.
--
-- `x-fusillade-request-id` is also stamped onto ordinary realtime requests for
-- correlation, so the UUID alone is not proof that the request was queued. The
-- request-origin/SLA predicate covers batch, async and batchless flex traffic
-- while deliberately leaving realtime traffic unconstrained.
--
-- Historical duplicate successes were reconciled before this migration was
-- introduced. Build concurrently so continuous analytics inserts are not
-- blocked while PostgreSQL validates the historical table.
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS uq_http_analytics_fusillade_success
    ON http_analytics (fusillade_request_id)
    WHERE fusillade_request_id IS NOT NULL
      AND status_code BETWEEN 200 AND 299
      AND (request_origin = 'fusillade' OR batch_sla <> '');
