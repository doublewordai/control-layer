-- Expansion only. Catalog authoring never changes this operator-owned switch.
SET LOCAL lock_timeout = '5s';
ALTER TABLE deployed_models ADD COLUMN routing_mode TEXT NOT NULL DEFAULT 'legacy'
    CHECK (routing_mode IN ('legacy', 'class_routes'));
COMMENT ON COLUMN deployed_models.routing_mode IS
    'Operator-controlled activation, not catalog-owned. Legacy until the class-routing runtime and rollout preflights are deployed.';
