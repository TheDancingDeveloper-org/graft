//! With the `otlp` feature, `init` builds an OTLP/HTTP exporter. Nothing
//! listens on the endpoint; export failures are the exporter's to log.
#![cfg(feature = "otlp")]

use graft_telemetry::{TelemetryConfig, init};

#[test]
fn init_with_otlp() {
    let t = init(&TelemetryConfig {
        service: "graft-test".into(),
        json_logs: false,
        metrics_addr: None,
        otlp_endpoint: Some("http://127.0.0.1:9/v1/traces".into()),
    })
    .unwrap();
    {
        let span = tracing::info_span!("rpc", op = "Fetch");
        let _g = span.enter();
        tracing::info!("inside a traced span");
    }
    drop(t);
}
