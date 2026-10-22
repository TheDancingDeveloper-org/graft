//! Metric names. Every name in `docs/design.md` §13 is listed here, so
//! dashboards and alerts have one source of truth.

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
