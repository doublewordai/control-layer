-- Validate the account priority-bias CHECK added NOT VALID in the previous
-- migration. VALIDATE scans `users` under SHARE UPDATE EXCLUSIVE, which does
-- not block reads or writes.
SET LOCAL lock_timeout = '5s';

ALTER TABLE users VALIDATE CONSTRAINT chk_users_priority_bias_seconds;
