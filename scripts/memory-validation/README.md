# Memory validation harness

Tooling for validating dwctl's memory observability on Linux against a known
allocation workload: the always-on jemalloc gauges, the process's resident and
anonymous memory, and the opt-in sampled heap profiles. It is a diagnostic
harness, not a test of business logic.

The harness is only meaningful when dwctl uses jemalloc (Linux). The example
binary mirrors `dwctl/src/main.rs`'s global allocator and compiled
`_rjem_malloc_conf`, so the decay and background-thread behaviour under test is
the one that ships.

## Files

| File | Purpose |
| --- | --- |
| `../../dwctl/examples/memory_validation.rs` | The workload binary: phase validation and `--overhead` mode. |
| `run.sh` | Builds the example and runs the off/on scenarios, writing JSON to an output dir. |
| `analyze_pprof.py` | Dependency-free decoder/analyzer for the gzip pprof output. |
| `test_analyze_pprof.py` | Unit tests plus a handwritten protobuf encoder for test profiles. |
| `pyroscope_local.sh` | Downloads, starts and queries a local Pyroscope to smoke-test ingestion. |

## Quick start

```bash
# Build once (SQLX_OFFLINE=true, cargo on PATH).
export PATH="$HOME/.cargo/bin:$PATH"
export SQLX_OFFLINE=true
cargo build --release -p dwctl --example memory_validation

# Full validation: scenarios A/B and 3 overhead runs each.
scripts/memory-validation/run.sh
# faster local check:
QUIET_SECS=5 ITERATIONS=20000 scripts/memory-validation/run.sh

# Decode a profile.
scripts/memory-validation/analyze_pprof.py target/memory-validation/<stamp>/scenario-b-on/heap-quiet.pb.gz

# Assert the retained ballast is attributed in the quiet profile.
scripts/memory-validation/analyze_pprof.py \
  target/memory-validation/<stamp>/scenario-b-on/heap-quiet.pb.gz \
  --assert-function retain_validation_ballast --min-bytes 100000000
```

`run.sh` defaults: quiet phase 30 s, 4 threads, 50 000 churn iterations per
thread, output under `target/memory-validation/<UTC stamp>`. Override with
`OUT_DIR`, `QUIET_SECS`, `THREADS`, `ITERATIONS`, `CARGO`.

## Phases and what to look for

`memory_validation` (without `--overhead`) runs:

1. **baseline** — idle process. This is the floor.
2. **burst** — ~1 GiB of transient allocations in sizes from 64 B to 1 MiB
   across threads, plus ~256 MiB of retained ballast built by
   `retain_validation_ballast`.
3. **release** — the transient data is dropped; the ballast stays.
4. **quiet** — the ballast stays, no purge is forced, and the process samples
   every second for the configured duration.

At every phase boundary the summary records `read_allocator_stats()` (when the
implementation is present), `/proc/self/status` `VmRSS`, `/proc/self/smaps_rollup`
`Anonymous` and `VmHWM`, and dumps a heap profile when sampling is active.

The shape to expect from a healthy implementation:

- `allocated` tracks the live bytes: it rises ~1.25 GiB at burst, drops by
  ~1 GiB at release, and stays near the ballast size through quiet.
- `active`/`resident` rise at burst and then **fall during quiet** as jemalloc's
  5 s decay timers run. Resident may stay above `allocated` while dirty pages
  have not decayed yet; that gap is the retained-but-unused memory the gauges
  exist to expose.
- `retained` is virtual address space jemalloc keeps after purging; it is not
  resident memory and may *grow* through quiet as purged extents are retained
  for reuse.

If the implementation is unavailable (non-Linux, or the stubs in place), each
phase reports `allocator.available: false` with an error string and the harness
still records RSS. It does not crash.

## Reading the numbers: allocated vs active vs resident vs retained

These answer different questions and should not be conflated:

- **allocated** — bytes in live allocations the application currently holds.
  Freed objects are not counted.
- **active** — pages the allocator has assigned to size-class bins and is
  actively using (allocated plus fragmentation inside bins).
- **resident** — physically resident allocator pages: active pages, metadata,
  and dirty pages awaiting decay. The allocator's contribution to what the
  kernel and cgroup limit see.
- **metadata** — allocator bookkeeping, not application data.
- **mapped** — virtual address space reserved (not necessarily resident).
- **retained** — virtual address space jemalloc kept mapped after purging it,
  instead of unmapping it. It is **not** resident RAM. A resident reading above
  `active` while idle is explained by dirty pages awaiting decay (and metadata),
  not by `retained`.

`VmRSS` from `/proc` is the whole process, including runtime stacks, code,
mappings and allocator metadata; `Anonymous` from `smaps_rollup` excludes
file-backed mappings. Use them to cross-check the allocator gauges, not to
replace them.

## Heap profile limits

The sampled profiles are statistical estimates, not exact accounting:

- Sampling happens only while jemalloc sampling is active. The process must
  start with `_RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:N`;
  serving alone (`heap_profiling.enabled`) does not enable sampling. The
  compiled `_rjem_malloc_conf` (background thread, decay) is applied first, and
  the environment string only overrides the options it names.
- `lg_prof_sample:N` means one sample per 2^N bytes on average (N=19 is
  512 KiB). Small or short-lived allocations are therefore under-represented;
  the profile shows *where held bytes were allocated*, not a complete census.
- The dump reports **in-use** allocations. Allocation counts (`alloc_space`,
  `alloc_objects`) are only present if the producer includes them.
- Profiles contain stacks and byte counts only — never request bodies, headers
  or payload contents.

`analyze_pprof.py` uses the `inuse_space` sample type when present, prints flat
(leaf-frame) and cumulative (all-frames, counted once per stack) totals, and
reports the fraction of unsymbolized frames (empty names or raw hex addresses).
A high unsymbolized ratio means the dump was taken without symbolized stacks
and function-level assertions will not work. `--assert-function` matches the
exact function name, or a substring of a qualified symbol name if there is no
exact match.

## Profiling overhead

`--overhead` runs a fixed allocation-churn workload (mixed sizes, `CHURN_BATCH`
allocations per iteration, across threads) and reports wall time, user+sys CPU
from `/proc/self/stat` (USER_HZ is fixed at 100 on Linux, so no `libc` is
needed) and peak RSS from `VmHWM`. `run.sh` runs it three times with sampling
off and three times with sampling on and prints medians and percentage
differences for both wall time and CPU:

```
_RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19
```

The same binary and the same workload run in both cases; only the allocator
environment differs. `run.sh` reports both wall-time and user+sys CPU overhead
and their medians. Wall time is sensitive to how many cores are free, so when
the two disagree the CPU figure is the steadier signal. Treat the result as an
indication, not a benchmark: jemalloc sampling adds a map lookup and a stack
walk per sample, so the overhead scales with bytes allocated, not with request
count.

## Pyroscope smoke test

`pyroscope_local.sh` downloads a pinned Pyroscope release (currently 2.3.1,
linux/amd64) into `$HOME/.cache/dwctl-memory-validation/pyroscope`, verifies it
against the release's `checksums.txt`, starts a local server with filesystem
storage under a temp directory, ingests a pprof over `/ingest?format=pprof`,
queries it back over `/querier.v1.QuerierService/{ProfileTypes,SelectMergeStacktraces}`,
and asserts a function name is present. Nothing is written into the repository.
If the download fails, the script reports the exact curl/checksum error.

```bash
scripts/memory-validation/pyroscope_local.sh
scripts/memory-validation/pyroscope_local.sh \
  --profile target/memory-validation/<stamp>/scenario-b-on/heap-quiet.pb.gz \
  --assert-function retain_validation_ballast
```

## Local viewer check

`analyze_pprof.py` needs no external tools. If you have Go, the standard viewer
is `go tool pprof`; the older distro `pprof` binary can also load these files.
Check with:

```bash
command -v go pprof pyroscope
go tool pprof -top target/memory-validation/<stamp>/scenario-b-on/heap-quiet.pb.gz
```

`pprof` and `pyroscope` are not required for the harness; `analyze_pprof.py` is
the authoritative decoder for the assertions.
