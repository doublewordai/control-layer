# Rollout: `user_model_usage_daily` (COR-506)

Replaces `user_model_usage` with a per-day rollup (`user_model_usage_daily`) that serves
both all-time and date-range `/usage` reads, so raw `http_analytics` is never scanned for
usage. This is billing-adjacent data, so the cutover is staged (expand → backfill →
cutover → contract) and the all-time read is never empty mid-flight.

Why staged: on a rolling deploy, old and new pods run at once. So (a) the read cutover must
not drop the old table in the same release, and (b) the daily table must be fully populated
(daemon forward-fill + one-off backfill) before any read points at it.

## The key invariant

Migration 110 seeds `user_model_usage_daily_cursor` with `last_processed_id = MAX(id)` and
an immutable `backfill_watermark = MAX(id)` (captured atomically at migration time).

- The **refresh daemon** only folds rows with `id > last_processed_id` (≥ the watermark) —
  forward-fill, no historical scan.
- The **backfill** only fills `id <= backfill_watermark` — history.

Disjoint id-ranges ⇒ the daemon and the backfill can run concurrently with no double-count
(a day straddling the watermark gets each side's slice summed by the additive upsert). This
is why the daemon does **not** need to be disabled during the backfill.

## Sequence

### Deploy 1 — expand (this PR)
- Migration 110: create `user_model_usage_daily` + cursor/watermark (does **not** drop
  `user_model_usage`).
- `refresh_user_model_usage_daily` + the in-process refresh daemon
  (`background_services.usage_refresh`, default enabled) + the analytics-batcher nudge.
- Reads unchanged; `user_model_usage` still maintained by the existing inline refresh.

Result: the daily table is dark (written forward from the watermark, not yet read). The
daemon running here is safe — it only touches `id > watermark`.

> If you'd rather run the backfill with zero concurrent daemon writes, ship this release
> with `background_services.usage_refresh.enabled=false` and enable it at Deploy 2. Not
> required — the watermark makes concurrent operation correct.

### Backfill (manual, between Deploy 1 and Deploy 2)
```bash
DATABASE_URL=<prod>  ./scripts/backfill_user_model_usage_daily.sh
# tune throughput/lag: BATCH_SIZE (default 20000), SLEEP_SECONDS (default 0.1)
```
Chunked PK id-sweep bounded by the watermark, each batch its own transaction, throttled,
idempotent + resumable (durable progress cursor). Safe to run while the app is live.

Then validate — daily sums must match the old rollup per (user, model):
```bash
psql "$DATABASE_URL" -f scripts/validate_user_model_usage_daily.sql
```
Expect zero rows, or a few tiny diffs on active users (the in-flight tail between the two
cursors). Large diffs ⇒ investigate; do **not** cut over.

### Deploy 2 — cutover
- Repoint all reads (all-time `get_user_model_breakdown`, range
  `get_user_model_breakdown_for_range`) to `user_model_usage_daily`; repoint
  `get_user_batch_count_for_range` to `batch_aggregates`.
- Remove the inline `refresh_user_model_usage` call; gate any inline daily refresh behind
  `?refresh=true`.
- **Keep** the `user_model_usage` table (still read by not-yet-replaced old pods during the
  rolling deploy; harmless once they're gone).

### Deploy 3 — contract
- Migration 111: `DROP TABLE user_model_usage; DROP TABLE user_model_usage_cursor;` — only
  after every pod is on the Deploy-2 read path.
- Drop the `user_model_usage_daily_backfill_progress` table (ops leftover from the backfill).

## Rollback
- Before Deploy 2: nothing user-facing changed — disable the daemon
  (`usage_refresh.enabled=false`) and/or drop the daily table; reads still use the old rollup.
- After Deploy 2, before Deploy 3: revert the read repoint; `user_model_usage` still exists
  and is still current on old-path pods.
