-- Give ledger rows their own idempotency key without changing source_id's
-- contract: source_id remains the canonical http_analytics row id.
--
-- The trigger is deliberately installed in the database so old and new
-- binaries are safe while a deployment rolls. Hidden batch-purpose API keys
-- are the authenticated internal marker; an external caller cannot turn a
-- spoofed x-fusillade-request-id header into a billing deduplication key.

-- Keep raw analytics aligned with logical request usage. A reclaimed request
-- can finish on two daemons with different (instance_id, correlation_id)
-- attempt identities; only the first trusted successful attempt is canonical.
-- Suppress later successes before every aggregate that reads http_analytics, while
-- preserving ordinary upserts for the same attempt identity.
CREATE OR REPLACE FUNCTION suppress_duplicate_fusillade_analytics()
RETURNS TRIGGER AS $$
DECLARE
    trusted_fusillade_request BOOLEAN;
BEGIN
    -- Failed physical attempts remain observable and must not prevent a later
    -- successful retry becoming the canonical logical-request usage row.
    IF NEW.fusillade_request_id IS NULL
       OR NEW.status_code IS NULL
       OR NEW.status_code NOT BETWEEN 200 AND 299 THEN
        RETURN NEW;
    END IF;

    SELECT EXISTS (
        SELECT 1
        FROM api_keys ak
        WHERE ak.id = NEW.api_key_id
          AND ak.purpose = 'batch'
          AND ak.hidden = true
    ) INTO trusted_fusillade_request;

    IF NOT trusted_fusillade_request THEN
        RETURN NEW;
    END IF;

    -- The ledger trigger below takes the same transaction-scoped lock. Holding
    -- it from analytics insertion through billing makes mixed old/new binaries
    -- serialize the complete logical-request write, not just one table.
    PERFORM pg_advisory_xact_lock(hashtextextended(NEW.fusillade_request_id::text, 0));

    IF EXISTS (
        SELECT 1
        FROM http_analytics ha
        JOIN api_keys existing_key ON existing_key.id = ha.api_key_id
        WHERE ha.fusillade_request_id = NEW.fusillade_request_id
          AND ha.status_code BETWEEN 200 AND 299
          AND existing_key.purpose = 'batch'
          AND existing_key.hidden = true
          AND NOT (
              ha.instance_id = NEW.instance_id
              AND ha.correlation_id = NEW.correlation_id
          )
    ) THEN
        RETURN NULL;
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS suppress_duplicate_fusillade_analytics_before_insert
    ON http_analytics;

CREATE TRIGGER suppress_duplicate_fusillade_analytics_before_insert
    BEFORE INSERT ON http_analytics
    FOR EACH ROW
    EXECUTE FUNCTION suppress_duplicate_fusillade_analytics();

ALTER TABLE credits_transactions
    ADD COLUMN IF NOT EXISTS deduplication_id UUID;

COMMENT ON COLUMN credits_transactions.deduplication_id IS
    'Stable idempotency key for a logical ledger transaction; authenticated Fusillade usage uses fusillade_request_id.';

CREATE OR REPLACE FUNCTION prepare_credit_transaction_deduplication()
RETURNS TRIGGER AS $$
DECLARE
    trusted_fusillade_request BOOLEAN;
BEGIN
    IF NEW.fusillade_request_id IS NULL THEN
        -- All purchases, grants, removals, and ordinary realtime usage take
        -- this path. Writers do not need to mention the new nullable column.
        NEW.deduplication_id := NULL;
        RETURN NEW;
    END IF;

    SELECT EXISTS (
        SELECT 1
        FROM api_keys ak
        WHERE ak.id = NEW.api_key_id
          AND ak.purpose = 'batch'
          AND ak.hidden = true
    ) INTO trusted_fusillade_request;

    IF trusted_fusillade_request AND NEW.transaction_type = 'usage' THEN
        -- Derive this in the database as well as the application so an old
        -- binary receives the same protection during a rolling deploy.
        NEW.deduplication_id := NEW.fusillade_request_id;
    ELSE
        -- The request id came from an untrusted header. Preserve ordinary
        -- billing semantics and do not retain it on the durable ledger.
        NEW.fusillade_request_id := NULL;
        NEW.deduplication_id := NULL;
        RETURN NEW;
    END IF;

    -- Serialize the existence check for this logical transaction. The UUID is
    -- still compared exactly below; a 64-bit advisory-key collision can only
    -- serialize unrelated inserts, never suppress one.
    PERFORM pg_advisory_xact_lock(hashtextextended(NEW.deduplication_id::text, 0));

    -- fusillade_request_id already has the partial index added in migration
    -- 120. deduplication_id deliberately mirrors it for this use case, so the
    -- trigger does not require another large index on this write-hot ledger.
    IF EXISTS (
        SELECT 1
        FROM credits_transactions ct
        JOIN api_keys existing_key ON existing_key.id = ct.api_key_id
        WHERE ct.transaction_type = 'usage'
          AND ct.fusillade_request_id = NEW.fusillade_request_id
          AND existing_key.purpose = 'batch'
          AND existing_key.hidden = true
    ) THEN
        RETURN NULL;
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS prepare_credit_transaction_deduplication_before_insert
    ON credits_transactions;

CREATE TRIGGER prepare_credit_transaction_deduplication_before_insert
    BEFORE INSERT ON credits_transactions
    FOR EACH ROW
    EXECUTE FUNCTION prepare_credit_transaction_deduplication();
