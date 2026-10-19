-- Keep application-owned task indexes separate from the dependency's migration
-- history. Underway validates its own history when older replicas start.
CREATE SCHEMA IF NOT EXISTS underway_extensions;
