//! The service's metrics (D41, `docs/notes/mirror-service.md` §3.3): one
//! `/metrics` endpoint, every mirror series labelled `repo`. A repository's
//! series exist only while the service has a task for it; the service drops
//! them all when the task stops, so the label's cardinality follows the
//! enabled rows and a removed or disabled mirror leaves nothing behind.
//!
//! The lag and divergence series come from the follower and are registered
//! by `graft-telemetry`; this adds the service's own.

use graft_proto::pb::MirrorErrorCode;
use prometheus::core::Collector;
use prometheus::{GaugeVec, IntCounter, IntGauge, IntGaugeVec, Opts, Registry};

use graft_telemetry::Metrics;

/// Wall-clock seconds of a repository's last successful poll.
pub const LAST_SUCCESS_SECONDS: &str = "graft_mirror_last_success_timestamp_seconds";
/// 1 for the typed error a repository's last poll ended in.
pub const POLL_ERROR: &str = "graft_mirror_poll_error";
/// 1 while a repository's request turn left work behind.
pub const REQUEST_BACKLOG: &str = "graft_mirror_request_backlog";
/// Leases lost by this process.
pub const LEASE_LOST_TOTAL: &str = "graft_mirror_lease_lost_total";
/// Repository tasks running.
pub const TASKS: &str = "graft_mirror_tasks";

/// Every `error_code` a poll can end in: the bound on `graft_mirror_poll_error`'s
/// `code` label, and the series [`ServiceMetrics::drop_repo`] removes.
const ERROR_CODES: [MirrorErrorCode; 10] = [
    MirrorErrorCode::UpstreamUnreachable,
    MirrorErrorCode::AuthFailed,
    MirrorErrorCode::NotFound,
    MirrorErrorCode::Diverged,
    MirrorErrorCode::ForcePushed,
    MirrorErrorCode::RateLimited,
    MirrorErrorCode::Timeout,
    MirrorErrorCode::UrlBlocked,
    MirrorErrorCode::LeaseLost,
    MirrorErrorCode::Internal,
];

/// The `code` label: the design's name for `code`.
fn code_name(code: MirrorErrorCode) -> &'static str {
    match code {
        MirrorErrorCode::Unspecified => "unspecified",
        MirrorErrorCode::UpstreamUnreachable => "upstream_unreachable",
        MirrorErrorCode::AuthFailed => "auth_failed",
        MirrorErrorCode::NotFound => "not_found",
        MirrorErrorCode::Diverged => "diverged",
        MirrorErrorCode::ForcePushed => "force_pushed",
        MirrorErrorCode::RateLimited => "rate_limited",
        MirrorErrorCode::Timeout => "timeout",
        MirrorErrorCode::UrlBlocked => "url_blocked",
        MirrorErrorCode::LeaseLost => "lease_lost",
        MirrorErrorCode::Internal => "internal",
    }
}

/// The service's series, over the follower's [`Metrics`].
#[derive(Clone, Debug)]
pub struct ServiceMetrics {
    base: Metrics,
    last_success: GaugeVec,
    poll_error: IntGaugeVec,
    request_backlog: IntGaugeVec,
    lease_lost_total: IntCounter,
    tasks: IntGauge,
}

impl ServiceMetrics {
    /// Registers the service's series in `registry`, next to `base` (which
    /// the caller registered there).
    pub fn register(registry: &Registry, base: Metrics) -> Result<Self, prometheus::Error> {
        let m = ServiceMetrics {
            base,
            last_success: GaugeVec::new(
                Opts::new(
                    LAST_SUCCESS_SECONDS,
                    "Unix time of the repository's last successful poll.",
                ),
                &["repo"],
            )?,
            poll_error: IntGaugeVec::new(
                Opts::new(
                    POLL_ERROR,
                    "1 for the error code the repository's last poll ended in; absent after a clean poll.",
                ),
                &["repo", "code"],
            )?,
            request_backlog: IntGaugeVec::new(
                Opts::new(
                    REQUEST_BACKLOG,
                    "1 while the repository's last request turn left work behind (a retry, or the per-turn cap).",
                ),
                &["repo"],
            )?,
            lease_lost_total: IntCounter::with_opts(Opts::new(
                LEASE_LOST_TOTAL,
                "Repository leases this process lost.",
            ))?,
            tasks: IntGauge::with_opts(Opts::new(TASKS, "Repository tasks running."))?,
        };
        let all: [Box<dyn Collector>; 5] = [
            Box::new(m.last_success.clone()),
            Box::new(m.poll_error.clone()),
            Box::new(m.request_backlog.clone()),
            Box::new(m.lease_lost_total.clone()),
            Box::new(m.tasks.clone()),
        ];
        for c in all {
            registry.register(c)?;
        }
        Ok(m)
    }

    /// The follower's series (lag, divergence), which gain `repo` too.
    pub fn base(&self) -> &Metrics {
        &self.base
    }

    /// A poll of `repo` ended: its error code (a clean poll has none), and
    /// the time of its last success.
    pub fn poll_finished(
        &self,
        repo: &str,
        last_success_s: Option<f64>,
        failure: Option<MirrorErrorCode>,
    ) {
        if let Some(t) = last_success_s {
            self.last_success.with_label_values(&[repo]).set(t);
        }
        for code in ERROR_CODES {
            if Some(code) != failure {
                let _ = self
                    .poll_error
                    .remove_label_values(&[repo, code_name(code)]);
            }
        }
        if let Some(code) = failure.filter(|c| ERROR_CODES.contains(c)) {
            self.poll_error
                .with_label_values(&[repo, code_name(code)])
                .set(1);
        }
    }

    /// `repo`'s request turn finished; `left` when work may remain.
    pub fn request_turn(&self, repo: &str, left: bool) {
        self.request_backlog
            .with_label_values(&[repo])
            .set(i64::from(left));
    }

    /// A lease was lost.
    pub fn lease_lost(&self) {
        self.lease_lost_total.inc();
    }

    /// `n` tasks are running.
    pub fn set_tasks(&self, n: usize) {
        self.tasks.set(n as i64);
    }

    /// Removes every series of `repo`, whose task stopped. `lines` are the
    /// lines its row had.
    pub fn drop_repo(&self, repo: &str, lines: &[String]) {
        for line in lines {
            let l = [repo, line.as_str()];
            let _ = self.base.mirror_lag_seconds.remove_label_values(&l);
            let _ = self.base.mirror_lag_changesets.remove_label_values(&l);
            let _ = self.base.mirror_divergence.remove_label_values(&l);
        }
        let _ = self.last_success.remove_label_values(&[repo]);
        let _ = self.request_backlog.remove_label_values(&[repo]);
        for code in ERROR_CODES {
            let _ = self
                .poll_error
                .remove_label_values(&[repo, code_name(code)]);
        }
    }
}
