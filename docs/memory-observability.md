# Memory observability and heap profiling

Diagnostics for a dwctl process whose memory grows over time:

- **Allocator gauges** (always on, Linux) that separate live allocations from
  memory jemalloc holds for other reasons.
- **Completed-response writer accounting** (always on) for the bytes held by
  queued and in-flight persistence records.
- **Sampled heap profiles** (opt-in) that attribute live bytes to allocation
  stacks.

These are diagnostics. They do not change allocation, purge/decay, queue
admission or persistence behaviour, and none of them fixes a memory problem on
its own.

## Allocator gauges

A background task (`allocator-metrics-sampler`) advances jemalloc's statistics
epoch and reads its global statistics every
`background_services.allocator_metrics.sample_interval` (default `15s`, minimum
`1s`). It runs when `enable_metrics` and
`background_services.allocator_metrics.enabled` (default `true`) are set. It
never runs on a request path; on non-Linux targets it idles. No labels.

| Metric | jemalloc stat | Meaning |
| --- | --- | --- |
| `dwctl_jemalloc_allocated_bytes` | `stats.allocated` | Bytes in live allocations the application holds. |
| `dwctl_jemalloc_active_bytes` | `stats.active` | Bytes in pages backing live allocations. `active - allocated` is fragmentation inside those pages. |
| `dwctl_jemalloc_resident_bytes` | `stats.resident` | Physically resident allocator pages: active pages, metadata, and dirty pages awaiting decay. |
| `dwctl_jemalloc_metadata_bytes` | `stats.metadata` | Allocator bookkeeping (also counted in `resident`). |
| `dwctl_jemalloc_mapped_bytes` | `stats.mapped` | Address space in active extents. Virtual, not RAM. |
| `dwctl_jemalloc_retained_bytes` | `stats.retained` | Address space jemalloc kept after purging instead of unmapping. Virtual, **not** RAM. |
| `dwctl_jemalloc_stats_errors_total` | — | Failed samples. |
| `dwctl_jemalloc_stats_last_success_timestamp_seconds` | — | Unix time of the last good sample. |

### Interpreting them

Compare with the container's working set and RSS at comparable load:

| Observation | Likely meaning | Next step |
| --- | --- | --- |
| `allocated` grows with the working set | Live Rust objects are growing (load, caches, queues, or a leak). | Heap profiles: which stacks own the growth. |
| `allocated` flat, `active` grows | Fragmentation: live objects spread across more pages. | Profiles for the allocation-size mix; do not assume a leak. |
| `active` flat, `resident` stays above it | Dirty pages awaiting decay, or metadata. Should fall within seconds with background purging. | If it does not fall, check the decay configuration was applied. |
| Working set well above `resident` | Memory outside jemalloc (stacks, file-backed pages, other allocators). | Compare `/proc/<pid>/smaps_rollup`. |
| `retained` / `mapped` grow | Address-space footprint only. | Not a memory-pressure signal by itself. |

## Completed-response writer accounting

The completed-response writer channel is bounded by record count, and each
record owns its request and response bodies. These gauges measure the bytes
that design can hold during a burst:

| Metric | Definition |
| --- | --- |
| `dwctl_requests_writer_queued_records` | Records handed to the writer channel (including a sender waiting on a full channel) and not yet taken by the writer task. |
| `dwctl_requests_writer_queued_body_bytes` | `String::capacity()` of `request_body` + `response_body` for those records. |
| `dwctl_requests_writer_batch_records` | Records taken by the writer and held until the flush that handles them finishes (success, terminal failure or drop). |
| `dwctl_requests_writer_batch_body_bytes` | Body bytes of those records **plus** the body copies made for persistence while the copies exist, so during a flush it counts originals and copies. |

Other record strings (model, path, key, owner) are bounded and excluded. The
counters are adjusted by guards that unwind on every path (failed send, channel
dropped with records inside, cancellation, flush failure, success and shutdown
drain), and the gauges are set from those counters, so they return to zero
when the writer is idle. The existing `dwctl_requests_writer_channel_depth` is
sampled only at flush time; prefer `queued_records`.

## Heap profiling (opt-in)

Two independent switches, both required.

1. **Sampling** is decided by jemalloc at process start. jemalloc reads
   `_RJEM_MALLOC_CONF` after the compiled-in configuration, so

   ```text
   _RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19
   ```

   adds sampling without changing the compiled `background_thread` and decay
   settings. Without it (the default) jemalloc does no profiling work. The
   release binary is always built with profiling support compiled in.
   `lg_prof_sample:19` samples on average once per 512 KiB allocated.
2. **Serving**: `heap_profiling.enabled` (`DWCTL_HEAP_PROFILING__ENABLED=true`)
   starts a separate listener on `heap_profiling.bind_address` (default
   `0.0.0.0:6060`) with one route, `GET /debug/pprof/heap`. It returns a
   gzip-compressed pprof protobuf of sampled in-use allocations with
   symbolized stacks. The listener is not part of the main router; do not
   expose it through a Service or ingress, and restrict it with a NetworkPolicy
   where the cluster enforces them. Dumps are serialized and rate limited to one
   start per second (otherwise `429`), and bounded by `heap_profiling.dump_timeout` (default
   `30s`, `504` on timeout); `503` means sampling is not active.

At startup dwctl logs its jemalloc profiling state, and warns if the listener is
enabled while sampling is off.

| Metric | Type | Meaning |
| --- | --- | --- |
| `dwctl_heap_profiling_active` | gauge | `1` if the process started with sampling on and it is active. |
| `dwctl_heap_profiling_sample_interval_bytes` | gauge | Mean sampling interval (`2^lg_prof_sample`). |
| `dwctl_heap_profile_dumps_total{outcome}` | counter | `ok`, `error`, `timeout`, `busy`, `unavailable`. |
| `dwctl_heap_profile_dump_duration_seconds` | histogram | Dump plus pprof conversion time. |
| `dwctl_heap_profile_last_dump_bytes` | gauge | Size of the last successful profile. |

### Collecting profiles

The process holds no profiling credentials and makes no outbound calls. Two
ways to collect:

- **Continuous**: a pull-based agent such as Grafana Alloy's pprof scraper
  fetches `/debug/pprof/heap` on an interval and forwards to Pyroscope / Grafana
  Cloud Profiles. Periodic collection keeps evidence from before an OOM kill;
  do not rely on a capture at shutdown.
- **One-off**: from a machine with cluster access,

  ```sh
  kubectl port-forward pod/<pod> 6060:6060
  curl -sf -o heap-$(date +%s).pb.gz localhost:6060/debug/pprof/heap
  ```

### Analysing profiles

- `go tool pprof -top heap.pb.gz` or `go tool pprof -http=:8080 heap.pb.gz`.
- `python3 scripts/memory-validation/analyze_pprof.py heap.pb.gz --top 30`
  (no third-party dependencies). `--diff early.pb.gz late.pb.gz` shows which
  functions gained in-use bytes between two captures.
- In Pyroscope, select the `memory:inuse_space` profile type for the service,
  and compare two time ranges.

Compare profile totals with `dwctl_jemalloc_allocated_bytes` at the same time:
a profile explains `allocated`, not `resident` or fragmentation.

### Limits of sampled profiles

- They are statistical estimates, scaled from samples. Small or rare
  allocations can be missed; totals are approximate.
- They cover only allocations made while sampling was active. A process that
  started without `prof:true` has no record of earlier allocations, and
  enabling sampling later tells you nothing about them.
- They show in-use (not yet freed) memory at the moment of the dump, not
  allocation rate or freed memory.
- They contain stack frames, sizes and counts only. No request or response
  content is recorded.

## Overhead

Measured on Linux x86-64 with a release build. Treat these as indications,
not a benchmark of your workload.

**Always on (sampling off, the default).**

- Allocator sampler: one epoch refresh and six mallctl reads every 15s, about
  6 µs per sample in a release build, off the async runtime.
- Writer accounting: a few relaxed atomic operations and four gauge updates
  per record, under an uncontended per-writer mutex.
- jemalloc compiled with profiling support but started without `prof:true`
  adds a predictable branch on allocation paths. It was not separately
  benchmarked against a build without the `profiling` feature.

**Sampling on (`lg_prof_sample:19`).**

- CPU scales with bytes allocated, not with requests. In a synthetic
  allocation-only churn benchmark (4 threads, nothing but malloc/free), CPU rose
  about 45–50%; with `lg_prof_sample:23` it rose about 20%. A service spends
  a small fraction of its CPU in the allocator, so expect far less. Compare the
  profiled pod's CPU with its peers before drawing conclusions from it.
- Memory: a few MiB of sampling metadata, plus the in-process symbolizer's cache
  (parsed symbol and debug data for the binary), about 33 MiB for the dwctl
  release binary. The cache is built on the first dump and kept for the life of
  the process, and it shows up in later profiles under
  `backtrace::symbolize` / `jemalloc_pprof` frames. Exclude those frames when
  reading profiles.
- Dumps: the first took about 55 ms (building the symbolizer cache), later ones
  2–10 ms. Gzipped profiles were 5–8 KB for an idle server and grow with the
  number of distinct sampled stacks. Dumps are serialized and at most one
  starts per second; excess requests get `429`.
- Each dump writes a temporary file under `$TMPDIR` (default `/tmp`). On a
  read-only root filesystem, mount a writable `/tmp` or dumps fail with `500`.

Every stack's leaf is jemalloc's own `prof_backtrace` frame, so a flat
"top functions" view is uninformative. Use cumulative views, the call tree or a
flame graph.

## Validation

`scripts/memory-validation/` contains a Linux harness (`dwctl/examples/memory_validation.rs`)
that runs baseline → allocation burst → release → quiet period with and without
sampling. It records the allocator gauges, `/proc` RSS and anonymous memory, and
heap profiles at each phase, then checks that a deliberately retained
allocation stack is attributed in the profile. See its README.
