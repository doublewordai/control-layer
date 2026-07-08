#!/usr/bin/env bash
#
# Backfill user_model_usage_daily from http_analytics history (COR-506, migration 110).
#
# Context: migration 110 adds the per-day usage rollup and seeds
# user_model_usage_daily_cursor.last_processed_id / .backfill_watermark to MAX(id) at
# migration time. The live refresh daemon folds only rows with id > last_processed_id
# (i.e. > the watermark) forward; this script fills the history behind the watermark
# (id <= backfill_watermark). The two id-ranges are disjoint, so this is safe to run
# while the app + daemon are live — a day straddling the watermark gets each side's
# slice summed by the additive upsert.
#
# Side-script, not a migration: aggregating all of http_analytics in one statement would
# hold a single long transaction and bloat. Here we sweep the PRIMARY KEY (id) in fixed
# ranges, each its own committed transaction, so locks/bloat stay small and
# replication/autovacuum keep up. Same shape as backfill_uncached_cost.sh.
#
# Idempotent + resumable: unlike a per-row UPDATE, an aggregating upsert is NOT safe to
# re-apply to the same rows (it would double-count). So each batch advances a DURABLE
# progress cursor (user_model_usage_daily_backfill_progress.last_swept_id) in the SAME
# statement as the upsert (a data-modifying CTE, executed to completion regardless of the
# final SELECT). A crashed batch rolls back both together; a re-run resumes from the last
# committed id — every id-range is applied exactly once. Re-running after completion is a
# no-op (nothing left in (cursor, watermark]).
#
# Day bucketing is UTC ((timestamp AT TIME ZONE 'UTC')::date), matching
# refresh_user_model_usage_daily — do NOT let the psql session timezone change it.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_user_model_usage_daily.sh
# Optional env:
#   BATCH_SIZE     id-range width swept per batch (default 20000)
#   SLEEP_SECONDS  pause between batches — throttles WAL volume / replication lag (default 0.1)

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the http_analytics database}"
BATCH_SIZE="${BATCH_SIZE:-20000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.1}"

# `-v ON_ERROR_STOP=1` makes psql exit non-zero on a SQL error (so set -e trips), and `-X`
# ignores ~/.psqlrc so a user's config can't alter behaviour. Applied to every call.
psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

# Durable progress cursor for this backfill. Created here, not in the migration: it is ops
# state for a one-off, not part of the schema. Single-row, mirrors the daemon's cursor shape.
psql_q "CREATE TABLE IF NOT EXISTS user_model_usage_daily_backfill_progress (
          id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id = TRUE),
          last_swept_id BIGINT NOT NULL DEFAULT 0,
          updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW());" >/dev/null
psql_q "INSERT INTO user_model_usage_daily_backfill_progress (last_swept_id)
        VALUES (0) ON CONFLICT (id) DO NOTHING;" >/dev/null

WATERMARK=$(psql_q "SELECT backfill_watermark FROM user_model_usage_daily_cursor WHERE id = TRUE;")
CURSOR=$(psql_q "SELECT last_swept_id FROM user_model_usage_daily_backfill_progress WHERE id = TRUE;")

if [ -z "$WATERMARK" ]; then
  echo "backfill_user_model_usage_daily: no watermark found (is migration 110 applied?)" >&2
  exit 1
fi

echo "backfill_user_model_usage_daily: sweeping id (${CURSOR}, ${WATERMARK}]  batch=${BATCH_SIZE}  sleep=${SLEEP_SECONDS}s"

total_rows=0
batches=0
SECONDS=0   # bash builtin: wall-clock seconds since reset
while [ "$CURSOR" -lt "$WATERMARK" ]; do
  hi=$(( CURSOR + BATCH_SIZE ))
  if [ "$hi" -gt "$WATERMARK" ]; then hi="$WATERMARK"; fi
  # One batch = one committed transaction (a single statement). The `agg` CTE upserts the
  # (user, model, UTC-day) slice for this id-range; the `prog` CTE advances the durable
  # cursor. Both are data-modifying CTEs, so both run to completion atomically; the final
  # SELECT just reports how many rollup rows the upsert touched.
  affected=$(psql_q "
    WITH agg AS (
      INSERT INTO user_model_usage_daily (user_id, model, usage_date, input_tokens, output_tokens, cost, request_count)
      SELECT user_id,
             model,
             (timestamp AT TIME ZONE 'UTC')::date,
             COALESCE(SUM(prompt_tokens), 0),
             COALESCE(SUM(completion_tokens), 0),
             COALESCE(SUM(total_cost), 0),
             COUNT(*)
      FROM http_analytics
      WHERE id > ${CURSOR} AND id <= ${hi}
            AND user_id IS NOT NULL AND model IS NOT NULL
            AND status_code BETWEEN 200 AND 299
      GROUP BY user_id, model, (timestamp AT TIME ZONE 'UTC')::date
      ON CONFLICT (user_id, model, usage_date) DO UPDATE SET
          input_tokens  = user_model_usage_daily.input_tokens  + EXCLUDED.input_tokens,
          output_tokens = user_model_usage_daily.output_tokens + EXCLUDED.output_tokens,
          cost          = user_model_usage_daily.cost          + EXCLUDED.cost,
          request_count = user_model_usage_daily.request_count + EXCLUDED.request_count,
          updated_at    = NOW()
      RETURNING 1
    ), prog AS (
      UPDATE user_model_usage_daily_backfill_progress
         SET last_swept_id = ${hi}, updated_at = NOW()
       WHERE id = TRUE
    )
    SELECT count(*) FROM agg;")
  total_rows=$(( total_rows + affected ))
  batches=$(( batches + 1 ))
  CURSOR="$hi"
  printf '  id<=%-12s  rollup rows +%-6s  (total %s)\n' "$hi" "$affected" "$total_rows"
  sleep "${SLEEP_SECONDS}"
done

elapsed=$SECONDS
echo "backfill_user_model_usage_daily: DONE"
printf '  rollup rows written : %s\n' "$total_rows"
printf '  duration            : %dm %02ds (wall-clock, incl. throttle sleeps)\n' $(( elapsed / 60 )) $(( elapsed % 60 ))
printf '  batches             : %s  (BATCH_SIZE=%s, SLEEP_SECONDS=%s)\n' "$batches" "$BATCH_SIZE" "$SLEEP_SECONDS"
printf '  id swept            : up to %s (watermark)\n' "$WATERMARK"
echo
echo "Validate before cutover — daily SUM vs the old all-time rollup should match per (user, model):"
echo "  psql \"\$DATABASE_URL\" -f scripts/validate_user_model_usage_daily.sql"
