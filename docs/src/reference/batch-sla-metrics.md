# Aggregate batch SLA metrics

The optional batch SLA collector replaces database queries in monitoring tools
with aggregate metadata on the existing internal metrics endpoint. It performs
no queries on the scrape path and grants no new API permissions.

```yaml
enable_metrics: true
background_services:
  batch_sla_metrics:
    enabled: true                  # default false; opt in after rollout validation
    sample_interval: 30s           # minimum 5s
    query_timeout: 10s             # positive and less than sample_interval
    excluded_standard_models: []   # deployment policy; never exported as labels
```

One collector holds a schema-specific advisory lock on a dedicated physical
connection to the Fusillade **direct** database. Other replicas remain inactive.
A connection loss, timeout, query error or shutdown releases that connection.
The next successful collector rebuilds its snapshot from the database, so a
restart does not lose counts. Collection uses primary pools and the request
manager's transaction-local schema routing; main model metadata and Fusillade
metadata may reside in different databases. Do not point the direct connection
at a transaction pooler.

The whole collection attempt has a deadline, including acquiring connections,
reading the model catalogue and reading batch metadata. Each transaction also
sets a local PostgreSQL statement timeout. A failure retains the previous
snapshot and timestamp but sets collection health to zero. It does not stop the
application. A scrape atomically reads the complete snapshot, never a mixture
of collections. Successful replacement removes disappeared model series.

## Metric contract

All metrics are gauges. Scrape infrastructure supplies cluster, namespace, job
and instance labels. Collector health metrics have no application labels.

| Metric suffix (prefix `dwctl_batch_sla_`) | Meaning |
| --- | --- |
| `collector_active` | 1 on the replica with a successful collection while holding leadership |
| `collection_success` | 1 if the last collection attempt succeeded |
| `last_success_timestamp_seconds` | Database timestamp of the last complete successful snapshot; initially 0 |
| `affected_batches` | Number of batches meeting the selected alert policy |
| `max_lateness_seconds` | Maximum overdue duration, for flex and standard policies |
| `min_time_remaining_seconds` | Minimum remaining time, for near-deadline policy |

The last three metrics carry only `policy`, `completion_window`, and `model`.
A model is exported by name only if it is currently available to the Everyone
group in the model catalogue. Flex batches use the first model in lexical order;
private names become `_private` and missing metadata becomes `_unknown`. Standard
batches use `_all`. An empty flex/near policy emits zero with model `_none`.
Before the first successful snapshot and while a replica is on standby, all
three policies emit zero placeholders with model `_none`, including standard.
In those states `collector_active` and `collection_success` are 0 and the
last-success timestamp is 0. These placeholders do not establish that no batches
are affected: consumers must require successful, active, fresh collection as
described below. A failure after a successful snapshot retains the previous
values and timestamp with collection health set to 0.

No user IDs, batch IDs, file IDs, API keys, request/response bodies, custom IDs or
free-form error strings are exported. Queries select metadata only. This does
not sanitize other existing metrics, logs or traces.

## Alert eligibility

- `flex`, window `1h`: unfinished, uncancelled, nondeleted batches past deadline,
  without an age cutoff; plus late terminal batches created within 96 hours and
  finished within the last hour. Uses the latest terminal timestamp.
- `standard`, window `24h`: unfinished overdue batches, plus batches with any
  late terminal timestamp within the last hour, without a creation-age cutoff.
  Each batch counts once. A batch containing only configured excluded models
  is excluded; mixed models and missing metadata remain eligible.
- `near`, window `_all`: unfinished, nondeleted batches created within 48 hours,
  with a future deadline less than `max(10m, min(30m, lifetime / 48))` away.
  Cancelling batches remain eligible, matching the existing policy. Counts are
  distinct batches per public model; private models do not contribute.

Deadline and age boundaries are strict. An on-time failure or cancellation is
not an SLA violation. Metrics preserve these eligibility rules, while grouping
private flex models together intentionally reduces their identifying detail.

Alert consumers must require `collection_success == 1`, `collector_active == 1`,
`up == 1`, and a recent last-success timestamp. Select one fresh replica per
monitored database; **never sum snapshots across replicas**. Emit a separate
monitoring-unavailable alert when no fresh collector exists, including when all
series are absent. Allow at least the sampling interval, scrape interval and
alert evaluation interval when comparing an alert with a live SQL snapshot.

## Validation and rollout

The database tests compare counts and timing details against frozen prior SQL
queries using synthetic batches, including age/deadline boundaries, terminal
states, stuck batches, mixed/excluded models and missing templates. Other tests
exercise a blocked database query, recovery, leader contention/release, schema
isolation and query errors.

Deploy the released application with collection disabled first. Measure query
plans and duration against representative metadata before enabling the
collector. The active-batch branch is deliberately unbounded to detect batches
stuck for days; statement timeouts bound its cost but do not guarantee sufficient
capacity. Enable collection, compare aggregate snapshots and verify failure
alerting before switching monitoring rules. Revoke the monitoring system's
separate database access only after that cutover; this feature itself does not
change credentials, grants or network access.
