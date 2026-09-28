//! Opt-in sampled jemalloc heap profiling.
//!
//! Serving is deliberately separate from sampling. jemalloc only samples if the
//! process started with `_RJEM_MALLOC_CONF` containing
//! `prof:true,prof_active:true,lg_prof_sample:N` (or the equivalent compiled-in
//! default); this module never turns sampling on. When
//! [`HeapProfilingConfig::enabled`] is set it serves
//! `GET /debug/pprof/heap` on a listener that is not part of the main router,
//! the `Service` or any ingress. Polling agents (for example an in-cluster
//! Alloy pprof scraper) or an operator via `kubectl port-forward` pull the
//! gzip-compressed pprof protobuf from there; the process holds no export
//! credentials and makes no outbound calls.
//!
//! See `docs/memory-observability.md` for the interface contract.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::metrics::errors::component;

/// Path served on the profiling listener. Alloy's pprof scraper and the
/// operator capture procedure both use it.
pub const HEAP_PROFILE_PATH: &str = "/debug/pprof/heap";

/// Heap profiling configuration. Off by default.
///
/// Serving profiles also requires the process to have started with jemalloc
/// sampling on (`_RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:N`);
/// this section never enables sampling by itself.
///
/// Env: `DWCTL_HEAP_PROFILING__ENABLED`, `DWCTL_HEAP_PROFILING__BIND_ADDRESS`,
/// `DWCTL_HEAP_PROFILING__DUMP_TIMEOUT`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeapProfilingConfig {
    /// Start the profiling listener (default: false).
    pub enabled: bool,
    /// Address of the separate profiling listener (default: 0.0.0.0:6060).
    /// Never routed by the Service or ingress; reached by in-cluster Alloy or
    /// `kubectl port-forward`.
    pub bind_address: String,
    /// Upper bound on one dump + pprof conversion (default: 30s).
    #[serde(with = "humantime_serde")]
    pub dump_timeout: Duration,
}

impl Default for HeapProfilingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: "0.0.0.0:6060".to_string(),
            dump_timeout: Duration::from_secs(30),
        }
    }
}

/// jemalloc profiling state as read from mallctl at call time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeapProfilingStatus {
    /// jemalloc was compiled with `--enable-prof` (`config.prof`).
    pub compiled: bool,
    /// The process started with `prof:true` (`opt.prof`).
    pub enabled: bool,
    /// Sampling is currently active (`prof.active`).
    pub active: bool,
    /// log2 of the mean sampling interval in bytes (`opt.lg_prof_sample`).
    pub lg_sample: Option<u32>,
}

/// Why a heap profile could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum HeapProfileError {
    /// Not built with jemalloc profiling, or the process did not start with `prof:true`.
    #[error("heap profiling is not available: {0}")]
    Unavailable(String),
    /// Another dump is in progress (dumps are serialised), or the previous one
    /// started less than a second ago.
    #[error("a heap profile dump is in progress or was requested too soon")]
    Busy,
    /// The dump exceeded `dump_timeout`.
    #[error("heap profile dump timed out")]
    Timeout,
    /// The dump or pprof conversion failed.
    #[error("heap profile dump failed: {0}")]
    Failed(String),
}

/// Minimum time between the starts of two dumps.
const MIN_DUMP_INTERVAL: Duration = Duration::from_secs(1);

fn last_dump_started() -> &'static Mutex<Option<Instant>> {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    &LAST
}

/// One dump at a time across the process. Held for the whole blocking dump so
/// that a caller which times out still keeps later callers out until the
/// blocking work actually finishes, rather than letting timed-out dumps pile up
/// extra blocking threads.
fn dump_semaphore() -> &'static Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(1)))
}

/// Read the current profiling state. Never fails; reports all-false when
/// unsupported or when a mallctl key is unavailable.
pub fn heap_profiling_status() -> HeapProfilingStatus {
    #[cfg(target_os = "linux")]
    {
        use tikv_jemalloc_ctl::raw;

        // SAFETY: each key's documented type is used: `config.prof`, `opt.prof`
        // and `prof.active` are bools and `opt.lg_prof_sample` is a size_t.
        // Every call is fallible and mapped with `.ok()`, so a key that does
        // not exist (for example jemalloc built without `--enable-prof`) reads
        // as false/None rather than panicking.
        unsafe {
            let compiled = raw::read::<bool>(b"config.prof\0").unwrap_or(false);
            let enabled = raw::read::<bool>(b"opt.prof\0").unwrap_or(false);
            let active = raw::read::<bool>(b"prof.active\0").unwrap_or(false);
            let lg_sample = raw::read::<usize>(b"opt.lg_prof_sample\0")
                .ok()
                .and_then(|lg| u32::try_from(lg).ok());

            HeapProfilingStatus {
                compiled,
                enabled,
                active,
                lg_sample,
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        HeapProfilingStatus::default()
    }
}

/// Log a one-shot summary of jemalloc's profiling state.
///
/// Intended to be called once during startup even when the profiling server is
/// disabled, so operators can tell whether the process is sampling without
/// scraping mallctl themselves. Does nothing (beyond a debug line) when
/// sampling was not requested at process start.
pub fn log_heap_profiling_status() {
    let status = heap_profiling_status();
    if status.enabled {
        info!(
            compiled = status.compiled,
            enabled = status.enabled,
            active = status.active,
            lg_sample = ?status.lg_sample,
            sample_interval_bytes = status.lg_sample.map_or(0, |lg| 1u64.checked_shl(lg).unwrap_or(u64::MAX)),
            "jemalloc heap profiling sampling is enabled"
        );
    } else if status.compiled {
        debug!(
            active = status.active,
            "jemalloc heap profiling is compiled in but sampling was not requested at process start; \
             set _RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19 to enable it"
        );
    }
}

/// Produce one gzip-compressed pprof protobuf heap profile (in-use sampled
/// allocations with symbolized stacks). Serialised: concurrent callers get
/// [`HeapProfileError::Busy`]. Blocking work runs off the async runtime and the
/// dump permit is held until that work completes, so a timed-out call still
/// blocks the next one.
pub async fn dump_heap_profile_pprof(timeout: Duration) -> Result<Vec<u8>, HeapProfileError> {
    describe_metrics();
    let started = Instant::now();

    // Take the permit before looking at the allocator: a second concurrent
    // caller is `Busy` regardless of whether profiling is on.
    let permit = match dump_semaphore().clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            record_dump_outcome("busy", started, None);
            return Err(HeapProfileError::Busy);
        }
    };

    let status = heap_profiling_status();
    if !status.enabled || !status.active {
        // Do not touch `jemalloc_pprof::PROF_CTL` here: its lazy initialiser is
        // only valid once jemalloc was built with prof and started with
        // prof:true.
        let reason = if !status.compiled {
            "jemalloc was built without profiling support"
        } else if !status.enabled {
            "jemalloc sampling was not enabled at process start"
        } else {
            "jemalloc sampling is currently inactive"
        };
        record_dump_outcome("unavailable", started, None);
        return Err(HeapProfileError::Unavailable(reason.to_string()));
    }

    // The listener is unauthenticated, so cap how often anyone can make this
    // process dump, whatever the scrape interval: a caller hammering the route
    // gets `Busy` instead of a dump every few milliseconds.
    {
        let mut last = last_dump_started().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.is_some_and(|prev| started.duration_since(prev) < MIN_DUMP_INTERVAL) {
            record_dump_outcome("busy", started, None);
            return Err(HeapProfileError::Busy);
        }
        *last = Some(started);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (permit, timeout);
        record_dump_outcome("unavailable", started, None);
        return Err(HeapProfileError::Unavailable(
            "heap profiling is only supported on Linux".to_string(),
        ));
    }

    #[cfg(target_os = "linux")]
    {
        let Some(ctl) = jemalloc_pprof::PROF_CTL.as_ref() else {
            record_dump_outcome("error", started, None);
            return Err(HeapProfileError::Failed("jemalloc profiling control is unavailable".to_string()));
        };
        // Uncontended in practice (our semaphore serialises dumps), but bound
        // it by the same budget so nothing can hold a request (and the dump
        // permit) indefinitely.
        let Ok(guard) = tokio::time::timeout(timeout, ctl.clone().lock_owned()).await else {
            record_dump_outcome("timeout", started, None);
            return Err(HeapProfileError::Timeout);
        };

        let blocking = tokio::task::spawn_blocking(move || {
            // Keep the permit alive for the whole blocking dump, even if the
            // caller has already timed out and dropped its future.
            let _permit: OwnedSemaphorePermit = permit;
            let mut guard = guard;
            guard.dump_pprof()
        });

        match tokio::time::timeout(timeout, blocking).await {
            Ok(Ok(Ok(bytes))) => {
                record_dump_outcome("ok", started, Some(bytes.len()));
                Ok(bytes)
            }
            Ok(Ok(Err(err))) => {
                record_dump_outcome("error", started, None);
                Err(HeapProfileError::Failed(err.to_string()))
            }
            Ok(Err(join_err)) => {
                record_dump_outcome("error", started, None);
                Err(HeapProfileError::Failed(format!("dump task failed: {join_err}")))
            }
            Err(_elapsed) => {
                record_dump_outcome("timeout", started, None);
                Err(HeapProfileError::Timeout)
            }
        }
    }
}

/// Serve heap profiles until `shutdown` fires.
///
/// Must not return before shutdown (the background task supervisor treats an
/// early return as a failure) and must not return `Err` for dump/export
/// failures; bind failure is logged and the task idles until shutdown.
pub async fn run_heap_profiling_server(config: HeapProfilingConfig, shutdown: CancellationToken) -> anyhow::Result<()> {
    describe_metrics();

    let status = heap_profiling_status();
    metrics::gauge!("dwctl_heap_profiling_active").set(if status.enabled && status.active { 1.0 } else { 0.0 });
    if status.enabled
        && let Some(lg) = status.lg_sample
    {
        metrics::gauge!("dwctl_heap_profiling_sample_interval_bytes").set(2f64.powi(i32::try_from(lg).unwrap_or(i32::MAX)));
    }

    if !status.enabled {
        warn!(
            bind_address = %config.bind_address,
            compiled = status.compiled,
            "heap profiling server is enabled but jemalloc sampling is off: start the process with \
             _RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19 to enable it"
        );
    }

    let listener = match tokio::net::TcpListener::bind(&config.bind_address).await {
        Ok(listener) => listener,
        Err(err) => {
            crate::background_error!(
                component::HEAP_PROFILING,
                "bind",
                Error,
                bind_address = %config.bind_address,
                error = %err,
                "failed to bind heap profiling listener; the profiling server will stay idle"
            );
            shutdown.cancelled().await;
            return Ok(());
        }
    };

    let app = heap_profile_router(config);
    let shutdown_signal = async move { shutdown.cancelled().await };

    // The only route is a dump bounded by `dump_timeout` (and serialised by the
    // dump semaphore), so a separate request-timeout layer is unnecessary. The
    // body limit is a defensive cap; GET requests carry no body.
    if let Err(err) = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal).await {
        crate::background_error!(
            component::HEAP_PROFILING,
            "serve",
            Error,
            error = %err,
            "heap profiling server stopped with an error"
        );
        // Never bubble up: the supervisor treats an early `Err` return as a
        // failure. The process-level shutdown token is what ends this task.
        return Ok(());
    }

    Ok(())
}

/// Router serving only `GET /debug/pprof/heap`.
fn heap_profile_router(config: HeapProfilingConfig) -> Router {
    Router::new()
        .route(HEAP_PROFILE_PATH, get(handle_get_heap_profile))
        .with_state(Arc::new(config))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

/// Handler for the single profiling route. Error bodies are short, static and
/// free of allocator/path detail; the metric counter carries the real reason.
#[tracing::instrument(skip_all)]
async fn handle_get_heap_profile(State(config): State<Arc<HeapProfilingConfig>>) -> Response {
    match dump_heap_profile_pprof(config.dump_timeout).await {
        Ok(bytes) => {
            let unix_time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            let disposition = format!("attachment; filename=\"heap-{unix_time}.pb.gz\"");
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/octet-stream"),
                    (header::CONTENT_DISPOSITION, disposition.as_str()),
                ],
                Body::from(bytes),
            )
                .into_response()
        }
        Err(HeapProfileError::Unavailable(_)) => (StatusCode::SERVICE_UNAVAILABLE, "heap profiling unavailable").into_response(),
        Err(HeapProfileError::Busy) => (
            StatusCode::TOO_MANY_REQUESTS,
            "heap profile dump already in progress or requested too soon",
        )
            .into_response(),
        Err(HeapProfileError::Timeout) => (StatusCode::GATEWAY_TIMEOUT, "heap profile dump timed out").into_response(),
        Err(HeapProfileError::Failed(_)) => (StatusCode::INTERNAL_SERVER_ERROR, "heap profile dump failed").into_response(),
    }
}

/// Emit a dump outcome counter, duration histogram and, on success, the last
/// dump size gauge. `outcome` is one of the contract's fixed set.
fn record_dump_outcome(outcome: &'static str, started: Instant, bytes: Option<usize>) {
    metrics::counter!("dwctl_heap_profile_dumps_total", "outcome" => outcome).increment(1);
    metrics::histogram!("dwctl_heap_profile_dump_duration_seconds").record(started.elapsed().as_secs_f64());
    if let Some(bytes) = bytes {
        metrics::gauge!("dwctl_heap_profile_last_dump_bytes").set(bytes as f64);
    }
}

/// Register help text once for the metrics this module emits.
fn describe_metrics() {
    static DESCRIBED: OnceLock<()> = OnceLock::new();
    DESCRIBED.get_or_init(|| {
        metrics::describe_gauge!(
            "dwctl_heap_profiling_active",
            "1 when jemalloc sampling is enabled and active at server start, otherwise 0"
        );
        metrics::describe_gauge!(
            "dwctl_heap_profiling_sample_interval_bytes",
            "Mean jemalloc heap sampling interval in bytes (2^opt.lg_prof_sample)"
        );
        metrics::describe_counter!(
            "dwctl_heap_profile_dumps_total",
            "Heap profile dump attempts by outcome (ok, error, timeout, busy, unavailable)"
        );
        metrics::describe_histogram!(
            "dwctl_heap_profile_dump_duration_seconds",
            "Time spent producing a heap profile, including blocking dump and pprof conversion"
        );
        metrics::describe_gauge!(
            "dwctl_heap_profile_last_dump_bytes",
            "Size in bytes of the most recent successful gzip pprof dump"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum_test::TestServer;

    fn test_config() -> HeapProfilingConfig {
        HeapProfilingConfig {
            enabled: true,
            bind_address: "127.0.0.1:0".to_string(),
            dump_timeout: Duration::from_secs(1),
        }
    }

    /// These tests cover the default (sampling off). Skip them when the suite
    /// itself runs under `_RJEM_MALLOC_CONF=prof:true`, as validation runs do.
    fn sampling_requested() -> bool {
        let requested = std::env::var("_RJEM_MALLOC_CONF").is_ok_and(|conf| conf.contains("prof:true"));
        if requested {
            eprintln!("skipping: _RJEM_MALLOC_CONF enables jemalloc sampling for this process");
        }
        requested
    }

    /// The dump tests share the process-wide dump semaphore, so they must not
    /// run concurrently with each other.
    async fn serial_dump_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        SERIAL.lock().await
    }

    #[tokio::test]
    async fn status_reports_consistently_and_dump_without_prof_is_unavailable() {
        let _serial = serial_dump_guard().await;
        if sampling_requested() {
            return;
        }
        // Without `prof:true` sampling is off even though jemalloc itself is
        // compiled with profiling support.
        let status = heap_profiling_status();
        assert!(!status.enabled, "sampling must be off without prof:true");
        assert!(!status.active, "sampling must be inactive without prof:true");

        let err = dump_heap_profile_pprof(Duration::from_millis(50))
            .await
            .expect_err("dump without profiling must fail");
        assert!(matches!(err, HeapProfileError::Unavailable(_)), "expected Unavailable, got {err:?}");
    }

    #[tokio::test]
    async fn concurrent_dump_is_busy() {
        let _serial = serial_dump_guard().await;
        let permit = dump_semaphore().clone().try_acquire_owned().expect("fresh semaphore");
        let err = dump_heap_profile_pprof(Duration::from_millis(50))
            .await
            .expect_err("second concurrent dump must be busy");
        assert!(matches!(err, HeapProfileError::Busy), "expected Busy, got {err:?}");
        drop(permit);
    }

    #[tokio::test]
    async fn heap_profile_endpoint_returns_503_when_unavailable() {
        let _serial = serial_dump_guard().await;
        if sampling_requested() {
            return;
        }
        let server = TestServer::new(heap_profile_router(test_config())).expect("build test server");
        let response = server.get(HEAP_PROFILE_PATH).await;
        assert_eq!(response.status_code(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn server_returns_ok_after_shutdown() {
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(run_heap_profiling_server(test_config(), shutdown.clone()));

        // Give the server a chance to bind, then stop it.
        tokio::task::yield_now().await;
        shutdown.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server should stop promptly")
            .expect("server task should not panic");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn server_idles_after_bind_failure_until_shutdown() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind occupied port");
        let addr = occupied.local_addr().expect("local addr");

        let mut config = test_config();
        config.bind_address = addr.to_string();

        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(run_heap_profiling_server(config, shutdown.clone()));

        // It must not return on bind failure; poll briefly to observe it still
        // running before cancelling.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!handle.is_finished(), "server must idle after bind failure, not return early");

        shutdown.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server should stop promptly")
            .expect("server task should not panic");
        assert!(result.is_ok());
    }
}
