//! Metric names, kinds and labels. Every metric in `docs/design.md` §13 is
//! listed here, so dashboards and alerts have one source of truth, and
//! [`Metrics`] registers each one under exactly this name and these labels.

use prometheus::core::Collector;
use prometheus::{
    GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, Registry,
};

/// RPC latency by op, tier and outcome.
pub const RPC_DURATION_SECONDS: &str = "graft_rpc_duration_seconds";
/// Land attempts per land (histogram).
pub const LAND_ATTEMPTS: &str = "graft_land_attempts";
/// Land outcomes.
pub const LAND_OUTCOME_TOTAL: &str = "graft_land_outcome_total";
/// Land request to visible on the line.
pub const LAND_TO_VISIBLE_SECONDS: &str = "graft_land_to_visible_seconds";
/// Paths resolved by line merge.
pub const REBASE_MERGED_PATHS_TOTAL: &str = "graft_rebase_merged_paths_total";
/// Cache hit ratio by tier.
pub const CACHE_HIT_RATIO: &str = "graft_cache_hit_ratio";
/// Requests served by joining an in-flight fetch.
pub const SINGLEFLIGHT_SHARED_TOTAL: &str = "graft_singleflight_shared_total";
/// Fetched bytes by tier and codec.
pub const FETCH_BYTES_TOTAL: &str = "graft_fetch_bytes_total";
/// Fetched objects.
pub const FETCH_OBJECTS_TOTAL: &str = "graft_fetch_objects_total";
/// Store operation latency.
pub const STORE_OP_SECONDS: &str = "graft_store_op_seconds";
/// SQLite writer queue depth.
pub const SQLITE_WRITER_QUEUE_DEPTH: &str = "graft_sqlite_writer_queue_depth";
/// Orphans by state.
pub const ORPHANS: &str = "graft_orphans";
/// Bytes reclaimed by GC.
pub const GC_RECLAIMED_BYTES_TOTAL: &str = "graft_gc_reclaimed_bytes_total";
/// Mirror lag in seconds.
pub const MIRROR_LAG_SECONDS: &str = "graft_mirror_lag_seconds";
/// Mirror lag in changesets.
pub const MIRROR_LAG_CHANGESETS: &str = "graft_mirror_lag_changesets";
/// 1 when a line has diverged.
pub const MIRROR_DIVERGENCE: &str = "graft_mirror_divergence";
/// Latest journal LSN.
pub const JOURNAL_LSN: &str = "graft_journal_lsn";
/// Active watch subscribers.
pub const WATCH_SUBSCRIBERS: &str = "graft_watch_subscribers";
/// Workspaces by state.
pub const WORKSPACES: &str = "graft_workspaces";
/// Expired leases.
pub const LEASE_EXPIRED_TOTAL: &str = "graft_lease_expired_total";

/// Every metric name, for registration tests.
pub const ALL: &[&str] = &[
    RPC_DURATION_SECONDS,
    LAND_ATTEMPTS,
    LAND_OUTCOME_TOTAL,
    LAND_TO_VISIBLE_SECONDS,
    REBASE_MERGED_PATHS_TOTAL,
    CACHE_HIT_RATIO,
    SINGLEFLIGHT_SHARED_TOTAL,
    FETCH_BYTES_TOTAL,
    FETCH_OBJECTS_TOTAL,
    STORE_OP_SECONDS,
    SQLITE_WRITER_QUEUE_DEPTH,
    ORPHANS,
    GC_RECLAIMED_BYTES_TOTAL,
    MIRROR_LAG_SECONDS,
    MIRROR_LAG_CHANGESETS,
    MIRROR_DIVERGENCE,
    JOURNAL_LSN,
    WATCH_SUBSCRIBERS,
    WORKSPACES,
    LEASE_EXPIRED_TOTAL,
];

/// What kind of Prometheus metric a name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    /// Monotonic counter.
    Counter,
    /// Gauge.
    Gauge,
    /// Histogram.
    Histogram,
}

/// One registry entry: name, kind, label names and help text.
#[derive(Clone, Copy, Debug)]
pub struct MetricSpec {
    /// Metric name.
    pub name: &'static str,
    /// Kind.
    pub kind: MetricKind,
    /// Label names, in order.
    pub labels: &'static [&'static str],
    /// Help text.
    pub help: &'static str,
}

const fn spec(
    name: &'static str,
    kind: MetricKind,
    labels: &'static [&'static str],
    help: &'static str,
) -> MetricSpec {
    MetricSpec {
        name,
        kind,
        labels,
        help,
    }
}

use MetricKind::{Counter, Gauge as G, Histogram as H};

/// Every metric with its kind and labels, in the order of [`ALL`].
pub const SPECS: &[MetricSpec] = &[
    spec(
        RPC_DURATION_SECONDS,
        H,
        &["op", "tier", "outcome"],
        "RPC latency by operation, tier and outcome.",
    ),
    spec(LAND_ATTEMPTS, H, &["line"], "Attempts per land."),
    spec(
        LAND_OUTCOME_TOTAL,
        Counter,
        &["line", "outcome"],
        "Land outcomes.",
    ),
    spec(
        LAND_TO_VISIBLE_SECONDS,
        H,
        &[],
        "Land request to visible on the line.",
    ),
    spec(
        REBASE_MERGED_PATHS_TOTAL,
        Counter,
        &[],
        "Paths resolved by line-level merge.",
    ),
    spec(CACHE_HIT_RATIO, G, &["tier"], "Cache hit ratio by tier."),
    spec(
        SINGLEFLIGHT_SHARED_TOTAL,
        Counter,
        &[],
        "Requests served by joining an in-flight fetch.",
    ),
    spec(
        FETCH_BYTES_TOTAL,
        Counter,
        &["tier", "codec"],
        "Fetched bytes by tier and codec.",
    ),
    spec(FETCH_OBJECTS_TOTAL, Counter, &[], "Fetched objects."),
    spec(
        STORE_OP_SECONDS,
        H,
        &["port", "op"],
        "Store operation latency by port and operation.",
    ),
    spec(
        SQLITE_WRITER_QUEUE_DEPTH,
        G,
        &[],
        "SQLite writer queue depth.",
    ),
    spec(ORPHANS, G, &["state"], "Orphaned changesets by state."),
    spec(
        GC_RECLAIMED_BYTES_TOTAL,
        Counter,
        &[],
        "Bytes reclaimed by GC.",
    ),
    spec(MIRROR_LAG_SECONDS, G, &["line"], "Mirror lag in seconds."),
    spec(
        MIRROR_LAG_CHANGESETS,
        G,
        &["line"],
        "Mirror lag in changesets.",
    ),
    spec(
        MIRROR_DIVERGENCE,
        G,
        &["line"],
        "1 when a mirrored line has diverged.",
    ),
    spec(JOURNAL_LSN, G, &["repo"], "Latest journal LSN."),
    spec(WATCH_SUBSCRIBERS, G, &[], "Active watch subscribers."),
    spec(WORKSPACES, G, &["state"], "Workspaces by state."),
    spec(
        LEASE_EXPIRED_TOTAL,
        Counter,
        &[],
        "Expired workspace leases.",
    ),
];

fn find(name: &str) -> &'static MetricSpec {
    SPECS
        .iter()
        .find(|s| s.name == name)
        .expect("every metric constant has a spec")
}

fn opts(name: &str) -> Opts {
    let s = find(name);
    Opts::new(s.name, s.help)
}

fn hopts(name: &str, buckets: Vec<f64>) -> HistogramOpts {
    let s = find(name);
    HistogramOpts::new(s.name, s.help).buckets(buckets)
}

/// Latency buckets: 0.5 ms to about 16 s.
fn latency_buckets() -> Vec<f64> {
    prometheus::exponential_buckets(0.0005, 2.0, 16).expect("valid buckets")
}

/// The metric handles. Cloning is cheap; clones share the same series.
#[derive(Clone)]
pub struct Metrics {
    /// [`RPC_DURATION_SECONDS`] `{op,tier,outcome}`.
    pub rpc_duration_seconds: HistogramVec,
    /// [`LAND_ATTEMPTS`] `{line}`.
    pub land_attempts: HistogramVec,
    /// [`LAND_OUTCOME_TOTAL`] `{line,outcome}`.
    pub land_outcome_total: IntCounterVec,
    /// [`LAND_TO_VISIBLE_SECONDS`].
    pub land_to_visible_seconds: Histogram,
    /// [`REBASE_MERGED_PATHS_TOTAL`].
    pub rebase_merged_paths_total: IntCounter,
    /// [`CACHE_HIT_RATIO`] `{tier}`.
    pub cache_hit_ratio: GaugeVec,
    /// [`SINGLEFLIGHT_SHARED_TOTAL`].
    pub singleflight_shared_total: IntCounter,
    /// [`FETCH_BYTES_TOTAL`] `{tier,codec}`.
    pub fetch_bytes_total: IntCounterVec,
    /// [`FETCH_OBJECTS_TOTAL`].
    pub fetch_objects_total: IntCounter,
    /// [`STORE_OP_SECONDS`] `{port,op}`.
    pub store_op_seconds: HistogramVec,
    /// [`SQLITE_WRITER_QUEUE_DEPTH`].
    pub sqlite_writer_queue_depth: IntGauge,
    /// [`ORPHANS`] `{state}`.
    pub orphans: IntGaugeVec,
    /// [`GC_RECLAIMED_BYTES_TOTAL`].
    pub gc_reclaimed_bytes_total: IntCounter,
    /// [`MIRROR_LAG_SECONDS`] `{line}`.
    pub mirror_lag_seconds: GaugeVec,
    /// [`MIRROR_LAG_CHANGESETS`] `{line}`.
    pub mirror_lag_changesets: IntGaugeVec,
    /// [`MIRROR_DIVERGENCE`] `{line}`.
    pub mirror_divergence: IntGaugeVec,
    /// [`JOURNAL_LSN`] `{repo}`.
    pub journal_lsn: IntGaugeVec,
    /// [`WATCH_SUBSCRIBERS`].
    pub watch_subscribers: IntGauge,
    /// [`WORKSPACES`] `{state}`.
    pub workspaces: IntGaugeVec,
    /// [`LEASE_EXPIRED_TOTAL`].
    pub lease_expired_total: IntCounter,
}

impl Metrics {
    /// Creates every metric in [`SPECS`] and registers it in `registry`.
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let l = |name: &str| find(name).labels;
        let m = Self {
            rpc_duration_seconds: HistogramVec::new(
                hopts(RPC_DURATION_SECONDS, latency_buckets()),
                l(RPC_DURATION_SECONDS),
            )?,
            land_attempts: HistogramVec::new(
                hopts(LAND_ATTEMPTS, vec![1.0, 2.0, 3.0, 5.0, 8.0, 13.0, 21.0]),
                l(LAND_ATTEMPTS),
            )?,
            land_outcome_total: IntCounterVec::new(
                opts(LAND_OUTCOME_TOTAL),
                l(LAND_OUTCOME_TOTAL),
            )?,
            land_to_visible_seconds: Histogram::with_opts(hopts(
                LAND_TO_VISIBLE_SECONDS,
                latency_buckets(),
            ))?,
            rebase_merged_paths_total: IntCounter::with_opts(opts(REBASE_MERGED_PATHS_TOTAL))?,
            cache_hit_ratio: GaugeVec::new(opts(CACHE_HIT_RATIO), l(CACHE_HIT_RATIO))?,
            singleflight_shared_total: IntCounter::with_opts(opts(SINGLEFLIGHT_SHARED_TOTAL))?,
            fetch_bytes_total: IntCounterVec::new(opts(FETCH_BYTES_TOTAL), l(FETCH_BYTES_TOTAL))?,
            fetch_objects_total: IntCounter::with_opts(opts(FETCH_OBJECTS_TOTAL))?,
            store_op_seconds: HistogramVec::new(
                hopts(STORE_OP_SECONDS, latency_buckets()),
                l(STORE_OP_SECONDS),
            )?,
            sqlite_writer_queue_depth: IntGauge::with_opts(opts(SQLITE_WRITER_QUEUE_DEPTH))?,
            orphans: IntGaugeVec::new(opts(ORPHANS), l(ORPHANS))?,
            gc_reclaimed_bytes_total: IntCounter::with_opts(opts(GC_RECLAIMED_BYTES_TOTAL))?,
            mirror_lag_seconds: GaugeVec::new(opts(MIRROR_LAG_SECONDS), l(MIRROR_LAG_SECONDS))?,
            mirror_lag_changesets: IntGaugeVec::new(
                opts(MIRROR_LAG_CHANGESETS),
                l(MIRROR_LAG_CHANGESETS),
            )?,
            mirror_divergence: IntGaugeVec::new(opts(MIRROR_DIVERGENCE), l(MIRROR_DIVERGENCE))?,
            journal_lsn: IntGaugeVec::new(opts(JOURNAL_LSN), l(JOURNAL_LSN))?,
            watch_subscribers: IntGauge::with_opts(opts(WATCH_SUBSCRIBERS))?,
            workspaces: IntGaugeVec::new(opts(WORKSPACES), l(WORKSPACES))?,
            lease_expired_total: IntCounter::with_opts(opts(LEASE_EXPIRED_TOTAL))?,
        };
        for c in m.collectors() {
            registry.register(c)?;
        }
        Ok(m)
    }

    /// Every metric as a collector, in the order of [`ALL`].
    pub fn collectors(&self) -> Vec<Box<dyn Collector>> {
        vec![
            Box::new(self.rpc_duration_seconds.clone()),
            Box::new(self.land_attempts.clone()),
            Box::new(self.land_outcome_total.clone()),
            Box::new(self.land_to_visible_seconds.clone()),
            Box::new(self.rebase_merged_paths_total.clone()),
            Box::new(self.cache_hit_ratio.clone()),
            Box::new(self.singleflight_shared_total.clone()),
            Box::new(self.fetch_bytes_total.clone()),
            Box::new(self.fetch_objects_total.clone()),
            Box::new(self.store_op_seconds.clone()),
            Box::new(self.sqlite_writer_queue_depth.clone()),
            Box::new(self.orphans.clone()),
            Box::new(self.gc_reclaimed_bytes_total.clone()),
            Box::new(self.mirror_lag_seconds.clone()),
            Box::new(self.mirror_lag_changesets.clone()),
            Box::new(self.mirror_divergence.clone()),
            Box::new(self.journal_lsn.clone()),
            Box::new(self.watch_subscribers.clone()),
            Box::new(self.workspaces.clone()),
            Box::new(self.lease_expired_total.clone()),
        ]
    }
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics").finish_non_exhaustive()
    }
}
