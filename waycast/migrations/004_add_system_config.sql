-- Add system configuration table to track seeding status
CREATE TABLE system_config (
    key VARCHAR PRIMARY KEY,
    value BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Insert initial seeding flag as false (not yet seeded)
INSERT INTO system_config (key, value) VALUES ('endpoints_seeded', false);