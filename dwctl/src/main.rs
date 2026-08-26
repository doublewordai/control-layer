use clap::Parser;
use dwctl::{Application, Config, telemetry};

// jemalloc, for its decay behaviour rather than its allocation speed.
//
// glibc's allocator keeps freed memory on its free lists and only returns
// contiguous top-of-heap on an explicit trim, so a process with bursty
// allocation holds a working set close to its historical peak indefinitely.
// The cgroup limit is enforced on that working set and the OOM killer acts on
// it, so retained-but-unused memory is indistinguishable from live memory to
// the kernel: a pod can sit near its limit while most of what it holds is free.
//
// That also breaks the batch daemon's memory gate, whose low mark is only
// reachable if the reading falls when work completes. Under glibc it does not,
// which leaves the gate unable to reopen on the memory signal alone.
//
// jemalloc returns dirty pages to the OS on a decay timer without the
// application asking, so the reading tracks live usage. Linux only: this is
// about the glibc behaviour above, and the default allocator is fine elsewhere.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// `background_thread` is the part that matters here, not the decay interval.
// By default jemalloc purges as a side effect of allocator activity, and the
// state this exists to fix is a pod holding memory while it is DELIBERATELY
// idle: once the memory gate suspends claiming there is little allocation
// happening to drive a purge, so the working set would stay high exactly when
// it needs to fall. A background thread purges on a timer regardless.
//
// `#[used]` is load-bearing: nothing in this crate reads the static, so without
// it the symbol is dropped before linking and jemalloc silently keeps its
// defaults. Verified by checking the symbol is present in the built binary.
#[cfg(target_os = "linux")]
#[used]
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "malloc_conf")]
pub static malloc_conf: &[u8] = b"background_thread:true,dirty_decay_ms:5000,muzzy_decay_ms:5000\0";

/// Wait for shutdown signal (SIGTERM or Ctrl+C)
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C, shutting down gracefully...");
        },
        _ = terminate => {
            tracing::info!("Received SIGTERM, shutting down gracefully...");
        },
    }
}

fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // 8MB stack per worker thread — the default 2MB overflows with deep
        // tracing-opentelemetry span nesting during batch request processing
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    // Parse CLI args
    let args = dwctl::config::Args::parse();

    // Load configuration
    let config = Config::load(&args)?;

    // Validate config consistency
    config.batches.validate();

    // If --validate flag is set, exit successfully after config validation
    if args.validate {
        println!("Configuration is valid.");
        return Ok(());
    }

    // Initialize telemetry (tracing + optional OpenTelemetry)
    let tracer_provider = telemetry::init_telemetry(config.enable_otel_export)?;

    tracing::debug!("{:?}", args);

    // Run the application with graceful shutdown on SIGTERM/Ctrl+C
    let shutdown = shutdown_signal();
    Application::new_with_config_path(config, Some(args.config.clone()), tracer_provider)
        .await?
        .serve(shutdown)
        .await
}
