-- Allow the 'continuation' API key purpose.
--
-- Mid-stream continuation issues resume legs on a hidden global
-- 'continuation'-purpose key (see dwctl/src/continuation/): the purpose is the
-- label onwards' model_traffic_rules matches on to steer resume traffic to a
-- model's continuation composite. The purpose CHECK constraint (migration 043)
-- predates it and would reject the key's insertion.

ALTER TABLE api_keys DROP CONSTRAINT api_keys_purpose_check;
ALTER TABLE api_keys
ADD CONSTRAINT api_keys_purpose_check
CHECK (purpose IN ('platform', 'inference', 'realtime', 'batch', 'playground', 'continuation'));
