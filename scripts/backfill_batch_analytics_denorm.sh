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
# The per-batch aggregate rides idx_analytics_fusillade_batch_id, so run this BEFORE that
# index is dropped (the contract PR) and after migration 116 + the batcher change are
# deployed to every pod.
#
# Safety (absolute SET, not +=): the batcher fold is additive, so this script only touches
# QUIESCENT batches — `updated_at` older than QUIESCENT_INTERVAL — for which no fold can be
# running concurrently. Those are exactly the batches whose totals are final (a batch is a
# bounded, short-lived unit of work). A batch still active at deploy is skipped now and
# backfilled on a later pass once it quiesces; re-running is safe (see idempotency).
#
# Idempotent + resumable: guarded by `analytics_backfilled_at IS NULL`, swept by max_seq in
# fixed ranges, each its own committed transaction. Re-run until it reports 0 rows to catch
# batches that were still hot on earlier passes.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_batch_analytics_denorm.sh
# Optional env:
#   BATCH_SIZE          max_seq-range width swept per transaction (default 50000)
#   SLEEP_SECONDS       pause between ranges (default 0.1)
#   QUIESCENT_INTERVAL  only backfill batches idle at least this long (default '2 hours').
#                       Must exceed the longest possible gap between folds of one batch;
#                       raise it (e.g. '25 hours' > the 24h batch SLA) for a single
#                       guaranteed-final pass.

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the credits/http_analytics database}"
BATCH_SIZE="${BATCH_SIZE:-50000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.1}"
QUIESCENT_INTERVAL="${QUIESCENT_INTERVAL:-2 hours}"

psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

MAX_SEQ=$(psql_q "SELECT COALESCE(MAX(max_seq), 0) FROM batch_aggregates;")
if [ -z "$MAX_SEQ" ] || [ "$MAX_SEQ" -eq 0 ]; then
  echo "backfill_batch_analytics_denorm: no batch_aggregates rows; nothing to do" >&2
  exit 0
fi

echo "backfill_batch_analytics_denorm: sweeping max_seq (0, ${MAX_SEQ}]  batch=${BATCH_SIZE}  sleep=${SLEEP_SECONDS}s  quiescent='${QUIESCENT_INTERVAL}'"

CURSOR=0
total_rows=0
batches=0
SECONDS=0
while [ "$CURSOR" -lt "$MAX_SEQ" ]; do
  hi=$(( CURSOR + BATCH_SIZE ))
  if [ "$hi" -gt "$MAX_SEQ" ]; then hi="$MAX_SEQ"; fi
  affected=$(psql_q "
    WITH upd AS (
      UPDATE batch_aggregates ba
         SET total_requests          = s.total_requests,
             total_prompt_tokens     = s.prompt_tokens,
             total_completion_tokens = s.completion_tokens,
             total_reasoning_tokens  = s.reasoning_tokens,
             total_tokens            = s.total_tokens,
             sum_duration_ms         = s.sum_duration_ms,
             count_duration_ms       = s.count_duration_ms,
             sum_ttfb_ms             = s.sum_ttfb_ms,
             count_ttfb_ms           = s.count_ttfb_ms,
             total_list_cost         = s.list_cost,
             analytics_backfilled_at = NOW()
        FROM LATERAL (
               SELECT
                 COUNT(*)                                            AS total_requests,
                 COALESCE(SUM(ha.prompt_tokens), 0)                   AS prompt_tokens,
                 COALESCE(SUM(ha.completion_tokens), 0)               AS completion_tokens,
                 COALESCE(SUM(ha.reasoning_tokens), 0)                AS reasoning_tokens,
                 COALESCE(SUM(ha.total_tokens), 0)                    AS total_tokens,
                 COALESCE(SUM(ha.duration_ms), 0)                     AS sum_duration_ms,
                 COUNT(ha.duration_ms)                                AS count_duration_ms,
                 COALESCE(SUM(ha.duration_to_first_byte_ms), 0)       AS sum_ttfb_ms,
                 COUNT(ha.duration_to_first_byte_ms)                  AS count_ttfb_ms,
                 COALESCE(SUM(ha.uncached_cost), 0)                   AS list_cost
               FROM http_analytics ha
               WHERE ha.fusillade_batch_id = ba.fusillade_batch_id
                 AND ha.user_id IS NOT NULL
                 AND ha.status_code BETWEEN 200 AND 299
             ) s
       WHERE ba.max_seq > ${CURSOR} AND ba.max_seq <= ${hi}
             AND ba.analytics_backfilled_at IS NULL
             AND ba.updated_at < NOW() - INTERVAL '${QUIESCENT_INTERVAL}'
      RETURNING 1
    )
    SELECT count(*) FROM upd;")
  total_rows=$(( total_rows + affected ))
  batches=$(( batches + 1 ))
  CURSOR="$hi"
  printf '  max_seq<=%-12s  rows +%-6s  (total %s)\n' "$hi" "$affected" "$total_rows"
  sleep "${SLEEP_SECONDS}"
done

elapsed=$SECONDS
echo "backfill_batch_analytics_denorm: DONE"
printf '  batch rows updated : %s\n' "$total_rows"
printf '  duration           : %dm %02ds\n' $(( elapsed / 60 )) $(( elapsed % 60 ))
printf '  ranges             : %s  (BATCH_SIZE=%s, SLEEP_SECONDS=%s)\n' "$batches" "$BATCH_SIZE" "$SLEEP_SECONDS"
echo "  re-run until 'rows +0' throughout to catch batches that were hot on this pass" >&2
