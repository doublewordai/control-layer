//! Lifecycle of the standalone gateway. Library consumers own their own server.
//!
//! Readiness fails first, then the proxy closes its listener and drains HTTP
//! connections through Axum/Hyper. The metrics listener stays up until proxy
//! response bodies finish, so scrapes and liveness probes cannot end the drain.

use std::{
    future::Future,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, anyhow};
use axum::{Router, http::StatusCode, routing::get};
use onwards::config::Config;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::{
    net::TcpListener,
    sync::oneshot,
    time::{sleep, timeout},
};
use tracing::{info, warn};

#[cfg(unix)]
/// Register termination signals before opening the listening sockets.
pub fn shutdown_signal() -> io::Result<impl Future<Output = io::Result<()>>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
        }
        Ok(())
    })
}

#[cfg(not(unix))]
/// Listen for the platform's console interrupt signal.
pub fn shutdown_signal() -> io::Result<impl Future<Output = io::Result<()>>> {
    Ok(tokio::signal::ctrl_c())
}

/// Serve both listeners, retaining health and metrics until Axum drains the proxy.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    metrics_listener: TcpListener,
    metrics_router: Router,
    config: &Config,
    shutdown_signal: impl Future<Output = io::Result<()>>,
) -> anyhow::Result<()> {
    let ready = Arc::new(AtomicBool::new(true));
    let readiness = ready.clone();
    let metrics_router = metrics_router
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route(
            "/readyz",
            get(move || {
                let readiness = readiness.clone();
                async move {
                    if readiness.load(Ordering::Acquire) {
                        (StatusCode::OK, "ready")
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, "draining")
                    }
                }
            }),
        );

    let (stop_proxy, proxy_stopped) = oneshot::channel();
    let mut proxy = tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = proxy_stopped.await;
            })
            .into_future(),
    );
    let (stop_metrics, metrics_stopped) = oneshot::channel();
    let mut metrics = tokio::spawn(
        axum::serve(metrics_listener, metrics_router)
            .with_graceful_shutdown(async {
                let _ = metrics_stopped.await;
            })
            .into_future(),
    );

    let mut metrics_finished = false;
    let trigger = tokio::select! {
        result = shutdown_signal => result.context("shutdown signal listener failed"),
        result = &mut proxy => {
            metrics.abort();
            result.context("proxy server task failed")??;
            return Err(anyhow!("proxy server stopped unexpectedly"));
        },
        result = &mut metrics => {
            metrics_finished = true;
            Err(anyhow!("metrics server stopped unexpectedly: {result:?}"))
        },
    };

    ready.store(false, Ordering::Release);
    info!(
        delay_secs = config.shutdown_delay_secs,
        "Gateway draining; readiness withdrawn"
    );
    // Kubernetes removes the pod from its Service while existing connections
    // continue to work during endpoint propagation. Do not reject them with 503.
    sleep(Duration::from_secs(config.shutdown_delay_secs)).await;
    let _ = stop_proxy.send(());
    info!(
        timeout_secs = config.shutdown_timeout_secs,
        "Closing proxy admission; waiting for response streams"
    );

    // Axum waits for connection tasks, including streaming bodies, not merely
    // for handlers to produce headers. It also closes idle keep-alive sockets.
    let drained = match timeout(
        Duration::from_secs(config.shutdown_timeout_secs),
        &mut proxy,
    )
    .await
    {
        Ok(result) => result
            .context("proxy server task failed while draining")
            .and_then(|result| result.context("proxy server failed while draining")),
        Err(_) => {
            proxy.abort();
            warn!(
                timeout_secs = config.shutdown_timeout_secs,
                "Gateway drain deadline exceeded; terminating active streams"
            );
            Err(anyhow!(
                "gateway drain deadline exceeded after {} seconds",
                config.shutdown_timeout_secs
            ))
        }
    };
    if drained.is_ok() {
        info!("Gateway request drain complete");
    }

    let _ = stop_metrics.send(());
    let metrics_result = if metrics_finished {
        Ok(())
    } else {
        match timeout(Duration::from_secs(5), &mut metrics).await {
            Ok(result) => result
                .context("metrics server task failed")
                .and_then(|result| result.context("metrics server failed")),
            Err(_) => {
                metrics.abort();
                Err(anyhow!("metrics server shutdown deadline exceeded"))
            }
        }
    };
    trigger.and(drained).and(metrics_result)
}
