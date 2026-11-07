//! `graft-mirror serve` (D41): the reconcile loop, leases, limits,
//! back-off, status coalescing and requests, against an in-process fake of
//! the server's mirror calls. The fencing and credential tests follow a
//! local git upstream into an in-process engine over the memory store,
//! whose lease rows the fake writes, so the engine fences ref moves by the
//! same epochs.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graft_engine::{Engine, EngineConfig};
use graft_mirror::credentials::SecretToken;
use graft_mirror::current_ref;
use graft_mirror::github::{ForgeError, NewPull, Pull};
use graft_mirror::service::batch_task::LeaseCredential;
use graft_mirror::service::control::Control;
use graft_mirror::service::follow_task::{FollowSpawner, Forges};
use graft_mirror::service::requests::{self, PullDesk, RequestFailure, RequestRunner};
use graft_mirror::service::status::PollFailure;
use graft_mirror::service::{
    ExitReason, MIGRATION_MARKER, PollReport, PollStep, ServeConfig, Service, ServiceMetrics,
    Spawner, TaskCtx, netguard,
};
use graft_proto::error::ApiResult;
use graft_proto::pb::{self, MirrorErrorCode, MirrorRequestKind};
use graft_proto::types::{RefKind, UpdateRef};
use graft_proto::{AdminApi, ApiError, CallContext, Principal, PrincipalKind, RepoApi};
use graft_store::memory::MemoryStore;
use graft_store::{LeaseRecord, MetaStore, MetaTxn, Precondition, TxnOutcome, Write};
use graft_testkit::clock::FakeClock;

const REPO: &str = "r";
const ENGINE_NOW: i64 = 1_000_000;

type Api = Engine<MemoryStore, MemoryStore>;

// ---- the fake server -------------------------------------------------------

#[derive(Clone, Debug)]
struct Lease {
    holder: Option<String>,
    epoch: u64,
    expires_at_ms: i64,
}

#[derive(Default)]
struct State {
    rows: Vec<pb::Mirror>,
    leases: HashMap<String, Lease>,
    statuses: Vec<pb::MirrorStatus>,
    stale_status: usize,
    credentials: HashMap<String, String>,
    pending: VecDeque<pb::MirrorRequest>,
    claimed: Vec<pb::MirrorRequest>,
    completed: Vec<(u64, pb::MirrorRequestOutcome)>,
    failed: Vec<(u64, String)>,
    list_calls: Vec<tokio::time::Instant>,
}

/// The server's mirror calls, in memory. Leases are mirrored into the
/// engine's store when one is attached.
#[derive(Default)]
struct Fake {
    state: Mutex<State>,
    store: Option<Arc<MemoryStore>>,
    server_down: AtomicBool,
    fail_next_complete: AtomicBool,
    /// Refuses this many more lease acquisitions, as if another holder had
    /// it for a moment.
    refuse_acquires: AtomicUsize,
}

impl Fake {
    fn with_store(store: Arc<MemoryStore>) -> Fake {
        Fake {
            store: Some(store),
            ..Fake::default()
        }
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn set_row(&self, row: pb::Mirror) {
        let mut st = self.st();
        st.rows.retain(|r| r.repo != row.repo);
        st.rows.push(row);
    }

    fn bump(&self, repo: &str) {
        let mut st = self.st();
        let row = st.rows.iter_mut().find(|r| r.repo == repo).unwrap();
        row.version += 1;
    }

    fn remove(&self, repo: &str) {
        self.st().rows.retain(|r| r.repo != repo);
    }

    fn lease(&self, repo: &str) -> Option<Lease> {
        self.st().leases.get(repo).cloned()
    }

    fn statuses(&self) -> Vec<pb::MirrorStatus> {
        self.st().statuses.clone()
    }

    /// Another process takes the lease (as after this one's lapsed).
    async fn steal(&self, repo: &str) -> u64 {
        let (prev, l) = {
            let mut st = self.st();
            let l = st.leases.get_mut(repo).unwrap();
            let prev = l.epoch;
            l.epoch += 1;
            l.holder = Some("other".to_owned());
            l.expires_at_ms = i64::MAX;
            (prev, l.clone())
        };
        self.put_store_lease(prev, &l).await;
        l.epoch
    }

    /// The lease lapses (its holder stopped renewing).
    fn lapse(&self, repo: &str) {
        let mut st = self.st();
        st.leases.get_mut(repo).unwrap().expires_at_ms = 0;
    }

    async fn put_store_lease(&self, expected: u64, l: &Lease) {
        let Some(store) = &self.store else {
            return;
        };
        let id = store.repo(REPO).await.unwrap().unwrap();
        let txn = MetaTxn {
            preconditions: vec![Precondition::LeaseEpochIs { epoch: expected }],
            writes: vec![Write::PutLease(LeaseRecord {
                holder: l.holder.clone(),
                epoch: l.epoch,
                // The engine's clock is fixed; held means not released.
                expires_at_ms: if l.holder.is_some() {
                    ENGINE_NOW + 3_600_000
                } else {
                    0
                },
            })],
        };
        assert!(matches!(
            store.commit(id, txn).await,
            Ok(TxnOutcome::Committed(_))
        ));
    }

    /// Mirrors a cut's claim (`epoch`) or its end (`None`) into the
    /// engine's store, as the server's request row and line slot record it.
    async fn put_store_cut(&self, req: &pb::MirrorRequest, epoch: Option<u64>) {
        use graft_store::{
            BatchState, LineSlot, MirrorRequestKind as Kind, MirrorRequestRecord,
            MirrorRequestState,
        };
        let Some(store) = &self.store else {
            return;
        };
        if req.kind != MirrorRequestKind::CutBatch as i32 {
            return;
        }
        let id = store.repo(&req.repo).await.unwrap().unwrap();
        let prior = store.get_mirror_request(id, req.id).await.unwrap();
        let slot = store
            .line_slot(id, &req.line, LineSlot::Batch)
            .await
            .unwrap();
        let record = MirrorRequestRecord {
            id: req.id,
            line: req.line.clone(),
            kind: Kind::CutBatch,
            client_request_id: format!("cut-{}", req.id),
            base: None,
            excludes: Vec::new(),
            upstream: None,
            state: if epoch.is_some() {
                MirrorRequestState::Claimed
            } else {
                MirrorRequestState::Done
            },
            batch: Some(if epoch.is_some() {
                BatchState::Requested
            } else {
                BatchState::Settled
            }),
            claimed_by_epoch: epoch,
            claimed_at_ms: epoch.map(|_| ENGINE_NOW),
            deadline_ms: ENGINE_NOW + 3_600_000,
            result: Default::default(),
            requested_by: "op".to_owned(),
            created_at_ms: ENGINE_NOW,
            updated_at_ms: ENGINE_NOW,
            version: prior.as_ref().map_or(1, |r| r.version + 1),
        };
        let txn = MetaTxn {
            preconditions: vec![
                Precondition::MirrorRequestVersionIs {
                    id: req.id,
                    version: prior.map(|r| r.version),
                },
                Precondition::LineSlotIs {
                    line: req.line.clone(),
                    slot: LineSlot::Batch,
                    request: slot,
                },
            ],
            writes: vec![
                Write::PutMirrorRequest(record),
                Write::SetLineSlot {
                    line: req.line.clone(),
                    slot: LineSlot::Batch,
                    request: epoch.map(|_| req.id),
                },
            ],
        };
        assert!(matches!(
            store.commit(id, txn).await,
            Ok(TxnOutcome::Committed(_))
        ));
    }

    /// Drops claimed request `id`, returning it.
    fn end_claim(&self, id: u64) -> Option<pb::MirrorRequest> {
        let mut st = self.st();
        let pos = st.claimed.iter().position(|r| r.id == id)?;
        Some(st.claimed.remove(pos))
    }

    fn held(&self, repo: &str, epoch: u64) -> ApiResult<()> {
        match self.st().leases.get(repo) {
            Some(l) if l.holder.is_some() && l.epoch == epoch => Ok(()),
            _ => Err(ApiError::LeaseLost {
                repo: repo.to_owned(),
            }),
        }
    }

    fn up(&self) -> ApiResult<()> {
        if self.server_down.load(Ordering::SeqCst) {
            Err(ApiError::Unavailable { retry_after_ms: 0 })
        } else {
            Ok(())
        }
    }
}

impl Control for Fake {
    async fn list_mirrors(
        &self,
        enabled: Option<bool>,
        _token: String,
    ) -> ApiResult<pb::MirrorPage> {
        self.st().list_calls.push(tokio::time::Instant::now());
        self.up()?;
        let mirrors = self
            .st()
            .rows
            .iter()
            .filter(|r| enabled.is_none_or(|e| r.enabled == e))
            .cloned()
            .collect();
        Ok(pb::MirrorPage {
            mirrors,
            next_page_token: String::new(),
        })
    }

    async fn acquire_lease(&self, repo: &str, instance: &str) -> ApiResult<pb::Lease> {
        self.up()?;
        let refuse = self.refuse_acquires.load(Ordering::SeqCst);
        if refuse > 0 {
            self.refuse_acquires.store(refuse - 1, Ordering::SeqCst);
            return Err(ApiError::LeaseLost {
                repo: repo.to_owned(),
            });
        }
        let (prev, l) = {
            let mut st = self.st();
            let now = 1;
            let l = st.leases.entry(repo.to_owned()).or_insert(Lease {
                holder: None,
                epoch: 0,
                expires_at_ms: 0,
            });
            if l.holder.is_some() && l.expires_at_ms > now {
                return Err(ApiError::LeaseLost {
                    repo: repo.to_owned(),
                });
            }
            let prev = l.epoch;
            l.epoch += 1;
            l.holder = Some(instance.to_owned());
            l.expires_at_ms = i64::MAX;
            (prev, l.clone())
        };
        self.put_store_lease(prev, &l).await;
        Ok(pb::Lease {
            repo: repo.to_owned(),
            epoch: l.epoch,
            expires_at_ms: l.expires_at_ms,
        })
    }

    async fn renew_lease(&self, repo: &str, epoch: u64) -> ApiResult<pb::Lease> {
        self.up()?;
        self.held(repo, epoch)?;
        Ok(pb::Lease {
            repo: repo.to_owned(),
            epoch,
            expires_at_ms: i64::MAX,
        })
    }

    async fn release_lease(&self, repo: &str, epoch: u64) -> ApiResult<()> {
        self.up()?;
        self.held(repo, epoch)?;
        let l = {
            let mut st = self.st();
            let l = st.leases.get_mut(repo).unwrap();
            l.holder = None;
            l.clone()
        };
        self.put_store_lease(epoch, &l).await;
        Ok(())
    }

    async fn credential(
        &self,
        repo: &str,
        _upstream: &str,
        epoch: u64,
    ) -> ApiResult<Option<SecretToken>> {
        self.up()?;
        self.held(repo, epoch)?;
        Ok(self
            .st()
            .credentials
            .get(repo)
            .and_then(|c| SecretToken::new(c.clone())))
    }

    async fn set_status(
        &self,
        repo: &str,
        _upstream: &str,
        status: pb::MirrorStatus,
    ) -> ApiResult<()> {
        self.up()?;
        if let Err(e) = self.held(repo, status.lease_epoch) {
            self.st().stale_status += 1;
            return Err(e);
        }
        self.st().statuses.push(status);
        Ok(())
    }

    async fn claim_request(&self, repo: &str, epoch: u64) -> ApiResult<Option<pb::MirrorRequest>> {
        self.up()?;
        self.held(repo, epoch)?;
        let req = {
            let mut st = self.st();
            // A request claimed under an older epoch returns to pending.
            let stale: Vec<_> = st
                .claimed
                .iter()
                .filter(|r| r.claimed_by_epoch != Some(epoch))
                .cloned()
                .collect();
            st.claimed.retain(|r| r.claimed_by_epoch == Some(epoch));
            st.pending.extend(stale);
            let Some(mut req) = st.pending.pop_front() else {
                return Ok(None);
            };
            req.claimed_by_epoch = Some(epoch);
            st.claimed.push(req.clone());
            req
        };
        self.put_store_cut(&req, Some(epoch)).await;
        Ok(Some(req))
    }

    async fn complete_request(
        &self,
        repo: &str,
        id: u64,
        epoch: u64,
        result: pb::MirrorRequestOutcome,
        _done: bool,
    ) -> ApiResult<()> {
        if self.fail_next_complete.swap(false, Ordering::SeqCst) {
            // The process "crashed" before the result was recorded.
            return Err(ApiError::Unavailable { retry_after_ms: 0 });
        }
        self.held(repo, epoch)?;
        let ended = self.end_claim(id);
        self.st().completed.push((id, result));
        if let Some(req) = ended {
            self.put_store_cut(&req, None).await;
        }
        Ok(())
    }

    async fn fail_request(&self, repo: &str, id: u64, epoch: u64, code: &str) -> ApiResult<()> {
        self.held(repo, epoch)?;
        let ended = self.end_claim(id);
        self.st().failed.push((id, code.to_owned()));
        if let Some(req) = ended {
            self.put_store_cut(&req, None).await;
        }
        Ok(())
    }
}

fn row(repo: &str, upstream: &str, interval_s: u32) -> pb::Mirror {
    pb::Mirror {
        repo: repo.to_owned(),
        upstream: upstream.to_owned(),
        lines: vec![pb::MirrorLine {
            branch: "main".to_owned(),
            line: "main".to_owned(),
            direction: "follow".to_owned(),
        }],
        interval_s,
        enabled: true,
        version: 1,
        ..Default::default()
    }
}

// ---- a fake poll ------------------------------------------------------------

/// What fake polls do, shared with the test.
#[derive(Default)]
struct Script {
    /// Every poll started: the row version and the virtual instant.
    started: Mutex<Vec<(u64, tokio::time::Instant)>>,
    /// Polls that ran to the end.
    finished: AtomicUsize,
    /// While set, a poll waits on `gate` before finishing.
    hold: AtomicBool,
    gate: tokio::sync::Notify,
    entered: tokio::sync::Notify,
    /// The failure every poll returns, if any.
    fail: Mutex<Option<PollFailure>>,
    /// The sha a successful poll reports.
    sha: Mutex<String>,
}

struct FakeStep {
    version: u64,
    script: Arc<Script>,
    /// Stands in for the follower: writes the repository's lag series.
    series: Option<(String, ServiceMetrics)>,
}

impl PollStep for FakeStep {
    async fn poll(&mut self, _credential: Option<SecretToken>) -> Result<PollReport, PollFailure> {
        self.script
            .started
            .lock()
            .unwrap()
            .push((self.version, tokio::time::Instant::now()));
        if self.script.hold.load(Ordering::SeqCst) {
            self.script.entered.notify_one();
            self.script.gate.notified().await;
        }
        self.script.finished.fetch_add(1, Ordering::SeqCst);
        if let Some((repo, m)) = &self.series {
            let l = [repo.as_str(), "main"];
            m.base().mirror_lag_seconds.with_label_values(&l).set(1.0);
            m.base().mirror_lag_changesets.with_label_values(&l).set(0);
            m.base().mirror_divergence.with_label_values(&l).set(0);
        }
        if let Some(f) = self.script.fail.lock().unwrap().clone() {
            return Err(f);
        }
        Ok(PollReport {
            imported_sha: Some(self.script.sha.lock().unwrap().clone()),
            requests: 1,
            ..PollReport::default()
        })
    }
}

struct FakeSpawner(Arc<Script>);

impl Spawner<Fake> for FakeSpawner {
    fn spawn(&self, task: TaskCtx<Fake>) {
        let mut step = FakeStep {
            version: task.row.version,
            script: self.0.clone(),
            series: task.metrics.clone().map(|m| (task.row.repo.clone(), m)),
        };
        tokio::spawn(async move { task.run(&mut step).await });
    }
}

fn config(dir: &Path) -> ServeConfig {
    let mut c = ServeConfig::new(dir);
    c.instance = "a".to_owned();
    c
}

/// A resolver that says every host is a public GitHub address.
fn public() -> netguard::Resolve {
    Arc::new(|_, port| {
        Box::pin(async move { Ok(vec![std::net::SocketAddr::from(([140, 82, 112, 3], port))]) })
    })
}

fn fake_service(fake: &Arc<Fake>, script: &Arc<Script>, dir: &Path) -> Service<Fake, FakeSpawner> {
    Service::with_resolver(
        fake.clone(),
        FakeSpawner(script.clone()),
        config(dir),
        public(),
    )
}

/// Lets spawned tasks run (virtual time does not move).
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

const GH: &str = "https://github.com/o/r.git";

// ---- metrics ---------------------------------------------------------------

/// A service exporting metrics into a fresh registry.
fn metered_service(
    fake: &Arc<Fake>,
    script: &Arc<Script>,
    dir: &Path,
) -> (
    Service<Fake, FakeSpawner>,
    ServiceMetrics,
    prometheus::Registry,
) {
    let (registry, base) = graft_telemetry::registry().unwrap();
    let metrics = ServiceMetrics::register(&registry, base).unwrap();
    let service = fake_service(fake, script, dir).with_metrics(metrics.clone());
    (service, metrics, registry)
}

/// The mirror series in the `/metrics` text that name `repo`, comments and
/// the repository-less series left out.
fn series_of(registry: &prometheus::Registry, repo: &str) -> Vec<String> {
    let needle = format!("repo=\"{repo}\"");
    graft_telemetry::encode(registry)
        .lines()
        .filter(|l| l.starts_with("graft_mirror_") && l.contains(&needle))
        .map(str::to_owned)
        .collect()
}

fn value_of(registry: &prometheus::Registry, name: &str) -> Option<String> {
    graft_telemetry::encode(registry)
        .lines()
        .find(|l| l.starts_with(name) && !l.starts_with('#'))
        .and_then(|l| l.rsplit(' ').next().map(str::to_owned))
}

#[tokio::test(start_paused = true)]
async fn every_mirror_series_carries_repo_and_exists_only_for_running_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let (mut service, _m, registry) = metered_service(&fake, &script, tmp.path());
    fake.set_row(row("r", GH, 15));
    fake.set_row(row("s", GH, 15));
    service.reconcile().await.unwrap();
    settle().await;

    // No mirror series without a repo label, but the service-wide ones.
    let text = graft_telemetry::encode(&registry);
    for l in text.lines().filter(|l| l.starts_with("graft_mirror_")) {
        assert!(
            l.contains("repo=")
                || l.starts_with("graft_mirror_tasks ")
                || l.starts_with("graft_mirror_lease_lost_total "),
            "{l}"
        );
    }
    let r = series_of(&registry, "r");
    for name in [
        "graft_mirror_lag_seconds",
        "graft_mirror_lag_changesets",
        "graft_mirror_divergence",
        "graft_mirror_last_success_timestamp_seconds",
    ] {
        assert!(r.iter().any(|l| l.starts_with(name)), "{name}: {r:?}");
    }
    assert_eq!(series_of(&registry, "s").len(), r.len());
    assert_eq!(
        value_of(&registry, "graft_mirror_tasks").as_deref(),
        Some("2")
    );

    // Stopping r's task removes all of r's series and none of s's.
    fake.remove("r");
    service.reconcile().await.unwrap();
    assert_eq!(
        service.next_exit().await.unwrap().reason,
        ExitReason::Stopped
    );
    assert_eq!(series_of(&registry, "r"), Vec::<String>::new());
    assert_eq!(series_of(&registry, "s").len(), r.len());
    assert_eq!(
        value_of(&registry, "graft_mirror_tasks").as_deref(),
        Some("1")
    );

    // Stopping the rest leaves nothing: the cardinality is the running tasks.
    service.stop_all().await;
    assert_eq!(series_of(&registry, "s"), Vec::<String>::new());
    assert_eq!(
        value_of(&registry, "graft_mirror_tasks").as_deref(),
        Some("0")
    );
}

#[tokio::test(start_paused = true)]
async fn a_restart_for_a_changed_row_leaves_no_series_of_the_old_row() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let (mut service, m, registry) = metered_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    settle().await;
    // A line the new row will not have.
    m.base()
        .mirror_divergence
        .with_label_values(&[REPO, "gone"])
        .set(1);
    assert!(
        series_of(&registry, REPO)
            .iter()
            .any(|l| l.contains("line=\"gone\""))
    );

    // The row changes and the task restarts; the old row's line is dropped
    // only if its row said so, so the row names it.
    let mut old = row(REPO, GH, 15);
    old.lines.push(pb::MirrorLine {
        branch: "gone".to_owned(),
        line: "gone".to_owned(),
        direction: "follow".to_owned(),
    });
    old.version = 2;
    fake.set_row(old);
    service.reconcile().await.unwrap();
    service.next_exit().await.unwrap();
    settle().await;
    let mut new = row(REPO, GH, 15);
    new.version = 3;
    fake.set_row(new);
    service.reconcile().await.unwrap();
    service.next_exit().await.unwrap();
    settle().await;
    assert!(
        !series_of(&registry, REPO)
            .iter()
            .any(|l| l.contains("line=\"gone\"")),
        "{:?}",
        series_of(&registry, REPO)
    );
}

#[tokio::test(start_paused = true)]
async fn an_error_is_one_series_until_the_poll_succeeds_and_goes_with_the_task() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    *script.fail.lock().unwrap() = Some(PollFailure::new(
        MirrorErrorCode::AuthFailed,
        "the credential was refused",
    ));
    let (mut service, _m, registry) = metered_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    settle().await;
    let errors = |registry: &prometheus::Registry| -> Vec<String> {
        series_of(registry, REPO)
            .into_iter()
            .filter(|l| l.starts_with("graft_mirror_poll_error"))
            .collect()
    };
    let e = errors(&registry);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains("code=\"auth_failed\"") && e[0].ends_with(" 1"));

    // A different failure replaces it rather than adding a series.
    *script.fail.lock().unwrap() = Some(PollFailure::new(
        MirrorErrorCode::Timeout,
        "the poll did not finish in time",
    ));
    tokio::time::sleep(Duration::from_secs(60)).await;
    let e = errors(&registry);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains("code=\"timeout\""));

    // Success clears it.
    *script.fail.lock().unwrap() = None;
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(errors(&registry), Vec::<String>::new());

    *script.fail.lock().unwrap() = Some(PollFailure::new(MirrorErrorCode::AuthFailed, "no"));
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(errors(&registry).len(), 1);
    service.stop_all().await;
    assert_eq!(series_of(&registry, REPO), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn a_lost_lease_is_counted_and_drops_the_series() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let (mut service, _m, registry) = metered_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    settle().await;
    assert!(!series_of(&registry, REPO).is_empty());

    fake.steal(REPO).await;
    service.renew().await;
    assert_eq!(
        service.next_exit().await.unwrap().reason,
        ExitReason::LeaseLost
    );
    assert_eq!(series_of(&registry, REPO), Vec::<String>::new());
    assert_eq!(
        value_of(&registry, "graft_mirror_lease_lost_total").as_deref(),
        Some("1")
    );
}

// ---- reconcile ---------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_row_added_through_the_api_starts_within_one_reconcile() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let mut service = fake_service(&fake, &script, tmp.path());
    service.reconcile().await.unwrap();
    assert!(service.running().is_empty());

    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    assert_eq!(service.running(), vec![(REPO.to_owned(), 1, 1)]);
    settle().await;
    assert_eq!(script.started.lock().unwrap().len(), 1);
    // Its directory was created with the migration marker; the lease is
    // held under the task's epoch and every status carries it.
    assert!(tmp.path().join(REPO).join(MIGRATION_MARKER).is_file());
    assert_eq!(fake.lease(REPO).unwrap().holder.as_deref(), Some("a"));
    let statuses = fake.statuses();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].lease_epoch, 1);
    assert_eq!(statuses[0].error_code, MirrorErrorCode::Unspecified as i32);
}

#[tokio::test(start_paused = true)]
async fn a_version_change_restarts_the_task_after_its_in_flight_poll() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    script.hold.store(true, Ordering::SeqCst);
    service.reconcile().await.unwrap();
    script.entered.notified().await;

    // The row changes while a poll is in flight: the task is told to stop,
    // but the poll is not cancelled.
    fake.bump(REPO);
    service.reconcile().await.unwrap();
    settle().await;
    assert_eq!(service.running(), vec![(REPO.to_owned(), 1, 1)]);
    assert_eq!(script.finished.load(Ordering::SeqCst), 0);

    script.hold.store(false, Ordering::SeqCst);
    script.gate.notify_one();
    let exit = service.next_exit().await.unwrap();
    assert_eq!(exit.reason, ExitReason::Stopped);
    assert_eq!(
        script.finished.load(Ordering::SeqCst),
        1,
        "the poll finished"
    );
    // Respawned from the new row under a fresh lease.
    assert_eq!(service.running(), vec![(REPO.to_owned(), 2, 2)]);
    settle().await;
    let versions: Vec<u64> = script.started.lock().unwrap().iter().map(|s| s.0).collect();
    assert_eq!(versions, [1, 2]);
}

#[tokio::test(start_paused = true)]
async fn a_removed_row_stops_its_task_releases_the_lease_and_keeps_the_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    settle().await;

    fake.remove(REPO);
    service.reconcile().await.unwrap();
    let exit = service.next_exit().await.unwrap();
    assert_eq!(exit.reason, ExitReason::Stopped);
    assert!(service.running().is_empty());
    assert_eq!(fake.lease(REPO).unwrap().holder, None, "released");
    assert!(tmp.path().join(REPO).join(MIGRATION_MARKER).is_file());
}

#[tokio::test(start_paused = true)]
async fn a_directory_without_the_migration_marker_is_not_adopted() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(REPO)).unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    settle().await;
    assert!(service.running().is_empty());
    assert!(script.started.lock().unwrap().is_empty());
    assert_eq!(
        fake.lease(REPO).unwrap().holder,
        None,
        "the lease is let go"
    );
    let st = fake.statuses();
    assert_eq!(st.len(), 1);
    assert!(
        st[0]
            .error_message
            .as_deref()
            .unwrap()
            .contains("migration marker")
    );
}

/// Writes the marker `import-config` leaves in `<data>/<repo>`.
fn migrated_dir(data: &Path, repo: &str, named: &str) {
    let dir = data.join(repo);
    std::fs::create_dir_all(dir.join("mirror.git")).unwrap();
    std::fs::write(
        dir.join(MIGRATION_MARKER),
        graft_mirror::migrate::marker_text("import-config", named),
    )
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_directory_copied_under_another_repository_is_not_adopted() {
    let tmp = tempfile::tempdir().unwrap();
    migrated_dir(tmp.path(), REPO, "elsewhere");
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    settle().await;
    assert!(service.running().is_empty());
    assert!(script.started.lock().unwrap().is_empty());
    assert_eq!(
        fake.lease(REPO).unwrap().holder,
        None,
        "the lease is let go"
    );
    let st = fake.statuses();
    assert!(
        st[0]
            .error_message
            .as_deref()
            .unwrap()
            .contains("another repository")
    );
}

#[tokio::test(start_paused = true)]
async fn a_copied_migrated_directory_runs_in_one_process_only() {
    // The migrated directory is copied into a second service's volume: both
    // adopt it, but the repository's lease lets only one of them run it.
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    migrated_dir(first.path(), REPO, REPO);
    migrated_dir(second.path(), REPO, REPO);
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    fake.set_row(row(REPO, GH, 15));
    let mut a = fake_service(&fake, &script, first.path());
    let mut b_config = config(second.path());
    b_config.instance = "b".to_owned();
    let mut b = Service::with_resolver(
        fake.clone(),
        FakeSpawner(script.clone()),
        b_config,
        public(),
    );
    a.reconcile().await.unwrap();
    b.reconcile().await.unwrap();
    settle().await;
    assert_eq!(a.running(), vec![(REPO.to_owned(), 1, 1)]);
    assert!(b.running().is_empty(), "the copy is fenced by the lease");
    assert_eq!(fake.lease(REPO).unwrap().holder.as_deref(), Some("a"));
    let versions: Vec<u64> = script.started.lock().unwrap().iter().map(|s| s.0).collect();
    assert_eq!(versions, [1], "one poll, from the lease holder");
}

// ---- leases ----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_lost_lease_stops_the_task_and_it_writes_no_more() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    settle().await;
    let written = fake.statuses().len();

    fake.steal(REPO).await;
    service.renew().await;
    let exit = service.next_exit().await.unwrap();
    assert_eq!(exit.reason, ExitReason::LeaseLost);
    assert!(service.running().is_empty());
    // Nothing more was written, and nothing was released: the lease is the
    // other holder's.
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(fake.statuses().len(), written);
    assert_eq!(fake.st().stale_status, 0);
    assert_eq!(fake.lease(REPO).unwrap().holder.as_deref(), Some("other"));
}

#[tokio::test(start_paused = true)]
async fn a_fenced_write_refused_mid_run_stops_the_task() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    script.hold.store(true, Ordering::SeqCst);
    service.reconcile().await.unwrap();
    script.entered.notified().await;
    // Lost while polling, before any renewal noticed: the status write
    // after the poll is refused, and the task stops on it.
    fake.steal(REPO).await;
    script.hold.store(false, Ordering::SeqCst);
    script.gate.notify_one();
    let exit = service.next_exit().await.unwrap();
    assert_eq!(exit.reason, ExitReason::LeaseLost);
    assert_eq!(fake.st().stale_status, 1);
    assert!(fake.statuses().is_empty());
}

// ---- back-off ----------------------------------------------------------------

fn gaps(at: &[tokio::time::Instant]) -> Vec<u64> {
    at.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect()
}

#[tokio::test(start_paused = true)]
async fn the_reconcile_loop_backs_off_while_the_server_is_down_capped_at_a_minute() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.server_down.store(true, Ordering::SeqCst);
    let script = Arc::new(Script::default());
    let service = fake_service(&fake, &script, tmp.path());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let run = tokio::spawn(service.run(async move {
        let _ = stop_rx.await;
    }));
    tokio::time::sleep(Duration::from_secs(250)).await;
    let calls = fake.st().list_calls.clone();
    assert_eq!(gaps(&calls), [1, 2, 4, 8, 16, 32, 60, 60, 60]);

    // The server is back: the next reconcile starts the row, and the loop
    // returns to its 15 s cadence.
    fake.server_down.store(false, Ordering::SeqCst);
    fake.set_row(row(REPO, GH, 15));
    tokio::time::sleep(Duration::from_secs(61)).await;
    let n = fake.st().list_calls.len();
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(fake.st().list_calls.len() - n, 4);
    assert!(!script.started.lock().unwrap().is_empty());
    stop_tx.send(()).unwrap();
    run.await.unwrap();
    assert_eq!(fake.lease(REPO).unwrap().holder, None, "released on stop");
}

#[tokio::test(start_paused = true)]
async fn a_failing_repository_backs_off_exponentially_and_reports_it() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    *script.fail.lock().unwrap() = Some(PollFailure::new(
        MirrorErrorCode::UpstreamUnreachable,
        "the upstream could not be reached",
    ));
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    tokio::time::sleep(Duration::from_secs(15 + 30 + 60 + 120 + 1)).await;
    let at: Vec<_> = script.started.lock().unwrap().iter().map(|s| s.1).collect();
    assert_eq!(gaps(&at), [15, 30, 60, 120]);
    let last = fake.statuses().pop().unwrap();
    assert_eq!(last.error_code, MirrorErrorCode::UpstreamUnreachable as i32);
    assert!(last.backoff_until_ms.is_some());

    // It recovers: back to the row's interval.
    *script.fail.lock().unwrap() = None;
    tokio::time::sleep(Duration::from_secs(240 + 31)).await;
    let at: Vec<_> = script.started.lock().unwrap().iter().map(|s| s.1).collect();
    assert_eq!(gaps(&at)[4..], [240, 15, 15]);
    assert_eq!(
        fake.statuses().pop().unwrap().error_code,
        MirrorErrorCode::Unspecified as i32
    );
}

#[tokio::test(start_paused = true)]
async fn retry_after_is_honoured() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    *script.fail.lock().unwrap() = Some(PollFailure {
        retry_after: Some(Duration::from_secs(300)),
        ..PollFailure::new(MirrorErrorCode::RateLimited, "rate limited")
    });
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    tokio::time::sleep(Duration::from_secs(301)).await;
    let at: Vec<_> = script.started.lock().unwrap().iter().map(|s| s.1).collect();
    assert_eq!(gaps(&at), [300]);
}

// ---- status ----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn status_is_written_once_per_change_and_once_a_minute_otherwise() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    *script.sha.lock().unwrap() = "a".repeat(40);
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    // Thirteen polls over three minutes, nothing changing.
    tokio::time::sleep(Duration::from_secs(180) + Duration::from_millis(1)).await;
    assert_eq!(script.started.lock().unwrap().len(), 13);
    assert_eq!(fake.statuses().len(), 4, "t = 0, 60, 120 and 180 s");

    // A change is written at the next poll.
    *script.sha.lock().unwrap() = "b".repeat(40);
    tokio::time::sleep(Duration::from_secs(15)).await;
    let st = fake.statuses();
    assert_eq!(st.len(), 5);
    assert_eq!(
        st[4].last_imported_sha.as_deref(),
        Some("b".repeat(40).as_str())
    );
}

#[tokio::test(start_paused = true)]
async fn an_upstream_resolving_to_a_blocked_address_is_refused_before_any_fetch() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let rebinding: netguard::Resolve = Arc::new(|_, port| {
        Box::pin(async move { Ok(vec![std::net::SocketAddr::from(([10, 0, 0, 5], port))]) })
    });
    let mut service = Service::with_resolver(
        fake.clone(),
        FakeSpawner(script.clone()),
        config(tmp.path()),
        rebinding,
    );
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    settle().await;
    assert!(script.started.lock().unwrap().is_empty(), "never polled");
    let st = fake.statuses();
    assert_eq!(st[0].error_code, MirrorErrorCode::UrlBlocked as i32);
}

// ---- requests ----------------------------------------------------------------

#[derive(Default)]
struct Desk {
    pulls: Mutex<Vec<Pull>>,
    created: AtomicUsize,
}

impl PullDesk for Desk {
    async fn find_open(&self, branch: &str) -> Result<Option<Pull>, ForgeError> {
        Ok(self
            .pulls
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.open && p.head_ref == branch)
            .cloned())
    }

    async fn create(&self, new: &NewPull) -> Result<Pull, ForgeError> {
        let n = self.created.fetch_add(1, Ordering::SeqCst) as u64 + 1;
        let pull = Pull {
            number: n,
            url: format!("https://github.com/o/r/pull/{n}"),
            node_id: format!("PR_{n}"),
            open: true,
            merged: false,
            merge_commit: None,
            head_sha: "0".repeat(40),
            head_ref: new.head.clone(),
            body: new.body.clone(),
            settled_ms: None,
        };
        self.pulls.lock().unwrap().push(pull.clone());
        Ok(pull)
    }
}

#[derive(Default)]
struct Runner {
    prepared: AtomicUsize,
}

impl RequestRunner for Runner {
    async fn cut_branch(&self, req: &pb::MirrorRequest) -> Result<Option<String>, RequestFailure> {
        Ok((self.prepared.load(Ordering::SeqCst) > 0).then(|| format!("graft/batch-{}", req.id)))
    }

    async fn prepare_batch(&self, req: &pb::MirrorRequest) -> Result<NewPull, RequestFailure> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        Ok(NewPull {
            title: "batch".to_owned(),
            body: String::new(),
            head: format!("graft/batch-{}", req.id),
            base: "main".to_owned(),
        })
    }

    async fn run(
        &self,
        _req: &pb::MirrorRequest,
    ) -> Result<pb::MirrorRequestOutcome, RequestFailure> {
        Err(requests::UNSUPPORTED)
    }
}

fn request(id: u64, kind: MirrorRequestKind) -> pb::MirrorRequest {
    pb::MirrorRequest {
        repo: REPO.to_owned(),
        id,
        line: "main".to_owned(),
        kind: kind as i32,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_crash_between_opening_the_pr_and_recording_it_adopts_the_pr() {
    let fake = Fake::default();
    let epoch = fake.acquire_lease(REPO, "a").await.unwrap().epoch;
    fake.st()
        .pending
        .push_back(request(7, MirrorRequestKind::CutBatch));
    let (desk, runner) = (Desk::default(), Runner::default());

    // The PR is opened, then the process dies before recording it.
    fake.fail_next_complete.store(true, Ordering::SeqCst);
    let err = requests::run_turn(&fake, &desk, &runner, REPO, epoch).await;
    assert!(err.is_err());
    assert_eq!(desk.created.load(Ordering::SeqCst), 1);
    assert!(fake.st().completed.is_empty());

    // The next process (a new epoch) claims it again and adopts that PR.
    fake.lapse(REPO);
    let epoch = fake.acquire_lease(REPO, "b").await.unwrap().epoch;
    let turn = requests::run_turn(&fake, &desk, &runner, REPO, epoch)
        .await
        .unwrap();
    assert_eq!(turn.finished, 1);
    assert_eq!(desk.created.load(Ordering::SeqCst), 1, "no second PR");
    assert_eq!(
        runner.prepared.load(Ordering::SeqCst),
        1,
        "the branch was not rebuilt"
    );
    let st = fake.st();
    assert_eq!(st.completed.len(), 1);
    let (id, outcome) = &st.completed[0];
    assert_eq!(*id, 7);
    assert_eq!(outcome.pr_number, Some(1));
    assert_eq!(outcome.branch.as_deref(), Some("graft/batch-7"));
}

#[tokio::test]
async fn unwired_kinds_fail_typed_and_a_purge_deletes_only_an_idle_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Fake::default();
    let epoch = fake.acquire_lease(REPO, "a").await.unwrap().epoch;
    fake.st()
        .pending
        .push_back(request(1, MirrorRequestKind::Rewind));
    fake.st()
        .pending
        .push_back(request(2, MirrorRequestKind::Purge));
    let (desk, runner) = (Desk::default(), Runner::default());
    requests::run_turn(&fake, &desk, &runner, REPO, epoch)
        .await
        .unwrap();
    assert_eq!(
        fake.st().failed,
        [(1, "unsupported".to_owned()), (2, "bad_request".to_owned())]
    );

    let dir = tmp.path().join(REPO);
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join(MIGRATION_MARKER), b"").unwrap();
    fake.st()
        .pending
        .push_back(request(3, MirrorRequestKind::Purge));
    assert!(requests::purge(&fake, REPO, epoch, &dir).await.unwrap());
    assert!(!dir.exists());
    assert_eq!(fake.st().completed.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_sweep_purges_a_removed_mirrors_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap();
    fake.remove(REPO);
    service.reconcile().await.unwrap();
    service.next_exit().await.unwrap();
    fake.st()
        .pending
        .push_back(request(9, MirrorRequestKind::Purge));
    // The sweep runs every fourth reconcile.
    for _ in 0..2 {
        service.reconcile().await.unwrap();
    }
    assert!(!tmp.path().join(REPO).exists());
    assert_eq!(fake.st().completed[0].0, 9);
    assert_eq!(fake.lease(REPO).unwrap().holder, None);
}

// ---- the real follower: fencing and credentials ---------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A bare upstream at `root/up.git`, with a work tree at `root/work`.
fn upstream(root: &Path) -> (PathBuf, PathBuf) {
    let bare = root.join("up.git");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(root, &["init", "--bare", "up.git"]);
    git(&work, &["init"]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    (bare, work)
}

fn commit(work: &Path, file: &str) -> String {
    std::fs::write(work.join(file), file).unwrap();
    git(work, &["add", "."]);
    git(work, &["commit", "-m", file]);
    git(work, &["push", "origin", "HEAD:main"]);
    git(work, &["rev-parse", "HEAD"])
}

fn engine() -> (Arc<Api>, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let api = Engine::new(
        store.clone(),
        store.clone(),
        Arc::new(FakeClock::at(ENGINE_NOW)),
        EngineConfig::default(),
    );
    (Arc::new(api), store)
}

fn mirror_cx(repo: &str) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "graft-mirror".to_owned(),
            kind: PrincipalKind::Mirror,
            session: None,
            work_item: None,
            // In-process: the followed marker is admin-only, and the
            // engine reads scopes from here rather than a token.
            scopes: vec![format!("repo:{repo}:mirror"), format!("repo:{repo}:admin")],
        },
        request_id: "test".to_owned(),
        deadline_ms: None,
    }
}

fn local_config(dir: &Path, instance: &str) -> ServeConfig {
    let mut c = ServeConfig::new(dir);
    c.instance = instance.to_owned();
    c.allow_local_upstreams = true;
    c.min_interval = Duration::from_millis(100);
    c
}

/// Waits, bounded, for `f` to hold; the bound only catches a hang.
async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..600 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("condition never held");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_process_on_the_same_data_is_fenced_by_the_lease_epoch() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, work) = upstream(tmp.path());
    let c1 = commit(&work, "a");
    let (api, store) = engine();
    api.create_repo(&mirror_cx(REPO), REPO.to_owned())
        .await
        .unwrap();
    let fake = Arc::new(Fake::with_store(store));
    fake.set_row(row(REPO, bare.to_str().unwrap(), 0));
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();

    let mut a = Service::new(
        fake.clone(),
        FollowSpawner::new(api.clone()).with_cx(mirror_cx),
        local_config(&data, "a"),
    );
    a.reconcile().await.unwrap();
    eventually(|| {
        fake.statuses()
            .iter()
            .any(|s| s.last_imported_sha.as_deref() == Some(c1.as_str()))
    })
    .await;
    let tip1 = current_ref(&*api, &mirror_cx(REPO), REPO, "main")
        .await
        .unwrap()
        .expect("followed");

    // A second process over the same data cannot start while A holds it.
    let mut b = Service::new(
        fake.clone(),
        FollowSpawner::new(api.clone()).with_cx(mirror_cx),
        local_config(&data, "b"),
    );
    b.reconcile().await.unwrap();
    assert!(b.running().is_empty());

    // A stalls and its lease lapses; B takes it under a new epoch.
    fake.lapse(REPO);
    b.reconcile().await.unwrap();
    assert_eq!(b.running(), vec![(REPO.to_owned(), 1, 2)]);
    // The engine refuses a line move under A's epoch now.
    let stale = api
        .update_ref(
            &mirror_cx(REPO),
            UpdateRef {
                repo: REPO.to_owned(),
                name: "main".to_owned(),
                expected: Some(tip1),
                new: tip1,
                kind: RefKind::Line,
                lease_epoch: Some(1),
            },
        )
        .await;
    assert!(
        matches!(stale, Err(ApiError::LeaseLost { .. })),
        "{stale:?}"
    );

    // A notices at its next fenced call and stops without writing.
    let exit = a.next_exit().await.unwrap();
    assert_eq!(exit.reason, ExitReason::LeaseLost);
    let c2 = commit(&work, "b");
    eventually(|| {
        fake.statuses()
            .iter()
            .any(|s| s.last_imported_sha.as_deref() == Some(c2.as_str()))
    })
    .await;
    {
        let st = fake.st();
        assert_eq!(st.stale_status, 0, "A wrote nothing under its old epoch");
        assert!(
            st.statuses
                .iter()
                .filter(|s| s.last_imported_sha.as_deref() == Some(c2.as_str()))
                .all(|s| s.lease_epoch == 2)
        );
    }
    b.stop_all().await;
}

/// A tracing writer into a shared buffer.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forced_auth_failure_leaves_no_credential_in_logs_or_status() {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // An upstream that refuses every credential.
    let forge = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(401).insert_header("WWW-Authenticate", "Basic realm=\"x\""),
        )
        .mount(&forge)
        .await;
    let secret = "ghp_serveSECRETvalue42";
    let tmp = tempfile::tempdir().unwrap();
    let (api, store) = engine();
    api.create_repo(&mirror_cx(REPO), REPO.to_owned())
        .await
        .unwrap();
    let fake = Arc::new(Fake::with_store(store));
    let upstream = format!("{}/o/r.git", forge.uri());
    fake.set_row(row(REPO, &upstream, 0));
    fake.st()
        .credentials
        .insert(REPO.to_owned(), secret.to_owned());

    let mut service = Service::new(
        fake.clone(),
        FollowSpawner::new(api.clone()).with_cx(mirror_cx),
        local_config(tmp.path(), "a"),
    );
    service.reconcile().await.unwrap();
    eventually(|| !fake.statuses().is_empty()).await;
    service.stop_all().await;

    // git asked, through the pipe, and sent the credential...
    let basic = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{secret}"))
    };
    let sent = forge.received_requests().await.unwrap().iter().any(|r| {
        r.headers
            .get("authorization")
            .is_some_and(|v| v.to_str().unwrap_or("").contains(&basic))
    });
    assert!(sent, "the credential reached git through the helper");
    // ...and the failure is typed, with no trace of it.
    let st = fake.statuses();
    assert_eq!(st[0].error_code, MirrorErrorCode::AuthFailed as i32);
    let status_text = format!("{st:?}");
    assert!(!status_text.contains(secret));
    assert!(!status_text.contains(&basic));
    let log = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("mirror"), "logs were captured");
    assert!(!log.contains(secret));
    assert!(!log.contains(&basic));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_lost_at_the_engine_mid_poll_fences_the_followed_move_and_stops_the_task() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, work) = upstream(tmp.path());
    let c1 = commit(&work, "a");
    let (api, store) = engine();
    api.create_repo(&mirror_cx(REPO), REPO.to_owned())
        .await
        .unwrap();
    let fake = Arc::new(Fake::with_store(store.clone()));
    fake.set_row(row(REPO, bare.to_str().unwrap(), 0));
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let mut a = Service::new(
        fake.clone(),
        FollowSpawner::new(api.clone()).with_cx(mirror_cx),
        local_config(&data, "a"),
    );
    a.reconcile().await.unwrap();
    eventually(|| {
        fake.statuses()
            .iter()
            .any(|s| s.last_imported_sha.as_deref() == Some(c1.as_str()))
    })
    .await;
    let tip1 = current_ref(&*api, &mirror_cx(REPO), REPO, "main")
        .await
        .unwrap()
        .expect("followed");

    // The engine's lease row moves on (another holder, epoch 2) while the
    // control plane still answers A as the holder: only the engine's fence
    // on the ref move can refuse A now.
    let id = store.repo(REPO).await.unwrap().unwrap();
    let txn = MetaTxn {
        preconditions: vec![Precondition::LeaseEpochIs { epoch: 1 }],
        writes: vec![Write::PutLease(LeaseRecord {
            holder: Some("other".to_owned()),
            epoch: 2,
            expires_at_ms: ENGINE_NOW + 3_600_000,
        })],
    };
    assert!(matches!(
        store.commit(id, txn).await,
        Ok(TxnOutcome::Committed(_))
    ));
    let written = fake.statuses().len();
    commit(&work, "b");

    // A's next poll imports the new commit and tries to move the line: the
    // engine refuses it, and the task stops on that. The bound only
    // catches a hang.
    let exit = tokio::time::timeout(Duration::from_secs(60), a.next_exit())
        .await
        .expect("the task stops")
        .unwrap();
    assert_eq!(exit.reason, ExitReason::LeaseLost);
    assert!(a.running().is_empty());
    let tip = current_ref(&*api, &mirror_cx(REPO), REPO, "main")
        .await
        .unwrap();
    assert_eq!(tip, Some(tip1), "the fenced move did not land");
    assert_eq!(
        fake.statuses().len(),
        written,
        "no status after the fenced move"
    );
}

/// §3.2: a purge applies only to a disabled or removed row. A purge
/// recorded while the row was off, then the row turned back on before the
/// purge ran, deletes nothing: the sweep skips a repository with a row,
/// and a running task refuses the purge, typed.
#[tokio::test(start_paused = true)]
async fn a_purge_recorded_before_a_re_enable_deletes_nothing() {
    for claim_requests in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let script = Arc::new(Script::default());
        let mut c = config(tmp.path());
        c.claim_requests = claim_requests;
        let mut service =
            Service::with_resolver(fake.clone(), FakeSpawner(script.clone()), c, public());
        fake.set_row(row(REPO, GH, 15));
        service.reconcile().await.unwrap();
        settle().await;
        // Disabled: the task stops and the purge is recorded.
        let mut off = row(REPO, GH, 15);
        off.enabled = false;
        off.version = 2;
        fake.set_row(off);
        service.reconcile().await.unwrap();
        assert_eq!(
            service.next_exit().await.unwrap().reason,
            ExitReason::Stopped
        );
        fake.st()
            .pending
            .push_back(request(11, MirrorRequestKind::Purge));
        // Re-enabled before any sweep ran the purge.
        let mut on = row(REPO, GH, 15);
        on.version = 3;
        fake.set_row(on);
        // Past several sweeps (every fourth reconcile) and task turns.
        for _ in 0..8 {
            service.reconcile().await.unwrap();
            settle().await;
            tokio::time::sleep(Duration::from_secs(16)).await;
            settle().await;
        }
        let dir = tmp.path().join(REPO);
        assert!(
            dir.join(MIGRATION_MARKER).is_file(),
            "the re-enabled row's data is kept (claims {claim_requests})"
        );
        assert_eq!(service.running(), vec![(REPO.to_owned(), 3, 2)]);
        let st = fake.st();
        assert!(st.completed.is_empty(), "the purge never completed");
        if claim_requests {
            assert_eq!(st.failed, [(11, "bad_request".to_owned())]);
        } else {
            assert!(st.failed.is_empty());
            assert_eq!(st.pending.len(), 1, "left for a later disable");
        }
    }
}

/// The same race, where the re-enabled row's task has not started yet (its
/// lease was briefly taken) when the sweep runs: the row alone keeps the
/// sweep from purging it.
#[tokio::test(start_paused = true)]
async fn a_sweep_never_purges_a_re_enabled_row_whose_task_has_not_started() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let script = Arc::new(Script::default());
    let mut service = fake_service(&fake, &script, tmp.path());
    fake.set_row(row(REPO, GH, 15));
    service.reconcile().await.unwrap(); // 1: started
    let mut off = row(REPO, GH, 15);
    off.enabled = false;
    off.version = 2;
    fake.set_row(off);
    service.reconcile().await.unwrap(); // 2: stopped
    assert_eq!(
        service.next_exit().await.unwrap().reason,
        ExitReason::Stopped
    );
    fake.st()
        .pending
        .push_back(request(12, MirrorRequestKind::Purge));
    let mut on = row(REPO, GH, 15);
    on.version = 3;
    fake.set_row(on);
    // Reconciles 3 and 4 cannot start the task; 4 runs the sweep, whose own
    // lease acquisition succeeds.
    fake.refuse_acquires.store(2, Ordering::SeqCst);
    service.reconcile().await.unwrap();
    service.reconcile().await.unwrap();
    assert!(service.running().is_empty(), "the task had not started");
    assert_eq!(fake.refuse_acquires.load(Ordering::SeqCst), 0);
    assert!(tmp.path().join(REPO).join(MIGRATION_MARKER).is_file());
    {
        let st = fake.st();
        assert!(st.completed.is_empty() && st.failed.is_empty());
        assert_eq!(st.pending.len(), 1);
    }
    // The next reconcile starts it, over the kept data.
    service.reconcile().await.unwrap();
    assert_eq!(service.running(), vec![(REPO.to_owned(), 3, 2)]);
}

// ---- the spawner's batch cut (D41 §3.5) ------------------------------------

/// PRs in memory, for a forge the spawner builds over a local upstream.
#[derive(Default)]
struct PrBook {
    /// Forges the spawner built.
    built: AtomicUsize,
    pulls: Mutex<Vec<Pull>>,
}

#[derive(Clone)]
struct LocalForge(Arc<PrBook>);

impl graft_mirror::github::Forge for LocalForge {
    async fn find_pull(&self, branch: &str) -> Result<Option<Pull>, ForgeError> {
        let pulls = self.0.pulls.lock().unwrap();
        Ok(pulls.iter().find(|p| p.head_ref == branch).cloned())
    }

    async fn create_pull(&self, new: &NewPull) -> Result<Pull, ForgeError> {
        let mut pulls = self.0.pulls.lock().unwrap();
        let n = pulls.len() as u64 + 1;
        let pull = Pull {
            number: n,
            url: format!("https://github.com/o/r/pull/{n}"),
            node_id: format!("PR_{n}"),
            open: true,
            merged: false,
            merge_commit: None,
            head_sha: "0".repeat(40),
            head_ref: new.head.clone(),
            body: new.body.clone(),
            settled_ms: None,
        };
        pulls.push(pull.clone());
        Ok(pull)
    }

    async fn get_pull(&self, number: u64) -> Result<Pull, ForgeError> {
        let pulls = self.0.pulls.lock().unwrap();
        pulls
            .iter()
            .find(|p| p.number == number)
            .cloned()
            .ok_or_else(|| ForgeError::Refused("status 404: Not Found".into()))
    }

    async fn update_pull(&self, _number: u64, _body: &str) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn enqueue(&self, _node_id: &str) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn check_runs(
        &self,
        _sha: &str,
    ) -> Result<Vec<graft_mirror::github::CheckRun>, ForgeError> {
        Ok(Vec::new())
    }

    async fn token(&self) -> Result<SecretToken, ForgeError> {
        Err(ForgeError::Refused(
            "a local upstream needs no token".into(),
        ))
    }

    async fn merging(&self, _node_id: &str) -> Result<bool, ForgeError> {
        Ok(false)
    }

    async fn close_pr(&self, _number: u64, _comment: &str) -> Result<(), ForgeError> {
        Ok(())
    }
}

/// Builds a [`LocalForge`] for every upstream, standing in for GitHub's.
struct LocalForges(Arc<PrBook>);

impl Forges for LocalForges {
    type Forge<C: Control> = LocalForge;

    fn forge<C: Control>(
        &self,
        _upstream: &str,
        _credential: LeaseCredential<C>,
    ) -> Option<Result<LocalForge, graft_mirror::MirrorError>> {
        self.0.built.fetch_add(1, Ordering::SeqCst);
        Some(Ok(LocalForge(self.0.clone())))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claimed_cut_request_runs_through_the_spawned_task_and_opens_its_pr() {
    use graft_model::{
        Author, AuthorKind, Change, EntryKind, Metadata, ObjectKind, RepoPath, Timestamp,
    };
    use graft_proto::Frame;
    use graft_proto::types::CreateChangeset;

    const LINE: &str = "integration";
    let tmp = tempfile::tempdir().unwrap();
    let (bare, work) = upstream(tmp.path());
    let c1 = commit(&work, "a");
    let (api, store) = engine();
    api.create_repo(&mirror_cx(REPO), REPO.to_owned())
        .await
        .unwrap();
    let fake = Arc::new(Fake::with_store(store));
    fake.set_row(row(REPO, bare.to_str().unwrap(), 0));
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let book = Arc::new(PrBook::default());
    let mut config = local_config(&data, "a");
    config.claim_requests = true;

    let mut service = Service::new(
        fake.clone(),
        FollowSpawner::new(api.clone())
            .with_cx(mirror_cx)
            .with_forges(LocalForges(book.clone())),
        config,
    );
    service.reconcile().await.unwrap();
    eventually(|| {
        fake.statuses()
            .iter()
            .any(|s| s.last_imported_sha.as_deref() == Some(c1.as_str()))
    })
    .await;
    assert_eq!(
        book.built.load(Ordering::SeqCst),
        1,
        "the task built a forge"
    );

    // One land on the integration line, cut from main's tip.
    let main = current_ref(&*api, &mirror_cx(REPO), REPO, "main")
        .await
        .unwrap()
        .expect("followed");
    let body = b"b1\n";
    let land = api
        .create_changeset(
            &mirror_cx(REPO),
            CreateChangeset {
                repo: REPO.to_owned(),
                parents: vec![main],
                changes: vec![Change::Put {
                    path: RepoPath::new("b.txt").unwrap(),
                    kind: EntryKind::File,
                    id: graft_model::ObjectId::hash(ObjectKind::Blob, body),
                }],
                author: Author {
                    name: "Tess".to_owned(),
                    email: "tess@example.com".to_owned(),
                    kind: AuthorKind::Agent,
                },
                authored_at: Timestamp {
                    ms: 1_700_000_000_000,
                    offset_min: 0,
                },
                message: "add b".to_owned(),
                metadata: Metadata::new(),
                inline_blobs: vec![Frame::encode(ObjectKind::Blob, body)],
                workspace: None,
            },
        )
        .await
        .unwrap()
        .id;
    api.update_ref(
        &mirror_cx(REPO),
        UpdateRef {
            repo: REPO.to_owned(),
            name: LINE.to_owned(),
            expected: None,
            new: land,
            kind: RefKind::Line,
            lease_epoch: None,
        },
    )
    .await
    .unwrap();

    let mut req = request(7, MirrorRequestKind::CutBatch);
    req.line = LINE.to_owned();
    fake.st().pending.push_back(req);
    eventually(|| {
        let st = fake.st();
        !st.completed.is_empty() || !st.failed.is_empty()
    })
    .await;
    let st = fake.st();
    assert!(st.failed.is_empty(), "the cut failed: {:?}", st.failed);
    let (id, outcome) = &st.completed[0];
    assert_eq!(*id, 7);
    assert_eq!(outcome.branch.as_deref(), Some("graft/batch-1"));
    assert_eq!(outcome.pr_number, Some(1));
    // The batch branch holds the land's one commit on main.
    let head = git(&bare, &["rev-parse", "graft/batch-1"]);
    assert_eq!(git(&bare, &["rev-parse", &format!("{head}~1")]), c1);
    let pulls = book.pulls.lock().unwrap();
    assert_eq!(pulls.len(), 1);
    assert_eq!(pulls[0].head_ref, "graft/batch-1");
}

/// The mirror's call context with the mirror set only (`repo:*:mirror`),
/// as a minted mirror token gives it after the migration.
fn mirror_only_cx(repo: &str) -> CallContext {
    let mut cx = mirror_cx(repo);
    cx.principal.scopes = vec!["repo:*:mirror".to_owned()];
    cx
}

/// WI-3426: a claimed cut runs with the mirror set only. The base moved
/// upstream after the land, so the cut creates the sync merge and moves
/// the batch's line, which the mirror kind may do only for the line its
/// claimed cut names. Fails on an engine without that grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mirror_token_cuts_a_batch_that_syncs_its_line() {
    use graft_model::{
        Author, AuthorKind, Change, EntryKind, Metadata, ObjectKind, RepoPath, Timestamp,
    };
    use graft_proto::Frame;
    use graft_proto::types::CreateChangeset;

    const LINE: &str = "integration";
    let tmp = tempfile::tempdir().unwrap();
    let (bare, work) = upstream(tmp.path());
    let c1 = commit(&work, "a");
    let (api, store) = engine();
    api.create_repo(&mirror_cx(REPO), REPO.to_owned())
        .await
        .unwrap();
    bind(&store, REPO, bare.to_str().unwrap()).await;
    let fake = Arc::new(Fake::with_store(store));
    fake.set_row(row(REPO, bare.to_str().unwrap(), 0));
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();

    // The migration sets the followed line and its marker up (admin).
    let mut setup = Service::new(
        fake.clone(),
        FollowSpawner::new(api.clone()).with_cx(mirror_cx),
        local_config(&data, "a"),
    );
    setup.reconcile().await.unwrap();
    eventually(|| {
        fake.statuses()
            .iter()
            .any(|s| s.last_imported_sha.as_deref() == Some(c1.as_str()))
    })
    .await;
    let main = current_ref(&*api, &mirror_cx(REPO), REPO, "main")
        .await
        .unwrap()
        .expect("followed");

    // One land on the integration line, cut from main's tip.
    let body = b"b1\n";
    let land = api
        .create_changeset(
            &mirror_cx(REPO),
            CreateChangeset {
                repo: REPO.to_owned(),
                parents: vec![main],
                changes: vec![Change::Put {
                    path: RepoPath::new("b.txt").unwrap(),
                    kind: EntryKind::File,
                    id: graft_model::ObjectId::hash(ObjectKind::Blob, body),
                }],
                author: Author {
                    name: "Tess".to_owned(),
                    email: "tess@example.com".to_owned(),
                    kind: AuthorKind::Agent,
                },
                authored_at: Timestamp {
                    ms: 1_700_000_000_000,
                    offset_min: 0,
                },
                message: "add b".to_owned(),
                metadata: Metadata::new(),
                inline_blobs: vec![Frame::encode(ObjectKind::Blob, body)],
                workspace: None,
            },
        )
        .await
        .unwrap()
        .id;
    api.update_ref(
        &mirror_cx(REPO),
        UpdateRef {
            repo: REPO.to_owned(),
            name: LINE.to_owned(),
            expected: None,
            new: land,
            kind: RefKind::Line,
            lease_epoch: None,
        },
    )
    .await
    .unwrap();

    // A restart with the mirror set only and claims on; main moves upstream.
    let book = Arc::new(PrBook::default());
    let mut config = local_config(&data, "b");
    config.claim_requests = true;
    let mut service = Service::new(
        fake.clone(),
        FollowSpawner::new(api.clone())
            .with_cx(mirror_only_cx)
            .with_forges(LocalForges(book.clone())),
        config,
    );
    fake.lapse(REPO);
    service.reconcile().await.unwrap();
    drop(setup);
    let c2 = commit(&work, "c");
    eventually(|| {
        fake.statuses()
            .iter()
            .any(|s| s.last_imported_sha.as_deref() == Some(c2.as_str()))
    })
    .await;

    let mut req = request(7, MirrorRequestKind::CutBatch);
    req.line = LINE.to_owned();
    fake.st().pending.push_back(req);
    eventually(|| {
        let st = fake.st();
        !st.completed.is_empty() || !st.failed.is_empty()
    })
    .await;
    {
        let st = fake.st();
        assert!(st.failed.is_empty(), "the cut failed: {:?}", st.failed);
        let (id, outcome) = &st.completed[0];
        assert_eq!(*id, 7);
        assert_eq!(outcome.pr_number, Some(1));
    }
    // The cut synced the line onto the moved base: its tip is a merge
    // whose parents are the land and main's new tip.
    let new_main = current_ref(&*api, &mirror_cx(REPO), REPO, "main")
        .await
        .unwrap()
        .expect("followed");
    assert_ne!(new_main, main);
    let tip = current_ref(&*api, &mirror_cx(REPO), REPO, LINE)
        .await
        .unwrap()
        .expect("line");
    let cs = api
        .get_changesets(
            &mirror_cx(REPO),
            graft_proto::types::GetChangesets {
                repo: REPO.to_owned(),
                ids: vec![tip],
            },
        )
        .await
        .unwrap();
    assert_eq!(cs.found[0].changeset.parents, vec![land, new_main]);
    // The batch branch holds the land's one commit on main's new tip.
    let head = git(&bare, &["rev-parse", "graft/batch-1"]);
    assert_eq!(git(&bare, &["rev-parse", &format!("{head}~1")]), c2);
}

/// Whether [`minted_cx`] still carries the admin scope of the migration.
static MIGRATING: AtomicBool = AtomicBool::new(true);

/// The mirror's call context as a minted mirror token gives it: the
/// `mirror` kind with only `repo:*:mirror`. While [`MIGRATING`], also
/// admin, as the followers that set the line and its marker up held.
fn minted_cx(repo: &str) -> CallContext {
    let mut cx = mirror_cx(repo);
    cx.principal.scopes = vec!["repo:*:mirror".to_owned()];
    if MIGRATING.load(Ordering::SeqCst) {
        cx.principal.scopes.push(format!("repo:{repo}:admin"));
    }
    cx
}

/// Binds `repo` to a mirror of `upstream` in the engine's store, as the
/// server's mirror row does (D41).
async fn bind(store: &MemoryStore, repo: &str, upstream: &str) {
    use graft_store::{MirrorDirection, MirrorExportRecord, MirrorLineRecord, MirrorRecord};
    let id = store.repo(repo).await.unwrap().expect("repo");
    let txn = MetaTxn {
        preconditions: vec![Precondition::MirrorVersionIs {
            upstream: upstream.to_owned(),
            version: None,
        }],
        writes: vec![Write::PutMirror(MirrorRecord {
            upstream: upstream.to_owned(),
            lines: vec![MirrorLineRecord {
                branch: "main".into(),
                line: "main".into(),
                direction: MirrorDirection::Follow,
            }],
            interval_s: 0,
            enabled: true,
            credential: None,
            kek_id: None,
            export: MirrorExportRecord {
                default_author: None,
                authors: Vec::new(),
            },
            version: 1,
            status: None,
            created_by: "op".into(),
            updated_by: "op".into(),
            updated_at_ms: 0,
        })],
    };
    assert!(matches!(
        store.commit(id, txn).await,
        Ok(TxnOutcome::Committed(_))
    ));
}

/// WI-3426: once a batch merges upstream, the next poll imports its
/// commits and moves the line and its followed marker. The mirror token
/// (only `repo:*:mirror`) must do all of that on a bound repository: it was
/// refused `repo:<r>:admin`, stopping the line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minted_mirror_token_follows_a_merged_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, work) = upstream(tmp.path());
    let c1 = commit(&work, "a");
    let (api, store) = engine();
    api.create_repo(&mirror_cx(REPO), REPO.to_owned())
        .await
        .unwrap();
    bind(&store, REPO, bare.to_str().unwrap()).await;
    let fake = Arc::new(Fake::with_store(store));
    fake.set_row(row(REPO, bare.to_str().unwrap(), 0));
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let mut service = Service::new(
        fake.clone(),
        FollowSpawner::new(api.clone()).with_cx(minted_cx),
        local_config(&data, "a"),
    );
    service.reconcile().await.unwrap();
    eventually(|| {
        fake.statuses()
            .iter()
            .any(|s| s.last_imported_sha.as_deref() == Some(c1.as_str()))
    })
    .await;

    // The migration is over: a new task (a restart) calls with the mirror
    // set only. A batch of two lands then merges upstream.
    MIGRATING.store(false, Ordering::SeqCst);
    let mut restarted = Service::new(
        fake.clone(),
        FollowSpawner::new(api.clone()).with_cx(minted_cx),
        local_config(&data, "b"),
    );
    fake.lapse(REPO);
    restarted.reconcile().await.unwrap();
    assert_eq!(restarted.running(), vec![(REPO.to_owned(), 1, 2)]);
    drop(service);
    commit(&work, "b");
    let c3 = commit(&work, "c");
    eventually(|| {
        fake.statuses()
            .iter()
            .any(|s| s.last_imported_sha.as_deref() == Some(c3.as_str()))
    })
    .await;
    let last = fake.statuses().last().cloned().expect("status");
    assert_eq!(
        last.error_code,
        MirrorErrorCode::Unspecified as i32,
        "{last:?}"
    );
    let tip = current_ref(&*api, &minted_cx(REPO), REPO, "main")
        .await
        .unwrap()
        .expect("line");
    let marker = current_ref(
        &*api,
        &minted_cx(REPO),
        REPO,
        &graft_proto::types::followed_ref("main"),
    )
    .await
    .unwrap();
    assert_eq!(marker, Some(tip));
}

// ---- serve's forges: GitHub or Forgejo by upstream (WI-3492) ---------------

/// A Forgejo PR as the API returns it.
fn forgejo_pull(number: u64, branch: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "html_url": format!("https://git.example/o/r/pulls/{number}"),
        "state": "open",
        "merged": false,
        "merge_commit_sha": null,
        "merged_at": null,
        "closed_at": null,
        "body": "batch body",
        "head": { "sha": "a".repeat(40), "ref": branch, "repo": { "full_name": "o/r" } },
        "base": { "ref": "main" },
    })
}

#[tokio::test]
async fn serve_picks_the_forge_by_upstream_and_forgejo_uses_the_row_credential() {
    use graft_mirror::github::{Forge, RetryPolicy};
    use graft_mirror::service::{AnyForge, ForgejoHost, ServeForges};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ROW_TOKEN: &str = "row-credential-for-tests-only";
    let server = MockServer::start().await;
    let fake = Arc::new(Fake::default());
    fake.st()
        .credentials
        .insert(REPO.to_owned(), ROW_TOKEN.to_owned());
    let epoch = fake.acquire_lease(REPO, "i").await.unwrap().epoch;
    let forges = ServeForges::new(vec![ForgejoHost {
        host: "git.example".to_owned(),
        api: server.uri(),
    }])
    .with_retry(RetryPolicy {
        attempts: 1,
        ..RetryPolicy::default()
    });
    let cred = |upstream: &str| LeaseCredential::new(fake.clone(), REPO, upstream, epoch);

    // GitHub stays GitHub; a local path or another host has no forge.
    let gh = "https://github.com/o/r.git";
    assert!(matches!(
        forges.forge(gh, cred(gh)),
        Some(Ok(AnyForge::GitHub(_)))
    ));
    for none in ["/srv/git/r.git", "https://elsewhere.example/o/r.git"] {
        assert!(forges.forge(none, cred(none)).is_none(), "{none}");
    }

    // An upstream on the Forgejo host opens its batch PR through Forgejo's
    // API with the row's credential, the one git fetches and pushes with.
    let fj = "https://git.example/o/r.git";
    let Some(Ok(forge @ AnyForge::Forgejo(_))) = forges.forge(fj, cred(fj)) else {
        panic!("a Forgejo upstream gets the Forgejo forge");
    };
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls"))
        .and(header(
            "authorization",
            format!("token {ROW_TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(forgejo_pull(3, "graft/batch-3")))
        .expect(1)
        .mount(&server)
        .await;
    let pull = forge
        .create_pull(&NewPull {
            title: "batch 3".to_owned(),
            body: "b".to_owned(),
            head: "graft/batch-3".to_owned(),
            base: "main".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!((pull.number, pull.head_ref.as_str()), (3, "graft/batch-3"));
    assert!(
        !forge.merging(&pull.node_id).await.unwrap(),
        "nothing merges a Forgejo batch but land-batch"
    );
    assert_eq!(forge.token().await.unwrap().expose(), ROW_TOKEN);

    // The lease gone, the credential is not read.
    fake.st().leases.remove(REPO);
    assert!(forge.token().await.is_err());
}
