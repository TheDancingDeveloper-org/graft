//! `graft-server serve`: open the stores, serve gRPC and HTTP on one
//! listener (and optionally `/metrics` on a second), and shut down
//! gracefully on SIGTERM or SIGINT.
//!
//! The signal handlers are the first thing installed, so a signal at any
//! point in start-up is a request to stop, never the default action that
//! kills the process. A signal before the listeners are bound exits 0 with
//! nothing bound.
//!
//! Start-up prints `graft-server signals armed` to stdout once the handlers
//! are installed, then one line per listener,
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
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore, StoreObserver};
use graft_telemetry::{Telemetry, TelemetryConfig};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::watch;
use tonic::transport::server::TcpIncoming;

use crate::admission::Admission;
use crate::app::{App, PutBudget};
use crate::ci::CiExchange;
use crate::config::ServerConfig;
use crate::denylist::Denylist;
use crate::grpc::{self, Services};
use crate::watch::Watches;
use crate::{http, keys, mcp};

/// Set by the start-up tests (only): start-up pauses after the handlers are
/// armed until a stop is requested. Unset in production.
pub(crate) const HOLD_START_UP_ENV: &str = "GRAFT_TEST_HOLD_START_UP";

/// Runs the server until a shutdown signal.
pub(crate) fn serve(config: ServerConfig) -> Result<(), String> {
    // Panics (an operation not built yet is a `todo!`) go to the log as one
    // line, not as raw text between JSON log lines.
    std::panic::set_hook(Box::new(|info| tracing::error!(panic = %info, "panic")));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("graft-server")
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let result = runtime.block_on(run(config));
    // Calls still running past the grace period are abandoned here.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

/// Records the metadata store's measurements in the server's registry.
#[derive(Debug)]
struct StoreTelemetry(Arc<Telemetry>);

impl StoreObserver for StoreTelemetry {
    fn writer_queue_depth(&self, depth: usize) {
        let depth = i64::try_from(depth).unwrap_or(i64::MAX);
        self.0.metrics().sqlite_writer_queue_depth.set(depth);
    }

    fn operation(&self, op: &'static str, elapsed: Duration) {
        self.0
            .metrics()
            .store_op_seconds
            .with_label_values(&["meta", op])
            .observe(elapsed.as_secs_f64());
    }
}

/// Opens the production stores. `telemetry` receives the metadata store's
/// measurements, when given.
pub(crate) fn open_stores(
    config: &ServerConfig,
    telemetry: Option<Arc<Telemetry>>,
) -> Result<(SqliteMetaStore, FsObjectStore), String> {
    if let Some(dir) = config.meta_db.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let observer = telemetry.map(|t| Arc::new(StoreTelemetry(t)) as Arc<dyn StoreObserver>);
    let meta = SqliteMetaStore::open_observed(SqliteConfig::new(&config.meta_db), observer)
        .map_err(|e| format!("open {}: {e:?}", config.meta_db.display()))?;
    // Every reader verifies the frames it decodes (the engine, the index,
    // fsck), so the store does not decode and hash each one first.
    let objects = FsObjectStore::open(&config.objects_dir, FsMode::Authoritative)
        .map_err(|e| format!("open {}: {e:?}", config.objects_dir.display()))?
        .verify_reads(false);
    Ok((meta, objects))
}

async fn run(config: ServerConfig) -> Result<(), String> {
    // Handlers first, before telemetry, stores or any listener. The forwarder
    // turns a signal into a flag that every later step can see.
    let mut shutdown = Shutdown::install()?;
    let (signal_tx, signal_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown.recv().await;
        let _ = signal_tx.send(true);
    });
    println!("graft-server signals armed");

    let telemetry = Arc::new(
        graft_telemetry::init(&TelemetryConfig {
            service: "graft-server".into(),
            json_logs: config.json_logs,
            metrics_addr: config.metrics.clone(),
            otlp_endpoint: config.otlp_endpoint.clone(),
        })
        .map_err(|e| format!("telemetry: {e}"))?,
    );
    let started = Instant::now();
    let (meta, objects) = open_stores(&config, Some(telemetry.clone()))?;
    let signing =
        keys::load_or_create(&config.key_path()).map_err(|e| format!("signing key: {e}"))?;
    let engine = Arc::new(Engine::new(
        Arc::new(meta),
        Arc::new(objects),
        Arc::new(SystemClock),
        EngineConfig::default(),
    ));
    let mut volumes = vec![("objects", config.objects_dir.clone())];
    if let Some(dir) = config
        .meta_db
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        volumes.push(("meta", dir.to_path_buf()));
    }
    let admission = Admission::new(volumes, config.min_free_fraction, config.min_free_bytes);
    let denylist = Denylist::open(&config.meta_db)?;
    let ci = CiExchange::load(&config.ci)?.map(Arc::new);
    if let Some(ci) = &ci {
        // Fetched before the server is ready, so the first CI job does not
        // wait on it; a failure is logged and retried on the timer.
        ci.refresh_all().await;
        tokio::spawn(ci.clone().refresh_forever());
    }
    let app = Arc::new(
        App::new(
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
        )
        .with_ci(ci),
    );

    if std::env::var_os(HOLD_START_UP_ENV).is_some() {
        // Test hook: hold start-up here until a stop is requested, so a test
        // can signal the server before any listener is bound.
        println!("graft-server holding start-up");
        let _ = signal_rx.clone().wait_for(|stop| *stop).await;
    }

    let listener = bind(&config.listen).await?;
    let addr = local_addr(&listener)?;
    let metrics_listener = match &config.metrics {
        Some(m) => Some(bind(m).await?),
        None => None,
    };
    if *signal_rx.borrow() {
        // Stopped during start-up: the listeners drop here, unserved.
        tracing::info!("stopped during start-up");
        return Ok(());
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    let stopped = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };

    let router = grpc::stamp_version(Services::new(app.clone()).routes().into_axum_router())
        .merge(http::ops_router(app.clone()))
        .merge(http::facade_router(app.clone()))
        .merge(mcp::router(app.clone()));
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(
        tonic::transport::Server::builder()
            .accept_http1(true)
            .add_routes(tonic::service::Routes::from(router))
            .serve_with_incoming_shutdown(incoming(listener), stopped(stop_rx.clone())),
    );
    let mut metrics_addr = None;
    if let Some(listener) = metrics_listener {
        metrics_addr = Some(local_addr(&listener)?);
        servers.spawn(
            tonic::transport::Server::builder()
                .accept_http1(true)
                .add_routes(tonic::service::Routes::from(http::ops_router(app.clone())))
                .serve_with_incoming_shutdown(incoming(listener), stopped(stop_rx)),
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
        () = stopped(signal_rx) => {}
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

/// Accepted connections with Nagle off. The builder's `tcp_nodelay` is
/// ignored by `serve_with_incoming*`, so it is set here; without it a small
/// response waits for the client's delayed ACK (about 40 ms on Linux).
fn incoming(listener: TcpListener) -> TcpIncoming {
    TcpIncoming::from(listener).with_nodelay(Some(true))
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

/// SIGTERM and SIGINT. The handlers are installed when this is built, so a
/// signal from then on is kept until it is received, never lost.
#[cfg(unix)]
struct Shutdown {
    term: Signal,
    interrupt: Signal,
}

#[cfg(unix)]
impl Shutdown {
    fn install() -> Result<Self, String> {
        Ok(Self {
            term: signal(SignalKind::terminate()).map_err(|e| format!("SIGTERM handler: {e}"))?,
            interrupt: signal(SignalKind::interrupt())
                .map_err(|e| format!("SIGINT handler: {e}"))?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.interrupt.recv() => {}
        }
    }
}

#[cfg(not(unix))]
struct Shutdown;

#[cfg(not(unix))]
impl Shutdown {
    fn install() -> Result<Self, String> {
        Ok(Self)
    }

    async fn recv(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::codegen::tokio_stream::StreamExt;

    #[tokio::test]
    async fn accepted_connections_have_nodelay() {
        let listener = bind("127.0.0.1:0").await.unwrap();
        let addr = local_addr(&listener).unwrap();
        let mut accepted = incoming(listener);
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let stream = accepted.next().await.unwrap().unwrap();
        assert!(stream.nodelay().unwrap());
    }
}
