-- Reconciliation retains compact evidence independently of event payload cleanup.
CREATE TABLE billing_reconciliation_cursor (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    last_request_id UUID,
    revision BIGINT NOT NULL DEFAULT 0,
    last_audit_at TIMESTAMPTZ,
    last_cycle_completed_at TIMESTAMPTZ,
    completed_cycles BIGINT NOT NULL DEFAULT 0
);
INSERT INTO billing_reconciliation_cursor(singleton) VALUES (true);

CREATE TABLE billing_reconciliation_issues (
    request_id UUID PRIMARY KEY,
    accepted_event_id UUID,
    reason TEXT NOT NULL CHECK (reason IN ('missing_accepted_event', 'missing_capture', 'processed_without_receipt')),
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at TIMESTAMPTZ
);
CREATE INDEX billing_reconciliation_issues_open_idx
    ON billing_reconciliation_issues (request_id) WHERE resolved_at IS NULL;
COMMENT ON TABLE billing_reconciliation_issues IS
    'Compact reconciliation evidence only; never authorizes reconstructed usage or a charge.';

ALTER TABLE billing_reconciliation_cursor
    ADD COLUMN last_receipt_id UUID,
    ADD COLUMN receipt_revision BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN last_receipt_audit_at TIMESTAMPTZ,
    ADD COLUMN last_receipt_cycle_completed_at TIMESTAMPTZ;
CREATE TABLE billing_integrity_issues (
    request_id UUID NOT NULL,
    event_id UUID NOT NULL,
    reason TEXT NOT NULL CHECK (reason IN (
        'receipt_source_mismatch', 'receipt_acceptance_mismatch', 'receipt_owner_mismatch',
        'receipt_ledger_mismatch', 'receipt_analytics_mismatch', 'receipt_event_mismatch',
        'receipt_batch_projection_mismatch'
    )),
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at TIMESTAMPTZ,
    PRIMARY KEY(request_id, reason)
);
CREATE INDEX billing_integrity_issues_open_idx ON billing_integrity_issues(request_id) WHERE resolved_at IS NULL;
COMMENT ON TABLE billing_integrity_issues IS
    'Bounded receipt integrity audit evidence; independent of event/acceptance retention. No automatic financial repair.';

-- Analytics timestamps anchor the external retention policy independently of queue cleanup.
ALTER TABLE billing_receipts ADD COLUMN analytics_timestamp TIMESTAMPTZ;
