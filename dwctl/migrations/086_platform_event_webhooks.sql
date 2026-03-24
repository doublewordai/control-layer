-- Platform event webhooks: extend webhook system with scoped event types
-- Enables PlatformManagers to receive webhooks for platform-wide events
-- (user creation, batch creation, API key creation)

-- Rename batch_id to resource_id (generic reference to triggering resource)
ALTER TABLE webhook_deliveries RENAME COLUMN batch_id TO resource_id;
ALTER TABLE webhook_deliveries ALTER COLUMN resource_id DROP NOT NULL;
ALTER INDEX idx_webhook_deliveries_batch_id RENAME TO idx_webhook_deliveries_resource_id;

-- Add scope column: 'own' for user-scoped, 'platform' for PM-scoped
ALTER TABLE user_webhooks
  ADD COLUMN scope TEXT NOT NULL DEFAULT 'own'
  CHECK (scope IN ('own', 'platform'));

-- Index for efficient platform webhook queries
CREATE INDEX idx_user_webhooks_platform
  ON user_webhooks (scope)
  WHERE scope = 'platform' AND enabled = true;

-- Trigger: only PlatformManagers can have platform-scoped webhooks.
-- This is a hard database-level guarantee — even if the application has a
-- bug in its query logic, a standard user can never hold a platform-scoped
-- webhook, so they can never receive platform event deliveries.
CREATE OR REPLACE FUNCTION enforce_platform_webhook_scope()
RETURNS TRIGGER AS $$
BEGIN
  IF NEW.scope = 'platform' THEN
    IF NOT EXISTS (
      SELECT 1 FROM user_roles
      WHERE user_id = NEW.user_id AND role = 'PLATFORMMANAGER'
    ) THEN
      RAISE EXCEPTION 'Only PlatformManagers can create platform-scoped webhooks';
    END IF;
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_enforce_platform_webhook_scope
  BEFORE INSERT OR UPDATE ON user_webhooks
  FOR EACH ROW
  EXECUTE FUNCTION enforce_platform_webhook_scope();
