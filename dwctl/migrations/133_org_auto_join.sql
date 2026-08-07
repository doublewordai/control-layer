-- Opt-in auto-join for a workspace's claimed email domain.
--
-- #1430 removed silent domain auto-join outright: anyone who could receive
-- mail at a company's domain used to land inside the organization that owns
-- its billing account, with no approval and no notice to either party. That
-- was the right default to remove, but it is a reasonable thing for an
-- organization to *choose* — a company that has already decided everyone at
-- `acme.com` belongs in Acme's workspace should not have to approve each
-- colleague by hand.
--
-- So the behaviour comes back as a setting rather than a rule, and it is the
-- organization's own owners who turn it on — the API refuses an org admin,
-- same gate as zero_data_retention. FALSE for every existing row: nobody who
-- has not asked for it gets the old behaviour back.
--
-- Lives on `users` because organizations *are* rows in `users`
-- (`user_type = 'organization'`), alongside the other org-wide flags
-- (`zero_data_retention`, `verified`, `low_balance_threshold`). It is
-- meaningless on a personal row, which claims no domain.
--
-- Per-organization rather than per-domain because an organization claims
-- exactly one domain, carried in `username` as `{domain}~{suffix}` (#1435).
-- Several workspaces may share a domain; each decides for itself, and only
-- the oldest surviving one is what a colleague's signup matches on anyway.
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS auto_join_enabled BOOLEAN NOT NULL DEFAULT false;

COMMENT ON COLUMN users.auto_join_enabled IS
    'Organizations only: when true, a signup whose email domain matches this workspace''s claimed domain becomes an active member immediately, instead of being offered the choice to request access. Default false.';
