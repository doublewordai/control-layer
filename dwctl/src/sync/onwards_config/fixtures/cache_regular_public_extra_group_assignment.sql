-- Add an overlapping group assignment to regular-public so user A matches both public and private paths.
INSERT INTO deployment_groups (id, deployment_id, group_id, granted_by)
VALUES
    (
        '60000000-0000-0000-0000-000000000099',
        '40000000-0000-0000-0000-000000000001',
        '00000000-0000-0000-0000-000000000aa1',
        '00000000-0000-0000-0000-000000000000'
    );
