//! jemalloc allocator statistics exported as Prometheus gauges.
//!
//! On Linux the dwctl binary uses jemalloc as its global allocator (see
//! `src/main.rs`), so these statistics describe the memory behaviour of the
//! whole process. On other targets there is no jemalloc to read and the sampler
//! idles; [`read_allocator_stats`] returns [`AllocatorStatsError::Unsupported`].
//!
//! One snapshot is a consistent read of jemalloc's global stats taken after
//! advancing the jemalloc epoch (many of the counters are cached and only
//! refresh when the epoch advances). The numbers mean:
//!
//! - `allocated` — live bytes the application currently holds. Growth here
//!   means live objects are growing; use a heap profile to find the stacks.
//! - `active` — bytes in pages jemalloc has carved out for the application.
//!   `active - allocated` is page-level fragmentation: allocated run space not
//!   currently occupied by live objects.
//! - `resident` — bytes in physically resident pages jemalloc maps. It includes
//!   allocator metadata and dirty/muzzy pages that have not yet been purged on
//!   the decay timer. `resident - active` is approximately those dirty pages
//!   plus metadata; it shrinks on its own as the background thread purges.
//! - `metadata` — bytes jemalloc keeps for its own bookkeeping. It is also
//!   included in `resident`.
//! - `mapped` — bytes of address space in active extents. This is virtual
//!   address space, not physical RAM.
//! - `retained` — virtual address space jemalloc kept mapped but purged or left
//!   untouched instead of returning to the OS (`munmap`). Retained memory is
//!   deliberately *not* resident RAM.
//!
//! Read them together: flat `allocated` with growing `active`/`resident`
//! indicates fragmentation or unpurged pages rather than a leak, while growing
//! `allocated` is a live-heap leak or legitimate load growth. `mapped` and
//! `retained` track address-space footprint and are not a memory-pressure
//! signal by themselves.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::metrics::errors::{component, record as record_background_error};

/// Allocator statistics sampling configuration.
///
/// Env: `DWCTL_BACKGROUND_SERVICES__ALLOCATOR_METRICS__ENABLED`,
/// `DWCTL_BACKGROUND_SERVICES__ALLOCATOR_METRICS__SAMPLE_INTERVAL`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AllocatorMetricsConfig {
    /// Sample jemalloc statistics in the background (default: true). Only has
    /// an effect on Linux, where jemalloc is the global allocator.
    pub enabled: bool,
    /// How often to refresh the jemalloc epoch and read statistics (default: 15s).
    #[serde(with = "humantime_serde")]
    pub sample_interval: Duration,
}

impl Default for AllocatorMetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval: Duration::from_secs(15),
        }
    }
}

/// One consistent snapshot of jemalloc's global statistics, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllocatorStats {
    pub allocated: u64,
    pub active: u64,
    pub resident: u64,
    pub metadata: u64,
    pub mapped: u64,
    pub retained: u64,
}

/// Why statistics could not be read.
#[derive(Debug, thiserror::Error)]
pub enum AllocatorStatsError {
    /// Not built with jemalloc as the global allocator (non-Linux targets).
    #[error("allocator statistics are unsupported on this platform")]
    Unsupported,
    /// A mallctl call failed.
    #[error("jemalloc mallctl failed: {0}")]
    Ctl(String),
}

/// Advance the jemalloc epoch and read a fresh snapshot. Blocking but cheap;
/// never call it on a request path.
pub fn read_allocator_stats() -> Result<AllocatorStats, AllocatorStatsError> {
    #[cfg(target_os = "linux")]
    {
        imp::read()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(AllocatorStatsError::Unsupported)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{AllocatorStats, AllocatorStatsError};
    use std::sync::OnceLock;

    use tikv_jemalloc_ctl::{epoch, epoch_mib, stats};

    /// MIBs for every mallctl operation the sampler performs. Resolving a MIB is
    /// a string lookup plus a mallctl round-trip, so they are resolved once and
    /// reused for the life of the process. The MIB types are plain `Copy` data
    /// and safe to share.
    #[derive(Clone, Copy)]
    struct Mibs {
        epoch: epoch_mib,
        allocated: stats::allocated_mib,
        active: stats::active_mib,
        resident: stats::resident_mib,
        metadata: stats::metadata_mib,
        mapped: stats::mapped_mib,
        retained: stats::retained_mib,
    }

    // Caching the failure too: if MIB resolution fails there is no point retrying
    // the same name lookups on every sample.
    static MIBS: OnceLock<Result<Mibs, String>> = OnceLock::new();

    fn mibs() -> Result<&'static Mibs, AllocatorStatsError> {
        match MIBS.get_or_init(|| Mibs::resolve().map_err(|e| e.to_string())) {
            Ok(mibs) => Ok(mibs),
            Err(err) => Err(AllocatorStatsError::Ctl(err.clone())),
        }
    }

    impl Mibs {
        fn resolve() -> Result<Self, tikv_jemalloc_ctl::Error> {
            Ok(Self {
                epoch: epoch::mib()?,
                allocated: stats::allocated::mib()?,
                active: stats::active::mib()?,
                resident: stats::resident::mib()?,
                metadata: stats::metadata::mib()?,
                mapped: stats::mapped::mib()?,
                retained: stats::retained::mib()?,
            })
        }
    }

    fn ctl(err: tikv_jemalloc_ctl::Error) -> AllocatorStatsError {
        AllocatorStatsError::Ctl(err.to_string())
    }

    pub(super) fn read() -> Result<AllocatorStats, AllocatorStatsError> {
        let mibs = *mibs()?;

        // Statistics are cached; advancing the epoch first is what makes the
        // reads below reflect the current heap state.
        mibs.epoch.advance().map_err(ctl)?;

        Ok(AllocatorStats {
            allocated: mibs.allocated.read().map_err(ctl)? as u64,
            active: mibs.active.read().map_err(ctl)? as u64,
            resident: mibs.resident.read().map_err(ctl)? as u64,
            metadata: mibs.metadata.read().map_err(ctl)? as u64,
            mapped: mibs.mapped.read().map_err(ctl)? as u64,
            retained: mibs.retained.read().map_err(ctl)? as u64,
        })
    }
}

const ALLOCATED: &str = "dwctl_jemalloc_allocated_bytes";
const ACTIVE: &str = "dwctl_jemalloc_active_bytes";
const RESIDENT: &str = "dwctl_jemalloc_resident_bytes";
const METADATA: &str = "dwctl_jemalloc_metadata_bytes";
const MAPPED: &str = "dwctl_jemalloc_mapped_bytes";
const RETAINED: &str = "dwctl_jemalloc_retained_bytes";
const STATS_ERRORS: &str = "dwctl_jemalloc_stats_errors_total";
const LAST_SUCCESS: &str = "dwctl_jemalloc_stats_last_success_timestamp_seconds";

/// Minimum sampling interval. Sub-second polling of mallctl buys nothing and
/// burns a blocking pool thread more often than necessary.
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// At most one failure log per this window, so a permanently broken mallctl
/// cannot spam the log.
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(300);

/// Describe every metric this module emits. Descriptions are registered once on
/// sampler startup so the exporter knows about them before the first sample.
fn describe_allocator_metrics() {
    use metrics::Unit;

    metrics::describe_gauge!(
        ALLOCATED,
        Unit::Bytes,
        "Live bytes currently allocated by the application (jemalloc stats.allocated). Growth means live objects are growing."
    );
    metrics::describe_gauge!(
        ACTIVE,
        Unit::Bytes,
        "Bytes in active pages jemalloc allocated from the OS (jemalloc stats.active). active - allocated is page-level fragmentation."
    );
    metrics::describe_gauge!(
        RESIDENT,
        Unit::Bytes,
        "Physically resident pages mapped by jemalloc (jemalloc stats.resident). Includes allocator metadata and dirty pages not yet purged; resident - active is dirty/muzzy pages awaiting decay plus metadata."
    );
    metrics::describe_gauge!(
        METADATA,
        Unit::Bytes,
        "Bytes jemalloc uses for its own metadata (jemalloc stats.metadata). Also included in resident."
    );
    metrics::describe_gauge!(
        MAPPED,
        Unit::Bytes,
        "Address space in active extents mapped by jemalloc (jemalloc stats.mapped). Virtual address space, not physical RAM."
    );
    metrics::describe_gauge!(
        RETAINED,
        Unit::Bytes,
        "Virtual address space jemalloc kept mapped but purged/untouched rather than returning to the OS (jemalloc stats.retained). Not resident RAM."
    );
    metrics::describe_counter!(STATS_ERRORS, "Total failed jemalloc allocator statistics samples.");
    metrics::describe_gauge!(
        LAST_SUCCESS,
        Unit::Seconds,
        "Unix timestamp of the last successful jemalloc statistics sample."
    );
}

/// Record one successful snapshot and refresh the last-success timestamp.
fn record_stats(stats: &AllocatorStats) {
    use metrics::gauge;

    gauge!(ALLOCATED).set(stats.allocated as f64);
    gauge!(ACTIVE).set(stats.active as f64);
    gauge!(RESIDENT).set(stats.resident as f64);
    gauge!(METADATA).set(stats.metadata as f64);
    gauge!(MAPPED).set(stats.mapped as f64);
    gauge!(RETAINED).set(stats.retained as f64);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    gauge!(LAST_SUCCESS).set(now);
}

/// Count a failed sample and log it, suppressing repeated logs within
/// [`ERROR_LOG_INTERVAL`].
fn record_failure(last_log: &mut Option<std::time::Instant>, error: &dyn std::fmt::Display) {
    metrics::counter!(STATS_ERRORS).increment(1);

    // Only the log is rate limited: `background_error!` would log every time,
    // so count the background error directly and log at most once per window.
    record_background_error(component::ALLOCATOR_METRICS, "stats_read", "warning");
    let now = std::time::Instant::now();
    if last_log.is_none_or(|prev| now.duration_since(prev) >= ERROR_LOG_INTERVAL) {
        tracing::warn!(
            component = component::ALLOCATOR_METRICS,
            reason = "stats_read",
            error = %error,
            "Failed to read jemalloc allocator statistics"
        );
        *last_log = Some(now);
    }
}

/// Run the sampler until `shutdown` fires.
///
/// Must not return before shutdown (the background task supervisor treats an
/// early return as a failure) and must not return `Err` for collection errors.
pub async fn run_allocator_metrics_sampler(config: AllocatorMetricsConfig, shutdown: CancellationToken) -> anyhow::Result<()> {
    if !config.enabled {
        shutdown.cancelled().await;
        return Ok(());
    }
    describe_allocator_metrics();

    // Clamp pathological intervals; the first tick still fires immediately.
    let sample_interval = config.sample_interval.max(MIN_SAMPLE_INTERVAL);
    if config.sample_interval < MIN_SAMPLE_INTERVAL {
        tracing::debug!(
            requested = ?config.sample_interval,
            effective = ?sample_interval,
            "Allocator metrics sample interval clamped"
        );
    }

    tracing::info!(interval = ?sample_interval, "Starting allocator metrics sampler");

    let mut interval = tokio::time::interval(sample_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut last_error_log: Option<std::time::Instant> = None;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("Allocator metrics sampler shutting down");
                break;
            }
            _ = interval.tick() => {
                // mallctl is a blocking syscall-like call. Off-load it so a slow
                // allocator does not stall the runtime, and keep at most one
                // read in flight by awaiting it before the next tick.
                match tokio::task::spawn_blocking(read_allocator_stats).await {
                    Ok(Ok(stats)) => {
                        record_stats(&stats);
                        last_error_log = None;
                    }
                    Ok(Err(AllocatorStatsError::Unsupported)) => {
                        // Non-Linux: nothing to sample. Stay alive until shutdown
                        // rather than returning early (the supervisor would treat
                        // that as a task failure).
                        tracing::info!(
                            "Allocator statistics are unsupported on this platform; allocator metrics sampler will idle"
                        );
                        shutdown.cancelled().await;
                        break;
                    }
                    Ok(Err(err)) => record_failure(&mut last_error_log, &err),
                    Err(join_err) => record_failure(&mut last_error_log, &format_args!("sampling task failed: {join_err}")),
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    use serial_test::serial;

    /// Parse the value of a rendered Prometheus metric by name, ignoring the
    /// `# HELP`/`# TYPE` lines.
    #[cfg(target_os = "linux")]
    fn rendered_value(rendered: &str, name: &str) -> f64 {
        rendered
            .lines()
            .find_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix(name)?.strip_prefix(' ')?;
                rest.split_whitespace().next()?.parse().ok()
            })
            .unwrap_or_else(|| panic!("metric {name} not found in:\n{rendered}"))
    }

    /// Live allocation held via jemalloc directly. The dwctl *test* binary does
    /// not install jemalloc as its global allocator (that lives in `main.rs`),
    /// so `Vec` allocations do not appear in jemalloc's stats. Tests that need
    /// to move `allocated` allocate through `tikv_jemallocator::Jemalloc`.
    #[cfg(target_os = "linux")]
    struct JemallocBlock {
        ptr: *mut u8,
        layout: std::alloc::Layout,
    }

    #[cfg(target_os = "linux")]
    impl JemallocBlock {
        fn touched(bytes: usize) -> Self {
            use std::alloc::{GlobalAlloc, Layout};

            let layout = Layout::from_size_align(bytes, 4096).unwrap();
            let ptr = unsafe { tikv_jemallocator::Jemalloc.alloc(layout) };
            assert!(!ptr.is_null(), "jemalloc allocation failed");
            // Touch every page so the memory is really backed by the allocator.
            unsafe {
                for offset in (0..bytes).step_by(4096) {
                    ptr.add(offset).write(0xAB);
                }
            }
            Self { ptr, layout }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for JemallocBlock {
        fn drop(&mut self) {
            use std::alloc::GlobalAlloc;
            unsafe { tikv_jemallocator::Jemalloc.dealloc(self.ptr, self.layout) };
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    // jemalloc statistics are process-wide; tests that allocate through jemalloc
    // or assert on its totals must not overlap.
    #[serial(jemalloc_stats)]
    fn stats_are_internally_consistent() {
        let stats = read_allocator_stats().expect("jemalloc statistics should be readable on Linux");

        assert!(stats.allocated <= stats.active, "{stats:?}");
        assert!(stats.active <= stats.mapped, "{stats:?}");
        assert!(stats.metadata > 0, "jemalloc metadata should be non-zero: {stats:?}");
        assert!(stats.resident > 0, "jemalloc resident should be non-zero: {stats:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[serial(jemalloc_stats)]
    fn epoch_refresh_observes_allocations_come_and_go() {
        const BLOCK: usize = 64 * 1024 * 1024;

        let before = read_allocator_stats().expect("jemalloc statistics should be readable on Linux");

        let block = JemallocBlock::touched(BLOCK);
        let during = read_allocator_stats().expect("jemalloc statistics should be readable on Linux");
        assert!(
            during.allocated >= before.allocated + 60 * 1024 * 1024,
            "allocated should grow by ~{BLOCK} bytes: before={} during={}",
            before.allocated,
            during.allocated
        );

        drop(block);
        let after = read_allocator_stats().expect("jemalloc statistics should be readable on Linux");
        assert!(
            after.allocated + 60 * 1024 * 1024 <= during.allocated,
            "allocated should fall back after the block is freed: during={} after={}",
            during.allocated,
            after.allocated
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[serial(jemalloc_stats)]
    fn sampler_records_all_gauges() {
        use metrics::with_local_recorder;
        use metrics_exporter_prometheus::PrometheusBuilder;

        // Hold a live jemalloc allocation so `allocated` is non-zero even though
        // the test binary's global allocator is the system allocator.
        let block = JemallocBlock::touched(4 * 1024 * 1024);
        let _ = &block;

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        with_local_recorder(&recorder, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");

            runtime.block_on(async {
                let shutdown = CancellationToken::new();
                let sampler_shutdown = shutdown.clone();
                let config = AllocatorMetricsConfig {
                    enabled: true,
                    // Deliberately below the clamp: the clamp is exercised here.
                    sample_interval: Duration::from_millis(10),
                };

                let sampler = tokio::spawn(async move { run_allocator_metrics_sampler(config, sampler_shutdown).await });

                // The first interval tick is immediate; wait for the rendered
                // gauge instead of sleeping a fixed amount.
                let mut rendered = String::new();
                for _ in 0..200 {
                    rendered = handle.render();
                    if rendered.contains(LAST_SUCCESS) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }

                shutdown.cancel();
                sampler.await.expect("sampler task should not panic").expect("sampler returns Ok");

                assert!(rendered.contains(ALLOCATED), "{rendered}");
                assert!(rendered.contains(ACTIVE), "{rendered}");
                assert!(rendered.contains(RESIDENT), "{rendered}");
                assert!(rendered.contains(METADATA), "{rendered}");
                assert!(rendered.contains(MAPPED), "{rendered}");
                assert!(rendered.contains(RETAINED), "{rendered}");

                for name in [ALLOCATED, ACTIVE, RESIDENT, METADATA, MAPPED, RETAINED] {
                    assert!(rendered_value(&rendered, name) > 0.0, "{name} should be non-zero:\n{rendered}");
                }
            });
        });
    }

    #[tokio::test]
    async fn sampler_returns_promptly_after_shutdown() {
        let shutdown = CancellationToken::new();
        let sampler_shutdown = shutdown.clone();
        let config = AllocatorMetricsConfig {
            enabled: true,
            sample_interval: Duration::from_secs(60),
        };

        let sampler = tokio::spawn(async move { run_allocator_metrics_sampler(config, sampler_shutdown).await });

        // Let the sampler reach its select loop before cancelling.
        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.cancel();

        let result = tokio::time::timeout(Duration::from_secs(2), sampler)
            .await
            .expect("sampler should stop promptly after shutdown")
            .expect("sampler task should not panic");
        assert!(result.is_ok());
    }

    /// Measurement, not a check: epoch refresh cost grows with thread and arena
    /// count and wall-clock bounds are flaky on a loaded runner. Run with
    /// `cargo test read_allocator_stats_cost -- --ignored --nocapture`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn read_allocator_stats_cost() {
        // Warm the MIB cache so the measured reads are the steady-state path.
        read_allocator_stats().expect("jemalloc statistics should be readable on Linux");

        let iterations = 10_000;
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            read_allocator_stats().expect("jemalloc statistics should be readable on Linux");
        }
        let elapsed = start.elapsed();
        let per_read_us = elapsed.as_secs_f64() * 1e6 / iterations as f64;
        println!("read_allocator_stats: {per_read_us:.3} us/read over {iterations} reads (epoch + 6 reads)");
    }
}
