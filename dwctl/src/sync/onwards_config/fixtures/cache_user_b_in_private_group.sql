-- Add user B to the private group used by regular-private and composite-priority.
INSERT INTO user_groups (id, user_id, group_id)
VALUES
    (
        '10000000-0000-0000-0000-000000000002',
        '00000000-0000-0000-0000-0000000000b1',
        '00000000-0000-0000-0000-000000000aa1'
    );
