-- Underway 0.2.0 Queue::dequeue; returning only the ID does not change selection.
with available_task as (
                select id
                from underway.task
                where task_queue_name = $1
                  and (
                      -- Find pending tasks...
                      state = $2
                      -- ...Or look for stalled tasks.
                      or (
                          state = $3
                          -- Has heartbeat stalled?
                          and last_heartbeat_at < now() - heartbeat
                          -- Are there remaining retries?
                          and (retry_policy).max_attempts > (
                              select count(*)
                              from underway.task_attempt
                              where task_queue_name = $1
                                and task_id = id
                          )
                      )
                  )
                  and created_at + delay <= now()
                order by
                  priority desc,
                  created_at,
                  id
                limit 1
                for update skip locked
            )
            update underway.task t
            set state = $3,
                last_attempt_at = now(),
                last_heartbeat_at = now()
            from available_task
            where t.task_queue_name = $1
              and t.id = available_task.id
            returning t.id;
