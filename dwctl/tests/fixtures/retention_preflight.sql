-- Minimal metadata plus large route tables: retired dates have no live routes.
CREATE TABLE retained_response_buckets (
    delete_on date PRIMARY KEY, state text, state_changed_at timestamptz,
    partition_schema text, partition_table text, partition_oid oid
);
CREATE TABLE retention_partition_retirements (
    parent_table text, partition_schema text, partition_table text,
    partition_schema_oid oid, parent_oid oid, partition_oid oid,
    lower_bound date, upper_bound date, completed_at timestamptz
);
CREATE TABLE retained_response_group_routes (group_id uuid PRIMARY KEY, delete_on date NOT NULL);
CREATE INDEX group_routes_bucket ON retained_response_group_routes (delete_on, group_id);
CREATE TABLE retained_response_request_routes (request_id uuid PRIMARY KEY, delete_on date NOT NULL);
CREATE INDEX request_routes_bucket ON retained_response_request_routes (delete_on, request_id);
INSERT INTO retained_response_buckets VALUES
    ('2026-09-01', 'retired', '2026-09-02 UTC', 'public', 'retained_response_objects_d20260901', 0),
    ('2026-10-01', 'active', '2026-09-02 UTC', 'public', 'retained_response_objects_d20261001', 0);
INSERT INTO retention_partition_retirements
SELECT 'retained_response_objects', partition_schema, partition_table, 0, 0, partition_oid,
       delete_on, delete_on + 1, state_changed_at
FROM retained_response_buckets WHERE state = 'retired';
INSERT INTO retained_response_group_routes
SELECT md5(i::text)::uuid, date '2026-10-01' + (i % 10) FROM generate_series(1, 100000) i;
INSERT INTO retained_response_request_routes SELECT group_id, delete_on FROM retained_response_group_routes;
ANALYZE retained_response_buckets;
ANALYZE retention_partition_retirements;
ANALYZE retained_response_group_routes;
ANALYZE retained_response_request_routes;
