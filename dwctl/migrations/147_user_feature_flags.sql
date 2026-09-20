-- Account-level opt-ins. Missing rows and disabled rows both mean disabled.
-- This table also holds organization flags: organizations have their own users row.
SET LOCAL lock_timeout = '5s';

CREATE TABLE user_feature_flags (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    feature_flag TEXT NOT NULL CHECK (feature_flag ~ '^[A-Z][A-Z0-9_]*$'),
    enabled BOOLEAN NOT NULL DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, feature_flag)
);

CREATE TRIGGER user_feature_flags_updated_at
    BEFORE UPDATE ON user_feature_flags
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER user_feature_flags_notify
    AFTER INSERT OR UPDATE OR DELETE ON user_feature_flags
    FOR EACH STATEMENT EXECUTE FUNCTION notify_config_change();

-- Shared by admission checks and background routing queries. Soft-deleted
-- accounts cannot acquire access through a leftover flag. Invoker privileges
-- and an unqualified table name preserve the configured application schema.
CREATE FUNCTION user_has_feature(account_id UUID, flag_name TEXT)
RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
    SELECT EXISTS (
        SELECT 1 FROM user_feature_flags f
        JOIN users u ON u.id = f.user_id
        WHERE f.user_id = account_id AND f.feature_flag = flag_name
          AND f.enabled AND NOT u.is_deleted
    );
$$;
