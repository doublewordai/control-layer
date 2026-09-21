#!/usr/bin/env bash
#
# Purge expired Underway background tasks in bounded batches.
#
# Underway tasks carry a `ttl` (14 days by default) but nothing deletes finished
# tasks unless a deletion routine runs. The in-process task-retention daemon
# (background_services.task_retention) keeps the table trimmed going forward;
# this script is for the one-off drain of a large existing backlog, where the
# daemon's small batches would take a long time to catch up.
#
# What it deletes, oldest first, one short transaction per batch:
#   * tasks that are not `in_progress`
#   * created more than MIN_AGE_DAYS ago (bounds the index range scan)
#   * whose own `created_at + ttl` has passed
# Optionally restricted to QUEUES (comma-separated queue names), e.g. queues
# that no release consumes any more.
#
# `task_attempt` rows go with their task through the schema's ON DELETE CASCADE.
#
# Safe to run alongside live workers: victims are selected FOR UPDATE SKIP
# LOCKED, each batch commits on its own, and a lock/statement timeout makes a
# blocked batch fail fast instead of queueing behind other work. Safe to stop
# and rerun at any point; progress is the rows already gone.
#
# Requires the idx_task_created_at index (shipped with the retention daemon).
# The script checks for it and refuses to run without it, because without it
# every batch sorts the whole task history.
#
# Space is returned to the operating system only by a later VACUUM (FULL) or
# pg_repack; a plain autovacuum makes the freed pages reusable for new rows.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/purge_underway_tasks.sh
# Optional env:
#   BATCH_SIZE      rows per batch (default 2000)
#   SLEEP_SECONDS   pause between batches (default 0.2)
#   MIN_AGE_DAYS    ignore tasks younger than this (default 14)
#   QUEUES          comma-separated queue names to restrict to (default: all)
#   MAX_BATCHES     stop after this many batches (default: unlimited)
#   LOCK_TIMEOUT    per-batch lock timeout (default 5s)
#   STATEMENT_TIMEOUT per-batch statement timeout (default 60s)
#   DRY_RUN=1       report how many rows qualify and exit

set -euo pipefail

: "${DATABASE_URL:?DATABASE_URL is required}"
BATCH_SIZE="${BATCH_SIZE:-2000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.2}"
MIN_AGE_DAYS="${MIN_AGE_DAYS:-14}"
QUEUES="${QUEUES:-}"
MAX_BATCHES="${MAX_BATCHES:-0}"
LOCK_TIMEOUT="${LOCK_TIMEOUT:-5s}"
STATEMENT_TIMEOUT="${STATEMENT_TIMEOUT:-60s}"
DRY_RUN="${DRY_RUN:-0}"

psql_q() {
    psql "$DATABASE_URL" -X -q -v ON_ERROR_STOP=1 -At "$@"
}

queue_filter=""
if [[ -n "$QUEUES" ]]; then
    # 'a,b' -> 'a','b'
    list=$(printf "'%s'," "${QUEUES//,/\' \'}" | sed "s/' '/','/g; s/,$//")
    queue_filter="AND task_queue_name IN ($list)"
fi

predicate="state <> 'in_progress'
      AND created_at < now() - make_interval(days => ${MIN_AGE_DAYS})
      AND created_at + ttl < now()
      ${queue_filter}"

if [[ "$(psql_q -c "SELECT to_regclass('underway.idx_task_created_at') IS NOT NULL")" != "t" ]]; then
    echo "underway.idx_task_created_at is missing; deploy the release that adds it before purging" >&2
    exit 1
fi

if [[ "$DRY_RUN" == "1" ]]; then
    echo "Rows qualifying for deletion (may take a while on a large table):"
    psql_q -c "SELECT task_queue_name, state, count(*) FROM underway.task WHERE ${predicate} GROUP BY 1, 2 ORDER BY 3 DESC"
    exit 0
fi

echo "Purging expired underway tasks: batch=${BATCH_SIZE} sleep=${SLEEP_SECONDS}s min_age=${MIN_AGE_DAYS}d queues=${QUEUES:-all}"

total=0
batch=0
while :; do
    batch=$((batch + 1))
    deleted=$(psql_q -c "SET lock_timeout = '${LOCK_TIMEOUT}'" \
                     -c "SET statement_timeout = '${STATEMENT_TIMEOUT}'" \
                     -c "WITH victims AS (
                             SELECT task_queue_name, id
                             FROM underway.task
                             WHERE ${predicate}
                             ORDER BY created_at
                             LIMIT ${BATCH_SIZE}
                             FOR UPDATE SKIP LOCKED
                         ), gone AS (
                             DELETE FROM underway.task t
                             USING victims v
                             WHERE t.task_queue_name = v.task_queue_name AND t.id = v.id
                             RETURNING 1
                         )
                         SELECT count(*) FROM gone" | tail -n 1)
    total=$((total + deleted))
    printf '%s batch %d: deleted %d (total %d)\n' "$(date -u +%FT%TZ)" "$batch" "$deleted" "$total"
    if (( deleted < BATCH_SIZE )); then
        echo "Done: nothing left to purge (total ${total})"
        break
    fi
    if (( MAX_BATCHES > 0 && batch >= MAX_BATCHES )); then
        echo "Stopping after ${MAX_BATCHES} batches (total ${total}); rerun to continue"
        break
    fi
    sleep "$SLEEP_SECONDS"
done
