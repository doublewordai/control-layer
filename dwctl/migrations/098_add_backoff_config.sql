-- Add inter-attempt retry backoff configuration for the onwards proxy.
--
-- onwards 0.29 introduced an optional `backoff` block on FallbackConfig that
-- inserts an exponential delay (with jitter) between fallback attempts, plus
-- a cumulative `max_total_backoff_ms` budget that bails out of the retry loop
-- early. See onwards #196 for the library-side change.
--
-- Storage mirrors the existing fallback_* pattern: discrete columns, not
-- JSONB. `backoff_enabled = FALSE` means "no inter-attempt delay" (legacy
-- zero-delay behaviour), so existing rows pick up no change.
--
-- The four shape columns (initial_ms, max_ms, factor, jitter) are always
-- populated with onwards' own defaults so that flipping `backoff_enabled` to
-- TRUE immediately yields a working config without forcing the admin to
-- fill every knob.
--
-- `backoff_max_total_ms` is an independent budget cap; only consulted when
-- backoff is enabled, so a NULL is meaningful (= no budget).

ALTER TABLE deployed_models
  ADD COLUMN backoff_enabled BOOLEAN NOT NULL DEFAULT FALSE,
  ADD COLUMN backoff_initial_ms INTEGER NOT NULL DEFAULT 100,
  ADD COLUMN backoff_max_ms INTEGER NOT NULL DEFAULT 5000,
  ADD COLUMN backoff_factor DOUBLE PRECISION NOT NULL DEFAULT 2.0,
  ADD COLUMN backoff_jitter TEXT NOT NULL DEFAULT 'full',
  ADD COLUMN backoff_max_total_ms INTEGER;

-- Validation: keep silly inputs out at the DB layer so the API and the
-- onwards config builder can trust them. The API also rejects them earlier
-- with friendlier error messages, but this is a backstop.
ALTER TABLE deployed_models
  ADD CONSTRAINT backoff_initial_ms_positive
    CHECK (backoff_initial_ms >= 1),
  ADD CONSTRAINT backoff_max_ms_at_least_initial
    CHECK (backoff_max_ms >= backoff_initial_ms),
  ADD CONSTRAINT backoff_factor_at_least_one
    CHECK (backoff_factor >= 1.0),
  ADD CONSTRAINT backoff_jitter_known
    CHECK (backoff_jitter IN ('none', 'full')),
  ADD CONSTRAINT backoff_max_total_ms_at_least_max
    CHECK (backoff_max_total_ms IS NULL OR backoff_max_total_ms >= backoff_max_ms);
