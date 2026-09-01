-- Denormalize created_by onto requests so the dashboard listing query can
-- filter by user without joining batches. The cross-table indirection prevents
-- Postgres from building a composite index on (created_by, sort_keys), which
-- means even users with no rows pay a full sort-index scan today.
--
-- Also relaxes requests.batch_id to NULL so responses can exist without a
-- parent batch row. The data migration below converts existing virtual-batch
-- responses (one batch per response, used as a workaround) into batchless
-- requests carrying their own created_by.

-- Nullable rather than NOT NULL: batched rows leave this NULL (attribution
-- lives on batches.created_by); batchless responses populate it directly.
-- The XOR CHECK below makes that invariant explicit at write time.
ALTER TABLE requests ADD COLUMN IF NOT EXISTS created_by TEXT;

ALTER TABLE requests ALTER COLUMN batch_id DROP NOT NULL;

-- Exactly one of (batch_id, created_by) is non-NULL: batched rows attribute
-- via the batch, batchless rows attribute via the request. Rejects both-set
-- (data corruption) and both-NULL (orphaned row).
--
-- The XOR also serves as a guard against an API contract violation: the
-- batchless-row creators (`create_realtime`, `create_flex`) coerce an
-- empty-string `created_by` to NULL before insert so the CHECK rejects it
-- loudly, rather than letting a phantom-user row land in the per-user
-- listing.
--
-- `NOT VALID` skips the full-table scan that the constraint addition would
-- otherwise trigger. Pre-migration every row has `batch_id NOT NULL` and
-- `created_by` doesn't exist (just added above), so the constraint is
-- trivially satisfied by every existing row — there's nothing to verify.
-- New writes still get the check enforced. A separate `ALTER TABLE
-- ... VALIDATE CONSTRAINT requests_attribution_xor` (which only takes
-- SHARE UPDATE EXCLUSIVE, no read/write blocking) can be run later if
-- formal validation is wanted; on the deploy-time path it's a no-op since
-- the data is known-clean.
ALTER TABLE requests ADD CONSTRAINT requests_attribution_xor
    CHECK ((batch_id IS NULL) <> (created_by IS NULL)) NOT VALID;

-- The two partial indexes that back the per-user listing query
-- (idx_requests_user_active_sort, idx_requests_user_created_sort) are created
-- by `scripts/backfill_responses_to_batchless.sql`, not here, so they can be
-- built with CONCURRENTLY against a live `requests` table — Postgres rejects
-- CONCURRENTLY inside a transaction, and sqlx wraps every migration in one.
-- Without those indexes the listing falls back to a seq scan but still
-- returns correct results.

-- Update active_request_templates to include templates without a parent file.
-- Batchless responses use templates with file_id IS NULL; the daemon's
-- claim_requests join needs to see them.
DROP VIEW IF EXISTS active_request_templates;
CREATE VIEW active_request_templates AS
SELECT rt.*
FROM request_templates rt
LEFT JOIN files f ON rt.file_id = f.id
WHERE rt.file_id IS NULL OR f.deleted_at IS NULL;

-- Mirror the same change on active_requests (currently unused but kept in sync).
DROP VIEW IF EXISTS active_requests;
CREATE VIEW active_requests AS
SELECT r.*
FROM requests r
LEFT JOIN batches b ON r.batch_id = b.id
WHERE r.batch_id IS NULL OR b.deleted_at IS NULL;

-- Backfill of pre-existing virtual-batch responses lives in a separate
-- script (`scripts/backfill_responses_to_batchless.sql`) so the migration
-- itself only changes schema. Run it once after deploying this migration
-- to make older responses visible in the new listing.
