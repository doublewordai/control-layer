-- A small active queue surrounded by old jobs, including an unused response
-- queue with pending jobs. The claim must constrain both queue and state.
INSERT INTO underway.task_queue(name) VALUES ('create-batch'), ('complete-response');
INSERT INTO underway.task(id, task_queue_name, input, state, created_at)
SELECT md5(n::text)::uuid,
       CASE WHEN n % 20 = 0 THEN 'create-batch' ELSE 'complete-response' END,
       '{}',
       CASE WHEN n % 20 <> 0 AND n % 3 = 0 THEN 'pending' ELSE 'succeeded' END::underway.task_state,
       now() - interval '1 day'
FROM generate_series(1, 100000) n;
INSERT INTO underway.task(id, task_queue_name, input, state, last_heartbeat_at)
SELECT md5(('active' || n)::text)::uuid, 'create-batch', '{}',
       CASE WHEN n % 2 = 0 THEN 'pending' ELSE 'in_progress' END::underway.task_state,
       now() - interval '1 minute'
FROM generate_series(1, 20) n;
ANALYZE underway.task;
ANALYZE underway.task_attempt;
