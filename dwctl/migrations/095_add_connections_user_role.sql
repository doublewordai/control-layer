-- Add ConnectionsUser role for external data source connections
-- This role allows users to create, manage, and sync their own connections.
-- Given to early testers of the connections feature.

-- sqlx:no-transaction

ALTER TYPE user_role ADD VALUE IF NOT EXISTS 'CONNECTIONSUSER';
