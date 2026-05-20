-- Pending email changes for organizations.
--
-- The organization email field is the canonical contact / billing /
-- notification address (rendered into Stripe receipts, invitation emails,
-- audit-log notifications, etc.). Previously, PATCH /organizations/{id}
-- accepted a new email and applied it immediately with no validation or
-- verification, which allowed a session with org-update privileges to
-- silently redirect every notification to an attacker-chosen address.
--
-- Email changes now go through a *double-opt-in* verification flow:
-- both the current contact address AND the new address must click a
-- verification link within 24 hours. The change is only applied to
-- `users.email` once both `*_confirmed_at` columns are set, at which
-- point the pending row is deleted in the same transaction.
--
-- This matches the standard SaaS account-takeover mitigation (Stripe,
-- GitHub, Google Cloud): requiring possession proof from both mailboxes
-- prevents (a) a session-hijack attacker — who controls the session but
-- not the old mailbox — from redirecting notifications, and (b) typos /
-- attacker-controlled new addresses from receiving notifications.

CREATE TABLE pending_org_email_changes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- At most one pending change per organization. A new PATCH atomically
    -- supersedes the previous one via INSERT ... ON CONFLICT, which
    -- guarantees older verification links stop working the moment a new
    -- request is accepted (no read-then-write race window).
    --
    -- NOTE: ON DELETE CASCADE only fires when the org row is physically
    -- removed from `users`. This codebase soft-deletes orgs by setting
    -- `is_deleted = true`, which does NOT trigger CASCADE — so a pending
    -- row can outlive a soft-deleted org. Application code must check
    -- `is_deleted = false` when consuming tokens, and indeed
    -- `confirm_*_email_side` joins `users` and filters on
    -- `is_deleted = false` for exactly that reason.
    organization_id UUID NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    new_email VARCHAR NOT NULL,
    requested_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Separate tokens for each mailbox. UNIQUE so a single token can only
    -- bind to one row, and so the confirm endpoint can dispatch on which
    -- column it matched.
    new_email_token_hash VARCHAR NOT NULL UNIQUE,
    old_email_token_hash VARCHAR NOT NULL UNIQUE,
    -- Per-side confirmation timestamps. The change is applied (and the
    -- row deleted) when BOTH are non-null.
    new_email_confirmed_at TIMESTAMPTZ,
    old_email_confirmed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);

-- The UNIQUE constraints above (organization_id, new_email_token_hash,
-- old_email_token_hash) provide the lookup indexes needed by the confirm
-- and supersede paths.
