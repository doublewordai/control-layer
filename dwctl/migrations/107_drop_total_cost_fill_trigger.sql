-- Remove the http_analytics.total_cost fidelity bridge added in migration 105.
--
-- Migration 105 converted `total_cost` from a GENERATED column to a batcher-written one,
-- and added a BEFORE INSERT trigger as a fallback: during the blue/green cutover the OLD
-- (pre-cache) release kept inserting rows without setting `total_cost`, and the trigger
-- reconstructed the dropped generation expression (tokens x prices) so those rows landed
-- with the list price instead of NULL. That cutover is now complete — every running
-- release writes `total_cost` explicitly (cache-adjusted on the new path; the old release
-- has drained), so the trigger has been a no-op and can go. New inserts are expected to
-- always set `total_cost` in code from here on.
--
-- ONLINE-SAFE / METADATA-ONLY: DROP TRIGGER takes a brief ACCESS EXCLUSIVE lock on
-- http_analytics to update the catalog but does NOT rewrite the table; DROP FUNCTION is
-- catalog-only. The trigger is dropped before the function it depends on. Both use
-- IF EXISTS so the migration is idempotent and safe if a prior attempt partially applied.

DROP TRIGGER IF EXISTS http_analytics_fill_total_cost_trg ON http_analytics;
DROP FUNCTION IF EXISTS http_analytics_fill_total_cost();
