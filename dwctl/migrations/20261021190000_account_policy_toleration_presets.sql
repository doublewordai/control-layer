-- A per-model or per-class scheduling-toleration preset from the account
-- catalog. It overrides the account's own preset (users.toleration_preset) for
-- requests to that model or class. Written only by the account catalog
-- reconciler.
SET LOCAL lock_timeout = '5s';

ALTER TABLE account_model_policies ADD COLUMN toleration_preset TEXT;

COMMENT ON COLUMN account_model_policies.toleration_preset IS
  'Id of a configured scheduling-toleration preset applied to this account''s requests to the model (serving_class NULL) or class, in place of the account''s own preset. NULL inherits. An id the config does not define fails closed to the most restrictive list.';
