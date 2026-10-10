SET LOCAL lock_timeout = '5s';

DROP TABLE IF EXISTS model_release_cutoffs;
ALTER TABLE batch_requests_archive DROP COLUMN IF EXISTS dispatched_tolerated;
ALTER TABLE requests DROP COLUMN IF EXISTS dispatched_tolerated;
