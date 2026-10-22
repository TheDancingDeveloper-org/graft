//! Tracing and metrics setup, and the metric name registry
//! (`docs/design.md` §13, work item G1.9).

pub mod metrics;

/// Telemetry configuration for a binary.
#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    /// Service name (`graft-server`, `graft-node`, ...).
    pub service: String,
    /// Emit JSON logs.
    pub json_logs: bool,
    /// Address for `/metrics`, if served separately.
    pub metrics_addr: Option<String>,
    /// OTLP endpoint for traces, if any.
    pub otlp_endpoint: Option<String>,
}

/// Initializes logging, metrics and tracing for the process.
pub fn init(_config: &TelemetryConfig) {
    todo!("G1.9")
}
