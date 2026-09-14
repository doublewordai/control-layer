-- Existing requests retain their original billing owner. New rows are assigned
-- atomically on first dispatch, before any inference can run. NOT VALID avoids
-- scanning existing heaps under the migration lock; new writes are checked.
ALTER TABLE requests ADD COLUMN billing_mode TEXT DEFAULT 'legacy';
ALTER TABLE requests ALTER COLUMN billing_mode DROP DEFAULT;
ALTER TABLE requests ADD COLUMN accepted_event_id UUID;
ALTER TABLE requests ADD CONSTRAINT requests_billing_mode_check
    CHECK (billing_mode IN ('legacy', 'durable')) NOT VALID;
ALTER TABLE requests ADD CONSTRAINT requests_durable_acceptance_check
    CHECK (billing_mode IS DISTINCT FROM 'durable' OR state <> 'completed' OR accepted_event_id IS NOT NULL) NOT VALID;

-- Archive inserts use an explicit target list because archive_bucket remains
-- the partition key in its original physical column position.
ALTER TABLE batch_requests_archive ADD COLUMN billing_mode TEXT DEFAULT 'legacy';
ALTER TABLE batch_requests_archive ALTER COLUMN billing_mode DROP DEFAULT;
ALTER TABLE batch_requests_archive ADD COLUMN accepted_event_id UUID;
ALTER TABLE batch_requests_archive ADD CONSTRAINT archive_billing_mode_check
    CHECK (billing_mode IN ('legacy', 'durable')) NOT VALID;
ALTER TABLE batch_requests_archive ADD CONSTRAINT archive_durable_acceptance_check
    CHECK (billing_mode IS DISTINCT FROM 'durable' OR state <> 'completed' OR accepted_event_id IS NOT NULL) NOT VALID;

-- Compact billing evidence outlives request bodies, archive partitions and
-- explicit payload erasure. Do not attach cascading foreign keys or expire it
-- with payload retention: consumers may recover after either payload plane moves.
CREATE TABLE billing_acceptances (
    request_id UUID PRIMARY KEY,
    accepted_event_id UUID NOT NULL,
    completed_at TIMESTAMPTZ NOT NULL,
    owner_id TEXT,
    batch_id UUID
);

CREATE FUNCTION preserve_billing_acceptance() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    accepted RECORD;
    acceptance_owner TEXT;
BEGIN
    -- The trigger runs only for terminal successful rows. Also inspect legacy
    -- completions so reusing an erased durable request ID cannot bypass evidence.
    EXECUTE format('SELECT * FROM %I.billing_acceptances WHERE request_id = $1', TG_TABLE_SCHEMA)
        INTO accepted USING NEW.id;
    IF NEW.billing_mode IS DISTINCT FROM 'durable' THEN
        IF accepted.request_id IS NOT NULL THEN
            RAISE EXCEPTION 'request already has durable billing acceptance' USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.accepted_event_id IS NULL OR NEW.completed_at IS NULL THEN
        RAISE EXCEPTION 'durable completion requires billing acceptance' USING ERRCODE = '23514';
    END IF;
    acceptance_owner := NULLIF(NEW.created_by, '');
    IF acceptance_owner IS NULL AND NEW.batch_id IS NOT NULL THEN
        EXECUTE format('SELECT NULLIF(created_by, '''') FROM %I.batches WHERE id = $1', TG_TABLE_SCHEMA)
            INTO acceptance_owner USING NEW.batch_id;
    END IF;
    EXECUTE format('INSERT INTO %I.billing_acceptances
        (request_id, accepted_event_id, completed_at, owner_id, batch_id)
        VALUES ($1, $2, $3, $4, $5) ON CONFLICT (request_id) DO NOTHING', TG_TABLE_SCHEMA)
        USING NEW.id, NEW.accepted_event_id, NEW.completed_at, acceptance_owner, NEW.batch_id;
    -- Re-read after INSERT: a concurrent first acceptance may have won the
    -- unique constraint wait. Never overwrite its identity or attribution.
    EXECUTE format('SELECT * FROM %I.billing_acceptances WHERE request_id = $1', TG_TABLE_SCHEMA)
        INTO STRICT accepted USING NEW.id;
    IF accepted.accepted_event_id IS DISTINCT FROM NEW.accepted_event_id
        OR accepted.owner_id IS DISTINCT FROM acceptance_owner
        OR accepted.batch_id IS DISTINCT FROM NEW.batch_id THEN
        RAISE EXCEPTION 'request billing acceptance conflicts with retained evidence' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER preserve_billing_acceptance
    BEFORE INSERT OR UPDATE OF state, billing_mode, accepted_event_id ON requests
    FOR EACH ROW WHEN (NEW.state = 'completed') EXECUTE FUNCTION preserve_billing_acceptance();

CREATE FUNCTION forbid_billing_acceptance_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'billing acceptance evidence is immutable' USING ERRCODE = '23514';
END
$$;
CREATE TRIGGER immutable_billing_acceptance
    BEFORE UPDATE OR DELETE ON billing_acceptances
    FOR EACH ROW EXECUTE FUNCTION forbid_billing_acceptance_mutation();
