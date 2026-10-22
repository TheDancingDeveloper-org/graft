//! Every metric in design §13 is registered under its documented name and
//! labels.

use std::collections::BTreeSet;

use graft_telemetry::metrics::{ALL, MetricKind, SPECS};

/// Design §13's table, transcribed: name and label names.
const DESIGN: &[(&str, &[&str])] = &[
    ("graft_rpc_duration_seconds", &["op", "tier", "outcome"]),
    ("graft_land_attempts", &["line"]),
    ("graft_land_outcome_total", &["line", "outcome"]),
    ("graft_land_to_visible_seconds", &[]),
    ("graft_rebase_merged_paths_total", &[]),
    ("graft_cache_hit_ratio", &["tier"]),
    ("graft_singleflight_shared_total", &[]),
    ("graft_fetch_bytes_total", &["tier", "codec"]),
    ("graft_fetch_objects_total", &[]),
    ("graft_store_op_seconds", &["port", "op"]),
    ("graft_sqlite_writer_queue_depth", &[]),
    ("graft_orphans", &["state"]),
    ("graft_gc_reclaimed_bytes_total", &[]),
    ("graft_mirror_lag_seconds", &["line"]),
    ("graft_mirror_lag_changesets", &["line"]),
    ("graft_mirror_divergence", &["line"]),
    ("graft_journal_lsn", &["repo"]),
    ("graft_watch_subscribers", &[]),
    ("graft_workspaces", &["state"]),
    ("graft_lease_expired_total", &[]),
];

#[test]
fn specs_match_design() {
    let names: Vec<&str> = SPECS.iter().map(|s| s.name).collect();
    assert_eq!(names, ALL);
    assert_eq!(SPECS.len(), DESIGN.len());
    for (spec, (name, labels)) in SPECS.iter().zip(DESIGN) {
        assert_eq!(spec.name, *name);
        assert_eq!(spec.labels, *labels, "{name}");
        assert!(!spec.help.is_empty(), "{name}");
        // Prometheus naming: counters end in _total; _total is only counters.
        assert_eq!(
            spec.kind == MetricKind::Counter,
            name.ends_with("_total"),
            "{name}"
        );
    }
    // design §13 calls graft_land_attempts a histogram.
    assert_eq!(SPECS[1].kind, MetricKind::Histogram);
}

#[test]
fn every_metric_is_registered_with_its_labels() {
    let (_registry, metrics) = graft_telemetry::registry().unwrap();
    let collectors = metrics.collectors();
    assert_eq!(collectors.len(), SPECS.len());
    let mut seen = BTreeSet::new();
    for (c, spec) in collectors.iter().zip(SPECS) {
        let descs = c.desc();
        assert_eq!(descs.len(), 1, "{}", spec.name);
        let d = descs[0];
        assert_eq!(d.fq_name, spec.name);
        assert_eq!(d.variable_labels, spec.labels, "{}", spec.name);
        assert_eq!(d.help, spec.help);
        seen.insert(d.fq_name.clone());
    }
    assert_eq!(seen.len(), ALL.len());
}

#[test]
fn exposition_uses_documented_names_and_labels() {
    let (registry, m) = graft_telemetry::registry().unwrap();
    m.rpc_duration_seconds
        .with_label_values(&["Fetch", "server", "ok"])
        .observe(0.01);
    m.land_attempts.with_label_values(&["main"]).observe(2.0);
    m.land_outcome_total
        .with_label_values(&["main", "landed"])
        .inc();
    m.land_to_visible_seconds.observe(0.2);
    m.rebase_merged_paths_total.inc();
    m.cache_hit_ratio.with_label_values(&["node"]).set(0.9);
    m.singleflight_shared_total.inc();
    m.fetch_bytes_total
        .with_label_values(&["node", "zstd"])
        .inc_by(10);
    m.fetch_objects_total.inc();
    m.store_op_seconds
        .with_label_values(&["meta", "put_ref"])
        .observe(0.001);
    m.sqlite_writer_queue_depth.set(3);
    m.orphans.with_label_values(&["held"]).set(1);
    m.gc_reclaimed_bytes_total.inc_by(5);
    m.mirror_lag_seconds.with_label_values(&["main"]).set(1.5);
    m.mirror_lag_changesets.with_label_values(&["main"]).set(2);
    m.mirror_divergence.with_label_values(&["main"]).set(0);
    m.journal_lsn.with_label_values(&["graft"]).set(42);
    m.watch_subscribers.set(4);
    m.workspaces.with_label_values(&["active"]).set(6);
    m.lease_expired_total.inc();

    let text = graft_telemetry::encode(&registry);
    for spec in SPECS {
        let kind = match spec.kind {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
        };
        assert!(
            text.contains(&format!("# TYPE {} {kind}\n", spec.name)),
            "{}",
            spec.name
        );
    }
    assert!(
        text.contains(
            r#"graft_rpc_duration_seconds_count{op="Fetch",outcome="ok",tier="server"} 1"#
        )
    );
    assert!(text.contains(r#"graft_journal_lsn{repo="graft"} 42"#));
    assert!(text.contains("graft_watch_subscribers 4"));
}
