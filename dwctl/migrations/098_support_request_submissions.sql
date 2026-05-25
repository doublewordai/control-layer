-- Audit + rate-limit table for POST /admin/api/v1/support/requests.
--
-- One row per accepted submission. The handler queries this table for a
-- per-user sliding-window count to cap how many support requests a single
-- account can submit per hour. This prevents one user (script error,
-- automation, or compromised credentials) from draining the shared
-- transactional-email quota.
--
-- Only metadata is stored — the subject and body of the request are sent
-- via the email path and never persisted here, to avoid this table
-- accumulating support content.

CREATE TABLE support_request_submissions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Compound index supports the per-user, last-window-hours COUNT query the
-- handler runs on every submission to enforce the rate limit. The DESC
-- ordering on created_at lets the planner pick the index for both the
-- COUNT (range scan) and any future "list my recent submissions" lookups.
CREATE INDEX idx_support_request_submissions_user_recent
    ON support_request_submissions (user_id, created_at DESC);
