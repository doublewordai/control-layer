-- Pending email changes for organizations.
--
-- The organization email field is the canonical contact / billing /
-- notification address (rendered into Stripe receipts, invitation emails,
-- audit-log notifications, etc.). Previously, PATCH /organizations/{id}
-- accepted a new email and applied it immediately with no validation or
-- verification, which allowed a session with org-update privileges to
-- silently redirect every notification to an attacker-chosen address.
--
-- Email changes now go through a verification flow: a pending change row
-- is created with a hashed token, a verification link is sent to the new
-- address, and a notice is sent to the old address. The change is only
-- applied when the token is consumed via the confirm endpoint.

CREATE TABLE pending_org_email_changes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- At most one pending change per organization. A new PATCH atomically
    -- supersedes the previous one via INSERT ... ON CONFLICT, which
    -- guarantees older verification links stop working the moment a new
    -- request is accepted (no read-then-write race window).
    organization_id UUID NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    new_email VARCHAR NOT NULL,
    requested_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash VARCHAR NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);

-- Both `UNIQUE` constraints above (organization_id and token_hash) provide
-- the lookup indexes needed by the confirm and supersede paths.
