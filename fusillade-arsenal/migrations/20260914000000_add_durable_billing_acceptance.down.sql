DROP TRIGGER preserve_billing_acceptance ON requests;
DROP FUNCTION preserve_billing_acceptance();
DROP TABLE billing_acceptances;
DROP FUNCTION forbid_billing_acceptance_mutation();
ALTER TABLE batch_requests_archive DROP COLUMN accepted_event_id, DROP COLUMN billing_mode;
ALTER TABLE requests DROP COLUMN accepted_event_id, DROP COLUMN billing_mode;
