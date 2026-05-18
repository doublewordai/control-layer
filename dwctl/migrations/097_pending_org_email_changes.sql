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
    organization_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    new_email VARCHAR NOT NULL,
    requested_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash VARCHAR NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);

-- Look up pending changes by org (used when superseding a prior request).
CREATE INDEX idx_pending_org_email_changes_org
    ON pending_org_email_changes(organization_id);

-- The unique constraint on token_hash already provides the lookup index for
-- the confirm endpoint.
