use clap::{Parser, Subcommand};
use dwctl::config::Args;
use dwctl::{Application, Config, migrations, telemetry};

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
// defaults.
//
// The symbol name is equally load-bearing. tikv-jemalloc-sys builds jemalloc
// with `--with-jemalloc-prefix=_rjem_` unless the
// `unprefixed_malloc_on_supported_platforms` feature is on, so the global it
// reads its options from is `_rjem_malloc_conf` (and the environment variable
// is `_RJEM_MALLOC_CONF`). Exporting plain `malloc_conf` links fine and is
// silently ignored: 10.16.0 shipped that way and ran with
// `background_thread:false, dirty_decay_ms:10000, muzzy_decay_ms:0`.
//
// Verify against the built image rather than the symbol table:
//   _RJEM_MALLOC_CONF=stats_print:true /app/dwctl --help 2>&1 | grep -E "background_thread|decay_ms"
// must print `opt.background_thread: true` and 5000 for both decay values.
#[cfg(target_os = "linux")]
#[used]
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "_rjem_malloc_conf")]
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

/// `dwctl` serves by default; `dwctl migrate` applies schema migrations and
/// exits, for running from a pre-rollout Job with the release image.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    args: Args,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Apply pending schema migrations to every configured database (main,
    /// fusillade, underway, and outlet when request logging is enabled), then
    /// exit. Repairs interrupted concurrent index builds first and verifies
    /// the result. Safe to rerun; competing runners serialise on a database
    /// advisory lock. Uses the same connection configuration as the server
    /// (direct endpoints only).
    Migrate {
        /// Verify schema compatibility without executing any DDL, exiting
        /// non-zero when the database is behind this release.
        #[arg(long)]
        check: bool,
    },
}

async fn async_main() -> anyhow::Result<()> {
    // Parse CLI args
    let cli = Cli::parse();
    let args = cli.args;

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

    if let Some(Command::Migrate { check }) = cli.command {
        let result = migrations::run_command(&config, check).await;
        if let Some(provider) = tracer_provider {
            let _ = provider.shutdown();
        }
        return match result {
            Ok(_) => Ok(()),
            Err(error) => {
                // `{:#}` prints the context chain (target, migration version)
                // ahead of the driver error, which is what an operator reading
                // a failed Job needs first.
                tracing::error!(error = format!("{error:#}"), "dwctl migrate failed");
                Err(error)
            }
        };
    }

    // Run the application with graceful shutdown on SIGTERM/Ctrl+C
    let shutdown = shutdown_signal();
    Application::new_with_config_path(config, Some(args.config.clone()), tracer_provider)
        .await?
        .serve(shutdown)
        .await
}
