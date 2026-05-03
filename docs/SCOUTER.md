# SCOUTER

## Recent Change: Pending Queue Counts by Model + Completion Window

- Added a new storage API to report queued request depth grouped by model and completion window (priority).
- API: `Storage::get_pending_request_counts_by_model_and_completion_window() -> Result<HashMap<String, HashMap<String, i64>>>`
  - Outer key: `model`
  - Inner key: `completion_window`
  - Value: count of matching requests
- Postgres implementation (`src/manager/postgres.rs`) runs a `GROUP BY (requests.model, batches.completion_window)` over pending requests with filters:
  - `requests.state = 'pending'`
  - `requests.is_escalated = false` (ignore escalated racing requests)
  - `requests.template_id IS NOT NULL`
  - `batches.cancelling_at IS NULL`
- Added integration coverage in `tests/integration.rs` to verify:
  - Claimed requests are excluded
  - Escalated requests are excluded
  - Counts are correct across multiple models and completion windows

## Fairness-aware Mode (`?fairness=true`)

The endpoint accepts an optional `fairness` query parameter:

- `GET /admin/api/v1/monitoring/pending-request-counts` — raw pending counts (default).
- `GET /admin/api/v1/monitoring/pending-request-counts?fairness=true` — counts with each user's contribution capped at the bucket's average.

### Policy

For each `(model, completion_window)` bucket:

- `p_u` = pending count for user `u` (only users with `p_u > 0`).
- `U` = number of such users.
- `T` = `sum(p_u)`.
- `fair_share` = `max(1, ceil(T / U))`.
- `effective(u)` = `min(p_u, fair_share)`.
- `effective_count` = `sum(effective(u))`.

Properties:

- Reduces to `T` when demand is balanced across users.
- Caps a single dominant user's contribution at the bucket average so the
  reported depth does not over-state demand a per-user fair scheduler will
  throttle.
- Stateless wrt in-flight counts — pure function of pending rows.

### Contract with `claim_requests`

The fairness-aware count is intended to stay directionally aligned with the
per-user fairness ordering applied by `fusillade::Storage::claim_requests`.
Both call sites carry a comment block referencing each other; the integration
test `test_effective_counts_track_claim_order` in `fusillade` will fail if
the policies drift in opposite directions.

If you change either policy:

1. Update the SQL in both `claim_requests` and
   `get_effective_pending_request_counts_by_model_and_window`.
2. Update the policy block in this doc.
3. Re-run the drift-detector test.
