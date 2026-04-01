CREATE TABLE provider_display_configs (
    provider_key TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    icon TEXT,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT provider_display_configs_key_nonempty CHECK (btrim(provider_key) <> ''),
    CONSTRAINT provider_display_configs_name_nonempty CHECK (btrim(display_name) <> '')
);

CREATE INDEX idx_provider_display_configs_sort_order
    ON provider_display_configs(sort_order, display_name);

CREATE TRIGGER provider_display_configs_updated_at
    BEFORE UPDATE ON provider_display_configs
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();
