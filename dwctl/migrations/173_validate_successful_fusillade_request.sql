-- Verify that the COR-673 database fence is usable and has not been replaced
-- with a broader predicate that would deduplicate ordinary realtime traffic.
DO $$
DECLARE
    index_row RECORD;
BEGIN
    SELECT
        index_catalog.indisvalid,
        index_catalog.indisready,
        index_catalog.indisunique,
        index_catalog.indnkeyatts,
        indexed_column.attname AS indexed_column,
        access_method.amname AS access_method,
        pg_get_expr(index_catalog.indpred, index_catalog.indrelid) AS predicate
    INTO index_row
    FROM pg_class index_relation
    JOIN pg_index index_catalog
      ON index_catalog.indexrelid = index_relation.oid
    JOIN pg_class table_relation
      ON table_relation.oid = index_catalog.indrelid
    JOIN pg_am access_method
      ON access_method.oid = index_relation.relam
    JOIN pg_attribute indexed_column
      ON indexed_column.attrelid = table_relation.oid
     AND indexed_column.attnum = index_catalog.indkey[0]
    WHERE index_relation.oid = to_regclass('uq_http_analytics_fusillade_success')
      AND table_relation.oid = 'http_analytics'::regclass;

    IF NOT FOUND
       OR NOT index_row.indisvalid
       OR NOT index_row.indisready
       OR NOT index_row.indisunique
       OR index_row.indnkeyatts <> 1
       OR index_row.indexed_column <> 'fusillade_request_id'
       OR index_row.access_method <> 'btree'
       OR index_row.predicate IS NULL
       OR position('fusillade_request_id IS NOT NULL' IN index_row.predicate) = 0
       OR position('status_code >= 200' IN index_row.predicate) = 0
       OR position('status_code <= 299' IN index_row.predicate) = 0
       OR position('request_origin = ''fusillade''::text' IN index_row.predicate) = 0
       OR position('batch_sla <> ''''::text' IN index_row.predicate) = 0
    THEN
        RAISE EXCEPTION
            'uq_http_analytics_fusillade_success is missing, invalid, or has the wrong definition';
    END IF;
END
$$;

COMMENT ON INDEX uq_http_analytics_fusillade_success IS
    'Allows one successful analytics row per queued Fusillade request; realtime correlation IDs are excluded';
