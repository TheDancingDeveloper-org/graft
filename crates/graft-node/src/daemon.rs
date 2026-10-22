//! The daemon: telemetry, the socket, the optional metrics listener, and a
//! graceful stop on SIGTERM or SIGINT.
//!
//! Start-up prints `graft-node listening on <socket>` (and
//! `graft-node metrics on <addr>`) to stdout once it is ready, so a
//! supervisor or a test can wait for it.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use graft_telemetry::TelemetryConfig;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_stream::wrappers::TcpListenerStream;

use crate::config::NodeConfig;
use crate::metrics::NodeMetrics;
use crate::{Node, socket};

/// Runs the node until a shutdown signal.
pub fn run(config: NodeConfig) -> Result<(), String> {
    let telemetry = graft_telemetry::init(&TelemetryConfig {
        service: "graft-node".into(),
        json_logs: config.json_logs,
        metrics_addr: config.metrics.clone(),
        otlp_endpoint: config.otlp_endpoint.clone(),
    })
    .map_err(|e| format!("telemetry: {e}"))?;
    std::panic::set_hook(Box::new(|info| tracing::error!(panic = %info, "panic")));
    let metrics = NodeMetrics::register(telemetry.registry().clone(), telemetry.metrics().clone())
        .map_err(|e| format!("metrics: {e}"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("graft-node")
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let result = runtime.block_on(serve(config, metrics));
    runtime.shutdown_timeout(Duration::from_secs(1));
    drop(telemetry);
    result
}

async fn serve(config: NodeConfig, metrics: NodeMetrics) -> Result<(), String> {
    let mode = config.mode().map_err(|e| e.to_string())?;
    // The socket first: its umask must not race other file creation.
    let listener = socket::bind(&config.socket, mode)
        .map_err(|e| format!("socket {}: {e}", config.socket.display()))?;
    let node = Node::open(&config, metrics)?;

    let (stop_tx, stop_rx) = watch::channel(false);
    let stopped = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };
    let mut servers = tokio::task::JoinSet::new();
    let mut metrics_addr = None;
    if let Some(addr) = &config.metrics {
        let l = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("metrics listen on {addr}: {e}"))?;
        metrics_addr = Some(
            l.local_addr()
                .map_err(|e| format!("metrics address: {e}"))?,
        );
        let m = Arc::new(node.metrics().clone());
        let router = Router::new()
            .route("/metrics", get(move || async move { m.encode() }))
            .route("/healthz", get(|| async { "ok" }));
        servers.spawn(async move {
            tonic::transport::Server::builder()
                .accept_http1(true)
                .add_routes(tonic::service::Routes::from(router))
                .serve_with_incoming_shutdown(TcpListenerStream::new(l), stopped(stop_rx.clone()))
                .await
                .map_err(|e| format!("metrics: {e}"))
        });
    }
    {
        let node = node.clone();
        let rx = stop_tx.subscribe();
        servers.spawn(async move { node.serve(listener, stopped(rx)).await });
    }

    tracing::info!(socket = %config.socket.display(), metrics = ?metrics_addr, upstream = %config.upstream, "serving");
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "graft-node listening on {}", config.socket.display());
        if let Some(m) = metrics_addr {
            let _ = writeln!(out, "graft-node metrics on {m}");
        }
        let _ = out.flush();
    }

    let mut failed = None;
    tokio::select! {
        () = shutdown_signal() => {}
        Some(result) = servers.join_next() => {
            failed = Some(match result {
                Ok(Ok(())) => "a listener stopped".to_owned(),
                Ok(Err(e)) => e,
                Err(e) => format!("listener task: {e}"),
            });
        }
    }
    tracing::info!("shutting down");
    let _ = stop_tx.send(true);
    let grace = Duration::from_secs(config.shutdown_grace_s);
    if tokio::time::timeout(grace, async {
        while servers.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tracing::warn!(
            grace_s = config.shutdown_grace_s,
            "calls still running after the grace period"
        );
    }
    let _ = std::fs::remove_file(&config.socket);
    tracing::info!("stopped");
    failed.map_or(Ok(()), Err)
}

/// SIGTERM or SIGINT.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
