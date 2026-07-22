-- Index for the claim query's BATCHLESS (flex/async) arms.
--
-- `claim_requests` has two batchless arms (Source A full-capacity and Source B
-- leaky-bucket) that scan pending rows with `batch_id IS NULL` per model. No
-- existing index has a `batch_id IS NULL` predicate — the indexes that include
-- batch_id as a *column* (e.g. idx_requests_pending_claim_pull, idx_requests_
-- batch_state) still contain every pending row — so Postgres uses
-- idx_requests_state (or idx_requests_state_model) and filters `batch_id IS NULL`
-- row-by-row. When a model has a large BATCHED backlog (e.g. millions of pending
-- rows in a few big batches), each batchless arm scans the entire pending set to
-- find the (often zero) batchless rows — measured at ~4s per scan on a 53M-row
-- table, and the claim runs two such arms per model every cycle.
--
-- This partial index contains ONLY batchless pending rows (normally a tiny set),
-- so the per-model lookup returns that subset directly instead of filtering the
-- whole pending heap. The arms then sort the (tiny) candidate set by a computed
-- fairness/deadline blend — not index-servable, but trivial at ~0 rows — so the
-- leading `model` key (isolating the subset) is what matters; `created_at` is a
-- harmless secondary key.
--
-- On large production tables, create this CONCURRENTLY before deploying so the
-- IF NOT EXISTS statement below becomes a no-op:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_pending_batchless
--   ON requests (model, created_at)
--   WHERE state = 'pending' AND batch_id IS NULL AND template_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_requests_pending_batchless
ON requests (model, created_at)
WHERE state = 'pending' AND batch_id IS NULL AND template_id IS NOT NULL;
