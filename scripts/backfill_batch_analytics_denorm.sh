#!/usr/bin/env bash
#
# Backfill batch_aggregates per-batch analytics (tokens / latency / cost) from
# http_analytics history (COR-524, migration 117).
#
# get_batch_analytics is repointed off raw http_analytics onto these columns (the contract
# PR). Going forward the analytics batcher folds each flush's batched requests into them in
# the same transaction it writes total_amount; this script fills historical batches.
#
# What it computes, per batch, over its *successful (status 2xx)* requests — INCLUDING free /
# zero-priced ones, matching the go-forward fold and get_batch_analytics's historical set (NOT
# just the billed rows; a free-model batch still reports tokens/latency):
#   total_requests                                  = COUNT(*)             -- the 2xx request count
#   total_prompt/completion/reasoning/total_tokens  = SUM(tokens)
#   sum_duration_ms  / count_duration_ms            = SUM / COUNT(duration_ms)
#   sum_ttfb_ms      / count_ttfb_ms                = SUM / COUNT(duration_to_first_byte_ms)
#   total_list_cost                                 = SUM(uncached_cost)   -- the list price (0 for free)
#
# --- Why one scan, not a per-batch sweep ---
# An earlier version swept batch_aggregates by max_seq ranges and aggregated http_analytics
# per batch. At prod scale that is ~500k random index probes into a 156M-row table — cold
# Neon pageserver reads at ~1.5s/batch => days, and it saturates PS_ReadIO against live
# traffic. Instead, aggregate ALL batches in ONE parallel sequential scan of http_analytics
# grouped by fusillade_batch_id (~13s / 500k batches on prod), then two bulk UPDATEs. The
# whole job runs in well under a minute.
#
# Aggregated set = the batch's successful (2xx) requests, excluding the system user
# (Uuid::nil()) and NULL user_id — matching the live fold. A batch with no retained
# http_analytics (aged out / all-non-2xx) is zero-stamped so get_batch_analytics reports
# zeros rather than a misleading absent value.
#
# Safety: only touches QUIESCENT batches — batch_aggregates.updated_at older than
# QUIESCENT_INTERVAL — so no live fold can be writing them concurrently (absolute SET, not
# +=). Idempotent + resumable: guarded by analytics_backfilled_at IS NULL, one committed
# transaction. Re-run to pick up batches that have quiesced since the last pass.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_batch_analytics_denorm.sh
# Optional env:
#   QUIESCENT_INTERVAL  only backfill batches idle at least this long (default '25 hours',
#                       > the 24h batch SLA, so a single pass is guaranteed-final per batch).

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the credits/http_analytics database}"
QUIESCENT_INTERVAL="${QUIESCENT_INTERVAL:-25 hours}"

echo "backfill_batch_analytics_denorm: set-based backfill  quiescent='${QUIESCENT_INTERVAL}'"

# One transaction: aggregate http_analytics once into a temp table, then bulk-update the
# eligible batches (with data) and zero-stamp the eligible batches with no retained data.
# DROP IF EXISTS + ON COMMIT DROP guard against a temp-table name lingering on a reused
# pooled backend (Neon PgBouncer keeps server connections across sessions).
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X <<SQL
\timing on
BEGIN;

DROP TABLE IF EXISTS _bf_analytics;
CREATE TEMP TABLE _bf_analytics ON COMMIT DROP AS
  SELECT fusillade_batch_id                             AS fbid,
         COUNT(id)                                       AS total_requests,
         COALESCE(SUM(prompt_tokens), 0)                 AS prompt_tokens,
         COALESCE(SUM(completion_tokens), 0)             AS completion_tokens,
         COALESCE(SUM(reasoning_tokens), 0)              AS reasoning_tokens,
         COALESCE(SUM(total_tokens), 0)                  AS total_tokens,
         COALESCE(SUM(duration_ms), 0)                   AS sum_duration_ms,
         COUNT(duration_ms)                              AS count_duration_ms,
         COALESCE(SUM(duration_to_first_byte_ms), 0)     AS sum_ttfb_ms,
         COUNT(duration_to_first_byte_ms)                AS count_ttfb_ms,
         COALESCE(SUM(uncached_cost), 0)                 AS total_list_cost
  FROM http_analytics
  WHERE fusillade_batch_id IS NOT NULL
    AND user_id IS NOT NULL
    -- Exclude the system user, matching the live fold which skips Uuid::nil().
    AND user_id <> '00000000-0000-0000-0000-000000000000'
    AND status_code BETWEEN 200 AND 299
  GROUP BY fusillade_batch_id;
CREATE INDEX ON _bf_analytics (fbid);

-- Batches WITH retained data.
UPDATE batch_aggregates ba
   SET total_requests          = b.total_requests,
       total_prompt_tokens     = b.prompt_tokens,
       total_completion_tokens = b.completion_tokens,
       total_reasoning_tokens  = b.reasoning_tokens,
       total_tokens            = b.total_tokens,
       sum_duration_ms         = b.sum_duration_ms,
       count_duration_ms       = b.count_duration_ms,
       sum_ttfb_ms             = b.sum_ttfb_ms,
       count_ttfb_ms           = b.count_ttfb_ms,
       total_list_cost         = b.total_list_cost,
       analytics_backfilled_at = NOW()
  FROM _bf_analytics b
 WHERE b.fbid = ba.fusillade_batch_id
   AND ba.analytics_backfilled_at IS NULL
   AND ba.updated_at < NOW() - INTERVAL '${QUIESCENT_INTERVAL}';

-- Eligible batches with NO retained http_analytics (aged out / all-non-2xx) -> zeros.
UPDATE batch_aggregates ba
   SET total_requests = 0, total_prompt_tokens = 0, total_completion_tokens = 0,
       total_reasoning_tokens = 0, total_tokens = 0, sum_duration_ms = 0,
       count_duration_ms = 0, sum_ttfb_ms = 0, count_ttfb_ms = 0, total_list_cost = 0,
       analytics_backfilled_at = NOW()
 WHERE ba.analytics_backfilled_at IS NULL
   AND ba.updated_at < NOW() - INTERVAL '${QUIESCENT_INTERVAL}'
   AND NOT EXISTS (SELECT 1 FROM _bf_analytics b WHERE b.fbid = ba.fusillade_batch_id);

COMMIT;
SQL

echo "backfill_batch_analytics_denorm: DONE (re-run to pick up batches that have since quiesced)"
