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
