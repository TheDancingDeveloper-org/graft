//! The trial attester (WI-2276): watches one repository's journal for new
//! drafts, checks each one out into a scratch directory, runs the configured
//! checks, and posts `Attest` per check, passed or failed, with a link to the
//! check's log.
//!
//! - Drafts come from `ChangesetCreated` entries held by a workspace. Land
//!   candidates and abandoned changesets are skipped. Before a draft runs,
//!   `GetChangesets` confirms it is still a draft.
//! - The queue keeps the newest draft per workspace (a land needs the
//!   stack's head attested, not every draft under it) and is bounded. The
//!   journal is read only while the queue has room, so nothing is dropped:
//!   the journal is the backlog.
//! - One draft at a time. Each check runs in its own process group with a
//!   cleared environment and a timeout; its output goes to a capped log
//!   file. After a check fails, the rest are attested failed without
//!   running (a build failure fails the tests too).
//! - With [`Config::run_as`], the checked-out tree and the check processes
//!   belong to that uid, so a check (a draft's build script or test) can't
//!   read the token, which only the attester's own uid can.
//! - The read position is persisted ([`Config::state_file`]) at the oldest
//!   draft not yet finished, so a restart re-runs at most what was queued.
//! - The token is never printed. It lives in the API client (whose `Debug`
//!   redacts it) and never reaches a check's environment or arguments.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::os::unix::fs::lchown;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use graft_client::Sparse;
use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{self, WcConfig};
use graft_model::ChangesetId;
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{Attest, DraftState, GetChangesets, JournalEntry, Watch};
use graft_proto::{ApiError, CallContext, Principal, PrincipalKind};
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// The journal entry kind that announces a new changeset.
pub const CHANGESET_CREATED: &str = "ChangesetCreated";

/// The checks run when none are configured (decision D6's names).
pub const DEFAULT_CHECKS: [&str; 2] = [
    "ci/build=cargo build --locked",
    "ci/test=cargo test --locked",
];

/// One check: the name it is attested under and the command that runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// Check name, for example `ci/build`.
    pub name: String,
    /// Program and arguments, run in the tree's root.
    pub argv: Vec<String>,
}

impl Check {
    /// Parses `name=program arg...` (arguments split on whitespace).
    pub fn parse(spec: &str) -> Result<Check, String> {
        let (name, cmd) = spec
            .split_once('=')
            .ok_or_else(|| format!("check {spec:?}: expected name=command"))?;
        let name = name.trim();
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_owned).collect();
        if name.is_empty() || argv.is_empty() {
            return Err(format!(
                "check {spec:?}: name and command must be non-empty"
            ));
        }
        Ok(Check {
            name: name.to_owned(),
            argv,
        })
    }

    /// The log file name: the check name with anything but `[A-Za-z0-9._]`
    /// replaced by `-`, plus `.log`.
    pub fn log_name(&self) -> String {
        let safe: String = self
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("{safe}.log")
    }
}

/// What the attester runs and where.
#[derive(Clone, Debug)]
pub struct Config {
    /// Repository watched and attested.
    pub repo: String,
    /// Checks, run in order.
    pub checks: Vec<Check>,
    /// Per-check time limit.
    pub timeout: Duration,
    /// Scratch directory; each draft is checked out in a subdirectory and
    /// removed afterwards.
    pub work_dir: PathBuf,
    /// Log directory: `<draft>/<check>.log`.
    pub log_dir: PathBuf,
    /// Prefix of the URL attested with each result; the log's path under
    /// [`Config::log_dir`] is appended.
    pub log_url_prefix: String,
    /// Where the read position is kept across restarts (none: start at the
    /// journal's beginning).
    pub state_file: Option<PathBuf>,
    /// Most drafts waiting at once.
    pub queue_cap: usize,
    /// `(uid, gid)` the tree and the checks belong to; none runs them as the
    /// attester's own user (development and tests).
    pub run_as: Option<(u32, u32)>,
    /// The checks' whole environment (it is cleared first). `PATH` falls
    /// back to the attester's own when absent.
    pub env: Vec<(String, String)>,
    /// Most bytes kept of a check's output; the rest is discarded.
    pub max_log_bytes: u64,
    /// A draft's logs are removed this long after they were written.
    pub log_retention: Duration,
    /// Long-poll wait for new journal entries, in ms.
    pub wait_ms: u32,
    /// Pause after an empty read when the queue is empty too.
    pub idle: Duration,
}

impl Config {
    /// A configuration for `repo` with the default checks and limits, and
    /// its directories under `root`.
    pub fn new(repo: impl Into<String>, root: &Path) -> Config {
        let log_dir = root.join("logs");
        Config {
            repo: repo.into(),
            checks: DEFAULT_CHECKS
                .iter()
                .map(|s| Check::parse(s).expect("default check"))
                .collect(),
            timeout: Duration::from_secs(45 * 60),
            work_dir: root.join("work"),
            log_url_prefix: format!("file://{}/", log_dir.display()),
            log_dir,
            state_file: Some(root.join("state").join("lsn")),
            queue_cap: 16,
            run_as: None,
            env: Vec::new(),
            max_log_bytes: 8 << 20,
            log_retention: Duration::from_secs(14 * 24 * 3600),
            wait_ms: 20_000,
            idle: Duration::from_secs(2),
        }
    }
}

/// A draft waiting to be checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The draft.
    pub draft: ChangesetId,
    /// The workspace holding it.
    pub workspace: String,
    /// Its `ChangesetCreated` entry's position.
    pub lsn: u64,
}

/// Reads a `ChangesetCreated` entry for a draft held by a workspace; any
/// other entry is `None`.
pub fn parse_entry(entry: &JournalEntry) -> Option<Pending> {
    if entry.kind != CHANGESET_CREATED {
        return None;
    }
    let body: serde_json::Value = serde_json::from_str(&entry.body).ok()?;
    if body.get("state")?.as_str()? != "draft" {
        return None;
    }
    let workspace = body.get("workspace")?.as_str()?.to_owned();
    let draft = ChangesetId::parse(body.get("id")?.as_str()?).ok()?;
    Some(Pending {
        draft,
        workspace,
        lsn: entry.lsn,
    })
}

/// The drafts waiting, oldest first, at most one per workspace.
#[derive(Debug)]
pub struct Queue {
    cap: usize,
    items: VecDeque<Pending>,
}

impl Queue {
    /// An empty queue holding at most `cap` drafts (at least one).
    pub fn new(cap: usize) -> Queue {
        Queue {
            cap: cap.max(1),
            items: VecDeque::new(),
        }
    }

    /// Whether another workspace's draft would not fit.
    pub fn is_full(&self) -> bool {
        self.items.len() >= self.cap
    }

    /// Drafts waiting.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Adds `p`. A newer draft of a workspace already waiting replaces that
    /// one in its place. `false` when the queue is full and `p` is for a new
    /// workspace.
    pub fn push(&mut self, p: Pending) -> bool {
        if let Some(slot) = self.items.iter_mut().find(|q| q.workspace == p.workspace) {
            *slot = p;
            return true;
        }
        if self.is_full() {
            return false;
        }
        self.items.push_back(p);
        true
    }

    /// Puts back a draft that could not be checked, first in line, unless
    /// its workspace has a newer one waiting. It may overfill the queue by
    /// one.
    pub fn requeue(&mut self, p: Pending) {
        if !self.items.iter().any(|q| q.workspace == p.workspace) {
            self.items.push_front(p);
        }
    }

    /// Takes the oldest.
    pub fn pop(&mut self) -> Option<Pending> {
        self.items.pop_front()
    }

    /// The lowest position still waiting.
    pub fn min_lsn(&self) -> Option<u64> {
        self.items.iter().map(|p| p.lsn).min()
    }
}

/// How one check ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exited zero.
    Passed,
    /// Exited non-zero, or was killed by a signal (`None`).
    Failed(Option<i32>),
    /// Ran past [`Config::timeout`] and was killed.
    TimedOut,
    /// Not run, because an earlier check failed.
    Skipped,
    /// Could not start, described.
    NotStarted(String),
}

impl Outcome {
    /// Whether it counts as a pass.
    pub fn passed(&self) -> bool {
        matches!(self, Outcome::Passed)
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Passed => f.write_str("passed"),
            Outcome::Failed(Some(code)) => write!(f, "failed (exit {code})"),
            Outcome::Failed(None) => f.write_str("failed (killed by a signal)"),
            Outcome::TimedOut => f.write_str("failed (timed out)"),
            Outcome::Skipped => f.write_str("failed (not run: an earlier check failed)"),
            Outcome::NotStarted(why) => write!(f, "failed (did not start: {why})"),
        }
    }
}

/// One check's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    /// Check name.
    pub check: String,
    /// How it ended.
    pub outcome: Outcome,
    /// The URL attested.
    pub url: String,
    /// Whether `Attest` succeeded; the error otherwise.
    pub attested: Result<(), ApiError>,
}

/// What became of one draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    /// Checked; one result per check, in order.
    Checked {
        /// The draft.
        draft: ChangesetId,
        /// Results.
        results: Vec<CheckResult>,
    },
    /// No longer a draft (landed, superseded, abandoned or collected) by
    /// the time it ran: nothing to attest.
    Skipped {
        /// The draft.
        draft: ChangesetId,
        /// Its state, described.
        state: String,
    },
    /// Could not be checked out; nothing was attested.
    NotCheckedOut {
        /// The draft.
        draft: ChangesetId,
        /// Why, described (never file contents).
        error: String,
    },
}

/// An attester failure that stops a read or a run (not a failing check).
#[derive(Debug)]
pub enum Error {
    /// The API refused or failed.
    Api(ApiError),
    /// A local file operation failed.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api(e) => write!(f, "api: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        Error::Api(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// The call context the attester calls with. The server derives the real
/// principal from the token; this one names the caller in request ids.
pub fn call_context() -> CallContext {
    CallContext {
        principal: Principal {
            subject: "graft-attester".to_owned(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: Vec::new(),
        },
        request_id: "graft-attester".to_owned(),
        deadline_ms: None,
    }
}

/// The attester over an API client.
pub struct Attester<A> {
    api: A,
    cx: CallContext,
    cfg: Config,
    queue: Queue,
    next_lsn: u64,
    running: Option<u64>,
}

impl<A: ObjectApi + RepoApi> Attester<A> {
    /// An attester reading from the saved position, if any. Creates the
    /// work and log directories.
    pub fn new(api: A, cfg: Config) -> Result<Self, Error> {
        fs::create_dir_all(&cfg.work_dir)?;
        fs::create_dir_all(&cfg.log_dir)?;
        let next_lsn = match &cfg.state_file {
            Some(path) => read_lsn(path)?,
            None => 1,
        };
        Ok(Attester {
            api,
            cx: call_context(),
            queue: Queue::new(cfg.queue_cap),
            cfg,
            next_lsn,
            running: None,
        })
    }

    /// The configuration.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Drafts waiting.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The next journal position to read.
    pub fn next_lsn(&self) -> u64 {
        self.next_lsn
    }

    /// Reads journal pages into the queue until it is full or a page brings
    /// nothing new, waiting up to `wait_ms` for the first. Returns the
    /// drafts queued.
    pub async fn poll(&mut self, wait_ms: u32) -> Result<usize, Error> {
        let mut queued = 0;
        let mut wait = wait_ms;
        while !self.queue.is_full() {
            let page = self
                .api
                .watch(
                    &self.cx,
                    Watch {
                        repo: self.cfg.repo.clone(),
                        from_lsn: self.next_lsn,
                        kinds: vec![CHANGESET_CREATED.to_owned()],
                        limit: 0,
                        wait_ms: wait,
                    },
                )
                .await?;
            let advanced = page.next_lsn > self.next_lsn;
            let mut next = page.next_lsn.max(self.next_lsn);
            for entry in &page.entries {
                if let Some(p) = parse_entry(entry) {
                    if !self.queue.push(p) {
                        // Full: read this entry again once there is room.
                        next = entry.lsn;
                        break;
                    }
                    queued += 1;
                }
            }
            self.next_lsn = next;
            if !advanced {
                break;
            }
            wait = 0;
        }
        self.save()?;
        Ok(queued)
    }

    /// Checks the oldest waiting draft; `None` when nothing waits. A draft
    /// that fails with an error (the server unreachable, a full disk) goes
    /// back to the front of the queue.
    pub async fn run_next(&mut self) -> Result<Option<Report>, Error> {
        let Some(p) = self.queue.pop() else {
            return Ok(None);
        };
        self.running = Some(p.lsn);
        let report = self.check_draft(&p).await;
        self.running = None;
        if report.is_err() {
            self.queue.requeue(p);
        }
        self.save()?;
        self.prune_logs();
        report.map(Some)
    }

    /// Reads and runs until the API fails `max_errors` times in a row
    /// (`None`: never gives up), logging each draft's results to stderr.
    pub async fn run(&mut self, max_errors: Option<u32>) -> Result<(), Error> {
        let mut errors = 0u32;
        let mut failed_runs = 0u32;
        loop {
            let started = Instant::now();
            let read = if self.queue.is_full() {
                Ok(0)
            } else {
                let wait = if self.queue.is_empty() {
                    self.cfg.wait_ms
                } else {
                    0
                };
                self.poll(wait).await
            };
            match read {
                Ok(_) => errors = 0,
                Err(e) => {
                    errors += 1;
                    log(&format!(
                        "reading the journal failed ({errors} in a row): {e}"
                    ));
                    if max_errors.is_some_and(|m| errors >= m) {
                        return Err(e);
                    }
                    tokio::time::sleep(backoff(errors)).await;
                    continue;
                }
            }
            match self.run_next().await {
                Ok(Some(report)) => {
                    failed_runs = 0;
                    log_report(&report);
                }
                Ok(None) => {
                    // A server that answers an empty read at once must not
                    // make this loop spin.
                    if let Some(rest) = self.cfg.idle.checked_sub(started.elapsed()) {
                        tokio::time::sleep(rest).await;
                    }
                }
                Err(e) => {
                    failed_runs += 1;
                    log(&format!(
                        "running a draft failed ({failed_runs} in a row; it waits): {e}"
                    ));
                    tokio::time::sleep(backoff(failed_runs)).await;
                }
            }
        }
    }

    async fn check_draft(&mut self, p: &Pending) -> Result<Report, Error> {
        let draft = p.draft;
        let views = self
            .api
            .get_changesets(
                &self.cx,
                GetChangesets {
                    repo: self.cfg.repo.clone(),
                    ids: vec![draft],
                },
            )
            .await?;
        let state = match views.found.first() {
            Some(v) if v.state == DraftState::Draft => None,
            Some(v) => Some(format!("{:?}", v.state).to_lowercase()),
            None if views.collected.contains(&draft) => Some("collected".to_owned()),
            None => Some("missing".to_owned()),
        };
        if let Some(state) = state {
            return Ok(Report::Skipped { draft, state });
        }

        let dir = self.cfg.work_dir.join(draft.to_string());
        self.remove_tree(&dir).await;
        let logs = self.cfg.log_dir.join(draft.to_string());
        fs::create_dir_all(&logs)?;
        let config = WcConfig {
            endpoint: None,
            repo: self.cfg.repo.clone(),
            workspace: None,
        };
        let cloned = workcopy::clone(
            &self.api,
            &self.cx,
            &dir,
            config,
            "main",
            Some(draft),
            Sparse::full(),
            &FetchOptions::default(),
        )
        .await;
        if let Err(e) = cloned {
            self.remove_tree(&dir).await;
            return Ok(Report::NotCheckedOut {
                draft,
                error: e.to_string(),
            });
        }
        if let Some((uid, gid)) = self.cfg.run_as
            && let Err(e) = chown_tree(&dir, uid, gid)
        {
            self.remove_tree(&dir).await;
            return Ok(Report::NotCheckedOut {
                draft,
                error: format!("handing the tree to uid {uid}: {e}"),
            });
        }

        let mut results = Vec::with_capacity(self.cfg.checks.len());
        let mut failed = false;
        for check in self.cfg.checks.clone() {
            let log_name = check.log_name();
            let log_path = logs.join(&log_name);
            let outcome = if failed {
                write_skipped(&log_path, &check, draft)?;
                Outcome::Skipped
            } else {
                self.run_check(&dir, &check, draft, &log_path).await?
            };
            failed |= !outcome.passed();
            let url = format!("{}{draft}/{log_name}", self.cfg.log_url_prefix);
            let attested = self
                .api
                .attest(
                    &self.cx,
                    Attest {
                        repo: self.cfg.repo.clone(),
                        changeset: draft,
                        check: check.name.clone(),
                        passed: outcome.passed(),
                        url: url.clone(),
                    },
                )
                .await;
            results.push(CheckResult {
                check: check.name.clone(),
                outcome,
                url,
                attested,
            });
        }
        self.remove_tree(&dir).await;
        Ok(Report::Checked { draft, results })
    }

    async fn run_check(
        &self,
        dir: &Path,
        check: &Check,
        draft: ChangesetId,
        log_path: &Path,
    ) -> Result<Outcome, Error> {
        let mut file = File::create(log_path)?;
        writeln!(file, "# graft-attester: {} on {draft}", check.name)?;
        writeln!(file, "# command: {}", check.argv.join(" "))?;
        writeln!(file, "# timeout: {}s", self.cfg.timeout.as_secs())?;
        let started = Instant::now();
        let sink = Arc::new(Mutex::new(Sink {
            file,
            left: self.cfg.max_log_bytes,
            truncated: false,
        }));

        let mut cmd = tokio::process::Command::new(&check.argv[0]);
        cmd.args(&check.argv[1..])
            .current_dir(dir)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        if !self.cfg.env.iter().any(|(k, _)| k == "PATH")
            && let Some(path) = std::env::var_os("PATH")
        {
            cmd.env("PATH", path);
        }
        cmd.envs(self.cfg.env.iter().map(|(k, v)| (k, v)));
        if let Some((uid, gid)) = self.cfg.run_as {
            cmd.uid(uid).gid(gid);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let outcome = Outcome::NotStarted(e.kind().to_string());
                finish_log(&sink, &outcome, started)?;
                return Ok(outcome);
            }
        };
        let pid = child.id();
        let out = child
            .stdout
            .take()
            .map(|s| tokio::spawn(copy(s, sink.clone())));
        let err = child
            .stderr
            .take()
            .map(|s| tokio::spawn(copy(s, sink.clone())));

        let waited = tokio::time::timeout(self.cfg.timeout, child.wait()).await;
        // Whatever happened, nothing the check started outlives it.
        if let Some(pid) = pid {
            kill_group(pid);
        }
        let outcome = match waited {
            Ok(Ok(status)) => status_outcome(status),
            Ok(Err(e)) => Outcome::NotStarted(e.kind().to_string()),
            Err(_) => {
                let _ = child.kill().await;
                Outcome::TimedOut
            }
        };
        for task in [out, err].into_iter().flatten() {
            // The group is dead, so the pipes close; don't hang if not.
            let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        }
        finish_log(&sink, &outcome, started)?;
        Ok(outcome)
    }

    /// Removes a draft's tree. A tree handed to [`Config::run_as`] is
    /// removed as that user (the files its checks wrote are theirs).
    async fn remove_tree(&self, dir: &Path) {
        if !dir.exists() {
            return;
        }
        if let Some((uid, gid)) = self.cfg.run_as {
            let _ = tokio::process::Command::new("rm")
                .arg("-rf")
                .arg("--")
                .arg(dir)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .uid(uid)
                .gid(gid)
                .stdin(Stdio::null())
                .status()
                .await;
        }
        if dir.exists()
            && let Err(e) = fs::remove_dir_all(dir)
        {
            log(&format!("removing {}: {e}", dir.display()));
        }
    }

    /// Removes the log directories older than [`Config::log_retention`].
    fn prune_logs(&self) {
        let Ok(entries) = fs::read_dir(&self.cfg.log_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > self.cfg.log_retention);
            if old && entry.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.cfg.state_file else {
            return Ok(());
        };
        let lsn = [self.queue.min_lsn(), self.running]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(self.next_lsn)
            .min(self.next_lsn);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, format!("{lsn}\n"))?;
        fs::rename(&tmp, path)
    }
}

/// Reads the saved position; 1 (the journal's start) when there is none.
fn read_lsn(path: &Path) -> io::Result<u64> {
    match fs::read_to_string(path) {
        Ok(text) => text.trim().parse::<u64>().map(|n| n.max(1)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: not a journal position", path.display()),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(1),
        Err(e) => Err(e),
    }
}

struct Sink {
    file: File,
    left: u64,
    truncated: bool,
}

/// Copies a check's output into the log, keeping at most the sink's
/// budget and draining (discarding) the rest.
async fn copy<R: AsyncRead + Unpin>(mut from: R, sink: Arc<Mutex<Sink>>) {
    let mut buf = vec![0u8; 16 << 10];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let mut s = sink.lock().expect("log sink");
        let keep = n.min(usize::try_from(s.left).unwrap_or(usize::MAX));
        if keep > 0 && s.file.write_all(&buf[..keep]).is_ok() {
            s.left -= keep as u64;
        }
        if keep < n && !s.truncated {
            s.truncated = true;
            let _ = writeln!(s.file, "\n# graft-attester: output truncated here");
        }
    }
}

fn finish_log(sink: &Mutex<Sink>, outcome: &Outcome, started: Instant) -> io::Result<()> {
    let mut s = sink.lock().expect("log sink");
    writeln!(
        s.file,
        "\n# graft-attester: {outcome} after {:.1}s",
        started.elapsed().as_secs_f64()
    )?;
    s.file.sync_all()
}

fn write_skipped(path: &Path, check: &Check, draft: ChangesetId) -> io::Result<()> {
    let mut file = File::create(path)?;
    writeln!(file, "# graft-attester: {} on {draft}", check.name)?;
    writeln!(file, "# graft-attester: {}", Outcome::Skipped)
}

fn status_outcome(status: ExitStatus) -> Outcome {
    if status.success() {
        Outcome::Passed
    } else {
        Outcome::Failed(status.code())
    }
}

fn kill_group(pid: u32) {
    if let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        // ESRCH (already gone) is the usual answer.
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

/// Hands `root` and everything under it to `uid:gid`, never following a
/// symlink.
pub fn chown_tree(root: &Path, uid: u32, gid: u32) -> io::Result<()> {
    lchown(root, Some(uid), Some(gid))?;
    if !fs::symlink_metadata(root)?.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            chown_tree(&entry.path(), uid, gid)?;
        } else {
            lchown(entry.path(), Some(uid), Some(gid))?;
        }
    }
    Ok(())
}

fn backoff(errors: u32) -> Duration {
    Duration::from_secs(1u64 << errors.min(6))
}

/// Writes one line to stderr.
pub fn log(msg: &str) {
    eprintln!("graft-attester: {msg}");
}

fn log_report(report: &Report) {
    match report {
        Report::Checked { draft, results } => {
            for r in results {
                let posted = match &r.attested {
                    Ok(()) => "attested".to_owned(),
                    Err(e) => format!("attest failed: {e}"),
                };
                log(&format!(
                    "{draft} {}: {} ({posted}; log {})",
                    r.check, r.outcome, r.url
                ));
            }
        }
        Report::Skipped { draft, state } => log(&format!("{draft}: skipped, now {state}")),
        Report::NotCheckedOut { draft, error } => {
            log(&format!(
                "{draft}: checkout failed, nothing attested: {error}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> ChangesetId {
        ChangesetId(graft_model::ObjectId([b; 32]))
    }

    fn pending(b: u8, ws: &str, lsn: u64) -> Pending {
        Pending {
            draft: id(b),
            workspace: ws.to_owned(),
            lsn,
        }
    }

    #[test]
    fn checks_parse_and_name_their_logs() {
        let c = Check::parse("ci/build=cargo build --locked").expect("parse");
        assert_eq!(c.name, "ci/build");
        assert_eq!(c.argv, ["cargo", "build", "--locked"]);
        assert_eq!(c.log_name(), "ci-build.log");
        assert!(Check::parse("ci/build").is_err());
        assert!(Check::parse("=cargo").is_err());
        assert!(Check::parse("x= ").is_err());
        assert_eq!(
            Check::parse("../x=a").expect("parse").log_name(),
            "..-x.log"
        );
    }

    #[test]
    fn only_workspace_drafts_are_queued() {
        let d = id(7);
        let entry = |kind: &str, body: String| JournalEntry {
            lsn: 9,
            kind: kind.to_owned(),
            body,
        };
        assert_eq!(
            parse_entry(&entry(
                CHANGESET_CREATED,
                format!("{{\"id\":\"{d}\",\"state\":\"draft\",\"workspace\":\"w\"}}")
            )),
            Some(Pending {
                draft: d,
                workspace: "w".to_owned(),
                lsn: 9
            })
        );
        for body in [
            format!("{{\"id\":\"{d}\",\"state\":\"draft\",\"holder\":\"land:1:ab\"}}"),
            format!("{{\"id\":\"{d}\",\"state\":\"abandoned\"}}"),
            "{\"id\":\"nope\",\"state\":\"draft\",\"workspace\":\"w\"}".to_owned(),
            "not json".to_owned(),
        ] {
            assert_eq!(parse_entry(&entry(CHANGESET_CREATED, body)), None);
        }
        assert_eq!(
            parse_entry(&entry(
                "RefMoved",
                format!("{{\"id\":\"{d}\",\"state\":\"draft\",\"workspace\":\"w\"}}")
            )),
            None
        );
    }

    #[test]
    fn the_queue_keeps_the_newest_draft_per_workspace_and_is_bounded() {
        let mut q = Queue::new(2);
        assert!(q.push(pending(1, "a", 1)));
        assert!(q.push(pending(2, "b", 2)));
        assert!(q.is_full());
        assert!(!q.push(pending(3, "c", 3)), "a third workspace waits");
        // A newer draft of `a` replaces the older one in its place.
        assert!(q.push(pending(4, "a", 4)));
        assert_eq!(q.len(), 2);
        assert_eq!(q.min_lsn(), Some(2));
        assert_eq!(q.pop(), Some(pending(4, "a", 4)));
        assert_eq!(q.pop(), Some(pending(2, "b", 2)));
        assert_eq!(q.pop(), None);

        // A draft put back goes first, unless its workspace moved on.
        let mut q = Queue::new(3);
        assert!(q.push(pending(2, "b", 2)));
        assert!(q.push(pending(5, "c", 5)));
        q.requeue(pending(4, "a", 4));
        q.requeue(pending(3, "c", 3));
        assert_eq!(q.pop(), Some(pending(4, "a", 4)));
        assert_eq!(q.pop(), Some(pending(2, "b", 2)));
        assert_eq!(q.pop(), Some(pending(5, "c", 5)));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn a_missing_state_file_starts_at_the_beginning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lsn");
        assert_eq!(read_lsn(&path).expect("read"), 1);
        fs::write(&path, "42\n").expect("write");
        assert_eq!(read_lsn(&path).expect("read"), 42);
        fs::write(&path, "x").expect("write");
        assert!(read_lsn(&path).is_err());
    }
}
