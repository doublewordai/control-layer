-- Support DISTINCT ON query in list_synced_keys: picks the latest terminal
-- entry per (connection_id, external_key) ordered by external_last_modified
-- DESC NULLS LAST, updated_at DESC.
CREATE INDEX idx_sync_entries_terminal_keys
    ON sync_entries (connection_id, external_key, external_last_modified DESC NULLS LAST, updated_at DESC)
    WHERE status IN ('activated', 'failed');
