//! `init` installs the global subscriber once and owns a full registry. One
//! test per file: the subscriber is process-global.

use graft_telemetry::{TelemetryConfig, TelemetryError, init, metrics::ALL};

#[test]
fn init_once() {
    let config = TelemetryConfig {
        service: "graft-test".into(),
        json_logs: true,
        metrics_addr: None,
        otlp_endpoint: None,
    };
    let t = init(&config).unwrap();
    tracing::info!(repo = "graft", "telemetry up");
    t.metrics().watch_subscribers.set(1);
    t.metrics().lease_expired_total.inc();
    let text = t.encode();
    assert!(text.contains("graft_watch_subscribers 1"));
    assert!(text.contains("graft_lease_expired_total 1"));
    assert!(ALL.iter().all(|n| n.starts_with("graft_")));

    assert!(matches!(
        init(&config),
        Err(TelemetryError::AlreadyInitialized)
    ));

    // Without the `otlp` feature an OTLP endpoint is an error, not ignored.
    #[cfg(not(feature = "otlp"))]
    assert!(matches!(
        init(&TelemetryConfig {
            otlp_endpoint: Some("http://127.0.0.1:4318/v1/traces".into()),
            ..config
        }),
        Err(TelemetryError::OtlpDisabled)
    ));
}
