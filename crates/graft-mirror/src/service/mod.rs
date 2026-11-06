//! `graft-mirror serve` (D41, `docs/notes/mirror-service.md` §3.3–§5): one
//! mirror service for every repository. Its config names the server, its
//! token, its data directory, its metrics address and its limits, and no
//! repositories: those are the server's enabled `repo_mirrors` rows.
//!
//! The reconcile loop runs on start and every `reconcile_s`:
//!
//! 1. `ListMirrors` with `enabled = true`. A row's change is its `version`
//!    against the version its task started from: one integer compare.
//! 2. A row with no task: take the repository's authority lease, check or
//!    create `<data>/<repo>` (with its migration marker) and spawn a task.
//! 3. A task whose row's version changed is told to stop. It finishes the
//!    poll it is in, exits, and is respawned from the new row; a restart
//!    never cancels a poll.
//! 4. A task whose row is gone or disabled stops the same way, is not
//!    respawned, and its lease is released. Its directory stays.
//! 5. Every few reconciles, a repository with a directory and no task is
//!    leased briefly to run its requests: a purge deletes the directory.
//!
//! Leases are renewed every 20 s. A task whose lease is lost (a renewal or
//! any fenced write answered `lease_lost`) stops after its current step and
//! writes nothing more. A failing server is retried with a back-off capped
//! at 60 s, by the loop and by every task, rather than hammered.
//!
//! There are no check jobs and no worker pool here (D22, D35): the service
//! schedules mirror polls and mirror requests only.

pub mod control;
pub mod limits;
pub mod metrics;
pub mod netguard;
pub mod requests;
pub mod status;
pub mod task;

use std::collections::{BTreeMap, HashMap};
use std::hash::RandomState;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graft_proto::ApiError;
use graft_proto::error::ApiResult;
use graft_proto::pb::{self, MirrorErrorCode};
use serde::Deserialize;
use tokio::sync::mpsc;

pub use control::Control;
use limits::{Backoff, FetchSlots, RateBudget};
pub use metrics::ServiceMetrics;
pub use task::{ExitReason, PollReport, PollStep, TaskCtl, TaskCtx, TaskExit};

use crate::MirrorError;

/// The file `import-config` writes into a repository's data directory when
/// it migrates a per-repository mirror (§6). `serve` adopts no existing
/// directory without it, so a stray copy of an old volume cannot start.
pub const MIGRATION_MARKER: &str = "graft-mirror-migrated";

/// A repository's git cache, under its data directory.
pub const GIT_DIR: &str = "mirror.git";

/// A repository's mirror database, under its data directory.
pub const DB_FILE: &str = "mirror.sqlite";

/// How often a repository with no task is checked for requests, in
/// reconciles.
pub const SWEEP_EVERY: u64 = 4;

/// The service's settings.
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// `<data>/<repo>/` per repository.
    pub data_dir: PathBuf,
    /// This process's lease instance id (random per start; never durable).
    pub instance: String,
    /// Time between reconciles.
    pub reconcile: Duration,
    /// Time between lease renewals.
    pub renew: Duration,
    /// The longest one poll may take.
    pub poll_timeout: Duration,
    /// Polls fetching at once.
    pub max_fetches: usize,
    /// Polls fetching at once from one upstream host.
    pub per_host: usize,
    /// Upstream requests per hour per host and credential; intervals
    /// stretch past 80% of it.
    pub host_budget_per_hour: u32,
    /// The cap on a failing repository's back-off.
    pub max_failure_backoff: Duration,
    /// The cap on the back-off while the graft server fails.
    pub server_backoff_cap: Duration,
    /// The shortest poll interval, whatever a row asks (the server enforces
    /// it too).
    pub min_interval: Duration,
    /// Claim and run batch, settle, exclude and rewind requests between
    /// polls. Off until the batcher runs requests; purges run regardless.
    pub claim_requests: bool,
    /// Poll an upstream that is a local path (tests only: the server never
    /// accepts one).
    pub allow_local_upstreams: bool,
}

impl ServeConfig {
    /// The defaults of §4 for `data_dir`.
    pub fn new(data_dir: impl Into<PathBuf>) -> ServeConfig {
        ServeConfig {
            data_dir: data_dir.into(),
            instance: random_instance(),
            reconcile: Duration::from_secs(15),
            renew: Duration::from_secs(20),
            poll_timeout: Duration::from_secs(120),
            max_fetches: 4,
            per_host: 2,
            host_budget_per_hour: 5000,
            max_failure_backoff: Duration::from_secs(15 * 60),
            server_backoff_cap: Duration::from_secs(60),
            min_interval: Duration::from_secs(15),
            claim_requests: false,
            allow_local_upstreams: false,
        }
    }
}

fn random_instance() -> String {
    let mut b = [0u8; 8];
    // A process id is a fallback only: the instance is not a security token.
    if getrandom::fill(&mut b).is_err() {
        b = u64::from(std::process::id()).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What every task shares: the fetch slots, the rate budget, the resolver
/// and the clock.
pub(crate) struct Shared {
    pub(crate) slots: FetchSlots,
    pub(crate) budget: Mutex<RateBudget>,
    pub(crate) resolve: netguard::Resolve,
    pub(crate) hasher: RandomState,
    start: tokio::time::Instant,
    start_ms: i64,
}

impl Shared {
    /// Milliseconds since the Unix epoch, on the runtime's clock (so a
    /// paused test clock moves it).
    pub(crate) fn now_ms(&self) -> i64 {
        self.start_ms + self.start.elapsed().as_millis() as i64
    }
}

/// Starts a repository's task. The real one builds a follower over the
/// repository's directory; tests build fakes.
pub trait Spawner<C: Control>: Send + Sync + 'static {
    /// Spawns `task`, which must end by sending its exit (as
    /// [`TaskCtx::run`] and [`TaskCtx::broken`] do).
    fn spawn(&self, task: TaskCtx<C>);
}

struct Running {
    version: u64,
    upstream: String,
    epoch: u64,
    expires_at_ms: i64,
    ctl: Arc<TaskCtl>,
    /// The lines the row had: the series to drop when the task stops.
    lines: Vec<String>,
}

/// A repository name usable as one directory name.
fn safe_dir_name(repo: &str) -> bool {
    !repo.is_empty()
        && !repo.starts_with('.')
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The mirror service.
pub struct Service<C: Control, S: Spawner<C>> {
    control: Arc<C>,
    spawner: S,
    config: Arc<ServeConfig>,
    shared: Arc<Shared>,
    tasks: HashMap<String, Running>,
    rows: BTreeMap<String, pb::Mirror>,
    exits_tx: mpsc::UnboundedSender<TaskExit>,
    exits_rx: mpsc::UnboundedReceiver<TaskExit>,
    server: Backoff,
    reconciles: u64,
    metrics: Option<ServiceMetrics>,
}

impl<C: Control, S: Spawner<C>> Service<C, S> {
    /// A service calling `control`, starting tasks with `spawner`.
    pub fn new(control: Arc<C>, spawner: S, config: ServeConfig) -> Self {
        Self::with_resolver(control, spawner, config, netguard::system_resolver())
    }

    /// [`Service::new`] resolving upstream hosts with `resolve`.
    pub fn with_resolver(
        control: Arc<C>,
        spawner: S,
        config: ServeConfig,
        resolve: netguard::Resolve,
    ) -> Self {
        let shared = Arc::new(Shared {
            slots: FetchSlots::new(config.max_fetches, config.per_host),
            budget: Mutex::new(RateBudget::new(config.host_budget_per_hour)),
            resolve,
            hasher: RandomState::new(),
            start: tokio::time::Instant::now(),
            start_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64),
        });
        let (exits_tx, exits_rx) = mpsc::unbounded_channel();
        let server = Backoff::new(Duration::from_secs(1), config.server_backoff_cap);
        Service {
            control,
            spawner,
            config: Arc::new(config),
            shared,
            tasks: HashMap::new(),
            rows: BTreeMap::new(),
            exits_tx,
            exits_rx,
            server,
            reconciles: 0,
            metrics: None,
        }
    }

    /// Exports metrics to `metrics`, dropping a repository's series when
    /// its task stops.
    pub fn with_metrics(mut self, metrics: ServiceMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// The running tasks: repository, row version and lease epoch.
    pub fn running(&self) -> Vec<(String, u64, u64)> {
        let mut v: Vec<_> = self
            .tasks
            .iter()
            .map(|(r, t)| (r.clone(), t.version, t.epoch))
            .collect();
        v.sort();
        v
    }

    /// Lists every enabled row, page by page.
    async fn list(&self) -> ApiResult<BTreeMap<String, pb::Mirror>> {
        let mut rows = BTreeMap::new();
        let mut token = String::new();
        // A bound on pages, so a server that never ends a listing cannot
        // hold the loop.
        for _ in 0..1000 {
            let page = self.control.list_mirrors(Some(true), token).await?;
            for row in page.mirrors {
                if !row.enabled {
                    continue;
                }
                match rows.get(&row.repo) {
                    Some(kept) => {
                        let kept: &pb::Mirror = kept;
                        tracing::warn!(
                            repo = %row.repo, kept = %kept.upstream, skipped = %row.upstream,
                            "mirror: a repository has two enabled upstreams; one is served"
                        );
                    }
                    None => {
                        rows.insert(row.repo.clone(), row);
                    }
                }
            }
            if page.next_page_token.is_empty() {
                return Ok(rows);
            }
            token = page.next_page_token;
        }
        Ok(rows)
    }

    /// One reconcile: list the rows, then start, restart and stop tasks to
    /// match. On a server failure, returns it; nothing is stopped.
    pub async fn reconcile(&mut self) -> ApiResult<()> {
        self.drain_exits().await;
        let rows = self.list().await?;
        self.rows = rows;
        let repos: Vec<String> = self.rows.keys().cloned().collect();
        for repo in repos {
            let row = self.rows[&repo].clone();
            match self.tasks.get(&repo) {
                Some(t) if t.version == row.version && t.upstream == row.upstream => {}
                Some(t) => {
                    tracing::info!(
                        repo = %repo, from = t.version, to = row.version,
                        "mirror row changed: restarting its task after the current poll"
                    );
                    t.ctl.stop();
                }
                None => self.start(row).await,
            }
        }
        for (repo, t) in &self.tasks {
            if !self.rows.contains_key(repo) {
                tracing::info!(repo = %repo, "mirror row removed or disabled: stopping its task");
                t.ctl.stop();
            }
        }
        self.reconciles += 1;
        if self.reconciles.is_multiple_of(SWEEP_EVERY) {
            self.sweep().await;
        }
        Ok(())
    }

    /// Takes the lease, prepares the directory and spawns `row`'s task.
    async fn start(&mut self, row: pb::Mirror) {
        let repo = row.repo.clone();
        if !safe_dir_name(&repo) {
            tracing::error!(repo = %repo, "mirror: repository name is not a safe directory name");
            return;
        }
        let lease = match self
            .control
            .acquire_lease(&repo, &self.config.instance)
            .await
        {
            Ok(l) => l,
            Err(e) => {
                tracing::info!(repo = %repo, error = ?e, "mirror lease not taken; retried next reconcile");
                return;
            }
        };
        let dir = self.config.data_dir.join(&repo);
        if let Err(why) = prepare_dir(&dir) {
            tracing::error!(repo = %repo, dir = %dir.display(), why, "mirror data directory refused");
            let status = pb::MirrorStatus {
                error_code: MirrorErrorCode::Internal as i32,
                error_message: Some(why.to_owned()),
                lease_epoch: lease.epoch,
                ..Default::default()
            };
            let _ = self.control.set_status(&repo, &row.upstream, status).await;
            let _ = self.control.release_lease(&repo, lease.epoch).await;
            return;
        }
        let ctl = Arc::new(TaskCtl::default());
        self.tasks.insert(
            repo.clone(),
            Running {
                version: row.version,
                upstream: row.upstream.clone(),
                epoch: lease.epoch,
                expires_at_ms: lease.expires_at_ms,
                ctl: ctl.clone(),
                lines: row.lines.iter().map(|l| l.line.clone()).collect(),
            },
        );
        self.note_tasks();
        tracing::info!(repo = %repo, version = row.version, epoch = lease.epoch, "mirror task starting");
        self.spawner.spawn(TaskCtx {
            control: self.control.clone(),
            shared: self.shared.clone(),
            config: self.config.clone(),
            row,
            epoch: lease.epoch,
            dir,
            ctl,
            metrics: self.metrics.clone(),
            exits: self.exits_tx.clone(),
        });
    }

    fn note_tasks(&self) {
        if let Some(m) = &self.metrics {
            m.set_tasks(self.tasks.len());
        }
    }

    /// Handles exits already sent.
    async fn drain_exits(&mut self) {
        while let Ok(exit) = self.exits_rx.try_recv() {
            self.exited(exit).await;
        }
    }

    /// Waits for the next task exit and handles it: releases the lease
    /// unless it was lost, and respawns the task when its row is still
    /// enabled.
    pub async fn next_exit(&mut self) -> Option<TaskExit> {
        let exit = self.exits_rx.recv().await?;
        self.exited(exit.clone()).await;
        Some(exit)
    }

    async fn exited(&mut self, exit: TaskExit) {
        let ours = self
            .tasks
            .get(&exit.repo)
            .is_some_and(|t| t.epoch == exit.epoch);
        if !ours {
            return;
        }
        if let Some(t) = self.tasks.remove(&exit.repo)
            && let Some(m) = &self.metrics
        {
            m.drop_repo(&exit.repo, &t.lines);
            if exit.reason == ExitReason::LeaseLost {
                m.lease_lost();
            }
        }
        self.note_tasks();
        if exit.reason != ExitReason::LeaseLost
            && let Err(e) = self.control.release_lease(&exit.repo, exit.epoch).await
        {
            tracing::info!(repo = %exit.repo, error = ?e, "mirror lease not released; it lapses");
        }
        if exit.reason == ExitReason::Stopped
            && let Some(row) = self.rows.get(&exit.repo).cloned()
        {
            self.start(row).await;
        }
    }

    /// Renews every held lease. A refused renewal, or one the server could
    /// not answer before the lease lapsed, stops that task.
    pub async fn renew(&mut self) {
        let now = self.shared.now_ms();
        let held: Vec<(String, u64)> = self
            .tasks
            .iter()
            .filter(|(_, t)| !t.ctl.lost())
            .map(|(r, t)| (r.clone(), t.epoch))
            .collect();
        for (repo, epoch) in held {
            match self.control.renew_lease(&repo, epoch).await {
                Ok(l) => {
                    if let Some(t) = self.tasks.get_mut(&repo) {
                        t.expires_at_ms = l.expires_at_ms;
                    }
                }
                Err(e) => {
                    let Some(t) = self.tasks.get(&repo) else {
                        continue;
                    };
                    if matches!(e, ApiError::LeaseLost { .. }) || now >= t.expires_at_ms {
                        tracing::warn!(repo = %repo, epoch, "mirror lease lost: stopping its task");
                        t.ctl.lose();
                    }
                }
            }
        }
    }

    /// Runs the requests of repositories with a directory and no task.
    async fn sweep(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.config.data_dir) else {
            return;
        };
        let idle: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                (safe_dir_name(&name)
                    && !self.tasks.contains_key(&name)
                    && !self.rows.contains_key(&name)
                    && e.path().join(MIGRATION_MARKER).is_file())
                .then(|| (name, e.path()))
            })
            .collect();
        for (repo, dir) in idle {
            let Ok(lease) = self
                .control
                .acquire_lease(&repo, &self.config.instance)
                .await
            else {
                continue;
            };
            match requests::purge(&*self.control, &repo, lease.epoch, &dir).await {
                Ok(true) => tracing::info!(repo = %repo, "mirror data purged"),
                Ok(false) => {}
                Err(e) => tracing::warn!(repo = %repo, error = ?e, "mirror purge sweep failed"),
            }
            let _ = self.control.release_lease(&repo, lease.epoch).await;
        }
    }

    /// Stops every task after its current step, waits up to the poll
    /// timeout for them, and releases their leases.
    pub async fn stop_all(&mut self) {
        for t in self.tasks.values() {
            t.ctl.stop();
        }
        self.rows.clear();
        let deadline = tokio::time::Instant::now() + self.config.poll_timeout;
        while !self.tasks.is_empty() {
            match tokio::time::timeout_at(deadline, self.exits_rx.recv()).await {
                Ok(Some(exit)) => self.exited(exit).await,
                _ => break,
            }
        }
    }

    /// Runs until `shutdown`: reconciles every `reconcile` (backing off
    /// while the server fails), renews every `renew`, and respawns tasks as
    /// they exit.
    pub async fn run(mut self, shutdown: impl std::future::Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        let mut next_reconcile = tokio::time::Instant::now();
        let mut renew = tokio::time::interval(self.config.renew);
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        renew.tick().await;
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                () = tokio::time::sleep_until(next_reconcile) => {
                    let wait = match self.reconcile().await {
                        Ok(()) => {
                            self.server.reset();
                            self.config.reconcile
                        }
                        Err(e) => {
                            let wait = self.server.fail();
                            tracing::warn!(error = ?e, wait_ms = wait.as_millis() as u64,
                                "mirror reconcile failed; backing off");
                            wait
                        }
                    };
                    next_reconcile = tokio::time::Instant::now() + wait;
                }
                _ = renew.tick() => self.renew().await,
                Some(exit) = self.exits_rx.recv() => self.exited(exit).await,
            }
        }
        tracing::info!("mirror service stopping");
        self.stop_all().await;
    }
}

/// Checks `dir`: an existing one must carry the migration marker; a new
/// one is created with it.
pub fn prepare_dir(dir: &Path) -> Result<(), &'static str> {
    if dir.exists() {
        if dir.join(MIGRATION_MARKER).is_file() {
            return Ok(());
        }
        return Err("the data directory has no migration marker; it was not adopted");
    }
    std::fs::create_dir_all(dir).map_err(|_| "the data directory could not be created")?;
    std::fs::write(dir.join(MIGRATION_MARKER), b"graft-mirror serve\n")
        .map_err(|_| "the migration marker could not be written")?;
    Ok(())
}

// ---- the real tasks -------------------------------------------------------

pub mod follow_task;
pub use follow_task::FollowSpawner;

// ---- the config file ------------------------------------------------------

/// `graft-mirror serve --config <file>`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServeFile {
    /// The graft server URL.
    pub server: String,
    /// The `mirror`-kind token's file (else `GRAFT_TOKEN`).
    pub token_file: Option<PathBuf>,
    /// `<data>/<repo>/` per repository.
    pub data_dir: PathBuf,
    /// Where `/metrics` is served.
    pub metrics_addr: Option<String>,
    /// JSON log lines.
    #[serde(default)]
    pub json_logs: bool,
    /// The limits (§4).
    #[serde(default)]
    pub limits: LimitsFile,
    /// Claim batch and rewind requests (off until the batcher runs them).
    #[serde(default)]
    pub claim_requests: bool,
}

/// `[limits]`: every field optional, defaulting to §4.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsFile {
    /// Seconds between reconciles (15).
    pub reconcile_s: Option<u64>,
    /// The longest poll, seconds (120).
    pub poll_timeout_s: Option<u64>,
    /// Polls fetching at once (4).
    pub max_fetches: Option<usize>,
    /// Polls fetching at once per host (2).
    pub per_host: Option<usize>,
    /// Upstream requests per hour per host and credential (5000).
    pub host_budget_per_hour: Option<u32>,
    /// The cap on a failing repository's back-off, seconds (900).
    pub max_backoff_s: Option<u64>,
    /// The cap on the server-down back-off, seconds (60).
    pub server_backoff_cap_s: Option<u64>,
    /// The shortest poll interval, seconds (15).
    pub min_interval_s: Option<u64>,
}

impl ServeFile {
    /// Reads and checks `path`.
    pub fn load(path: &Path) -> Result<ServeFile, MirrorError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| MirrorError::Config(format!("reading {}: {e}", path.display())))?;
        let file: ServeFile = toml::from_str(&text)
            .map_err(|e| MirrorError::Config(format!("{}: {e}", path.display())))?;
        file.config()?;
        Ok(file)
    }

    /// The service settings it gives.
    pub fn config(&self) -> Result<ServeConfig, MirrorError> {
        let l = &self.limits;
        let mut c = ServeConfig::new(&self.data_dir);
        let secs = |v: Option<u64>, d: Duration| v.map_or(d, Duration::from_secs);
        c.reconcile = secs(l.reconcile_s, c.reconcile);
        c.poll_timeout = secs(l.poll_timeout_s, c.poll_timeout);
        c.max_fetches = l.max_fetches.unwrap_or(c.max_fetches);
        c.per_host = l.per_host.unwrap_or(c.per_host);
        c.host_budget_per_hour = l.host_budget_per_hour.unwrap_or(c.host_budget_per_hour);
        c.max_failure_backoff = secs(l.max_backoff_s, c.max_failure_backoff);
        c.server_backoff_cap = secs(l.server_backoff_cap_s, c.server_backoff_cap);
        c.min_interval = secs(l.min_interval_s, c.min_interval);
        c.claim_requests = self.claim_requests;
        if c.reconcile.is_zero()
            || c.poll_timeout.is_zero()
            || c.max_fetches == 0
            || c.per_host == 0
            || c.host_budget_per_hour == 0
            || c.min_interval.is_zero()
        {
            return Err(MirrorError::Config(
                "[limits]: every limit must be above zero".to_owned(),
            ));
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_without_the_marker_is_refused_and_a_new_one_marked() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = tmp.path().join("r");
        prepare_dir(&fresh).expect("created");
        assert!(fresh.join(MIGRATION_MARKER).is_file());
        prepare_dir(&fresh).expect("adopted");
        let stray = tmp.path().join("old");
        std::fs::create_dir(&stray).unwrap();
        std::fs::write(stray.join(DB_FILE), b"").unwrap();
        assert!(prepare_dir(&stray).is_err());
    }

    #[test]
    fn repository_names_must_be_one_safe_directory() {
        for ok in ["graft", "rust-core_2", "a.b"] {
            assert!(safe_dir_name(ok), "{ok}");
        }
        for bad in ["", "..", ".hidden", "a/b", "a\\b", "a b"] {
            assert!(!safe_dir_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_config_file_has_defaults_and_refuses_zero_limits() {
        let f: ServeFile =
            toml::from_str("server = \"http://s:1\"\ndata_dir = \"/d\"\n").expect("parses");
        let c = f.config().unwrap();
        assert_eq!(c.reconcile, Duration::from_secs(15));
        assert_eq!(c.max_fetches, 4);
        assert!(!c.claim_requests);
        let f: ServeFile = toml::from_str(
            "server = \"http://s:1\"\ndata_dir = \"/d\"\n[limits]\nmax_fetches = 0\n",
        )
        .unwrap();
        assert!(f.config().is_err());
        assert!(
            toml::from_str::<ServeFile>("server = \"x\"\ndata_dir = \"/d\"\nrepos = []\n").is_err()
        );
    }
}
