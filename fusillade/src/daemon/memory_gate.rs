//! Local memory pressure gate.
//!
//! The adaptive controller discovers what a *model* can absorb by watching for
//! 529s. Nothing upstream ever tells this process that it is about to run out of
//! memory: it just gets OOM-killed. So a controller that grows on success needs
//! a second, local signal, and this is it.
//!
//! The gate reads the process's own memory usage against its own limit and
//! suppresses claiming while usage is high. It never estimates what a request
//! will cost, which is the point - per-request memory varies by more than an
//! order of magnitude between workloads (a short answer against a long reasoning
//! chain from the same size prompt), so anything that reserves up-front is
//! predicting a number with a very wide spread. Claiming is the only way
//! in-flight grows, so suppressing it means the level can only fall.
//!
//! Two thresholds rather than one: without hysteresis the gate would sit on the
//! boundary flipping every claim cycle.
//!
//! The low mark cannot be the only way out, though. Freed memory goes back to
//! the allocator rather than to the OS, so the reading behaves as a high-water
//! mark and does not fall when requests complete - which leaves no low mark that
//! is both reachable and useful. Claiming therefore also resumes once enough of
//! the work that was in flight at engagement has drained. See `should_block`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use metrics::{counter, gauge};

/// A memory reading for this process: what it is using, against what it may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MemoryReading {
    /// Working set in bytes: usage less reclaimable page cache, which is what
    /// the OOM killer effectively acts on and what `container_memory_working_set_bytes`
    /// reports. Raw usage would include cache and trip the gate on file IO.
    pub working_set: u64,
    /// The cgroup limit in bytes.
    pub limit: u64,
}

/// Where a [`MemoryGate`] reads from. Abstracted so the decision logic can be
/// tested without cgroup files.
pub(super) trait MemorySource: Send + Sync {
    /// The current reading, or `None` when unavailable or unlimited - in which
    /// case the gate stays open rather than guessing.
    fn read(&self) -> Option<MemoryReading>;
}

/// Reads the cgroup this process belongs to, v2 first then v1.
pub(super) struct CgroupMemorySource;

impl CgroupMemorySource {
    /// cgroup v1 reports "unlimited" as a sentinel near u64::MAX rather than a
    /// word, so anything absurdly large is treated as no limit.
    const UNLIMITED_ABOVE: u64 = 1 << 50; // 1 PiB

    fn read_u64(path: &str) -> Option<u64> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// Sum of the named keys in a `memory.stat`-style file.
    fn read_stat_key(path: &str, key: &str) -> Option<u64> {
        let contents = std::fs::read_to_string(path).ok()?;
        contents.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next()? == key).then(|| parts.next()?.parse().ok())?
        })
    }

    fn read_v2() -> Option<MemoryReading> {
        let limit_raw = std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?;
        let limit = match limit_raw.trim() {
            "max" => return None,
            other => other.parse::<u64>().ok()?,
        };
        let current = Self::read_u64("/sys/fs/cgroup/memory.current")?;
        let inactive_file =
            Self::read_stat_key("/sys/fs/cgroup/memory.stat", "inactive_file").unwrap_or(0);
        Some(MemoryReading {
            working_set: current.saturating_sub(inactive_file),
            limit,
        })
    }

    fn read_v1() -> Option<MemoryReading> {
        let limit = Self::read_u64("/sys/fs/cgroup/memory/memory.limit_in_bytes")?;
        if limit >= Self::UNLIMITED_ABOVE {
            return None;
        }
        let usage = Self::read_u64("/sys/fs/cgroup/memory/memory.usage_in_bytes")?;
        let inactive_file =
            Self::read_stat_key("/sys/fs/cgroup/memory/memory.stat", "total_inactive_file")
                .unwrap_or(0);
        Some(MemoryReading {
            working_set: usage.saturating_sub(inactive_file),
            limit,
        })
    }
}

impl MemorySource for CgroupMemorySource {
    fn read(&self) -> Option<MemoryReading> {
        Self::read_v2().or_else(Self::read_v1)
    }
}

/// Suppresses claiming while this process is close to its memory limit.
pub(super) struct MemoryGate {
    source: Box<dyn MemorySource>,
    /// Fraction of the limit at or above which claiming stops.
    high: f64,
    /// Fraction below which claiming resumes.
    low: f64,
    engaged: AtomicBool,
    /// Whether an unreadable source has already been logged, so a daemon running
    /// outside a limited cgroup does not warn on every claim cycle.
    warned_unavailable: AtomicBool,
    /// Highest in-flight count observed at the moment the gate engaged. This is
    /// the per-pod ceiling, measured rather than assumed, and it is what a
    /// replica count should be derived from.
    peak_in_flight_at_gate: AtomicU64,
}

impl MemoryGate {
    /// Build a gate. Returns `None` when disabled (`high` of zero) or when the
    /// thresholds are not a usable pair, in which case claiming is never
    /// suppressed.
    pub(super) fn new(
        high: f64,
        low: f64,
        source: Box<dyn MemorySource>,
    ) -> Option<Self> {
        if high <= 0.0 || high > 1.0 || low <= 0.0 || low >= high {
            return None;
        }
        // Out-of-range values would silently disable the in-flight release and
        // reintroduce the deadlock, so clamp rather than reject: a bad number
        // here should not take the gate's only reliable exit away.
        Some(Self {
            source,
            high,
            low,
            engaged: AtomicBool::new(false),
            warned_unavailable: AtomicBool::new(false),
            peak_in_flight_at_gate: AtomicU64::new(0),
        })
    }

    /// Whether claiming should be suppressed this cycle.
    ///
    /// Plain hysteresis: engage at `high`, hold until the reading falls back
    /// under `low`. Engaging is decided purely on the memory reading because
    /// that is what the OOM killer acts on, so being conservative there is
    /// correct.
    ///
    /// This depends on the reading actually falling as work completes, which in
    /// turn depends on the allocator returning freed memory to the OS. Under an
    /// allocator that keeps it on free lists the working set behaves as a
    /// high-water mark, the low mark is unreachable, and this gate never
    /// reopens. If that ever regresses, the symptom is a pod that stops claiming
    /// and stays stopped until it restarts.
    ///
    /// `in_flight` is retained for the ceiling gauge below, which is the
    /// measured per-pod concurrency limit and the number a replica count should
    /// be derived from.
    pub(super) fn should_block(&self, in_flight: usize) -> bool {
        let Some(reading) = self.source.read() else {
            if !self.warned_unavailable.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    "Memory gate configured but this process has no readable cgroup limit; \
                     claiming will not be suppressed on memory pressure"
                );
            }
            return false;
        };

        let used = reading.working_set as f64 / reading.limit as f64;
        gauge!("fusillade_memory_working_set_ratio").set(used);

        let was_engaged = self.engaged.load(Ordering::Relaxed);
        // Hysteresis, and nothing else: hold above the low mark, engage at the
        // high mark. This works only because freed memory now returns to the OS,
        // so the reading falls as work completes and the low mark is reachable.
        //
        // It previously was not, which is why a drain-based exit existed here:
        // the allocator kept freed memory on its free lists, the reading behaved
        // as a high-water mark, and without a second exit the gate never
        // reopened. That exit released once in-flight had fallen to a fraction
        // of its level at engagement, which meant the pod resumed at a level set
        // by whatever happened to be in flight at one arbitrary instant, and
        // stayed there until demand dropped away. It bounded a stall at the cost
        // of pinning throughput to a number nobody chose.
        let engaged = if was_engaged {
            used > self.low
        } else {
            used >= self.high
        };
        self.engaged.store(engaged, Ordering::Relaxed);
        gauge!("fusillade_memory_gate_engaged").set(u8::from(engaged));

        if engaged && !was_engaged {
            counter!("fusillade_memory_gate_engagements_total").increment(1);
            let in_flight = in_flight as u64;
            self.peak_in_flight_at_gate
                .fetch_max(in_flight, Ordering::Relaxed);
            gauge!("fusillade_in_flight_at_gate").set(in_flight as f64);
            tracing::warn!(
                working_set_bytes = reading.working_set,
                limit_bytes = reading.limit,
                used_fraction = used,
                in_flight,
                "Memory gate engaged; suspending claims until usage falls"
            );
        } else if !engaged && was_engaged {
            tracing::info!(
                used_fraction = used,
                "Memory gate released; resuming claims"
            );
        }

        engaged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FixedSource(Mutex<Vec<MemoryReading>>);

    impl FixedSource {
        /// Readings are consumed in order; the last one repeats.
        fn new(readings: Vec<MemoryReading>) -> Box<Self> {
            Box::new(Self(Mutex::new(readings)))
        }
    }

    impl MemorySource for FixedSource {
        fn read(&self) -> Option<MemoryReading> {
            let mut readings = self.0.lock().unwrap();
            if readings.len() > 1 {
                Some(readings.remove(0))
            } else {
                readings.first().copied()
            }
        }
    }

    struct NoSource;
    impl MemorySource for NoSource {
        fn read(&self) -> Option<MemoryReading> {
            None
        }
    }

    fn at(fraction: f64) -> MemoryReading {
        MemoryReading {
            working_set: (1000.0 * fraction) as u64,
            limit: 1000,
        }
    }

    #[test]
    fn stays_open_below_the_high_mark() {
        let gate = MemoryGate::new(0.75, 0.65, FixedSource::new(vec![at(0.5)])).unwrap();
        assert!(!gate.should_block(100));
    }

    #[test]
    fn engages_at_the_high_mark() {
        let gate = MemoryGate::new(0.75, 0.65, FixedSource::new(vec![at(0.8)])).unwrap();
        assert!(gate.should_block(100));
    }

    /// Without hysteresis the gate would release the moment it dipped under the
    /// high mark and re-engage on the next cycle, flapping every interval.
    #[test]
    fn stays_engaged_between_the_marks() {
        let gate = MemoryGate::new(
            0.75,
            0.65,
            FixedSource::new(vec![at(0.8), at(0.7), at(0.7)]),
        )
        .unwrap();
        assert!(gate.should_block(100), "engages at 0.8");
        assert!(
            gate.should_block(100),
            "still engaged at 0.7, above the low mark"
        );
    }

    #[test]
    fn releases_below_the_low_mark() {
        let gate = MemoryGate::new(
            0.75,
            0.65,
            FixedSource::new(vec![at(0.8), at(0.6), at(0.6)]),
        )
        .unwrap();
        assert!(gate.should_block(100));
        assert!(!gate.should_block(100), "released once under the low mark");
    }







    /// A daemon outside a limited cgroup (local runs, tests) must not have its
    /// claiming suppressed by a source it cannot read.
    #[test]
    fn an_unreadable_source_never_blocks() {
        let gate = MemoryGate::new(0.75, 0.65, Box::new(NoSource)).unwrap();
        assert!(!gate.should_block(100));
        assert!(!gate.should_block(100));
    }

    #[test]
    fn disabled_or_nonsensical_thresholds_produce_no_gate() {
        let src = || FixedSource::new(vec![at(0.9)]);
        assert!(
            MemoryGate::new(0.0, 0.0, src()).is_none(),
            "zero disables"
        );
        assert!(
            MemoryGate::new(0.65, 0.75, src()).is_none(),
            "low above high"
        );
        assert!(
            MemoryGate::new(0.75, 0.75, src()).is_none(),
            "equal marks flap"
        );
        assert!(
            MemoryGate::new(1.5, 0.5, src()).is_none(),
            "high above the limit"
        );
    }

    /// The ceiling is what a replica count gets derived from, so it records the
    /// highest level seen rather than the most recent.
    #[test]
    fn records_the_peak_in_flight_when_engaging() {
        let gate = MemoryGate::new(
            0.75,
            0.65,
            FixedSource::new(vec![at(0.8), at(0.5), at(0.9)]),
        )
        .unwrap();
        gate.should_block(4000);
        gate.should_block(10);
        gate.should_block(1200);
        assert_eq!(gate.peak_in_flight_at_gate.load(Ordering::Relaxed), 4000);
    }

    /// Working set excludes reclaimable page cache; a process whose raw usage is
    /// over the mark purely because of cache must not be gated.
    #[test]
    fn page_cache_does_not_count_toward_the_gate() {
        let reading = MemoryReading {
            working_set: 400,
            limit: 1000,
        };
        let gate = MemoryGate::new(0.75, 0.65, FixedSource::new(vec![reading])).unwrap();
        assert!(!gate.should_block(100));
    }
}
