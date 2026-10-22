//! The node's metrics: the shared registry names (`docs/design.md` §13) with
//! `tier="node"`, plus node-only series under `graft_node_*`.

use graft_telemetry::Metrics;
use prometheus::{IntCounter, IntCounterVec, IntGauge, Opts, Registry};

/// The tier label value.
pub(crate) const TIER: &str = "node";

/// Every series the node records.
#[derive(Clone, Debug)]
pub struct NodeMetrics {
    registry: Registry,
    /// The shared registry handles.
    pub base: Metrics,
    /// Objects answered from the cache.
    pub cache_hits: IntCounter,
    /// Objects not in the cache.
    pub cache_misses: IntCounter,
    /// Objects fetched from upstream (one per single-flight leader).
    pub upstream_objects: IntCounter,
    /// Calls shed, by reason (`queue`, `wait`, `inflight`, `breaker`).
    pub shed: IntCounterVec,
    /// Corrupt objects, by source (`cache`, `upstream`).
    pub corrupt: IntCounterVec,
    /// Upstream answers that broke the protocol: unrequested or repeated
    /// ids, oversized answers.
    pub upstream_protocol_errors: IntCounter,
    /// Fetched objects served but not cached, by reason (`disk`, `error`,
    /// `repos`).
    pub cache_skipped: IntCounterVec,
    /// Open socket connections.
    pub connections: IntGauge,
    /// Connections closed on accept because the bound was reached.
    pub connections_refused: IntCounter,
    /// Distinct objects being fetched.
    pub inflight_objects: IntGauge,
    /// Calls forwarded upstream.
    pub forwarded: IntCounter,
}

impl NodeMetrics {
    /// A fresh registry with the shared and node series.
    pub fn new() -> Result<Self, prometheus::Error> {
        let (registry, base) = graft_telemetry::registry()?;
        Self::register(registry, base)
    }

    /// Registers the node series beside `base` in `registry`.
    pub fn register(registry: Registry, base: Metrics) -> Result<Self, prometheus::Error> {
        let counter = |name: &str, help: &str| -> Result<IntCounter, prometheus::Error> {
            let c = IntCounter::with_opts(Opts::new(name, help))?;
            registry.register(Box::new(c.clone()))?;
            Ok(c)
        };
        let counter_vec =
            |name: &str, help: &str, label: &str| -> Result<IntCounterVec, prometheus::Error> {
                let c = IntCounterVec::new(Opts::new(name, help), &[label])?;
                registry.register(Box::new(c.clone()))?;
                Ok(c)
            };
        let gauge = |name: &str, help: &str| -> Result<IntGauge, prometheus::Error> {
            let g = IntGauge::with_opts(Opts::new(name, help))?;
            registry.register(Box::new(g.clone()))?;
            Ok(g)
        };
        Ok(Self {
            cache_hits: counter("graft_node_cache_hits_total", "Objects served from cache.")?,
            cache_misses: counter("graft_node_cache_misses_total", "Objects not in cache.")?,
            upstream_objects: counter(
                "graft_node_upstream_objects_total",
                "Objects fetched from upstream.",
            )?,
            shed: counter_vec("graft_node_shed_total", "Calls shed, by reason.", "reason")?,
            corrupt: counter_vec(
                "graft_node_corrupt_total",
                "Corrupt objects, by source.",
                "source",
            )?,
            upstream_protocol_errors: counter(
                "graft_node_upstream_protocol_errors_total",
                "Upstream answers that broke the protocol.",
            )?,
            cache_skipped: counter_vec(
                "graft_node_cache_skipped_total",
                "Fetched objects served but not cached, by reason.",
                "reason",
            )?,
            connections: gauge("graft_node_connections", "Open socket connections.")?,
            connections_refused: counter(
                "graft_node_connections_refused_total",
                "Connections closed at the bound.",
            )?,
            inflight_objects: gauge(
                "graft_node_inflight_objects",
                "Distinct objects being fetched.",
            )?,
            forwarded: counter("graft_node_forwarded_total", "Calls forwarded upstream.")?,
            registry,
            base,
        })
    }

    /// The registry in the Prometheus text format.
    pub fn encode(&self) -> String {
        graft_telemetry::encode(&self.registry)
    }

    /// Records a cache lookup and refreshes the hit ratio.
    pub(crate) fn lookup(&self, hit: bool) {
        if hit {
            self.cache_hits.inc();
        } else {
            self.cache_misses.inc();
        }
        let (h, m) = (self.cache_hits.get(), self.cache_misses.get());
        self.base
            .cache_hit_ratio
            .with_label_values(&[TIER])
            .set(h as f64 / (h + m).max(1) as f64);
    }

    /// Records one call's latency and outcome.
    pub(crate) fn rpc(&self, op: &str, outcome: &str, started: std::time::Instant) {
        self.base
            .rpc_duration_seconds
            .with_label_values(&[op, TIER, outcome])
            .observe(started.elapsed().as_secs_f64());
    }
}
