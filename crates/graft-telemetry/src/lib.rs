//! Tracing and metrics setup, and the metric name registry
//! (`docs/design.md` §13, work item G1.9).
//!
//! Logs go to stderr (so a CLI's stdout stays clean), as JSON when asked, and
//! filtered by `RUST_LOG` (default `info`). Metrics live in a per-process
//! Prometheus [`Registry`]; the binary serves [`Telemetry::encode`] at
//! `/metrics`. OTLP trace export is behind the `otlp` feature.

use std::fmt;
use std::io;

use prometheus::{Encoder as _, Registry, TextEncoder};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer, Registry as SubscriberRegistry};

pub mod metrics;

pub use metrics::Metrics;

/// Span field names every request span carries (design §13).
pub mod fields {
    /// Repository name.
    pub const REPO: &str = "repo";
    /// API operation.
    pub const OP: &str = "op";
    /// Client request id.
    pub const REQUEST_ID: &str = "request_id";
    /// Principal subject.
    pub const PRINCIPAL: &str = "principal";
    /// Session id (set by the caller's orchestrator, if any).
    pub const SESSION: &str = "session";
    /// Work item (set by the caller's orchestrator, if any).
    pub const WORK_ITEM: &str = "work_item";
    /// Serving tier (`server`, `node`, `client`).
    pub const TIER: &str = "tier";
}

/// Telemetry configuration for a binary.
#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    /// Service name (`graft-server`, `graft-node`, ...).
    pub service: String,
    /// Emit JSON logs.
    pub json_logs: bool,
    /// Address for `/metrics`, if served separately. The binary serves it;
    /// this crate only renders the registry.
    pub metrics_addr: Option<String>,
    /// OTLP/HTTP traces URL (for example `http://tempo:4318/v1/traces`), if
    /// any. Needs the `otlp` feature.
    pub otlp_endpoint: Option<String>,
}

/// Why telemetry could not start.
#[derive(Debug)]
pub enum TelemetryError {
    /// A global tracing subscriber is already installed.
    AlreadyInitialized,
    /// A metric could not be registered.
    Metrics(prometheus::Error),
    /// An OTLP endpoint was configured but the `otlp` feature is off.
    OtlpDisabled,
    /// The OTLP exporter could not be built.
    Otlp(String),
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitialized => f.write_str("telemetry is already initialized"),
            Self::Metrics(e) => write!(f, "metric registration failed: {e}"),
            Self::OtlpDisabled => {
                f.write_str("an OTLP endpoint is configured but graft was built without `otlp`")
            }
            Self::Otlp(e) => write!(f, "OTLP exporter: {e}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

impl From<prometheus::Error> for TelemetryError {
    fn from(e: prometheus::Error) -> Self {
        Self::Metrics(e)
    }
}

/// The process's telemetry: the metric registry and handles, and the trace
/// exporter. Keep it alive for the life of the process; dropping it flushes
/// pending traces.
pub struct Telemetry {
    registry: Registry,
    metrics: Metrics,
    #[cfg(feature = "otlp")]
    tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Telemetry {
    /// The metric handles.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// The Prometheus registry, for binaries that add their own collectors.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The registry in the Prometheus text exposition format, for `/metrics`.
    pub fn encode(&self) -> String {
        encode(&self.registry)
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        #[cfg(feature = "otlp")]
        if let Some(p) = self.tracer_provider.take() {
            // Best effort: the process is going away.
            let _ = p.shutdown();
        }
    }
}

impl fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Telemetry").finish_non_exhaustive()
    }
}

/// Renders `registry` in the Prometheus text exposition format.
pub fn encode(registry: &Registry) -> String {
    let mut out = Vec::new();
    // Encoding into a Vec only fails on a malformed metric family, which a
    // registry never yields.
    let _ = TextEncoder::new().encode(&registry.gather(), &mut out);
    String::from_utf8(out).unwrap_or_default()
}

/// A fresh registry with every graft metric registered.
pub fn registry() -> Result<(Registry, Metrics), prometheus::Error> {
    let registry = Registry::new();
    let metrics = Metrics::register(&registry)?;
    Ok((registry, metrics))
}

/// The log formatting layer `init` installs, writing to `writer`. Exposed so
/// tests can capture exactly what production would log.
pub fn log_layer<S, W>(json: bool, writer: W) -> Box<dyn Layer<S> + Send + Sync + 'static>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_target(true);
    if json {
        layer
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .boxed()
    } else {
        layer.with_ansi(false).boxed()
    }
}

/// Initializes logging, metrics and tracing for the process. Call once, early
/// in `main`; a second call fails with `AlreadyInitialized`.
pub fn init(config: &TelemetryConfig) -> Result<Telemetry, TelemetryError> {
    let (registry, metrics) = registry()?;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let base = SubscriberRegistry::default()
        .with(filter)
        .with(log_layer(config.json_logs, io::stderr));

    #[cfg(feature = "otlp")]
    {
        let (layer, tracer_provider) = match &config.otlp_endpoint {
            Some(endpoint) => {
                let (tracer, provider) = otlp::tracer(&config.service, endpoint)?;
                (
                    Some(tracing_opentelemetry::layer().with_tracer(tracer)),
                    Some(provider),
                )
            }
            None => (None, None),
        };
        base.with(layer)
            .try_init()
            .map_err(|_| TelemetryError::AlreadyInitialized)?;
        Ok(Telemetry {
            registry,
            metrics,
            tracer_provider,
        })
    }

    #[cfg(not(feature = "otlp"))]
    {
        if config.otlp_endpoint.is_some() {
            return Err(TelemetryError::OtlpDisabled);
        }
        base.try_init()
            .map_err(|_| TelemetryError::AlreadyInitialized)?;
        Ok(Telemetry { registry, metrics })
    }
}

#[cfg(feature = "otlp")]
mod otlp {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig as _};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::trace::{SdkTracerProvider, Tracer};

    use crate::TelemetryError;

    /// An OTLP/HTTP batch exporter to `endpoint`, and a tracer on it.
    pub(crate) fn tracer(
        service: &str,
        endpoint: &str,
    ) -> Result<(Tracer, SdkTracerProvider), TelemetryError> {
        let exporter = SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .map_err(|e| TelemetryError::Otlp(e.to_string()))?;
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                Resource::builder()
                    .with_service_name(service.to_owned())
                    .build(),
            )
            .build();
        Ok((provider.tracer("graft"), provider))
    }
}
