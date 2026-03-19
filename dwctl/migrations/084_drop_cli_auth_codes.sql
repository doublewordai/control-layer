-- Drop the cli_auth_codes table. The two-step code exchange flow was replaced
-- with a single-step localhost redirect that delivers keys directly.
-- The table is no longer used.

DROP TABLE IF EXISTS cli_auth_codes;
