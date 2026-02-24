-- Make component-b endpoint URL invalid so it should be skipped during load.
UPDATE inference_endpoints
SET url = 'not-a-valid-url'
WHERE id = '30000000-0000-0000-0000-000000000002';
