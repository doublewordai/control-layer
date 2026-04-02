-- Connections: configured integration points for external data sources
CREATE TABLE connections (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id             UUID NOT NULL REFERENCES users(id),
    api_key_id          UUID REFERENCES api_keys(id),
    kind                VARCHAR NOT NULL DEFAULT 'source',
    provider            VARCHAR NOT NULL,
    name                VARCHAR NOT NULL,
    config_encrypted    BYTEA NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at          TIMESTAMPTZ
);

CREATE INDEX idx_connections_user_id ON connections (user_id) WHERE deleted_at IS NULL;

CREATE TRIGGER set_connections_updated_at
    BEFORE UPDATE ON connections
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- Sync operations: one per sync trigger (manual or scheduled)
CREATE TABLE sync_operations (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    connection_id   UUID NOT NULL REFERENCES connections(id),
    status          VARCHAR NOT NULL DEFAULT 'pending',
    strategy        VARCHAR NOT NULL DEFAULT 'snapshot',
    strategy_config JSONB,
    files_found     INT NOT NULL DEFAULT 0,
    files_skipped   INT NOT NULL DEFAULT 0,
    files_ingested  INT NOT NULL DEFAULT 0,
    files_failed    INT NOT NULL DEFAULT 0,
    batches_created INT NOT NULL DEFAULT 0,
    error_summary   JSONB,
    triggered_by    UUID NOT NULL REFERENCES users(id),
    sync_config     JSONB NOT NULL DEFAULT '{}',
    started_at      TIMESTAMPTZ,
    completed_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_sync_operations_connection ON sync_operations (connection_id);
CREATE INDEX idx_sync_operations_status ON sync_operations (status) WHERE status NOT IN ('completed', 'failed', 'cancelled');

-- Sync entries: one per external file per sync operation
CREATE TABLE sync_entries (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    sync_id                 UUID NOT NULL REFERENCES sync_operations(id),
    connection_id           UUID NOT NULL REFERENCES connections(id),
    external_key            VARCHAR NOT NULL,
    external_last_modified  TIMESTAMPTZ,
    external_size_bytes     BIGINT,
    status                  VARCHAR NOT NULL DEFAULT 'pending',
    file_id                 UUID,
    batch_id                UUID,
    template_count          INT,
    error                   TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Dedup: find previously-synced files for this connection
CREATE INDEX idx_sync_entries_dedup
    ON sync_entries (connection_id, external_key, external_last_modified)
    WHERE status NOT IN ('failed', 'skipped', 'pending', 'deleted');

-- Job lookups: find entries by sync + status
CREATE INDEX idx_sync_entries_sync_status
    ON sync_entries (sync_id, status);

CREATE TRIGGER set_sync_entries_updated_at
    BEFORE UPDATE ON sync_entries
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();
