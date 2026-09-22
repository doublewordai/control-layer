-- Verify that the COR-673 database fence is usable and has exactly the shipped
-- definition. An exact comparison guards the key shape, access method, options,
-- absence of INCLUDE columns, and the predicate's boolean semantics.
DO $$
DECLARE
    index_row RECORD;
    expected_definition CONSTANT TEXT :=
        'CREATE UNIQUE INDEX uq_http_analytics_fusillade_success ON public.http_analytics USING btree (fusillade_request_id) WHERE ((fusillade_request_id IS NOT NULL) AND ((status_code >= 200) AND (status_code <= 299)) AND (request_origin = ''fusillade''::text))';
BEGIN
    SELECT
        index_catalog.indisvalid,
        index_catalog.indisready,
        pg_get_indexdef(index_relation.oid) AS definition
    INTO index_row
    FROM pg_class index_relation
    JOIN pg_index index_catalog
      ON index_catalog.indexrelid = index_relation.oid
    WHERE index_relation.oid = to_regclass('uq_http_analytics_fusillade_success')
      AND index_catalog.indrelid = 'http_analytics'::regclass;

    IF NOT FOUND
       OR NOT index_row.indisvalid
       OR NOT index_row.indisready
       OR index_row.definition <> expected_definition
    THEN
        RAISE EXCEPTION
            'uq_http_analytics_fusillade_success is missing, invalid, or has the wrong definition (actual: %, expected: %)',
            index_row.definition,
            expected_definition;
    END IF;
END
$$;

COMMENT ON INDEX uq_http_analytics_fusillade_success IS
    'Allows one successful analytics row per trusted queued Fusillade request; realtime correlation IDs are excluded';
