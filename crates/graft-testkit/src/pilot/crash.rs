//! The long crash suite (exit criterion 2, design §6.6 and §14): `kill -9`
//! the real `graft-server` again and again under mixed load, and after
//! every restart check that nothing it acknowledged was lost and that
//! `admin fsck` is clean.
//!
//! The load is two writers on their own lines (upload blobs, create a
//! changeset on the line's tip, move the line by compare-and-swap) and a
//! reader that resolves the lines and reads acknowledged changesets back. A
//! write counts as acknowledged only once its call returned `Ok`. After a
//! kill the server is down, so fsck runs then; on the next start every
//! acknowledged blob and changeset must be there, and each line must point
//! at its last acknowledged target or at the one whose move was in flight.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use graft_model::{ChangesetId, ObjectId, ObjectKind};

use super::PilotError;
use super::proc::{Bins, Site, Token};
use super::rpc::{CallError, Rpc};
use crate::bench::{Summary, millis};
use crate::crash::Delays;

/// The repository the suite writes.
const REPO: &str = "crash";

/// Writers, each on its own line.
const WRITERS: usize = 2;

/// The longest wait for the load's first acknowledgement after a start.
const FIRST_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// What the suite found.
#[derive(Clone, Debug, Default)]
pub struct CrashOutcome {
    /// `kill -9` restarts done.
    pub restarts: usize,
    /// Acknowledged changesets, blobs and ref moves over the whole suite.
    pub acked_changesets: usize,
    /// Acknowledged blobs.
    pub acked_blobs: usize,
    /// Acknowledged ref moves.
    pub acked_moves: usize,
    /// Reads the reader completed.
    pub reads: u64,
    /// Acknowledged writes missing after a restart, one line each.
    pub lost: Vec<String>,
    /// Restarts after which fsck was not clean.
    pub fsck_failures: Vec<usize>,
    /// Spawn to listening, per start.
    pub startups: Vec<Duration>,
}

impl CrashOutcome {
    /// Zero acknowledged writes lost and every fsck clean.
    pub fn passed(&self) -> bool {
        self.lost.is_empty() && self.fsck_failures.is_empty()
    }

    /// The report section.
    pub fn markdown(&self) -> String {
        let mut r = String::new();
        let _ = writeln!(
            r,
            "{} `kill -9` restarts under mixed load ({WRITERS} writers, 1 reader). Acknowledged: {} changesets, {} blobs, {} ref moves; {} reads.\n",
            self.restarts, self.acked_changesets, self.acked_blobs, self.acked_moves, self.reads
        );
        let _ = writeln!(r, "- Acknowledged writes lost: **{}**", self.lost.len());
        let _ = writeln!(
            r,
            "- fsck not clean after: {}",
            if self.fsck_failures.is_empty() {
                "none".to_owned()
            } else {
                format!("{:?}", self.fsck_failures)
            }
        );
        if let Some(s) = Summary::of(&millis(&self.startups)) {
            let _ = writeln!(
                r,
                "- Restart to serving: p50 {:.1} ms, p95 {:.1} ms",
                s.p50, s.p95
            );
        }
        for l in self.lost.iter().take(20) {
            let _ = writeln!(r, "  - {l}");
        }
        r
    }
}

#[derive(Debug, Default)]
struct LineState {
    acked: Option<ChangesetId>,
    pending: Option<ChangesetId>,
}

#[derive(Debug, Default)]
struct State {
    lines: Vec<LineState>,
    changesets: Vec<ChangesetId>,
    blobs: Vec<ObjectId>,
    moves: usize,
    reads: u64,
}

fn line_name(w: usize) -> String {
    format!("crash-{w}")
}

async fn writer(
    url: String,
    token: Token,
    w: usize,
    iteration: usize,
    state: Arc<Mutex<State>>,
    seq: Arc<AtomicU64>,
) {
    let Ok(mut rpc) = Rpc::connect(&url, &token).await else {
        return;
    };
    let line = line_name(w);
    loop {
        let n = seq.fetch_add(1, Ordering::Relaxed);
        let files: Vec<(String, Vec<u8>)> = vec![
            (
                format!("w{w}/f{}.txt", n % 16),
                format!("{line} {iteration} {n}\n").repeat(8).into_bytes(),
            ),
            (
                format!("w{w}/log/{n}.txt"),
                format!("entry {n}\n").into_bytes(),
            ),
        ];
        let blobs: Vec<Vec<u8>> = files.iter().map(|(_, b)| b.clone()).collect();
        if rpc.put_blobs(REPO, &blobs).await.is_err() {
            return;
        }
        let parent = {
            let mut s = state.lock().expect("state");
            s.blobs
                .extend(blobs.iter().map(|b| ObjectId::hash(ObjectKind::Blob, b)));
            s.lines[w].acked
        };
        let Ok(created) = rpc.commit(REPO, parent, &files, n).await else {
            return;
        };
        {
            let mut s = state.lock().expect("state");
            s.changesets.push(created.id);
            s.lines[w].pending = Some(created.id);
        }
        if rpc
            .update_ref(REPO, &line, parent, created.id)
            .await
            .is_err()
        {
            return;
        }
        let mut s = state.lock().expect("state");
        s.lines[w] = LineState {
            acked: Some(created.id),
            pending: None,
        };
        s.moves += 1;
    }
}

async fn reader(url: String, token: Token, state: Arc<Mutex<State>>) {
    let Ok(mut rpc) = Rpc::connect(&url, &token).await else {
        return;
    };
    loop {
        for w in 0..WRITERS {
            if rpc.resolve(REPO, &line_name(w)).await.is_err() {
                return;
            }
        }
        let sample: Vec<ChangesetId> = {
            let s = state.lock().expect("state");
            s.changesets.iter().rev().take(8).copied().collect()
        };
        if rpc.missing_changesets(REPO, &sample).await.is_err() {
            return;
        }
        state.lock().expect("state").reads += 1;
    }
}

/// Checks every acknowledged write after a restart, and adopts each line's
/// actual target (the in-flight move may have landed).
async fn check(
    url: &str,
    token: &Token,
    state: &Mutex<State>,
    restart: usize,
    lost: &mut Vec<String>,
) -> Result<(), CallError> {
    let mut rpc = Rpc::connect(url, token).await?;
    let (changesets, blobs) = {
        let s = state.lock().expect("state");
        (s.changesets.clone(), s.blobs.clone())
    };
    for id in rpc.missing_changesets(REPO, &changesets).await? {
        lost.push(format!("after restart {restart}: changeset {id} missing"));
    }
    for id in rpc.missing_objects(REPO, &blobs).await? {
        lost.push(format!(
            "after restart {restart}: blob {} missing",
            id.to_hex()
        ));
    }
    for w in 0..WRITERS {
        let actual = rpc.resolve(REPO, &line_name(w)).await?;
        let mut s = state.lock().expect("state");
        let line = &mut s.lines[w];
        if actual != line.acked && (actual.is_none() || actual != line.pending) {
            lost.push(format!(
                "after restart {restart}: line {} points at {actual:?}, acknowledged {:?}",
                line_name(w),
                line.acked
            ));
        }
        line.acked = actual;
        line.pending = None;
    }
    Ok(())
}

/// Runs `restarts` kill -9 cycles in `dir` (created, then reused). Kill
/// points come from `seed`, so a failure replays.
pub fn crash_suite(
    bins: &Bins,
    dir: &Path,
    restarts: usize,
    seed: u64,
) -> Result<CrashOutcome, PilotError> {
    let _ = std::fs::remove_dir_all(dir);
    let site = Site::new(bins, dir)?;
    let token = site.mint("crash-suite", &[&format!("repo:{REPO}:admin")])?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(|e| PilotError::Step(format!("runtime: {e}")))?;
    let state = Arc::new(Mutex::new(State {
        lines: (0..WRITERS).map(|_| LineState::default()).collect(),
        ..State::default()
    }));
    let seq = Arc::new(AtomicU64::new(0));
    let mut delays = Delays::new(seed);
    let mut out = CrashOutcome::default();

    for i in 0..=restarts {
        let server = site.start()?;
        out.startups.push(server.startup);
        let url = server.url();
        let step = |e: CallError| PilotError::Step(format!("crash suite, start {i}: {e}"));
        if i == 0 {
            rt.block_on(async { Rpc::connect(&url, &token).await?.create_repo(REPO).await })
                .map_err(step)?;
        } else {
            rt.block_on(check(&url, &token, &state, i, &mut out.lost))
                .map_err(step)?;
        }
        if i == restarts {
            server.kill9();
            break;
        }

        let before = state.lock().expect("state").moves;
        let mut tasks = Vec::new();
        for w in 0..WRITERS {
            tasks.push(rt.spawn(writer(
                url.clone(),
                token.clone(),
                w,
                i,
                state.clone(),
                seq.clone(),
            )));
        }
        tasks.push(rt.spawn(reader(url.clone(), token.clone(), state.clone())));
        // Wait until the load is writing, then kill at a seeded point.
        let deadline = Instant::now() + FIRST_ACK_TIMEOUT;
        while state.lock().expect("state").moves == before {
            if Instant::now() > deadline {
                return Err(PilotError::Timeout(format!(
                    "crash suite, start {i}: no write acknowledged"
                )));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(delays.next(Duration::from_millis(20), Duration::from_millis(400)));
        server.kill9();
        let joined = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(60), async {
                for t in tasks {
                    let _ = t.await;
                }
            })
            .await
        });
        if joined.is_err() {
            return Err(PilotError::Timeout(format!(
                "crash suite, start {i}: load did not stop"
            )));
        }
        if !site.fsck(REPO)? {
            out.fsck_failures.push(i + 1);
        }
        out.restarts += 1;
    }
    let s = state.lock().expect("state");
    out.acked_changesets = s.changesets.len();
    out.acked_blobs = s.blobs.len();
    out.acked_moves = s.moves;
    out.reads = s.reads;
    Ok(out)
}
