//! Per-land check refs on a post-land line (WI-2624) against a local bare
//! repository standing in for GitHub, and an in-process engine over the
//! memory store whose `GetLineConfig` answers as the server does.
//!
//! Covers: every land of one journal page gets its export recorded, then a
//! ref pushed; a re-run after a crash gives the same shas; a line that is
//! not post-land is refused and no authority is claimed; settled lands lose
//! their refs and the live refs are exactly the unsettled lands; the cost
//! follows the change and the cursor is durable; the token stays out of
//! argv.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use graft_engine::policy::LineChecks;
use graft_engine::{Engine, EngineConfig, LineConfig};
use graft_git::GitSha;
use graft_git::map::MirrorMap;
use graft_mirror::check_refs::{
    CheckRefLine, CheckRefs, CheckRefsConfig, CheckRefsOutcome, CheckRefsReport,
};
use graft_mirror::credentials::NoCredentials;
use graft_mirror::{ExportLog, Mirror, MirrorDb, MirrorError, current_ref};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectKind, RepoPath, Timestamp,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, Frame};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use support::{REPO, advance, clock, cx, git, move_line};

type Api = Engine<MemoryStore, MemoryStore>;

const LINE: &str = "backlog-1";
const PREFIX: &str = "graft/check/backlog-1/";

macro_rules! forward {
    ($($name:ident($req:ty) -> $out:ty;)*) => {
        $(
            async fn $name(&self, cx: &CallContext, req: $req) -> graft_proto::error::ApiResult<$out> {
                self.api.$name(cx, req).await
            }
        )*
    };
}

/// The engine, answering `GetLineConfig` as the server does (`checks`, or
/// unimplemented for `None`), and counting the reads check refs make.
struct Server<'a> {
    api: &'a Api,
    checks: Option<LineChecksMode>,
    /// `(from_lsn, entries returned)` per `Watch`.
    watches: Mutex<Vec<(u64, usize)>>,
    /// Rows returned per `GetLandChecks`.
    rows: Mutex<Vec<usize>>,
}

impl<'a> Server<'a> {
    fn new(api: &'a Api, checks: Option<LineChecksMode>) -> Self {
        Server {
            api,
            checks,
            watches: Mutex::new(Vec::new()),
            rows: Mutex::new(Vec::new()),
        }
    }

    fn reset(&self) {
        self.watches.lock().unwrap().clear();
        self.rows.lock().unwrap().clear();
    }
}

impl ObjectApi for Server<'_> {
    forward! {
        get_objects(GetObjects) -> Objects;
        has_objects(HasObjects) -> Vec<graft_model::ObjectId>;
        put_objects(PutObjects) -> Vec<graft_model::ObjectId>;
    }
}

impl RepoApi for Server<'_> {
    forward! {
        create_changeset(CreateChangeset) -> Created;
        get_changesets(GetChangesets) -> Changesets;
        resolve_ref(ResolveRef) -> RefView;
        list_refs(ListRefs) -> Vec<RefView>;
        update_ref(UpdateRef) -> RefView;
        stat(PathAt) -> EntryView;
        list_tree(ListTree) -> Entries;
        read_file(PathAt) -> FileContent;
        log(Log) -> History;
        is_ancestor(Pair) -> bool;
        merge_base(Pair) -> Option<ChangesetId>;
        diff(Diff) -> DiffResult;
        blame(Blame) -> BlameResult;
        rebase(Rebase) -> ChangesetId;
        revert(Revert) -> ChangesetId;
        land(Land) -> Landed;
        attest(Attest) -> ();
    }
    async fn watch(
        &self,
        cx: &CallContext,
        req: Watch,
    ) -> graft_proto::error::ApiResult<JournalPage> {
        let from = req.from_lsn;
        let page = self.api.watch(cx, req).await?;
        self.watches
            .lock()
            .unwrap()
            .push((from, page.entries.len()));
        Ok(page)
    }
    async fn get_land_checks(
        &self,
        cx: &CallContext,
        req: GetLandChecks,
    ) -> graft_proto::error::ApiResult<LandCheckPage> {
        let page = self.api.get_land_checks(cx, req).await?;
        self.rows.lock().unwrap().push(page.rows.len());
        Ok(page)
    }
    async fn get_line_config(
        &self,
        _cx: &CallContext,
        req: GetLineConfig,
    ) -> graft_proto::error::ApiResult<graft_proto::types::LineConfig> {
        match self.checks {
            Some(checks) => Ok(graft_proto::types::LineConfig {
                line: req.line,
                checks,
                required: vec!["ci/test".to_owned()],
                retries: 0,
            }),
            None => Err(ApiError::unimplemented("GetLineConfig")),
        }
    }
}

/// `RecordExports` as the server takes it. Each record is checked against
/// upstream at the moment it arrives: the ref must not exist yet, so the
/// export is recorded before the push. `fail_at` refuses the n-th call.
struct Records {
    upstream: PathBuf,
    got: Mutex<Vec<(ChangesetId, GitSha)>>,
    refs_before: Mutex<Vec<Option<String>>>,
    fail_at: Mutex<Option<usize>>,
}

impl Records {
    fn new(upstream: &Path) -> Records {
        Records {
            upstream: upstream.to_owned(),
            got: Mutex::new(Vec::new()),
            refs_before: Mutex::new(Vec::new()),
            fail_at: Mutex::new(None),
        }
    }

    fn got(&self) -> Vec<(ChangesetId, GitSha)> {
        self.got.lock().unwrap().clone()
    }
}

impl ExportLog for Records {
    fn record_exports<'s>(
        &'s self,
        _cx: &'s CallContext,
        repo: &'s str,
        exports: &'s [(ChangesetId, GitSha)],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ApiError>> + Send + 's>>
    {
        Box::pin(async move {
            assert_eq!(repo, REPO);
            let mut got = self.got.lock().unwrap();
            let mut fail = self.fail_at.lock().unwrap();
            if *fail == Some(got.len()) {
                *fail = None;
                return Err(ApiError::Unavailable { retry_after_ms: 1 });
            }
            for (c, s) in exports {
                let before = support::branch(&self.upstream, &format!("{PREFIX}{c}"));
                self.refs_before.lock().unwrap().push(before);
                got.push((*c, *s));
            }
            Ok(())
        })
    }
}

/// GitHub's git (bare), the mirror's directory and DB, and an engine whose
/// `backlog-1` line has post-land checks on `ci/test` with no retry.
struct World {
    _tmp: tempfile::TempDir,
    upstream: PathBuf,
    mirror_dir: PathBuf,
    db_path: PathBuf,
    db: MirrorDb,
    api: Api,
    n: std::cell::Cell<i64>,
}

impl World {
    async fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("github.git");
        support::github(&upstream, true);
        let mut line = LineConfig::direct(LINE);
        line.checks = LineChecks::PostLand {
            required: vec!["ci/test".to_owned()],
            retries: 0,
        };
        let store = Arc::new(MemoryStore::new());
        let api = Engine::new(
            store.clone(),
            store,
            Arc::new(FakeClock::at(1_000_000)),
            EngineConfig {
                lines: vec![line],
                ..EngineConfig::default()
            },
        );
        api.create_repo(&cx(), REPO.to_owned()).await.unwrap();
        let db_path = tmp.path().join("mirror.sqlite");
        let db = MirrorDb::open(&db_path).unwrap();
        let w = World {
            mirror_dir: tmp.path().join("mirror.git"),
            upstream,
            db_path,
            db,
            api,
            _tmp: tmp,
            n: std::cell::Cell::new(0),
        };
        // The line's fork point: an admin move, as pr-batch's sync is.
        let base = w.changeset(None, "README").await;
        move_line(&w.api, LINE, None, base).await;
        w
    }

    fn check_refs<'a>(
        &'a self,
        server: &'a Server<'a>,
        log: &'a Records,
    ) -> CheckRefs<'a, Server<'a>, NoCredentials> {
        CheckRefs::new(
            server,
            cx(),
            &self.db,
            Mirror::new(&self.mirror_dir, self.upstream.to_str().unwrap()),
            &NoCredentials,
            log,
            CheckRefsConfig::new(REPO, vec![CheckRefLine::new(LINE)]),
        )
        .with_clock(clock)
    }

    async fn poll(&self, server: &Server<'_>, log: &Records) -> CheckRefsOutcome {
        advance(1000);
        let mut out = self.check_refs(server, log).poll_once().await.unwrap();
        assert_eq!(out.len(), 1);
        let (line, outcome) = out.pop().unwrap();
        assert_eq!(line, LINE);
        outcome
    }

    async fn polled(&self, server: &Server<'_>, log: &Records) -> CheckRefsReport {
        match self.poll(server, log).await {
            CheckRefsOutcome::Polled(r) => r,
            other => panic!("the poll failed: {other:?}"),
        }
    }

    async fn tip(&self) -> ChangesetId {
        current_ref(&self.api, &cx(), REPO, LINE)
            .await
            .unwrap()
            .unwrap()
    }

    /// One changeset writing a new file on `parent`.
    async fn changeset(&self, parent: Option<ChangesetId>, file: &str) -> ChangesetId {
        self.n.set(self.n.get() + 1);
        let n = self.n.get();
        let body = format!("{file} {n}\n");
        self.api
            .create_changeset(
                &cx(),
                CreateChangeset {
                    repo: REPO.to_owned(),
                    parents: parent.into_iter().collect(),
                    changes: vec![Change::Put {
                        path: RepoPath::new(file).unwrap(),
                        kind: EntryKind::File,
                        id: graft_model::ObjectId::hash(ObjectKind::Blob, body.as_bytes()),
                    }],
                    author: Author {
                        name: "Tess".to_owned(),
                        email: "tess@example.com".to_owned(),
                        kind: AuthorKind::Agent,
                    },
                    authored_at: Timestamp {
                        ms: 1_700_000_000_000 + n * 1000,
                        offset_min: 0,
                    },
                    message: format!("add {file}"),
                    metadata: Metadata::new(),
                    inline_blobs: vec![Frame::encode(ObjectKind::Blob, body.as_bytes())],
                    workspace: None,
                },
            )
            .await
            .unwrap()
            .id
    }

    async fn land_head(&self, head: ChangesetId) -> ChangesetId {
        match self
            .api
            .land(
                &cx(),
                Land {
                    repo: REPO.to_owned(),
                    head,
                    line: LINE.to_owned(),
                    squash: false,
                    merge: MergeMode::File,
                    deadline_ms: None,
                    merged_from: None,
                },
            )
            .await
            .unwrap()
        {
            Landed::Landed { landed, .. } => landed,
            other => panic!("not landed: {other:?}"),
        }
    }

    /// Lands one new file through `Land`, which gives it a pending row.
    async fn land(&self) -> ChangesetId {
        let tip = self.tip().await;
        let file = format!("f{}.txt", self.n.get() + 1);
        let id = self.changeset(Some(tip), &file).await;
        self.land_head(id).await
    }

    /// An admin move of the line (pr-batch's sync): no land, no row.
    async fn admin_move(&self) -> ChangesetId {
        let tip = self.tip().await;
        let file = format!("sync{}.txt", self.n.get() + 1);
        let id = self.changeset(Some(tip), &file).await;
        move_line(&self.api, LINE, Some(tip), id).await;
        id
    }

    async fn land_revert(&self, land: ChangesetId) -> ChangesetId {
        let tip = self.tip().await;
        let revert = self
            .api
            .revert(
                &cx(),
                Revert {
                    repo: REPO.to_owned(),
                    changeset: land,
                    onto: tip,
                    workspace: None,
                    check_url: None,
                },
            )
            .await
            .unwrap();
        self.land_head(revert).await
    }

    /// The post-land checker reports `ci/test` on `land`.
    async fn report(&self, land: ChangesetId, pass: bool) {
        let mut c = cx();
        c.principal.scopes.push(format!("repo:{REPO}:attest"));
        self.api
            .attest_v2(
                &c,
                AttestV2 {
                    repo: REPO.to_owned(),
                    changeset: land,
                    kind: AttestKind::Check,
                    check: "ci/test".to_owned(),
                    status: if pass {
                        AttestStatus::Pass
                    } else {
                        AttestStatus::Fail
                    },
                    by: Some("checker".to_owned()),
                    url: "https://ci.example/run".to_owned(),
                },
            )
            .await
            .unwrap();
    }

    /// Every row of the line, by changeset.
    async fn rows(&self) -> BTreeMap<ChangesetId, LandCheck> {
        graft_client::land_checks(
            &self.api,
            &cx(),
            GetLandChecks {
                repo: REPO.to_owned(),
                line: LINE.to_owned(),
                from_pos: 0,
                page: Page {
                    limit: 0,
                    token: None,
                },
            },
            10_000,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.changeset, r))
        .collect()
    }

    /// The lands no check will run on again: what must not hold a ref.
    async fn unsettled(&self) -> BTreeSet<ChangesetId> {
        self.rows()
            .await
            .into_values()
            .filter(|r| match r.state {
                LandCheckState::Good | LandCheckState::Reverted => false,
                LandCheckState::Bad => r.revert.is_none(),
                _ => true,
            })
            .map(|r| r.changeset)
            .collect()
    }

    /// The check refs upstream holds: changeset → commit.
    fn upstream_refs(&self) -> BTreeMap<ChangesetId, String> {
        let out = git(
            &self.upstream,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                &format!("refs/heads/{PREFIX}"),
            ],
        );
        out.lines()
            .map(|l| {
                let (name, sha) = l.split_once(' ').unwrap();
                let id = name.strip_prefix(&format!("refs/heads/{PREFIX}")).unwrap();
                (ChangesetId::parse(id).unwrap(), sha.to_owned())
            })
            .collect()
    }

    fn sha_of(&self, id: ChangesetId) -> String {
        self.db.sha_of(id).unwrap().unwrap().to_hex()
    }
}

fn post_land(api: &Api) -> Server<'_> {
    Server::new(api, Some(LineChecksMode::PostLand))
}

// Done when 1: three lands in one journal page give three records and three
// refs, each recorded before its ref exists; an admin move gets none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_land_in_one_page_is_recorded_then_pushed() {
    let w = World::new().await;
    let server = post_land(&w.api);
    let log = Records::new(&w.upstream);
    let first = w.polled(&server, &log).await;
    assert_eq!(first, CheckRefsReport::default(), "nothing landed yet");

    let a = w.land().await;
    let sync = w.admin_move().await;
    let b = w.land().await;
    let c = w.land().await;
    server.reset();
    let report = w.polled(&server, &log).await;

    // All three moves came in one journal page.
    let watches = server.watches.lock().unwrap().clone();
    assert_eq!(watches.iter().filter(|(_, n)| *n > 0).count(), 1);
    let lands = [a, b, c];
    let created: Vec<ChangesetId> = report.created.iter().map(|(c, _)| *c).collect();
    assert_eq!(created, lands);
    assert_eq!(report.held, 3);
    let recorded: Vec<ChangesetId> = log.got().iter().map(|(c, _)| *c).collect();
    assert_eq!(recorded, lands, "one record per land, in line order");
    assert!(
        log.refs_before.lock().unwrap().iter().all(Option::is_none),
        "each export is recorded before its ref is pushed"
    );
    let refs = w.upstream_refs();
    assert_eq!(refs.keys().copied().collect::<Vec<_>>(), {
        let mut v = lands.to_vec();
        v.sort();
        v
    });
    for (land, sha) in log.got() {
        assert_eq!(refs[&land], sha.to_hex(), "the ref is the recorded export");
        assert_eq!(w.sha_of(land), sha.to_hex(), "the mirror map's commit");
    }
    assert!(!refs.contains_key(&sync), "an admin move is not a land");
    // The ref's commit carries the land's tree: its first parent chain
    // includes the admin move's export.
    let parent = git(&w.upstream, &["rev-parse", &format!("{}^", refs[&b])]);
    assert_eq!(parent, w.sha_of(sync));

    // A poll with nothing new changes nothing.
    let again = w.polled(&server, &log).await;
    assert!(again.created.is_empty() && again.deleted.is_empty());
    assert_eq!(log.got().len(), 3);
}

// Done when 1: a crash part-way through a page, and a crash after the pushes
// before the mirror DB recorded them, re-run to the same shas.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rerun_after_a_crash_gives_the_same_shas() {
    let w = World::new().await;
    let server = post_land(&w.api);
    let log = Records::new(&w.upstream);
    w.polled(&server, &log).await;
    let start = w.db.check_line(LINE).unwrap().unwrap().pos;
    let lands = [w.land().await, w.land().await, w.land().await];

    // The second record fails: the first land is pushed, the rest not.
    *log.fail_at.lock().unwrap() = Some(1);
    assert!(matches!(
        w.poll(&server, &log).await,
        CheckRefsOutcome::Failed(_)
    ));
    let partial = w.upstream_refs();
    assert_eq!(partial.keys().copied().collect::<Vec<_>>(), [lands[0]]);
    // The second land was exported before its record failed; the re-run,
    // at a later clock, exports it to the same commit.
    let second = w.sha_of(lands[1]);
    let report = w.polled(&server, &log).await;
    let created: Vec<ChangesetId> = report.created.iter().map(|(c, _)| *c).collect();
    assert_eq!(created, lands[1..]);
    let refs = w.upstream_refs();
    assert_eq!(refs.len(), 3);
    assert_eq!(refs[&lands[0]], partial[&lands[0]], "the pushed ref kept");
    assert_eq!(refs[&lands[1]], second);

    // Crash after the pushes, before the DB marked them pushed: each push
    // is redone on its lease and finds its own sha.
    let conn = rusqlite::Connection::open(&w.db_path).unwrap();
    conn.execute("UPDATE check_refs SET pushed = 0", [])
        .unwrap();
    let report = w.polled(&server, &log).await;
    assert!(report.created.is_empty());
    assert_eq!(w.upstream_refs(), refs);
    assert!(w.db.check_refs(LINE).unwrap().iter().all(|r| r.pushed));

    // The check ref rows are lost altogether (a DB restored from before the
    // pushes). The clock has moved on, so a fresh export would date
    // differently; the mirror map keeps every sha the same.
    conn.execute("DELETE FROM check_refs", []).unwrap();
    conn.execute("UPDATE check_lines SET dirty = 1, pos = ?1", [start as i64])
        .unwrap();
    drop(conn);
    advance(3_600_000);
    let rerun = w.polled(&server, &log).await;
    let shas: BTreeMap<ChangesetId, String> = rerun
        .created
        .iter()
        .map(|(c, s)| (*c, s.to_hex()))
        .collect();
    assert_eq!(shas, refs, "the same shas, pushed on a refused lease");
    assert_eq!(w.upstream_refs(), refs);
    assert_eq!(rerun.held, 3);
    for (land, sha) in log.got() {
        assert_eq!(refs[&land], sha.to_hex(), "every record names the same sha");
    }
}

// Done when 2: a line that is not post-land is refused in one line, and
// check refs never claim a line's authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_line_that_is_not_post_land_is_refused() {
    let w = World::new().await;
    w.land().await;
    let log = Records::new(&w.upstream);
    for checks in [Some(LineChecksMode::None), None] {
        let server = Server::new(&w.api, checks);
        let err = w
            .check_refs(&server, &log)
            .poll_once()
            .await
            .expect_err("refused");
        let msg = err.to_string();
        assert!(matches!(err, MirrorError::Config(_)), "{msg}");
        assert!(!msg.contains('\n'), "one line: {msg}");
        assert!(msg.contains(LINE) && msg.contains("post-land"), "{msg}");
    }
    assert!(w.upstream_refs().is_empty());
    assert!(log.got().is_empty());
    assert!(w.db.check_line(LINE).unwrap().is_none());

    let server = post_land(&w.api);
    w.polled(&server, &log).await;
    w.land().await;
    w.polled(&server, &log).await;
    assert_eq!(w.upstream_refs().len(), 1);
    assert_eq!(w.db.authority(LINE).unwrap(), None, "no authority claimed");
}

// Done when 3: settled lands (good, reverted, bad and reverted) lose their
// refs with a lease, and after every poll the live refs are exactly the
// unsettled lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settled_lands_lose_their_refs_and_live_refs_stay_bounded() {
    let w = World::new().await;
    let server = post_land(&w.api);
    let log = Records::new(&w.upstream);
    w.polled(&server, &log).await;
    let mut lands = Vec::new();
    for _ in 0..5 {
        lands.push(w.land().await);
    }
    let [a, b, c, d, e] = lands[..] else {
        unreachable!()
    };
    assert_eq!(w.polled(&server, &log).await.held, 5);

    w.report(a, true).await;
    w.report(b, false).await;
    w.report(c, true).await;
    let report = w.polled(&server, &log).await;
    assert_eq!(report.deleted, [a, c], "c settles behind the bad b");
    let live: BTreeSet<_> = w.upstream_refs().into_keys().collect();
    assert_eq!(live, BTreeSet::from([b, d, e]), "bad, unreverted b is kept");
    assert_eq!(live, w.unsettled().await);

    let r = w.land_revert(b).await;
    let report = w.polled(&server, &log).await;
    assert_eq!(report.deleted, [b]);
    assert_eq!(
        report.created.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        [r]
    );
    let live: BTreeSet<_> = w.upstream_refs().into_keys().collect();
    assert_eq!(live, BTreeSet::from([d, e, r]));
    assert_eq!(live, w.unsettled().await);

    for land in [d, e, r] {
        w.report(land, true).await;
    }
    let report = w.polled(&server, &log).await;
    assert_eq!(report.held, 0);
    assert!(w.upstream_refs().is_empty());
    assert!(w.db.check_refs(LINE).unwrap().is_empty());

    // A random walk: lands, admin moves and verdicts interleave; after each
    // poll upstream holds a ref for exactly the unsettled lands.
    let mut rng = support::Rng(0x2624);
    let mut pending: Vec<ChangesetId> = Vec::new();
    let mut most = 0;
    for _ in 0..40 {
        match rng.below(4) {
            0 if !pending.is_empty() => {
                let i = rng.below(pending.len() as u64) as usize;
                let land = pending.remove(i);
                w.report(land, true).await;
            }
            1 => {
                w.admin_move().await;
            }
            _ => pending.push(w.land().await),
        }
        let report = w.polled(&server, &log).await;
        let live: BTreeSet<_> = w.upstream_refs().into_keys().collect();
        assert_eq!(live, w.unsettled().await);
        assert_eq!(report.held, live.len());
        most = most.max(live.len());
    }
    assert!(most > 1, "the walk held several refs at once");
}

// Done when 4: a steady poll reads the journal from the durable cursor and
// the rows from the oldest unsettled land, not the line's history; a new
// mirror process resumes from the cursor in the DB; the first poll of a
// long line finds its ends with O(log n) probes and gives old lands no ref.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cost_follows_the_change_and_the_cursor_is_durable() {
    let w = World::new().await;
    for _ in 0..64 {
        let land = w.land().await;
        w.report(land, true).await;
    }
    let server = post_land(&w.api);
    let log = Records::new(&w.upstream);
    let first = w.polled(&server, &log).await;
    assert_eq!(
        first,
        CheckRefsReport::default(),
        "lands before start: no ref"
    );
    let reads = server.rows.lock().unwrap().clone();
    let rows: usize = reads.iter().sum();
    assert!(rows <= 20, "{rows} rows read to find the end of 64");
    assert!(reads.len() <= 20, "{} row reads", reads.len());
    let watches = server.watches.lock().unwrap().len();
    assert!(watches <= 24, "{watches} journal reads to find its end");

    let pending = w.land().await;
    let state = w.db.check_line(LINE).unwrap().unwrap();
    server.reset();
    // A new process over the same DB.
    let report = w.polled(&server, &log).await;
    assert_eq!(report.created.len(), 1);
    let watches = server.watches.lock().unwrap().clone();
    assert_eq!(watches[0].0, state.lsn, "resumes from the durable cursor");
    let entries: usize = watches.iter().map(|(_, n)| n).sum();
    assert!(entries <= 2, "{entries} journal entries read for one land");
    let rows: usize = server.rows.lock().unwrap().iter().sum();
    assert!(rows <= 1, "{rows} rows read for one land on a 64-land line");

    // Steady state with one unsettled land: nothing in the journal, one row.
    server.reset();
    w.polled(&server, &log).await;
    let rows: usize = server.rows.lock().unwrap().iter().sum();
    assert_eq!(rows, 1);
    w.report(pending, true).await;
    w.polled(&server, &log).await;
    assert!(w.upstream_refs().is_empty());

    // Idle: no move, no ref held, so no rows are read at all.
    server.reset();
    w.polled(&server, &log).await;
    assert!(server.rows.lock().unwrap().is_empty());
}

// Done when 4 (review blocker): a land stuck bad behind a growing tail of
// settled lands costs one row a poll, however long the tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stuck_land_costs_one_row_whatever_settles_behind_it() {
    let w = World::new().await;
    let server = post_land(&w.api);
    let log = Records::new(&w.upstream);
    w.polled(&server, &log).await;
    let stuck = w.land().await;
    w.report(stuck, false).await;
    w.polled(&server, &log).await;
    let mut steady = Vec::new();
    for tail in [8, 64] {
        for _ in 0..tail {
            let land = w.land().await;
            w.report(land, true).await;
        }
        // The new rows are read once: settled, so no ref.
        let report = w.polled(&server, &log).await;
        assert!(report.created.is_empty());
        server.reset();
        w.polled(&server, &log).await;
        steady.push(server.rows.lock().unwrap().iter().sum::<usize>());
        assert_eq!(
            w.upstream_refs().into_keys().collect::<Vec<_>>(),
            [stuck],
            "only the stuck land holds a ref"
        );
    }
    assert_eq!(
        steady,
        [1, 1],
        "rows read per poll do not grow with the tail"
    );
}

// Done when 4 (review blocker): a land more than MAX_ROWS rows behind a
// stuck land still gets its ref; the new rows are read in capped steps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_land_past_the_row_cap_still_gets_its_ref() {
    use graft_mirror::check_refs::MAX_ROWS;
    let w = World::new().await;
    let server = post_land(&w.api);
    let log = Records::new(&w.upstream);
    w.polled(&server, &log).await;
    let stuck = w.land().await;
    w.report(stuck, false).await;
    for _ in 0..MAX_ROWS + 8 {
        let land = w.land().await;
        w.report(land, true).await;
    }
    let late = w.land().await;
    let mut created = Vec::new();
    for _ in 0..3 {
        server.reset();
        created.extend(w.polled(&server, &log).await.created);
        let rows: usize = server.rows.lock().unwrap().iter().sum();
        assert!(rows <= MAX_ROWS as usize + 2, "{rows} rows in one poll");
    }
    let created: Vec<ChangesetId> = created.into_iter().map(|(c, _)| c).collect();
    assert_eq!(created, [stuck, late]);
    let live: BTreeSet<_> = w.upstream_refs().into_keys().collect();
    assert_eq!(live, BTreeSet::from([stuck, late]));
    assert!(
        !w.db.check_line(LINE).unwrap().unwrap().dirty,
        "all rows read"
    );
}

// Done when 5: through CheckRefs, the GitHub token goes with each check ref
// push and delete in the environment only, never in argv. A fake `git` on
// PATH logs the network commands (push, ls-remote) and passes every other
// command to the real git. The test re-runs itself in a child process with
// that PATH, since this crate forbids the `unsafe` that changing its own
// environment needs.
const CHILD: &str = "GRAFT_WI2624_CHILD";
const TOKEN: &str = "ghs_wi2624_fake_token_value";
const TEST: &str = "the_token_is_never_in_argv";

#[test]
fn the_token_is_never_in_argv() {
    if let Ok(dir) = std::env::var(CHILD) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        return rt.block_on(child(Path::new(&dir)));
    }
    let real = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("git on PATH");
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = dir.path().join("git.log");
    let git = bin.join("git");
    std::fs::write(
        &git,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" push \"*|*\" ls-remote \"*)\n  \
             printf 'call argv=%s header=%s\\n' \"$*\" \"${{GIT_CONFIG_VALUE_0:-none}}\" >>'{}'\n  \
             exit 0;;\nesac\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.path().join("token"), TOKEN).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env("PATH", path)
        .env(CHILD, dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = calls.lines().collect();
    assert_eq!(lines.len(), 2, "one push, one delete: {calls}");
    use base64::Engine as _;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{TOKEN}"));
    for line in &lines {
        let (argv, header) = line.split_once(" header=").unwrap();
        assert!(!argv.contains(TOKEN) && !argv.contains(&basic), "{line}");
        assert_eq!(header, format!("Authorization: Basic {basic}"));
    }
    assert!(lines[0].contains(&format!(":refs/heads/{PREFIX}")) && lines[0].contains(" push "));
    assert!(
        lines[1].contains(&format!(" :refs/heads/{PREFIX}")),
        "a delete"
    );
    assert!(lines[1].contains("--force-with-lease="), "on a lease");
}

/// The child: CheckRefs over an HTTPS upstream with a token file, through
/// one land's ref push and, once it settles, its delete.
async fn child(dir: &Path) {
    let w = World::new().await;
    let server = post_land(&w.api);
    let log = Records::new(&w.upstream);
    let token = graft_mirror::credentials::StaticToken::load(&dir.join("token")).unwrap();
    let checks = || {
        CheckRefs::new(
            &server,
            cx(),
            &w.db,
            Mirror::new(&w.mirror_dir, "https://github.com/o/r.git"),
            &token,
            &log,
            CheckRefsConfig::new(REPO, vec![CheckRefLine::new(LINE)]),
        )
        .with_clock(clock)
    };
    checks().poll_once().await.unwrap();
    let land = w.land().await;
    let out = checks().poll_once().await.unwrap();
    assert!(
        matches!(&out[0].1, CheckRefsOutcome::Polled(r) if r.created.len() == 1),
        "{out:?}"
    );
    w.report(land, true).await;
    let out = checks().poll_once().await.unwrap();
    assert!(
        matches!(&out[0].1, CheckRefsOutcome::Polled(r) if r.deleted == [land]),
        "{out:?}"
    );
}
