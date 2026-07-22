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
