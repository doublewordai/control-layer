-- Temporary authorization codes for CLI login flow.
-- Created by GET /authentication/cli-callback, exchanged by POST /authentication/cli-exchange.
-- Codes are single-use and short-lived (60 seconds).

CREATE TABLE cli_auth_codes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    code TEXT NOT NULL UNIQUE,
    inference_key_id UUID NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    platform_key_id UUID NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    account_name TEXT NOT NULL DEFAULT 'personal',
    org_id UUID,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for code lookup during exchange
CREATE INDEX idx_cli_auth_codes_code ON cli_auth_codes(code);

-- Index for cleanup of expired codes
CREATE INDEX idx_cli_auth_codes_expires_at ON cli_auth_codes(expires_at);
