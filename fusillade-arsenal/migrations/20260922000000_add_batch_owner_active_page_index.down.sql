-- no-transaction
--
-- Drop the owner active-page index added by 20260922000000. Reverting returns
-- owner-scoped active-first pages to walking idx_batches_owner_created_at_id
-- (or idx_batches_active) as before this index existed.
--
-- CONCURRENTLY for the same reason as the up migration, which requires the
-- `-- no-transaction` directive above and one statement in the file.

DROP INDEX CONCURRENTLY IF EXISTS idx_batches_owner_active_created_at_id;