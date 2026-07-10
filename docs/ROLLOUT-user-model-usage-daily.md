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

- The **refresh daemon** only folds rows with `id > last_processed_id` (i.e. > the watermark) —
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

Then validate — daily must match **raw `http_analytics`**, the source of truth, per
(user, model):
```bash
psql "$DATABASE_URL" -f scripts/validate_user_model_usage_daily.sql
```
Expect zero rows, or a few tiny diffs on active users (the in-flight tail between the daemon
cursor and the scan). Large diffs ⇒ investigate; do **not** cut over.

> **Do not validate against the old `user_model_usage` table.** It is a long-lived
> accumulator (since migration 069) that has drifted *upward* from what `http_analytics`
> actually contains — measured ~13% high in aggregate, and up to ~25× on individual
> (user, model) pairs — from historical double-counting. The daily rollup, rebuilt from raw,
> is the *more correct* number; the old table is not a trustworthy baseline. (This drift is
> also why the cutover lowers displayed all-time usage — see Deploy 2.)

> **Scope the raw comparison to the retention window.** Once `http_analytics` retention is
> cut (COR-509), raw only covers recent days, so daily-vs-raw can only be validated for
> `usage_date >= now() - <retention window>`. History before that is validated at backfill
> time (while raw is still complete) and thereafter trusted as immutable. The validate
> script bounds its raw scan to this window.

### Deploy 2 — cutover
- Repoint all reads (all-time `get_user_model_breakdown`, range
  `get_user_model_breakdown_for_range`) to `user_model_usage_daily`; repoint
  `get_user_batch_count_for_range` to `batch_aggregates`.
- Remove the inline `refresh_user_model_usage` call; gate any inline daily refresh behind
  `?refresh=true`. Reads otherwise rely on the daemon's forward-fill (eventually consistent,
  sub-second under load).
- **Summable usage** (tokens, cost, request/batch counts) comes from the rollups. **Non-summable
  stats stay on raw `http_analytics`** within the retention window: latency p95/p99, status-code
  breakdowns, and the intra-day time series. These are already retention-bounded and are not
  part of this cutover.
- **Granularity change:** the rollup is keyed by UTC `usage_date`, so date-range usage reads
  become **UTC-day-granular** (a range is `usage_date BETWEEN start::date AND end::date`).
  Sub-day ranges collapse to whole days — the UI is updated to present day-level granularity
  (control-layer `dashboard/` and `app-doubleword-private/`).
- **Keep** the `user_model_usage` table (still read by not-yet-replaced old pods during the
  rolling deploy; harmless once they're gone).

> **Billing sign-off required before merge.** Moving the all-time read off the (inflated) old
> table onto the (correct) daily rollup **lowers displayed all-time usage ~13% in aggregate**,
> concentrated on specific (user, model) pairs. It is more accurate, but it is user-visible and
> billing-adjacent — get explicit sign-off and decide whether to notify affected accounts.

### Deploy 3 — contract
- Migration 111: `DROP TABLE user_model_usage; DROP TABLE user_model_usage_cursor;` — only
  after every pod is on the Deploy-2 read path.
- Drop the `user_model_usage_daily_backfill_progress` table (ops leftover from the backfill).

## Rollback
- Before Deploy 2: nothing user-facing changed — disable the daemon
  (`usage_refresh.enabled=false`) and/or drop the daily table; reads still use the old rollup.
- After Deploy 2, before Deploy 3: revert the read repoint; `user_model_usage` still exists
  and is still current on old-path pods.
