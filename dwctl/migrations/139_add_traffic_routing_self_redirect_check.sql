-- DB backstop: a traffic routing redirect rule can never target the model it
-- belongs to. A self-referencing redirect (redirect_target_id =
-- deployed_model_id) is a byte-identical no-op for the routing engine (it
-- resolves back to the same pool the request was already bound for), but it
-- violates the documented invariant `resolve_traffic_rules` enforces and, no
-- `CHECK` constraint being present to catch it, the bad row survives and is
-- republished to the routing config on every sync.
--
-- The application's `resolve_traffic_rules` guard rejects self-redirects at
-- request time, but only a CHECK constraint makes the invariant robust to
-- *every* code path that builds a rule row -- the guard was bypassed once
-- already by the PATCH handler resolving rules before the alias rename was
-- committed (a redirect to the model's *old* alias resolved to the model's
-- own id). This constraint is the storage-layer backstop so a future code
-- path cannot reintroduce the same state silently.
--
-- Any pre-existing self-redirect rows (only producible by the pre-fix bug)
-- are removed first: a self-redirect is a no-op, so dropping the rule
-- restores direct routing without any observable behaviour change.

DELETE FROM model_traffic_rules
WHERE action = 'redirect' AND redirect_target_id = deployed_model_id;

ALTER TABLE model_traffic_rules
  ADD CONSTRAINT no_self_redirect
    CHECK (redirect_target_id IS NULL OR redirect_target_id <> deployed_model_id);
