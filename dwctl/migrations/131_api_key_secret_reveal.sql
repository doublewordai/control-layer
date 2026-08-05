-- First-reveal tracking for issued API keys.
--
-- When an org owner/admin issues a key to another member, the holder must be
-- able to fetch the secret ONCE from their own account ("Reveal"). The
-- issuing admin still sees the secret in the create response (e.g. to stash
-- it in a vault) — that does NOT count as the reveal. `secret_revealed_at`
-- records the moment the HOLDER first fetched the secret:
--
--   * NULL     → the holder has not yet revealed the key. The one-off
--                POST /users/{id}/api-keys/{key_id}/reveal is available to
--                them — and only them, never to admins (admins rotate).
--   * non-NULL → the reveal has been consumed (or never existed, for
--                self-created keys, which are born revealed). From here the
--                only way to see a secret is rotation. Org admins see this
--                timestamp as "opened at" — proof the holder has engaged
--                with the key, so they don't rotate it without cause.
--
-- Rotation deliberately does NOT touch this column: an admin can rotate an
-- unrevealed key any number of times and the holder still gets their one
-- reveal (of the then-current secret). Once revealed, no rotation ever
-- makes the key revealable again.
--
-- The DEFAULT NOW() (set LAST, below) makes every insert path fail closed
-- (born revealed = no reveal available): self-created keys, hidden system
-- keys, and cap-scope children. The ONLY path that clears it is the create
-- handler when a manager issues a key to a DIFFERENT user
-- (ApiKeys::mark_secret_reveal_pending).
--
-- Deliberately split into add-nullable → backfill → set-default rather than
-- ADD COLUMN ... DEFAULT NOW(): the bare ADD COLUMN is a pure metadata
-- change (no rewrite under any Postgres version or default-volatility
-- rules), the backfill only touches rows that need it, and the default then
-- applies to new rows only.
ALTER TABLE api_keys ADD COLUMN secret_revealed_at TIMESTAMPTZ;

-- Existing keys: their secrets were shown at creation under the old flow, so
-- no key becomes retroactively revealable. Backfill with created_at rather
-- than the migration timestamp so the admin-facing "opened at" display stays
-- sensible for old keys.
UPDATE api_keys SET secret_revealed_at = created_at WHERE secret_revealed_at IS NULL;

ALTER TABLE api_keys ALTER COLUMN secret_revealed_at SET DEFAULT NOW();
