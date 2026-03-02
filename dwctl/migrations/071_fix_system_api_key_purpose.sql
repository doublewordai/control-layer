-- Fix system API key purpose: should be 'platform' for admin API access.
-- Migration 026 defaulted all keys to 'inference', and migration 043 converted
-- non-hidden 'inference' keys to 'realtime'. The system key was never set to
-- 'platform', causing 401s for internal services (e.g. scouter) that call
-- admin endpoints.
UPDATE api_keys
SET purpose = 'platform'
WHERE id = '00000000-0000-0000-0000-000000000000';
