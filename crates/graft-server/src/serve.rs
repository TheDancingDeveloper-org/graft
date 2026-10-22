//! `graft-server serve`: open the stores, serve gRPC and HTTP on one
//! listener (and optionally `/metrics` on a second), and shut down
//! gracefully on SIGTERM or SIGINT.
//!
//! Start-up prints one line per listener to stdout,
//! `graft-server listening on <addr>` and `graft-server metrics on <addr>`,
//! once the server is ready to answer, so a supervisor or a test can wait
//! for it and learn a port chosen with `:0`.

use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graft_engine::{Engine, EngineConfig};
use graft_model::SystemClock;
use graft_store_fs::{FsMode, FsObjectStore};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use graft_telemetry::TelemetryConfig;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tonic::codegen::tokio_stream::wrappers::TcpListenerStream;

use crate::admission::Admission;
use crate::app::{App, PutBudget};
use crate::config::ServerConfig;
use crate::denylist::Denylist;
use crate::grpc::{self, Services};
use crate::watch::Watches;
use crate::{http, keys};

/// Runs the server until a shutdown signal.
pub(crate) fn serve(config: ServerConfig) -> Result<(), String> {
    let telemetry = graft_telemetry::init(&TelemetryConfig {
        service: "graft-server".into(),
        json_logs: config.json_logs,
        metrics_addr: config.metrics.clone(),
        otlp_endpoint: config.otlp_endpoint.clone(),
    })
    .map_err(|e| format!("telemetry: {e}"))?;
    // Panics (an operation not built yet is a `todo!`) go to the log as one
    // line, not as raw text between JSON log lines.
    std::panic::set_hook(Box::new(|info| tracing::error!(panic = %info, "panic")));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("graft-server")
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let result = runtime.block_on(run(config, Arc::new(telemetry)));
    // Calls still running past the grace period are abandoned here.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

/// Opens the production stores.
pub(crate) fn open_stores(
    config: &ServerConfig,
) -> Result<(SqliteMetaStore, FsObjectStore), String> {
    if let Some(dir) = config.meta_db.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let meta = SqliteMetaStore::open(SqliteConfig::new(&config.meta_db))
        .map_err(|e| format!("open {}: {e:?}", config.meta_db.display()))?;
    let objects = FsObjectStore::open(&config.objects_dir, FsMode::Authoritative)
        .map_err(|e| format!("open {}: {e:?}", config.objects_dir.display()))?;
    Ok((meta, objects))
}

async fn run(
    config: ServerConfig,
    telemetry: Arc<graft_telemetry::Telemetry>,
) -> Result<(), String> {
    let started = Instant::now();
    let (meta, objects) = open_stores(&config)?;
    let signing =
        keys::load_or_create(&config.key_path()).map_err(|e| format!("signing key: {e}"))?;
    let engine = Arc::new(Engine::new(
        Arc::new(meta),
        Arc::new(objects),
        Arc::new(SystemClock),
        EngineConfig::default(),
    ));
    let mut volumes = vec![config.objects_dir.clone()];
    if let Some(dir) = config
        .meta_db
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        volumes.push(dir.to_path_buf());
    }
    let admission = Admission::new(volumes, config.min_free_fraction, config.min_free_bytes);
    let denylist = Denylist::open(&config.meta_db)?;
    let app = Arc::new(App::new(
        engine,
        signing,
        admission,
        telemetry,
        config.max_token_ttl_s,
        Watches::new(config.watch_limits()),
        PutBudget::new(config.max_put_call_bytes, config.put_buffer_bytes).with_waits(
            std::time::Duration::from_millis(config.put_wait_ms),
            std::time::Duration::from_millis(config.put_idle_ms),
        ),
        denylist,
    ));

    let listener = bind(&config.listen).await?;
    let addr = local_addr(&listener)?;
    let metrics_listener = match &config.metrics {
        Some(m) => Some(bind(m).await?),
        None => None,
    };

    let (stop_tx, stop_rx) = watch::channel(false);
    let stopped = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };

    let router = grpc::stamp_version(Services::new(app.clone()).routes().into_axum_router())
        .merge(http::ops_router(app.clone()))
        .merge(http::facade_router(app.clone()));
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(
        tonic::transport::Server::builder()
            .accept_http1(true)
            .tcp_nodelay(true)
            .add_routes(tonic::service::Routes::from(router))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                stopped(stop_rx.clone()),
            ),
    );
    let mut metrics_addr = None;
    if let Some(listener) = metrics_listener {
        metrics_addr = Some(local_addr(&listener)?);
        servers.spawn(
            tonic::transport::Server::builder()
                .accept_http1(true)
                .add_routes(tonic::service::Routes::from(http::ops_router(app.clone())))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stopped(stop_rx)),
        );
    }

    app.set_ready(true);
    tracing::info!(
        %addr,
        metrics = ?metrics_addr,
        startup_ms = started.elapsed().as_millis() as u64,
        "serving"
    );
    announce(addr, metrics_addr);

    // Serve until a signal, or until a listener fails.
    let mut failed = None;
    tokio::select! {
        () = shutdown_signal() => {}
        Some(result) = servers.join_next() => {
            failed = Some(match result {
                Ok(Ok(())) => "a listener stopped".to_owned(),
                Ok(Err(e)) => format!("listener: {e}"),
                Err(e) => format!("listener task: {e}"),
            });
        }
    }

    tracing::info!("shutting down");
    app.set_ready(false);
    let _ = stop_tx.send(true);
    let grace = Duration::from_secs(config.shutdown_grace_s);
    let drained = tokio::time::timeout(grace, async {
        while servers.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            grace_s = config.shutdown_grace_s,
            "calls still running after the grace period"
        );
    }
    tracing::info!("stopped");
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn bind(addr: &str) -> Result<TcpListener, String> {
    TcpListener::bind(addr)
        .await
        .map_err(|e| format!("listen on {addr}: {e}"))
}

fn local_addr(listener: &TcpListener) -> Result<SocketAddr, String> {
    listener
        .local_addr()
        .map_err(|e| format!("local address: {e}"))
}

fn announce(addr: SocketAddr, metrics: Option<SocketAddr>) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "graft-server listening on {addr}");
    if let Some(m) = metrics {
        let _ = writeln!(out, "graft-server metrics on {m}");
    }
    let _ = out.flush();
}

/// SIGTERM or SIGINT.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
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
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
