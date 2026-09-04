-- Recreate on the partitioned parent; Postgres builds and attaches a child
-- index on every existing partition, and the LIKE-based partition helper picks
-- them up again for future weeks.
CREATE INDEX IF NOT EXISTS idx_request_templates_g2_custom_id
    ON request_templates_g2 (custom_id);

CREATE INDEX IF NOT EXISTS idx_request_templates_g2_model
    ON request_templates_g2 (model);
