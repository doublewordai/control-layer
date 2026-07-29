#!/usr/bin/env bash
#
# Backfill http_analytics.finish_reason from the response bodies fusillade already stores
# (migration 125).
#
# Purpose: `finish_reason = 'tool_calls'` is the only signal we have that a caller is
# running a CLIENT-side tool loop. The client executes the tool itself and sends a fresh
# request, so the follow-up arrives with its own fusillade_request_id and is otherwise
# indistinguishable from an ordinary multi-turn message. The batcher sets the column at
# INSERT going forward; this fills history. ('length' vs 'stop' also identifies truncated
# generations, which has so far had to be inferred.)
#
# NOT related to tool_iterations, which counts how many internal inference steps a
# SERVER-side tool loop drove for one user request. The two are complementary.
#
# ##############################################################################
# # READ THIS FIRST: this backfill is visible in ClickHouse, and not harmlessly. #
# ##############################################################################
#
# `clay.http_analytics` is a SharedReplacingMergeTree(_peerdb_version), and the bi repo
# reads it RAW -- no FINAL, no `_current` view -- on the documented grounds that it is
# append-only and therefore carries no duplicate row versions.
#
# An UPDATE breaks that assumption. Every row this script touches arrives over CDC as a
# NEW row version, and until a background merge collapses it, every query over the
# affected window COUNTS THAT ROW TWICE. That is not hypothetical: the same failure on
# `clay.users` reported Total Credits Spent as 438,948 against a true 231,868 (+89%), and
# two refreshes 20s apart disagreed by 200k credits.
#
# 30 days is ~37M rows. Backfilling all of it at once means up to 37M duplicate versions
# and materially wrong usage/cost/persona dashboards until merges catch up.
#
# So:
#   * Run DRY_RUN=1 first and look at the row count. It is usually far smaller than the
#     window size -- only rows whose stored body actually yields a finish_reason are
#     written -- and that number is what lands in ClickHouse, not the 37M.
#   * Prefer several narrow runs (a few days each) over one wide one, so the merge backlog
#     stays small and any over-reporting window is short.
#   * Tell whoever reads the dashboards. Or have bi read the affected cards with FINAL for
#     the duration.
#   * Afterwards you can force the collapse from ClickHouse:
#         OPTIMIZE TABLE clay.http_analytics FINAL;      -- expensive; per-partition is kinder
#
# --- Not advancing the WAL faster than the ClickPipe can drain it ---
# The pipe consumes a logical replication slot. Bulk UPDATEs generate WAL far faster than
# a normal request load, and if the slot's confirmed_flush_lsn falls behind, Postgres
# RETAINS every WAL segment since -- which on Neon means growing storage, and past some
# point a stalled or dropped pipe. So before each window this script measures the retained
# WAL on the furthest-behind logical slot and, if it exceeds MAX_SLOT_LAG_BYTES, stops
# issuing work and waits for the pipe to drain below RESUME_SLOT_LAG_BYTES. It is a
# throttle, not a check: the sweep paces itself to whatever the pipe can actually keep up
# with, and a slow pipe makes the backfill slower rather than making it dangerous.
#
# --- Why sweep id windows (COR-524e lesson) ---
# Driving this from fusillade.requests, or as one big UPDATE, means tens of millions of
# random reads into a 176M-row table; on Neon those are cold pageserver fetches and the run
# projects to many hours, with a single long transaction that loses all progress on a blip.
# Instead we sweep contiguous http_analytics.id ranges: each window scans http_analytics
# SEQUENTIALLY by pk (Neon's prefetch fast path), probes fusillade.requests by its pk, and
# commits on its own. Bounded, resumable, blip-safe.
#
# --- Why a regex and not `response_body::jsonb` ---
# Cheaper and strictly more capable. `response_body` is TEXT and holds one of two shapes:
# a JSON object for a non-streamed response, or the raw SSE transcript
# ("data: {...}\ndata: {...}\ndata: [DONE]") for a streamed one -- and the SSE shape is not
# valid JSON, so a cast raises and would abort the whole window. The regex reads both, and
# it self-selects the terminal frame for free: the non-final stream frames carry
# "finish_reason":null, which is unquoted and so cannot match `"([a-z_]+)"`. Malformed or
# truncated bodies simply yield no match and are skipped rather than erroring.
#
# Idempotent + resumable: guarded by `finish_reason IS NULL`; safe to re-run. Resume from
# the last printed `ha.id<` via START_ID.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_finish_reason.sh
#   DRY_RUN=1 DATABASE_URL=...   ./scripts/backfill_finish_reason.sh    # count only, no writes
# Optional env:
#   DAYS                   how far back to backfill (default 30). Resolved to a START_ID.
#   WINDOW                 http_analytics.id ids scanned per transaction (default 50000)
#   SLEEP_SECONDS          pause between windows (default 0.1)
#   START_ID / MAX_ID      explicit id bounds; override DAYS
#   DRY_RUN                1 = report what would be written, write nothing
#   MAX_SLOT_LAG_BYTES     pause when the furthest-behind logical slot retains more than
#                          this (default 2147483648 = 2 GiB)
#   RESUME_SLOT_LAG_BYTES  resume once it drains below this (default 1073741824 = 1 GiB)
#   LAG_POLL_SECONDS       how often to re-check while paused (default 10)

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the dwctl database (must also expose the fusillade schema)}"
WINDOW="${WINDOW:-50000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.1}"
DAYS="${DAYS:-30}"
DRY_RUN="${DRY_RUN:-0}"
MAX_SLOT_LAG_BYTES="${MAX_SLOT_LAG_BYTES:-2147483648}"
RESUME_SLOT_LAG_BYTES="${RESUME_SLOT_LAG_BYTES:-1073741824}"
LAG_POLL_SECONDS="${LAG_POLL_SECONDS:-10}"

psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

# Retained WAL on the furthest-behind logical slot, in bytes. 0 when there is no logical
# slot on this database -- which also means nothing here is feeding a ClickPipe, so the
# throttle has nothing to protect and correctly does not engage.
slot_lag_bytes() {
  psql_q "
    SELECT coalesce(max(pg_wal_lsn_diff(pg_current_wal_lsn(), confirmed_flush_lsn))::bigint, 0)
    FROM pg_replication_slots
    WHERE slot_type = 'logical' AND confirmed_flush_lsn IS NOT NULL;"
}

human_gib() { awk -v b="$1" 'BEGIN { printf "%.2f GiB", b / 1073741824 }'; }

# Block until the pipe has caught up. Called before each window, so the sweep paces itself
# to the slowest consumer instead of burying it.
await_pipe() {
  local lag
  lag="$(slot_lag_bytes)"
  [ "$lag" -le "$MAX_SLOT_LAG_BYTES" ] && return 0
  echo "  ! replication slot retains $(human_gib "$lag") (> $(human_gib "$MAX_SLOT_LAG_BYTES")) — pausing for the ClickPipe" >&2
  while [ "$lag" -gt "$RESUME_SLOT_LAG_BYTES" ]; do
    sleep "$LAG_POLL_SECONDS"
    lag="$(slot_lag_bytes)"
    printf '    …still %s\n' "$(human_gib "$lag")" >&2
  done
  echo "  ✓ drained to $(human_gib "$lag") — resuming" >&2
}

# Terminal finish_reason out of a stored body, JSON or SSE. See the header.
FR_EXPR="substring(r.response_body from '\"finish_reason\"\\s*:\\s*\"([a-z_]+)\"')"

if [ -z "${START_ID:-}" ]; then
  START_ID="$(psql_q "SELECT coalesce(min(id), 0) FROM http_analytics WHERE timestamp >= now() - interval '${DAYS} days'")"
fi
MAX_ID="${MAX_ID:-$(psql_q 'SELECT coalesce(max(id), 0) FROM http_analytics')}"

echo "backfill_finish_reason: ha.id windows [${START_ID}, ${MAX_ID}]  window=${WINDOW}  sleep=${SLEEP_SECONDS}s  dry_run=${DRY_RUN}"
echo "  slot throttle: pause above $(human_gib "$MAX_SLOT_LAG_BYTES"), resume below $(human_gib "$RESUME_SLOT_LAG_BYTES")"
echo "  starting slot lag: $(human_gib "$(slot_lag_bytes)")"
[ "$DRY_RUN" = "1" ] && echo "  DRY RUN — counting only, nothing written"

lo="$START_ID"
total=0
SECONDS=0
while [ "$lo" -le "$MAX_ID" ]; do
  hi=$(( lo + WINDOW ))
  await_pipe

  if [ "$DRY_RUN" = "1" ]; then
    # Same predicate as the write, so the count is exactly what a real run would touch --
    # i.e. exactly how many duplicate row versions ClickHouse would have to merge away.
    n=$(psql_q "
      SELECT count(*)
        FROM http_analytics ha
        JOIN fusillade.requests r ON r.id = ha.fusillade_request_id
       WHERE ha.id >= ${lo} AND ha.id < ${hi}
         AND ha.finish_reason IS NULL
         AND ha.fusillade_request_id IS NOT NULL
         AND r.response_body IS NOT NULL
         AND ${FR_EXPR} IS NOT NULL;")
  else
    n=$(psql_q "
      WITH upd AS (
        UPDATE http_analytics ha
           SET finish_reason = ${FR_EXPR}
          FROM fusillade.requests r
         WHERE r.id = ha.fusillade_request_id
           AND ha.id >= ${lo} AND ha.id < ${hi}
           AND ha.finish_reason IS NULL
           AND ha.fusillade_request_id IS NOT NULL
           AND r.response_body IS NOT NULL
           AND ${FR_EXPR} IS NOT NULL
        RETURNING 1)
      SELECT count(*) FROM upd;")
  fi

  total=$(( total + n ))
  printf '  ha.id<%-12s  %s +%-7s  (total %s)  [%ds]\n' \
    "$hi" "$([ "$DRY_RUN" = "1" ] && echo would-set || echo set)" "$n" "$total" "$SECONDS"
  lo="$hi"
  sleep "${SLEEP_SECONDS}"
done

echo "backfill_finish_reason: DONE"
printf '  rows %s : %s\n' "$([ "$DRY_RUN" = "1" ] && echo 'that would be set' || echo 'set             ')" "$total"
printf '  duration            : %dm %02ds\n' $(( SECONDS / 60 )) $(( SECONDS % 60 ))
printf '  final slot lag      : %s\n' "$(human_gib "$(slot_lag_bytes)")"
if [ "$DRY_RUN" != "1" ] && [ "$total" -gt 0 ]; then
  echo "  ${total} rows are now duplicated in clay.http_analytics until ClickHouse merges them." >&2
  echo "  Usage/cost/persona cards over-report over the affected window meanwhile — see the header." >&2
fi
