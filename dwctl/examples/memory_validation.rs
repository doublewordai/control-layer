//! Linux validation harness for dwctl memory observability.
//!
//! Runs a controlled allocation workload and records, at each phase boundary,
//! jemalloc's statistics (via [`dwctl::allocator_metrics::read_allocator_stats`]),
//! the process's resident and anonymous memory from `/proc/self/*`, and an
//! optional heap profile (via [`dwctl::profiling::dump_heap_profile_pprof`]).
//! The point is to validate the diagnostics against a known workload:
//!
//! * `baseline`  — idle process, before any validation allocations.
//! * `burst`     — ~1 GiB of transient data plus ~256 MiB of retained ballast
//!   built by [`retain_validation_ballast`].
//! * `release`   — transient data dropped, ballast still held.
//! * `quiet`     — no forced purge; sample every second while the allocator's
//!   decay timers run.
//!
//! `--overhead` instead runs a fixed allocation-churn workload and reports wall
//! time, user+sys CPU and peak RSS, so it can be compared with and without
//! jemalloc sampling (`_RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19`).
//!
//! This example is a separate binary, so it declares jemalloc as its global
//! allocator and exports the same `_rjem_malloc_conf` as the `dwctl` binary.
//! It must be built with the same allocator settings as production, otherwise
//! the decay and background-thread behaviour it validates is not the one that
//! ships. See `scripts/memory-validation/README.md`.
//!
//! Run with: `cargo run --release -p dwctl --example memory_validation -- --help`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use dwctl::allocator_metrics::read_allocator_stats;
use dwctl::profiling::{HeapProfilingStatus, dump_heap_profile_pprof, heap_profiling_status};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Allocator setup. Must mirror `dwctl/src/main.rs` exactly.
// ---------------------------------------------------------------------------

// jemalloc only when it is the production allocator. See main.rs for why Linux
// chooses jemalloc over glibc: decay returns dirty pages to the OS on a timer
// without the application asking.
#[cfg(target_os = "linux")]
#[global_allocator]
static VALIDATION_ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// Copied verbatim from `dwctl/src/main.rs`. Keep the two in sync: the harness
// only tells you about the production allocator if the allocator starts with
// the same `background_thread` and decay settings. `_RJEM_MALLOC_CONF` is
// applied after this string and overrides only the options it names, so a
// profiling run can add `prof:true` without dropping these.
//
// `#[used]` and the `_rjem_` symbol name are both load-bearing; see main.rs.
#[cfg(target_os = "linux")]
#[used]
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static malloc_conf: &[u8] = b"background_thread:true,dirty_decay_ms:5000,muzzy_decay_ms:5000\0";

// ---------------------------------------------------------------------------
// Workload constants
// ---------------------------------------------------------------------------

/// Transient data allocated during the burst phase, across all threads.
const TRANSIENT_BURST_BYTES: usize = 1024 * 1024 * 1024;
/// Retained ballast built by [`retain_validation_ballast`]; held through the
/// quiet phase so a sampled profile can attribute live bytes to it.
const RETAINED_BALLAST_BYTES: usize = 256 * 1024 * 1024;
/// Allocation sizes for the transient burst: 64 B .. 1 MiB, like request
/// bodies and response buffers.
const TRANSIENT_SIZES: [usize; 10] = [64, 256, 1024, 4096, 8192, 16 * 1024, 64 * 1024, 128 * 1024, 512 * 1024, 1024 * 1024];
/// Allocation sizes for the retained ballast; larger chunks keep the number of
/// live allocations (and thus bookkeeping) small.
const BALLAST_SIZES: [usize; 3] = [1024 * 1024, 2 * 1024 * 1024, 4 * 1024 * 1024];
/// Mixed sizes for the `--overhead` churn workload.
const CHURN_SIZES: [usize; 7] = [64, 256, 1024, 4096, 16 * 1024, 64 * 1024, 256 * 1024];
/// Allocations per churn iteration.
const CHURN_BATCH: usize = 8;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Validate dwctl's memory diagnostics against a controlled allocation workload.
#[derive(Debug, Parser)]
#[command(name = "memory_validation", about)]
struct Args {
    /// Directory for the JSON summary and heap profile dumps.
    #[arg(long, default_value = "memory-validation-out")]
    out: PathBuf,

    /// Seconds to spend in the quiet phase, sampling once per second.
    #[arg(long, default_value_t = 30)]
    quiet_secs: u64,

    /// Number of OS threads that share the burst and churn workloads.
    #[arg(long, default_value_t = 4)]
    threads: usize,

    /// Timeout for one heap profile dump, in seconds.
    #[arg(long, default_value_t = 30)]
    dump_timeout_secs: u64,

    /// Run the fixed allocation-churn overhead workload instead of the
    /// phase-based validation.
    #[arg(long)]
    overhead: bool,

    /// Churn iterations per thread in `--overhead` mode.
    #[arg(long, default_value_t = 50_000)]
    iterations: usize,
}

// ---------------------------------------------------------------------------
// /proc readers
// ---------------------------------------------------------------------------

/// Resident set size in bytes, from `/proc/self/status` `VmRSS:`.
fn read_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    read_status_kb(&status, "VmRSS:")
}

/// Peak resident set size in bytes, from `/proc/self/status` `VmHWM:`.
fn read_peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    read_status_kb(&status, "VmHWM:")
}

/// Anonymous (not file-backed) memory in bytes, from `/proc/self/smaps_rollup`.
fn read_anonymous_bytes() -> Option<u64> {
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    read_status_kb(&rollup, "Anonymous:")
}

fn read_status_kb(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

/// `(utime, stime)` in clock ticks from `/proc/self/stat`. Linux fixes USER_HZ
/// at 100, so no `sysconf(_SC_CLK_TCK)` (and therefore no `libc`) is needed.
fn read_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // `comm` may contain spaces and parentheses, so skip past the last ')'.
    let rest = stat.rfind(')').map(|i| &stat[i + 1..])?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After `comm` the fields are state, ppid, ...; utime is field 14 and
    // stime is field 15 of the full line, i.e. offsets 11 and 12 here.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime.saturating_add(stime))
}

fn ticks_to_millis(ticks: u64) -> u64 {
    ticks.saturating_mul(10)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

/// Build the transient burst partition: `total_bytes` of live allocations with
/// sizes cycling through [`TRANSIENT_SIZES`]. Returns the allocations so the
/// caller controls when they are freed.
#[inline(never)]
fn build_transient_partition(total_bytes: usize, seed: usize) -> Vec<Vec<u8>> {
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    let mut i = seed;
    while total < total_bytes {
        let size = TRANSIENT_SIZES[i % TRANSIENT_SIZES.len()];
        let fill = ((i.wrapping_mul(31).wrapping_add(seed)) & 0xff) as u8;
        let mut chunk = Vec::with_capacity(size);
        chunk.resize(size, fill);
        total += size;
        chunks.push(chunk);
        i += 1;
    }
    chunks
}

/// Build the ~256 MiB retained ballast. `#[inline(never)]` and the stable name
/// matter: the heap profile assertions and `scripts/memory-validation` look for
/// `retain_validation_ballast` by name.
#[inline(never)]
fn retain_validation_ballast() -> Vec<Vec<u8>> {
    let mut ballast: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    let mut i = 0usize;
    while total < RETAINED_BALLAST_BYTES {
        let size = BALLAST_SIZES[i % BALLAST_SIZES.len()];
        let mut chunk = Vec::with_capacity(size);
        chunk.resize(size, (i & 0xff) as u8);
        total += size;
        ballast.push(chunk);
        i += 1;
    }
    ballast
}

/// One partition of the fixed `--overhead` churn: repeated batches of mixed-size
/// allocations, each freed before the next iteration. The same function runs in
/// both the sampling-off and sampling-on processes so the work is identical.
#[inline(never)]
fn churn_partition(iterations: usize) {
    let mut checksum = 0u8;
    for i in 0..iterations {
        let mut batch: Vec<Vec<u8>> = Vec::with_capacity(CHURN_BATCH);
        for j in 0..CHURN_BATCH {
            let size = CHURN_SIZES[(i + j) % CHURN_SIZES.len()];
            let fill = ((i ^ j) & 0xff) as u8;
            let mut chunk = Vec::with_capacity(size);
            chunk.resize(size, fill);
            checksum = checksum.wrapping_add(chunk[size - 1]);
            batch.push(chunk);
        }
        std::hint::black_box(&batch);
        drop(batch);
    }
    std::hint::black_box(checksum);
}

/// Split `total` into `threads` partitions, none empty when possible.
fn partition(total: usize, threads: usize) -> Vec<usize> {
    let threads = threads.max(1);
    let base = total / threads;
    let remainder = total % threads;
    (0..threads).map(|i| base + usize::from(i < remainder)).collect()
}

// ---------------------------------------------------------------------------
// Snapshots and JSON model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize)]
struct StatsJson {
    allocated: u64,
    active: u64,
    resident: u64,
    metadata: u64,
    mapped: u64,
    retained: u64,
}

#[derive(Debug, Serialize)]
struct AllocatorSnapshot {
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<StatsJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn allocator_snapshot() -> AllocatorSnapshot {
    match read_allocator_stats() {
        Ok(stats) => AllocatorSnapshot {
            available: true,
            stats: Some(StatsJson {
                allocated: stats.allocated,
                active: stats.active,
                resident: stats.resident,
                metadata: stats.metadata,
                mapped: stats.mapped,
                retained: stats.retained,
            }),
            error: None,
        },
        Err(error) => AllocatorSnapshot {
            available: false,
            stats: None,
            error: Some(error.to_string()),
        },
    }
}

#[derive(Debug, Serialize)]
struct ProfilingStatusJson {
    compiled: bool,
    enabled: bool,
    active: bool,
    lg_sample: Option<u32>,
}

impl From<HeapProfilingStatus> for ProfilingStatusJson {
    fn from(status: HeapProfilingStatus) -> Self {
        Self {
            compiled: status.compiled,
            enabled: status.enabled,
            active: status.active,
            lg_sample: status.lg_sample,
        }
    }
}

#[derive(Debug, Serialize)]
struct ProfileOutcome {
    attempted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct PhaseSnapshot {
    phase: String,
    elapsed_ms: u64,
    timestamp_unix_ms: u64,
    allocator: AllocatorSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anonymous_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_rss_bytes: Option<u64>,
    profile: ProfileOutcome,
}

#[derive(Debug, Serialize)]
struct QuietSample {
    elapsed_ms: u64,
    timestamp_unix_ms: u64,
    allocator: AllocatorSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anonymous_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
struct PhaseSummary {
    mode: &'static str,
    started_unix_ms: u64,
    duration_ms: u64,
    quiet_seconds: u64,
    threads: usize,
    out_dir: String,
    profiling: ProfilingStatusJson,
    jemalloc_conf: Option<String>,
    phases: Vec<PhaseSnapshot>,
    quiet_samples: Vec<QuietSample>,
}

#[derive(Debug, Serialize)]
struct OverheadSummary {
    mode: &'static str,
    timestamp_unix_ms: u64,
    iterations_per_thread: usize,
    threads: usize,
    wall_ms: u64,
    user_sys_ms: u64,
    peak_rss_bytes: Option<u64>,
    rss_before_bytes: Option<u64>,
    profiling: ProfilingStatusJson,
    jemalloc_conf: Option<String>,
    allocator: AllocatorSnapshot,
}

fn jemalloc_conf() -> Option<String> {
    std::env::var("_RJEM_MALLOC_CONF").ok()
}

// ---------------------------------------------------------------------------
// Phase runner
// ---------------------------------------------------------------------------

async fn capture_profile(out_dir: &Path, phase: &str, status: HeapProfilingStatus, timeout: Duration) -> ProfileOutcome {
    if !status.active {
        return ProfileOutcome {
            attempted: false,
            path: None,
            bytes: None,
            error: None,
        };
    }
    match dump_heap_profile_pprof(timeout).await {
        Ok(bytes) => {
            let path = out_dir.join(format!("heap-{phase}.pb.gz"));
            match std::fs::write(&path, &bytes) {
                Ok(()) => ProfileOutcome {
                    attempted: true,
                    path: Some(path.display().to_string()),
                    bytes: Some(bytes.len() as u64),
                    error: None,
                },
                Err(error) => ProfileOutcome {
                    attempted: true,
                    path: Some(path.display().to_string()),
                    bytes: Some(bytes.len() as u64),
                    error: Some(format!("failed to write profile: {error}")),
                },
            }
        }
        Err(error) => ProfileOutcome {
            attempted: true,
            path: None,
            bytes: None,
            error: Some(error.to_string()),
        },
    }
}

#[allow(clippy::too_many_lines)]
async fn run_phases(args: &Args) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.out)?;
    let status = heap_profiling_status();
    let dump_timeout = Duration::from_secs(args.dump_timeout_secs);
    let started_unix_ms = unix_millis();
    let started = Instant::now();

    let mut phases: Vec<PhaseSnapshot> = Vec::new();
    let mut quiet_samples: Vec<QuietSample> = Vec::new();

    let snapshot = |phase: &str, allocator: AllocatorSnapshot, profile: ProfileOutcome| -> PhaseSnapshot {
        PhaseSnapshot {
            phase: phase.to_string(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            timestamp_unix_ms: unix_millis(),
            allocator,
            rss_bytes: read_rss_bytes(),
            anonymous_bytes: read_anonymous_bytes(),
            peak_rss_bytes: read_peak_rss_bytes(),
            profile,
        }
    };

    // Baseline: nothing validation-specific allocated yet.
    let baseline_profile = capture_profile(&args.out, "baseline", status, dump_timeout).await;
    phases.push(snapshot("baseline", allocator_snapshot(), baseline_profile));

    // Burst: ~1 GiB transient across threads plus the retained ballast. Each
    // thread builds and returns its partition; all partitions are held live
    // until the burst snapshot is taken.
    let threads = args.threads.max(1);
    let transient_per_thread: Vec<usize> = partition(TRANSIENT_BURST_BYTES, threads);
    let mut transient_chunks: Vec<Vec<u8>> = Vec::new();
    {
        let mut handles = Vec::with_capacity(threads);
        for (seed, &bytes) in transient_per_thread.iter().enumerate() {
            handles.push(tokio::task::spawn_blocking(move || build_transient_partition(bytes, seed)));
        }
        for handle in handles {
            match handle.await {
                Ok(mut chunks) => transient_chunks.append(&mut chunks),
                Err(error) => eprintln!("transient allocation task failed: {error}"),
            }
        }
    }
    let ballast = retain_validation_ballast();
    std::hint::black_box(&ballast);

    let burst_profile = capture_profile(&args.out, "burst", status, dump_timeout).await;
    phases.push(snapshot("burst", allocator_snapshot(), burst_profile));

    // Release: drop the transient data; keep the ballast.
    drop(transient_chunks);
    std::hint::black_box(&ballast);
    let release_profile = capture_profile(&args.out, "release", status, dump_timeout).await;
    phases.push(snapshot("release", allocator_snapshot(), release_profile));

    // Quiet: no forced purge. Let the decay timers run and sample every second.
    let quiet_started = Instant::now();
    let quiet_deadline = Duration::from_secs(args.quiet_secs);
    loop {
        let elapsed = quiet_started.elapsed();
        if elapsed >= quiet_deadline {
            break;
        }
        let remaining = quiet_deadline.saturating_sub(elapsed);
        tokio::time::sleep(remaining.min(Duration::from_secs(1))).await;
        quiet_samples.push(QuietSample {
            elapsed_ms: quiet_started.elapsed().as_millis() as u64,
            timestamp_unix_ms: unix_millis(),
            allocator: allocator_snapshot(),
            rss_bytes: read_rss_bytes(),
            anonymous_bytes: read_anonymous_bytes(),
        });
    }

    // Keep the ballast live across the quiet phase and the final dump.
    std::hint::black_box(&ballast);
    let quiet_profile = capture_profile(&args.out, "quiet", status, dump_timeout).await;
    phases.push(snapshot("quiet", allocator_snapshot(), quiet_profile));

    let summary = PhaseSummary {
        mode: "phases",
        started_unix_ms,
        duration_ms: started.elapsed().as_millis() as u64,
        quiet_seconds: args.quiet_secs,
        threads,
        out_dir: args.out.display().to_string(),
        profiling: status.into(),
        jemalloc_conf: jemalloc_conf(),
        phases,
        quiet_samples,
    };
    let json = serde_json::to_string_pretty(&summary)?;
    std::fs::write(args.out.join("summary.json"), &json)?;
    println!("{json}");
    Ok(())
}

async fn run_overhead(args: &Args) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.out)?;
    let status = heap_profiling_status();
    let threads = args.threads.max(1);
    let rss_before = read_rss_bytes();

    let started = Instant::now();
    let cpu_before = read_cpu_ticks().unwrap_or(0);
    let mut handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let iterations = args.iterations;
        handles.push(tokio::task::spawn_blocking(move || churn_partition(iterations)));
    }
    let mut join_failures = 0usize;
    for handle in handles {
        match handle.await {
            Ok(()) => {}
            Err(error) => {
                join_failures += 1;
                eprintln!("churn task failed: {error}");
            }
        }
    }
    let cpu_after = read_cpu_ticks().unwrap_or(0);
    let wall = started.elapsed();

    let summary = OverheadSummary {
        mode: "overhead",
        timestamp_unix_ms: unix_millis(),
        iterations_per_thread: args.iterations,
        threads,
        wall_ms: wall.as_millis() as u64,
        user_sys_ms: ticks_to_millis(cpu_after.saturating_sub(cpu_before)),
        peak_rss_bytes: read_peak_rss_bytes(),
        rss_before_bytes: rss_before,
        profiling: status.into(),
        jemalloc_conf: jemalloc_conf(),
        allocator: allocator_snapshot(),
    };
    let json = serde_json::to_string_pretty(&summary)?;
    std::fs::write(args.out.join("overhead.json"), &json)?;
    println!("{json}");
    if join_failures > 0 {
        anyhow::bail!("{join_failures} churn task(s) failed");
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.overhead {
        run_overhead(&args).await
    } else {
        run_phases(&args).await
    }
}
