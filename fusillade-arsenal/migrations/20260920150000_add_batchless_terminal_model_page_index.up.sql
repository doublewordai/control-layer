-- no-transaction
-- Bounded access path for zero-match status+model pages on the Responses
-- listing (GET /admin/api/v1/batches/requests).
--
-- The list_requests terminal arm answers `created_by IS NOT NULL AND state
-- NOT IN (active states) AND state = $2 AND model = $3[1] ORDER BY created_at
-- DESC, id DESC LIMIT n`. Before this index the only plan that satisfied the
-- ordering was an ordered walk over idx_requests_created_tier with the state
-- and model predicates as residual filters. That walk has no early exit: when
-- the filter pair matches nothing -- which is exactly what production fed it
-- on 2026-09-20, `status=failed&model=detail-zai/glm-5.2`, a deleted alias
-- persisted in a console user's filter -- it reads the entire terminal
-- history. The walk exceeded the code's 30s PAGE_BUDGET statement timeout and
-- the handler returned 500, which is what fired the ControlLayerSystemErrors
-- alert four times in five minutes (all four requests took 30.2s).
--
-- This index keeps the arm ordered for the (model, state) prefix while
-- bounding the scan to the matching prefix: a zero-match prefix ends at the
-- first index boundary instead of after the whole history, and a matching
-- page stops at LIMIT. The bound `state = $2::text` equality (see PageShape's
-- status_bound) is what makes the state column usable as a key bound under
-- generic prepared plans; the partial predicate matches the arm's own literal
-- `state NOT IN (...)` qual, so the index stays eligible in generic plans too.
--
-- Keep this file to one statement: concurrent builds cannot run in a
-- transaction. For populated deployments, prebuild using this file on a
-- direct connection, validate with the following migration, and ANALYZE
-- requests before rollout. A failed build can leave an INVALID index; IF NOT
-- EXISTS alone is not proof of success. The following migration validates the
-- definition and readiness. Existing indexes stay in place, so the previous
-- query remains rollback-safe.
--
-- Size is bounded by construction: the predicate covers only batchless
-- terminal rows (`created_by` is set exactly for responses, and batched rows
-- fall out of scope), which is the population the content-retention lifecycle
-- keeps small.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_batchless_terminal_model_page
ON requests (model, state, created_at DESC, id DESC)
WHERE created_by IS NOT NULL
  AND state NOT IN ('processing', 'claimed', 'pending');
