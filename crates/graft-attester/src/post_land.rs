//! Post-land mode (WI-2598; design §8.7 steps 2-5, decisions D25 and D37):
//! checks what landed on one checked line, after it landed, and reverts the
//! first bad land. It runs on the operator's infrastructure; the server
//! still runs nothing, and nothing here gates a land.
//!
//! - **Read.** The journal is the record: `RefMoved` entries of the line
//!   name its lands in order, `Attested` entries what each land's checks
//!   reported (whoever posted them, this checker before a restart
//!   included), and `LandReverted` entries which lands a revert undid. Only
//!   the read position is kept locally ([`Config::state_file`]): the
//!   position of the oldest land not yet settled. A restart reads from
//!   there and finds every verdict it had already posted, so it neither
//!   attests a land again nor lands a second revert.
//! - **Settled.** A land is `good` when every configured check's latest
//!   report passed, `bad` once a check failed more than `retries` times,
//!   as the server's land-checks row reads it.
//! - **Skip ahead.** When behind, the newest unsettled land is checked
//!   first. If it passes, every unsettled land up to it is good.
//! - **Bisect.** If it fails, the lands between the last good one and it
//!   are halved until the first bad land is found: at most
//!   `ceil(log2 n) + 1` runs of the failing check for `n` lands. The lands
//!   before it are attested good; it is attested failed.
//! - **Retry.** A failed check is run again, up to `retries` more times.
//!   One that then passes is a flake: the land is good, only the pass is
//!   attested, and [`PostLand::flakes`] counts it. A check that keeps
//!   failing is attested failed once per run, so the server's row reads
//!   `bad`.
//! - **Revert.** The first bad land is reverted (each changeset of its
//!   stack, newest first, with line merge) on the line's tip and landed
//!   through the ordinary land path. The server stamps each revert
//!   `graft.reverts`, and `graft.revert-check` with the link of the last
//!   failed run read for the land, and journals `LandReverted` with the
//!   failed check. Lands after it stay on the line; they are
//!   checked again on top of the revert: the revert's land is the snapshot
//!   that settles them. A revert that conflicts, or one that does not fix
//!   the line, is never forced: nothing is landed, the line is reported
//!   [`Blocked`], and the checker stops for the operator.
//!
//! The line must be configured `checks = "post-land"` on the server, so its
//! revert lands journal `LandReverted`. Costs follow the change: the read
//! is O(journal entries since the oldest unsettled land), and the window of
//! lands held is bounded ([`PostLand::set_window`]).

use std::fmt;
use std::fs;
use std::future::Future;
use std::io;
use std::path::Path;
use std::time::Instant;

use graft_client::Sparse;
use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{self, WcConfig};
use graft_model::ChangesetId;
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{
    AttestKind, AttestStatus, AttestV2, GetChangesets, JournalEntry, Land, Landed, MergeMode,
    ResolveRef, Revert, Watch,
};
use graft_proto::{ApiError, CallContext};

use crate::{
    Check, Config, Error, Outcome, backoff, call_context, chown_tree, client_error, is_transient,
    log, read_lsn, remove_tree, run_command,
};

/// The journal entry kind written with each ref move.
pub const REF_MOVED: &str = "RefMoved";

/// The journal entry kind written with each attestation.
pub const ATTESTED: &str = "Attested";

/// The journal entry kind written when a land reverts a checked landing.
pub const LAND_REVERTED: &str = "LandReverted";

/// Metadata naming the changeset a revert inverts.
pub const REVERTS: &str = "graft.reverts";

/// Metadata naming the failed check run a revert answers.
pub const REVERT_CHECK: &str = "graft.revert-check";

/// The display name attestations are posted under.
pub const ATTESTER_NAME: &str = "graft-attester post-land";

/// Most changesets one land's revert inverts; a larger land is reported
/// blocked rather than reverted piecemeal.
pub const MAX_REVERT_CHAIN: usize = 64;

/// Default most lands held at once. The journal is the backlog.
pub const DEFAULT_WINDOW: usize = 1024;

/// Tries of a revert's land that lose the race for the line.
const LAND_TRIES: u32 = 5;

/// One run of one check on one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// How it ended.
    pub outcome: Outcome,
    /// Link to its log, attested with its result.
    pub url: String,
}

/// Runs the checks. [`ProcessRunner`] checks the snapshot out and runs each
/// check as a process; tests use a fake.
pub trait Runner<A> {
    /// Runs `check` on the landed snapshot `at`. `attempt` counts from 1
    /// for each check of each snapshot. An error (the server unreachable, a
    /// local I/O failure) runs nothing and is tried again later; a check
    /// that cannot run on this snapshot is a failed [`Run`].
    fn run(
        &mut self,
        api: &A,
        at: ChangesetId,
        check: &Check,
        attempt: u32,
    ) -> impl Future<Output = Result<Run, Error>>;

    /// Done with the current snapshot: drop its tree.
    fn release(&mut self) -> impl Future<Output = ()>;
}

/// The real runner: checks the snapshot out under [`Config::work_dir`] and
/// runs each check there in its own process group, in a cleared
/// environment, with the timeout and the capped log of the draft mode.
/// Logs go to `<log_dir>/<snapshot>/<attempt>/<check>.log`.
pub struct ProcessRunner {
    cfg: Config,
    cx: CallContext,
    tree: Option<ChangesetId>,
}

impl ProcessRunner {
    /// A runner with `cfg`'s directories, limits and environment.
    pub fn new(cfg: Config) -> ProcessRunner {
        ProcessRunner {
            cfg,
            cx: call_context(),
            tree: None,
        }
    }

    fn dir(&self, at: ChangesetId) -> std::path::PathBuf {
        self.cfg.work_dir.join(format!("land-{at}"))
    }
}

impl<A: ObjectApi + RepoApi> Runner<A> for ProcessRunner {
    async fn run(
        &mut self,
        api: &A,
        at: ChangesetId,
        check: &Check,
        attempt: u32,
    ) -> Result<Run, Error> {
        let rel = format!("{at}/{attempt}/{}", check.log_name());
        let log_path = self.cfg.log_dir.join(&rel);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let url = format!("{}{rel}", self.cfg.log_url_prefix);
        let dir = self.dir(at);
        if self.tree != Some(at) {
            Runner::<A>::release(self).await;
            remove_tree(&self.cfg, &dir).await;
            let config = WcConfig {
                endpoint: None,
                repo: self.cfg.repo.clone(),
                workspace: None,
            };
            let cloned = workcopy::clone(
                api,
                &self.cx,
                &dir,
                config,
                &self.cfg.line,
                Some(at),
                Sparse::full(),
                &FetchOptions::default(),
            )
            .await;
            let failed = match cloned {
                Ok(_) => match self.cfg.run_as {
                    Some((uid, gid)) => chown_tree(&dir, uid, gid)
                        .err()
                        .map(|e| format!("handing the tree to uid {uid}: {e}")),
                    None => None,
                },
                Err(e) if is_transient(&e) => {
                    remove_tree(&self.cfg, &dir).await;
                    return Err(client_error(e));
                }
                Err(e) => Some(e.to_string()),
            };
            if let Some(why) = failed {
                remove_tree(&self.cfg, &dir).await;
                let outcome = Outcome::NotCheckedOut(why);
                fs::write(
                    &log_path,
                    format!(
                        "# graft-attester: {} on {at}\n# graft-attester: {outcome}\n",
                        check.name
                    ),
                )?;
                return Ok(Run { outcome, url });
            }
            self.tree = Some(at);
        }
        let outcome = run_command(&self.cfg, &dir, check, at, &log_path).await?;
        Ok(Run { outcome, url })
    }

    async fn release(&mut self) {
        if let Some(at) = self.tree.take() {
            let dir = self.dir(at);
            remove_tree(&self.cfg, &dir).await;
        }
    }
}

/// Why the line is blocked: what the operator must decide.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockReason {
    /// The revert conflicts with what landed after the bad land; nothing
    /// was landed.
    RevertConflict {
        /// The conflicting paths.
        paths: Vec<String>,
    },
    /// The land policy refused the revert.
    Rejected {
        /// The policy.
        policy: String,
        /// Why.
        detail: String,
    },
    /// Every change of the bad land is already undone on the tip.
    NothingToRevert,
    /// The land's stack is longer than [`MAX_REVERT_CHAIN`].
    TooLarge,
    /// A check still fails on top of a revert, or on a revert itself: the
    /// culprit cannot be told apart by line snapshots.
    RevertDidNotFix {
        /// The check that fails.
        check: String,
        /// Its last run.
        url: String,
    },
}

/// The line is blocked: the checker landed nothing and waits for the
/// operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocked {
    /// The line.
    pub line: String,
    /// The land that could not be dealt with.
    pub land: ChangesetId,
    /// Why.
    pub reason: BlockReason,
}

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {} blocked at land {}: ", self.line, self.land)?;
        match &self.reason {
            BlockReason::RevertConflict { paths } => {
                write!(f, "its revert conflicts on {}", paths.join(", "))
            }
            BlockReason::Rejected { policy, detail } => {
                write!(f, "its revert was rejected by {policy}: {detail}")
            }
            BlockReason::NothingToRevert => f.write_str("nothing left to revert"),
            BlockReason::TooLarge => {
                write!(f, "more than {MAX_REVERT_CHAIN} changesets to revert")
            }
            BlockReason::RevertDidNotFix { check, url } => {
                write!(f, "{check} still fails on top of a revert ({url})")
            }
        }
    }
}

/// What one [`PostLand::step`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostLandReport {
    /// Nothing unsettled to check.
    Idle,
    /// The newest unsettled land passed: these lands, oldest first, were
    /// attested good.
    Good {
        /// The snapshot that passed.
        at: ChangesetId,
        /// The lands attested.
        lands: Vec<ChangesetId>,
    },
    /// A check failed: `good` were attested good, `land` failed.
    Bad {
        /// The first bad land.
        land: ChangesetId,
        /// The check that failed.
        check: String,
        /// Lands before it attested good, oldest first.
        good: Vec<ChangesetId>,
    },
    /// A bad land was reverted.
    Reverted {
        /// The bad land.
        land: ChangesetId,
        /// The revert, as landed.
        revert: ChangesetId,
    },
    /// The line is blocked.
    Blocked(Blocked),
}

/// One check report read from the journal.
#[derive(Clone, Debug)]
struct Report {
    lsn: u64,
    check: String,
    passed: bool,
    url: String,
}

/// One land of the line, as read.
#[derive(Clone, Debug)]
struct LandRec {
    lsn: u64,
    old: ChangesetId,
    landed: ChangesetId,
    reports: Vec<Report>,
    /// A `LandReverted` names this land.
    reverted: bool,
    /// The lands this land reverts.
    reverts: Vec<ChangesetId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Good,
    Bad,
    Unsettled,
}

/// What one evaluation of a snapshot found.
enum Verdict {
    /// Every check passed: `(check, url)` of each passing run.
    Pass(Vec<(String, String)>),
    /// `check` failed every try (`urls`); `passed` ran before it.
    Fail {
        check: String,
        urls: Vec<String>,
        passed: Vec<(String, String)>,
    },
}

/// The post-land checker over an API client and a check runner.
pub struct PostLand<A, R> {
    api: A,
    runner: R,
    cx: CallContext,
    cfg: Config,
    retries: u32,
    window: usize,
    /// Lands read, in line order, from the oldest not settled.
    lands: Vec<LandRec>,
    /// Next journal position to read.
    read_lsn: u64,
    /// The first land left unread because the window was full.
    overflow: Option<u64>,
    /// Lands named by a `LandReverted` whose revert's `RefMoved` (same
    /// transaction, next) is not read yet.
    reverted_pending: Vec<ChangesetId>,
    /// Flakes (a failed run, then a pass) per check.
    flakes: std::collections::BTreeMap<String, u64>,
    blocked: Option<Blocked>,
}

impl<A: ObjectApi + RepoApi, R: Runner<A>> PostLand<A, R> {
    /// A checker of `cfg.line` with `cfg.checks`, retrying a failed check
    /// `retries` times (the line's `retries` on the server), reading from
    /// the saved position, if any.
    pub fn new(api: A, runner: R, cfg: Config, retries: u32) -> Result<Self, Error> {
        fs::create_dir_all(&cfg.work_dir)?;
        fs::create_dir_all(&cfg.log_dir)?;
        let read_lsn = match &cfg.state_file {
            Some(path) => read_lsn(path)?,
            None => 1,
        };
        Ok(PostLand {
            api,
            runner,
            cx: call_context(),
            cfg,
            retries,
            window: DEFAULT_WINDOW,
            lands: Vec::new(),
            read_lsn,
            overflow: None,
            reverted_pending: Vec::new(),
            flakes: std::collections::BTreeMap::new(),
            blocked: None,
        })
    }

    /// Most lands held at once (at least 1).
    pub fn set_window(&mut self, lands: usize) {
        self.window = lands.max(1);
    }

    /// Flaky runs seen per check: failed, then passed on a retry. Exported
    /// as `graft_post_land_flakes_total{check}`.
    pub fn flakes(&self) -> &std::collections::BTreeMap<String, u64> {
        &self.flakes
    }

    /// The runner.
    pub fn runner(&self) -> &R {
        &self.runner
    }

    /// The position a restart reads from.
    pub fn cursor(&self) -> u64 {
        [self.lands.first().map(|l| l.lsn), self.overflow]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(self.read_lsn)
            .min(self.read_lsn)
    }

    /// Reads what is new (waiting up to `wait_ms` for it), then settles
    /// what it can: one revert, or one round of skip-ahead and bisection.
    /// Once blocked, every step reports [`PostLandReport::Blocked`] and
    /// does nothing.
    pub async fn step(&mut self, wait_ms: u32) -> Result<PostLandReport, Error> {
        if let Some(b) = &self.blocked {
            return Ok(PostLandReport::Blocked(b.clone()));
        }
        self.read(wait_ms).await?;
        let report = self.settle().await;
        self.compact();
        self.save()?;
        let report = report?;
        if let PostLandReport::Blocked(b) = &report {
            self.blocked = Some(b.clone());
        }
        Ok(report)
    }

    /// Steps until the line is blocked (returned) or the API fails
    /// `max_errors` times in a row (`None`: never gives up), logging each
    /// report.
    pub async fn run(&mut self, max_errors: Option<u32>) -> Result<Blocked, Error> {
        let mut errors = 0u32;
        loop {
            let started = Instant::now();
            match self.step(self.cfg.wait_ms).await {
                Ok(PostLandReport::Blocked(b)) => {
                    log(&format!("{b}; stopping for the operator"));
                    return Ok(b);
                }
                Ok(PostLandReport::Idle) => {
                    errors = 0;
                    if let Some(rest) = self.cfg.idle.checked_sub(started.elapsed()) {
                        tokio::time::sleep(rest).await;
                    }
                }
                Ok(report) => {
                    errors = 0;
                    log_report(&self.cfg.line, &report);
                }
                Err(e) => {
                    errors += 1;
                    log(&format!("post-land step failed ({errors} in a row): {e}"));
                    if max_errors.is_some_and(|m| errors >= m) {
                        return Err(e);
                    }
                    tokio::time::sleep(backoff(errors)).await;
                }
            }
        }
    }

    // ---- reading ----------------------------------------------------------

    async fn read(&mut self, wait_ms: u32) -> Result<(), Error> {
        if let Some(lsn) = self.overflow
            && self.lands.len() < self.window
        {
            // Room again: read the skipped lands. Reports and reverts read
            // twice are recognised by position.
            self.overflow = None;
            self.read_lsn = self.read_lsn.min(lsn);
        }
        let mut wait = wait_ms;
        loop {
            let page = self
                .api
                .watch(
                    &self.cx,
                    Watch {
                        repo: self.cfg.repo.clone(),
                        from_lsn: self.read_lsn,
                        kinds: vec![
                            REF_MOVED.to_owned(),
                            ATTESTED.to_owned(),
                            LAND_REVERTED.to_owned(),
                        ],
                        limit: 0,
                        wait_ms: wait,
                    },
                )
                .await?;
            for entry in &page.entries {
                self.apply(entry);
            }
            let advanced = page.next_lsn > self.read_lsn;
            self.read_lsn = page.next_lsn.max(self.read_lsn);
            if !advanced || page.entries.is_empty() {
                return Ok(());
            }
            wait = 0;
        }
    }

    fn apply(&mut self, entry: &JournalEntry) {
        let Ok(body) = serde_json::from_str::<serde_json::Value>(&entry.body) else {
            return;
        };
        let id = |key: &str| {
            body.get(key)
                .and_then(|v| v.as_str())
                .and_then(|s| ChangesetId::parse(s).ok())
        };
        match entry.kind.as_str() {
            REF_MOVED => {
                if body.get("name").and_then(|v| v.as_str()) != Some(self.cfg.line.as_str()) {
                    return;
                }
                let reverts = std::mem::take(&mut self.reverted_pending);
                // The line's creation is no land.
                let (Some(old), Some(landed)) = (id("old"), id("new")) else {
                    return;
                };
                if self.lands.last().is_some_and(|l| l.lsn >= entry.lsn)
                    || self.overflow.is_some_and(|o| o <= entry.lsn)
                {
                    return;
                }
                if self.lands.len() >= self.window {
                    self.overflow = Some(entry.lsn);
                    return;
                }
                self.lands.push(LandRec {
                    lsn: entry.lsn,
                    old,
                    landed,
                    reports: Vec::new(),
                    reverted: false,
                    reverts,
                });
            }
            ATTESTED => {
                let kind = body.get("kind").and_then(|v| v.as_str()).unwrap_or("check");
                let check = body.get("check").and_then(|v| v.as_str()).unwrap_or("");
                if kind != "check" || !self.cfg.checks.iter().any(|c| c.name == check) {
                    return;
                }
                let passed = match body.get("status").and_then(|v| v.as_str()) {
                    Some("pass") => true,
                    Some("fail") => false,
                    Some(_) => return,
                    None => match body.get("passed").and_then(|v| v.as_bool()) {
                        Some(p) => p,
                        None => return,
                    },
                };
                let Some(changeset) = id("changeset") else {
                    return;
                };
                if let Some(rec) = self.lands.iter_mut().find(|l| l.landed == changeset)
                    && !rec.reports.iter().any(|r| r.lsn == entry.lsn)
                {
                    rec.reports.push(Report {
                        lsn: entry.lsn,
                        check: check.to_owned(),
                        passed,
                        url: body
                            .get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                    });
                }
            }
            LAND_REVERTED => {
                if body.get("line").and_then(|v| v.as_str()) != Some(self.cfg.line.as_str()) {
                    return;
                }
                let Some(land) = id("land") else {
                    return;
                };
                if let Some(rec) = self.lands.iter_mut().find(|l| l.landed == land) {
                    rec.reverted = true;
                }
                self.reverted_pending.push(land);
            }
            _ => {}
        }
    }

    // ---- settling ---------------------------------------------------------

    fn state(&self, rec: &LandRec) -> State {
        let mut all_passed = true;
        for check in &self.cfg.checks {
            let mut fails = 0u64;
            let mut latest = None;
            for r in rec.reports.iter().filter(|r| r.check == check.name) {
                if !r.passed {
                    fails += 1;
                }
                latest = Some(r.passed);
            }
            if fails > u64::from(self.retries) {
                return State::Bad;
            }
            all_passed &= latest == Some(true);
        }
        if all_passed {
            State::Good
        } else {
            State::Unsettled
        }
    }

    /// The unsettled lands grouped by the snapshot that settles them: a
    /// land on its own, or the lands after a reverted bad land together
    /// with the land that follows the revert (their snapshots carry the bad
    /// change, so only one on top of the revert says anything). Lands
    /// waiting for a revert are in no unit.
    fn units(&self) -> Vec<Vec<usize>> {
        let mut units = Vec::new();
        let mut carry = Vec::new();
        let mut open_bad: Vec<ChangesetId> = Vec::new();
        for (i, rec) in self.lands.iter().enumerate() {
            open_bad.retain(|b| !rec.reverts.contains(b));
            match self.state(rec) {
                State::Good => continue,
                State::Bad => {
                    open_bad.push(rec.landed);
                    continue;
                }
                State::Unsettled => {}
            }
            carry.push(i);
            if open_bad.is_empty() {
                units.push(std::mem::take(&mut carry));
            }
        }
        units
    }

    async fn settle(&mut self) -> Result<PostLandReport, Error> {
        // A bad land not yet reverted comes first: nothing after it says
        // anything until it is.
        if let Some(i) = self
            .lands
            .iter()
            .position(|l| !l.reverted && self.state(l) == State::Bad)
        {
            return self.revert(i).await;
        }
        let units = self.units();
        let Some(top) = units.last() else {
            return Ok(PostLandReport::Idle);
        };
        let at = self.lands[*top.last().expect("a unit has a land")].landed;
        let (mut lo, mut hi) = (-1isize, units.len() as isize - 1);
        let (mut lo_pass, mut hi_fail) = (None, None);
        match self.evaluate(at).await? {
            Verdict::Pass(passed) => {
                lo = hi;
                lo_pass = Some(passed);
            }
            fail => hi_fail = Some(fail),
        }
        // Bisect: everything at or below `lo` passed, `hi` failed.
        while lo < hi && hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            let unit = &units[mid as usize];
            let at = self.lands[*unit.last().expect("a unit has a land")].landed;
            match self.evaluate(at).await? {
                Verdict::Pass(passed) => {
                    lo = mid;
                    lo_pass = Some(passed);
                }
                fail => {
                    hi = mid;
                    hi_fail = Some(fail);
                }
            }
        }
        // Attest the good ones, oldest first.
        let mut good = Vec::new();
        if let Some(passed) = &lo_pass {
            for unit in &units[..=(lo as usize)] {
                for &i in unit {
                    self.attest_passes(i, passed).await?;
                    good.push(self.lands[i].landed);
                }
            }
        }
        let Some(Verdict::Fail {
            check,
            urls,
            passed,
        }) = hi_fail
        else {
            return Ok(PostLandReport::Good { at, lands: good });
        };
        let unit = &units[hi as usize];
        let first = unit[0];
        if unit.len() > 1 || !self.lands[first].reverts.is_empty() {
            // On top of a revert: the culprit is not one land.
            return Ok(PostLandReport::Blocked(Blocked {
                line: self.cfg.line.clone(),
                land: self.lands[*unit.last().expect("a unit has a land")].landed,
                reason: BlockReason::RevertDidNotFix {
                    check,
                    url: urls.last().cloned().unwrap_or_default(),
                },
            }));
        }
        self.attest_passes(first, &passed).await?;
        let land = self.lands[first].landed;
        // One failure per run, enough to read `bad`; those already posted
        // (before a restart) count.
        let posted = self.lands[first]
            .reports
            .iter()
            .filter(|r| r.check == check && !r.passed)
            .count() as u64;
        let need = u64::from(self.retries) + 1;
        for n in posted..need {
            let url = &urls[(n as usize) % urls.len()];
            self.attest(land, &check, false, url).await?;
        }
        Ok(PostLandReport::Bad { land, check, good })
    }

    /// Runs every check on `at`, each retried up to `retries` times.
    async fn evaluate(&mut self, at: ChangesetId) -> Result<Verdict, Error> {
        let mut passed = Vec::new();
        for check in self.cfg.checks.clone() {
            let mut urls = Vec::new();
            let mut pass = None;
            for attempt in 1..=self.retries.saturating_add(1) {
                let run = self.runner.run(&self.api, at, &check, attempt).await?;
                if run.outcome.passed() {
                    pass = Some(run.url);
                    break;
                }
                urls.push(run.url);
            }
            match pass {
                Some(url) => {
                    if !urls.is_empty() {
                        let n = self.flakes.entry(check.name.clone()).or_insert(0);
                        *n += 1;
                        log(&format!(
                            "graft_post_land_flakes_total{{check=\"{}\"}} {n} ({at}: failed, then passed)",
                            check.name
                        ));
                    }
                    passed.push((check.name.clone(), url));
                }
                None => {
                    self.runner.release().await;
                    return Ok(Verdict::Fail {
                        check: check.name.clone(),
                        urls,
                        passed,
                    });
                }
            }
        }
        self.runner.release().await;
        Ok(Verdict::Pass(passed))
    }

    /// Attests each of `passed` on land `i` whose latest report is not a
    /// pass already.
    async fn attest_passes(&self, i: usize, passed: &[(String, String)]) -> Result<(), Error> {
        let rec = &self.lands[i];
        for (check, url) in passed {
            let latest = rec.reports.iter().rev().find(|r| &r.check == check);
            if latest.is_some_and(|r| r.passed) {
                continue;
            }
            self.attest(rec.landed, check, true, url).await?;
        }
        Ok(())
    }

    async fn attest(
        &self,
        land: ChangesetId,
        check: &str,
        passed: bool,
        url: &str,
    ) -> Result<(), Error> {
        let req = AttestV2 {
            repo: self.cfg.repo.clone(),
            changeset: land,
            kind: AttestKind::Check,
            check: check.to_owned(),
            status: AttestStatus::from_passed(passed),
            // A check report is the v0.1 shape: kind CHECK and a pass or
            // fail. Leaving `by` unset lets a server that only implements
            // `attest` accept it. Review and approval set the name.
            by: None,
            url: url.to_owned(),
        };
        let tries = self.cfg.attest_tries.max(1);
        let mut delay = self.cfg.retry_delay;
        for tried in 1..=tries {
            match self.api.attest_v2(&self.cx, req.clone()).await {
                Ok(()) => return Ok(()),
                Err(e) if e.is_retryable() && tried < tries => {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2);
                }
                Err(e) => return Err(Error::Api(e)),
            }
        }
        unreachable!("the last try returns")
    }

    // ---- reverting --------------------------------------------------------

    fn blocked(&self, land: ChangesetId, reason: BlockReason) -> PostLandReport {
        PostLandReport::Blocked(Blocked {
            line: self.cfg.line.clone(),
            land,
            reason,
        })
    }

    /// Lands the revert of bad land `i` on the line's tip.
    async fn revert(&mut self, i: usize) -> Result<PostLandReport, Error> {
        let (land, old) = (self.lands[i].landed, self.lands[i].old);
        // The link of the last failed run, for `graft.revert-check`.
        let check_url = self.lands[i]
            .reports
            .iter()
            .rev()
            .find(|r| !r.passed && !r.url.is_empty())
            .map(|r| r.url.clone());
        // The land's stack, newest first.
        let mut chain = Vec::new();
        let mut c = land;
        while c != old {
            if chain.len() >= MAX_REVERT_CHAIN {
                return Ok(self.blocked(land, BlockReason::TooLarge));
            }
            chain.push(c);
            let views = self
                .api
                .get_changesets(
                    &self.cx,
                    GetChangesets {
                        repo: self.cfg.repo.clone(),
                        ids: vec![c],
                    },
                )
                .await?;
            let Some(parent) = views.found.first().and_then(|v| v.changeset.first_parent()) else {
                return Err(Error::Api(ApiError::NotFound {
                    what: format!("changeset {c}"),
                }));
            };
            c = parent;
        }
        let tip = self
            .api
            .resolve_ref(
                &self.cx,
                ResolveRef {
                    repo: self.cfg.repo.clone(),
                    name: self.cfg.line.clone(),
                },
            )
            .await?
            .target;
        let mut head = tip;
        for c in chain {
            let inverse = match self
                .api
                .revert(
                    &self.cx,
                    Revert {
                        repo: self.cfg.repo.clone(),
                        changeset: c,
                        onto: head,
                        workspace: None,
                        check_url: check_url.clone(),
                    },
                )
                .await
            {
                Ok(id) => id,
                Err(e) => return self.revert_failed(land, e),
            };
            head = inverse;
        }
        for tried in 1..=LAND_TRIES {
            let landed = self
                .api
                .land(
                    &self.cx,
                    Land {
                        repo: self.cfg.repo.clone(),
                        head,
                        line: self.cfg.line.clone(),
                        squash: false,
                        merge: MergeMode::Line,
                        deadline_ms: None,
                        merged_from: None,
                    },
                )
                .await;
            match landed {
                Ok(Landed::Landed { landed, .. }) => {
                    return Ok(PostLandReport::Reverted {
                        land,
                        revert: landed,
                    });
                }
                Ok(_) => {
                    return Ok(self.blocked(
                        land,
                        BlockReason::Rejected {
                            policy: "queued".to_owned(),
                            detail: "the line hands lands to a pull request".to_owned(),
                        },
                    ));
                }
                Err(ApiError::Contended { .. }) if tried < LAND_TRIES => {}
                Err(e) => return self.revert_failed(land, e),
            }
        }
        unreachable!("the last try returns")
    }

    fn revert_failed(&self, land: ChangesetId, e: ApiError) -> Result<PostLandReport, Error> {
        let reason = match e {
            ApiError::Conflict { paths, .. } => BlockReason::RevertConflict {
                paths: paths.into_iter().map(|p| p.path).collect(),
            },
            ApiError::MergeConflict(m) => BlockReason::RevertConflict {
                paths: m.paths.into_iter().map(|p| p.path).collect(),
            },
            ApiError::Rejected { policy, detail } if policy != "unimplemented" => {
                BlockReason::Rejected { policy, detail }
            }
            ApiError::InvalidChange { rule, .. } if rule == "revert-empty" => {
                BlockReason::NothingToRevert
            }
            e => return Err(Error::Api(e)),
        };
        Ok(self.blocked(land, reason))
    }

    // ---- position ---------------------------------------------------------

    /// Drops the settled lands at the front of the window, keeping a bad
    /// land until its revert is read (the lands between are judged by it).
    fn compact(&mut self) {
        let mut open_bad: Vec<ChangesetId> = Vec::new();
        let mut keep_from = 0;
        for (i, rec) in self.lands.iter().enumerate() {
            open_bad.retain(|b| !rec.reverts.contains(b));
            match self.state(rec) {
                State::Good => {}
                State::Bad if rec.reverted => open_bad.push(rec.landed),
                _ => break,
            }
            if open_bad.is_empty() {
                keep_from = i + 1;
            }
        }
        self.lands.drain(..keep_from);
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.cfg.state_file else {
            return Ok(());
        };
        write_lsn(path, self.cursor())
    }
}

fn write_lsn(path: &Path, lsn: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{lsn}\n"))?;
    fs::rename(&tmp, path)
}

fn log_report(line: &str, report: &PostLandReport) {
    match report {
        PostLandReport::Idle => {}
        PostLandReport::Good { at, lands } => {
            log(&format!("{line}: {} land(s) good, up to {at}", lands.len()))
        }
        PostLandReport::Bad { land, check, good } => log(&format!(
            "{line}: {land} is the first bad land ({check} failed); {} before it good",
            good.len()
        )),
        PostLandReport::Reverted { land, revert } => {
            log(&format!("{line}: reverted {land} with {revert}"))
        }
        PostLandReport::Blocked(b) => log(&b.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_line_names_the_land_and_the_paths() {
        let land = ChangesetId(graft_model::ObjectId([3; 32]));
        let b = Blocked {
            line: "integration".to_owned(),
            land,
            reason: BlockReason::RevertConflict {
                paths: vec!["a.txt".to_owned()],
            },
        };
        let text = b.to_string();
        assert!(text.contains("integration"), "{text}");
        assert!(text.contains(&land.to_string()), "{text}");
        assert!(text.contains("a.txt"), "{text}");
    }
}
