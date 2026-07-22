-- Shared window-validity helpers for API-key spending caps.
--
-- Cap windows are CALENDAR-ALIGNED (UTC), not rolling: a 'daily' cap resets at
-- UTC midnight, 'weekly' at the ISO week boundary, 'monthly' on the 1st. No
-- window end is stored anywhere — "are we in the same window?" is purely a
-- date_trunc comparison between window_started_at and now(). NULL interval =
-- one-off cap whose window never expires.
--
-- Both the batcher fold (which lazily rolls window_spend over at the first
-- billed request past a boundary) and the onwards sync eligibility predicate
-- (which un-caps keys at the boundary without any traffic) MUST use this one
-- function so the two sites can never disagree about window membership.

-- Capped-root lookup for the sync eligibility predicate. Prod EXPLAIN showed
-- the planner rewriting the predicate's NOT EXISTS into a hashed subplan whose
-- build seq-scans api_keys filtering `spend_limit IS NOT NULL`, rebuilt per
-- model; this partial index keeps that build proportional to the number of
-- capped keys (a small minority by design) instead of the whole key table.
CREATE INDEX idx_api_keys_capped_roots
  ON api_keys(id)
  WHERE spend_limit IS NOT NULL;

CREATE OR REPLACE FUNCTION api_key_cap_window_current(started_at timestamptz, cap_interval text)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
  SELECT CASE
    -- No checkpoint yet: nothing has been folded, so there is no current window.
    WHEN started_at IS NULL THEN false
    -- One-off cap: the window never expires.
    WHEN cap_interval IS NULL THEN true
    WHEN cap_interval = 'daily'
      THEN date_trunc('day',   started_at AT TIME ZONE 'utc') = date_trunc('day',   now() AT TIME ZONE 'utc')
    WHEN cap_interval = 'weekly'
      THEN date_trunc('week',  started_at AT TIME ZONE 'utc') = date_trunc('week',  now() AT TIME ZONE 'utc')
    WHEN cap_interval = 'monthly'
      THEN date_trunc('month', started_at AT TIME ZONE 'utc') = date_trunc('month', now() AT TIME ZONE 'utc')
    -- Unknown intervals are prevented by the CHECK constraint on api_keys;
    -- treat defensively as one-off.
    ELSE true
  END
$$;

-- Whether a windowed cap is within `grace_seconds` of its next calendar
-- boundary. Used ONLY by enforcement (the sync eligibility predicate and the
-- error enricher) to readmit exhausted scopes slightly EARLY: the fallback
-- sync ticks every ~5 minutes (onwards_sync.fallback_interval_milliseconds)
-- and nothing writes at a calendar boundary (so no NOTIFY fires there); by
-- treating an exhausted window as already over during the final grace period,
-- some fallback tick inside that period readmits the keys BEFORE the boundary
-- instead of up to one tick after it. A short pre-boundary overrun on an
-- already-over-budget key beats post-midnight rejections that look broken.
--
-- INVARIANT: grace_seconds must be >= the deployed fallback interval, or
-- readmission degrades to after-the-boundary again. Default 300s matches the
-- prod fallback default; keep them in lockstep.
--
-- MUST NOT be used in the batcher fold's rollover decision — rolling the
-- window early would reset window_spend repeatedly during the grace period
-- and misattribute end-of-window spend to the next window.
CREATE OR REPLACE FUNCTION api_key_cap_near_boundary(cap_interval text, grace_seconds int DEFAULT 300)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
  SELECT CASE
    -- One-off caps have no boundary: never in grace.
    WHEN cap_interval IS NULL THEN false
    WHEN cap_interval = 'daily'
      THEN now() AT TIME ZONE 'utc' >= date_trunc('day',   now() AT TIME ZONE 'utc') + interval '1 day'   - make_interval(secs => grace_seconds)
    WHEN cap_interval = 'weekly'
      THEN now() AT TIME ZONE 'utc' >= date_trunc('week',  now() AT TIME ZONE 'utc') + interval '1 week'  - make_interval(secs => grace_seconds)
    WHEN cap_interval = 'monthly'
      THEN now() AT TIME ZONE 'utc' >= date_trunc('month', now() AT TIME ZONE 'utc') + interval '1 month' - make_interval(secs => grace_seconds)
    ELSE false
  END
$$;

-- Next calendar boundary for a windowed cap, for user-facing "resets at ..."
-- messages (error enrichment, key listings). NULL for one-off caps.
CREATE OR REPLACE FUNCTION api_key_cap_window_resets_at(cap_interval text)
RETURNS timestamptz
LANGUAGE sql
STABLE
AS $$
  SELECT CASE
    WHEN cap_interval = 'daily'
      THEN (date_trunc('day',   now() AT TIME ZONE 'utc') + interval '1 day')   AT TIME ZONE 'utc'
    WHEN cap_interval = 'weekly'
      THEN (date_trunc('week',  now() AT TIME ZONE 'utc') + interval '1 week')  AT TIME ZONE 'utc'
    WHEN cap_interval = 'monthly'
      THEN (date_trunc('month', now() AT TIME ZONE 'utc') + interval '1 month') AT TIME ZONE 'utc'
    ELSE NULL
  END
$$;
