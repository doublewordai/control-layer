-- Roll back the query first. This only removes the terminal-model page index.
-- The validate migration has no schema effect to undo.
DROP INDEX CONCURRENTLY IF EXISTS idx_requests_batchless_terminal_model_page;
COMMENT ON INDEX idx_requests_batchless_terminal_model_page IS NULL;
