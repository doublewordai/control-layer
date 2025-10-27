-- Add LISTEN/NOTIFY trigger for probe changes
-- This allows the probe scheduler to react immediately to changes instead of polling

CREATE OR REPLACE FUNCTION notify_probe_change() RETURNS trigger AS $$
BEGIN
  PERFORM pg_notify('probe_changes', json_build_object(
    'action', TG_OP,
    'probe_id', COALESCE(NEW.id, OLD.id),
    'active', CASE
      WHEN TG_OP = 'DELETE' THEN OLD.active
      ELSE NEW.active
    END
  )::text);
  RETURN COALESCE(NEW, OLD);
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER probe_change_trigger
AFTER INSERT OR UPDATE OR DELETE ON probes
FOR EACH ROW EXECUTE FUNCTION notify_probe_change();
