-- no-transaction
-- COR-537: seek one customer's page without walking other customers' history.
-- Keep this file to one statement: concurrent builds cannot run in a transaction.
-- For populated deployments, prebuild using this file on a direct connection,
-- validate with the following migration, and ANALYZE batches before rollout.
-- A failed build can leave an INVALID index; IF NOT EXISTS alone is not proof
-- of success. The following migration validates the definition and readiness.
-- Existing indexes stay in place, so the previous query remains rollback-safe.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_owner_created_at_id
ON batches (created_by, created_at DESC, id DESC)
WHERE deleted_at IS NULL;
