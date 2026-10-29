-- DW-1005: hash any row inserted without one between the backfill and the
-- check (none unless a release older than the dual-write one was still
-- serving; the check now rejects such inserts), then validate the check.
-- Neither statement blocks reads or other writes: the UPDATE locks only the
-- rows it changes, VALIDATE takes SHARE UPDATE EXCLUSIVE. A later release
-- turns this into SET NOT NULL (instant with a validated check) and drops it.
UPDATE api_keys
SET secret_hash = sha256(convert_to(secret, 'UTF8')),
    secret_prefix = left(secret, 8)
WHERE secret_hash IS NULL;

ALTER TABLE api_keys VALIDATE CONSTRAINT api_keys_secret_hash_not_null;
