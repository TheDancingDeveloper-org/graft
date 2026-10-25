//! G3.1: `Land`, the `direct`, `attested` and `queued` policies, path
//! rules, `Attest` and the post-land revert hook, on both stores; plus
//! deterministic lockstep races.

mod common;

use std::sync::{Arc, Mutex};

use common::race::{Task, YieldSleeper, Yielding, run_scheduled};
use common::*;
use graft_engine::policy::{
    Authority, Candidate, HUMAN_REVIEW, HookFuture, LandPolicy, LandedEvent, PathRule,
    RevertRequest, Verdict,
};
use graft_engine::{EngineConfig, LandHooks, LineConfig, REBASED_FROM};
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, PrincipalKind, RepoApi, WorkspaceApi};
use graft_store::memory::MemoryStore;
use graft_store::{ChangesetRecord, MetaStore, RecordState};

const LINES: &[&str] = &["main", "trunk", "rel", "gp"];

/// `main` is D6 (opted in: the default is `direct`, D22), `trunk` is direct with path rules, `rel` is queued on
/// `ci/test`, `gp` is git-primary. Other lines are direct.
fn config() -> EngineConfig {
    let mut trunk = LineConfig::direct("trunk");
    trunk.path_rules = vec![
        PathRule {
            prefix: p("deploy"),
            checks: vec!["ci/deploy".to_owned()],
            human_review: true,
            merge: None,
        },
        PathRule {
            prefix: p("docs"),
            checks: vec!["ci/docs".to_owned()],
            human_review: false,
            merge: None,
        },
    ];
    let mut rel = LineConfig::direct("rel");
    rel.policy = LandPolicy::Queued {
        checks: vec!["ci/test".to_owned()],
    };
    let mut gp = LineConfig::direct("gp");
    gp.authority = Authority::GitPrimary;
    EngineConfig {
        lines: vec![LineConfig::d6("main"), trunk, rel, gp],
        ..EngineConfig::default()
    }
}

/// What the hooks saw, and what the candidate test answers.
#[derive(Default)]
struct Recorder {
    landed: Mutex<Vec<LandedEvent>>,
    reverts: Mutex<Vec<RevertRequest>>,
    tested: Mutex<Vec<Candidate>>,
    verdict: Mutex<Option<Verdict>>,
}

impl LandHooks for Recorder {
    fn test_candidate<'a>(&'a self, candidate: &'a Candidate) -> HookFuture<'a, Verdict> {
        self.tested.lock().expect("tested").push(candidate.clone());
        let verdict = self
            .verdict
            .lock()
            .expect("verdict")
            .clone()
            .unwrap_or(Verdict::Fail("no verdict".to_owned()));
        Box::pin(async move { verdict })
    }
    fn landed(&self, event: &LandedEvent) {
        self.landed.lock().expect("landed").push(event.clone());
    }
    fn revert_requested(&self, request: &RevertRequest) {
        self.reverts.lock().expect("reverts").push(request.clone());
    }
}

macro_rules! on_both {
    ($($name:ident),* $(,)?) => {
        mod memory_store {
            $( #[test] fn $name() { super::$name(super::hooked(super::memory_with(super::config()))) } )*
        }
        mod sqlite_store {
            $( #[test] fn $name() { super::$name(super::hooked(super::sqlite_with(super::config()))) } )*
        }
    };
}

/// A fixture with a [`Recorder`] installed and an instant backoff.
fn hooked<M: MetaStore>(f: Fixture<M>) -> (Fixture<M>, Arc<Recorder>) {
    let hooks = Arc::new(Recorder::default());
    let installed = hooks.clone();
    let f = f.map_engine(|e| {
        e.with_land_hooks(installed)
            .with_sleeper(Arc::new(YieldSleeper))
    });
    (f, hooks)
}

/// A repository whose lines all start at a root holding `files`.
struct T<'f, M> {
    r: Repo<'f, M>,
    root: ChangesetId,
}

impl<'f, M: MetaStore> T<'f, M> {
    fn new(f: &'f Fixture<M>, files: &[(&str, &[u8])]) -> Self {
        let r = Repo::new(f, "r");
        let root = r.commit(vec![], files, &[]).id;
        for line in LINES {
            r.update_ref(line, None, root, RefKind::Line).expect("line");
        }
        Self { r, root }
    }

    fn commit(&self, parent: ChangesetId, files: &[(&str, &[u8])]) -> ChangesetId {
        self.r.commit(vec![parent], files, &[]).id
    }

    fn land_as(&self, cx: &CallContext, head: ChangesetId, line: &str) -> ApiResult<Landed> {
        block_on(self.r.e().land(cx, land_req(head, line)))
    }

    fn land(&self, head: ChangesetId, line: &str) -> ApiResult<Landed> {
        self.land_as(&self.r.cx, head, line)
    }

    fn attest_as(
        &self,
        kind: PrincipalKind,
        id: ChangesetId,
        check: &str,
        passed: bool,
    ) -> ApiResult<()> {
        let mut cx = self.r.cx.clone();
        cx.principal.kind = kind;
        cx.principal.subject = format!("{kind:?}").to_lowercase();
        if kind == PrincipalKind::Ci {
            // A CI token as the OIDC exchange mints it (WI-2272): attest
            // only, pinned to the changeset it attests. An unpinned admin
            // CI principal no longer passes.
            cx = ci_cx(id);
        }
        self.attest_as_cx(&cx, id, check, passed)
    }

    fn attest_as_cx(
        &self,
        cx: &CallContext,
        id: ChangesetId,
        check: &str,
        passed: bool,
    ) -> ApiResult<()> {
        block_on(self.r.e().attest(
            cx,
            Attest {
                repo: "r".to_owned(),
                changeset: id,
                check: check.to_owned(),
                passed,
                url: format!("https://ci.example/{check}"),
            },
        ))
    }

    fn attest(&self, id: ChangesetId, check: &str, passed: bool) {
        self.attest_as(PrincipalKind::Ci, id, check, passed)
            .expect("attest");
    }

    fn tip(&self, line: &str) -> RefView {
        block_on(self.r.e().resolve_ref(
            &self.r.cx,
            ResolveRef {
                repo: "r".to_owned(),
                name: line.to_owned(),
            },
        ))
        .expect("resolve")
    }

    fn record(&self, id: ChangesetId) -> ChangesetRecord {
        let meta = self.r.e().stores().0;
        let repo = block_on(meta.repo("r")).expect("repo").expect("exists");
        block_on(meta.changeset(repo, id))
            .expect("read")
            .expect("stored")
            .1
    }

    fn files(&self, id: ChangesetId) -> Vec<(String, Vec<u8>)> {
        let root = self.r.changeset(id).changeset.root;
        self.r.fetch(root).into_iter().collect()
    }

    fn kinds(&self) -> Vec<String> {
        self.r.journal().into_iter().map(|e| e.kind).collect()
    }
}

/// A `ci` principal as the OIDC exchange mints it: `read` and `attest` on
/// `r`, pinned to `pin`.
fn ci_cx(pin: ChangesetId) -> CallContext {
    let mut cx = cx(&["repo:r:read", "repo:r:attest"]);
    cx.principal.kind = PrincipalKind::Ci;
    cx.principal.subject = format!("ci:github:o/r#{pin}");
    cx
}

fn denied_attest() -> ApiResult<()> {
    Err(ApiError::PermissionDenied {
        scope: "repo:r:attest".to_owned(),
        path: None,
    })
}

fn land_req(head: ChangesetId, line: &str) -> Land {
    Land {
        repo: "r".to_owned(),
        head,
        line: line.to_owned(),
        squash: false,
        merge: MergeMode::File,
        deadline_ms: None,
        merged_from: None,
    }
}

fn landed(result: ApiResult<Landed>) -> (ChangesetId, u64, u64, bool) {
    match result {
        Ok(Landed::Landed {
            landed,
            line_version,
            pos,
            rebased,
            merged_paths,
        }) => {
            assert!(merged_paths.is_empty(), "file-level lands merge nothing");
            (landed, line_version, pos, rebased)
        }
        other => panic!("expected Landed, got {other:?}"),
    }
}

fn rejected(result: ApiResult<Landed>) -> (String, String) {
    match result {
        Err(ApiError::Rejected { policy, detail }) => (policy, detail),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

fn files(list: &[(&str, &[u8])]) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = list
        .iter()
        .map(|(p, b)| ((*p).to_owned(), b.to_vec()))
        .collect();
    out.sort();
    out
}

fn fast_forward_lands_the_head<M: MetaStore>((f, hooks): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    let (id, version, pos, rebased) = landed(t.land(draft, "trunk"));
    assert_eq!((id, version, pos, rebased), (draft, 2, 1, false));
    assert_eq!(t.tip("trunk").target, draft);
    assert_eq!(t.r.changeset(draft).state, DraftState::Landed);
    let record = t.record(draft);
    assert_eq!((record.holder, record.superseded_by), (None, None));
    let kinds = t.kinds();
    assert_eq!(&kinds[kinds.len() - 2..], ["RefMoved", "LandAttempt"]);
    // The line index has the new position.
    let log = block_on(t.r.e().log(
        &t.r.cx,
        Log {
            repo: "r".to_owned(),
            from: draft,
            until: None,
            paths: vec![p("b")],
            page: Page::default(),
        },
    ))
    .expect("log");
    assert_eq!(log.changesets.len(), 1);
    let meta = t.r.e().stores().0;
    let repo = block_on(meta.repo("r")).expect("repo").expect("exists");
    assert_eq!(block_on(meta.line_pos(repo, "trunk", draft)), Ok(Some(1)));
    assert_eq!(
        block_on(meta.scan_line_path(repo, "trunk", &p("b"), u64::MAX, 10)),
        Ok(vec![(1, draft)])
    );
    // `trunk` has no post-land testing.
    assert!(hooks.landed.lock().expect("landed").is_empty());
}

fn a_moved_tip_is_rebased_and_the_line_ends_with_both<M: MetaStore>(
    (f, _): (Fixture<M>, Arc<Recorder>),
) {
    let t = T::new(&f, &[("a", b"1")]);
    let one = t.commit(t.root, &[("b", b"2")]);
    let two = t.commit(t.root, &[("c", b"3")]);
    landed(t.land(one, "trunk"));
    let (id, version, pos, rebased) = landed(t.land(two, "trunk"));
    assert!(rebased);
    assert_ne!(id, two);
    assert_eq!((version, pos), (3, 2));
    let candidate = t.r.changeset(id);
    assert_eq!(candidate.changeset.parents, vec![one]);
    assert_eq!(
        candidate.changeset.metadata.get(REBASED_FROM),
        Some(&two.to_string())
    );
    assert_eq!(candidate.state, DraftState::Landed);
    assert_eq!(t.record(id).holder, None);
    let original = t.r.changeset(two);
    assert_eq!(original.state, DraftState::Superseded);
    assert_eq!(original.superseded_by, Some(id));
    assert_eq!(
        t.files(t.tip("trunk").target),
        files(&[("a", b"1"), ("b", b"2"), ("c", b"3")])
    );
}

fn same_file_land_reports_the_stale_base<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let one = t.commit(t.root, &[("a", b"one")]);
    let two = t.commit(t.root, &[("a", b"two")]);
    landed(t.land(one, "trunk"));
    match t.land(two, "trunk") {
        Err(ApiError::StaleBase(detail)) => {
            assert_eq!(detail.current_tip, one);
            assert_eq!(detail.paths, ["a"]);
            assert!(!detail.truncated);
            let debug = format!("{detail:?}");
            assert!(!debug.contains("one"));
            assert!(!debug.contains("two"));
        }
        other => panic!("expected StaleBase, got {other:?}"),
    }
    assert_eq!(t.tip("trunk").target, one);
    assert_eq!(t.kinds().last().map(String::as_str), Some("LandAttempt"));
    let last = t.r.journal().pop().expect("entry");
    assert!(
        last.body.contains("\"outcome\":\"conflict\""),
        "{}",
        last.body
    );
    // The loser is untouched.
    assert_eq!(t.r.changeset(two).state, DraftState::Abandoned);
}

fn stale_base_paths_are_bounded<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let seed: Vec<(String, String)> = (0..65)
        .map(|i| (format!("changed-{i:02}"), "base".to_owned()))
        .collect();
    let seed_files: Vec<(&str, &[u8])> = seed
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_bytes()))
        .collect();
    let t = T::new(&f, &seed_files);
    let tip: Vec<(String, String)> = (0..65)
        .map(|i| (format!("changed-{i:02}"), format!("tip-{i}")))
        .collect();
    let tip_files: Vec<(&str, &[u8])> = tip
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_bytes()))
        .collect();
    let one = t.commit(t.root, &tip_files);
    let ours: Vec<(String, String)> = (0..65)
        .map(|i| (format!("changed-{i:02}"), format!("ours-{i}")))
        .collect();
    let ours_files: Vec<(&str, &[u8])> = ours
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_bytes()))
        .collect();
    let two = t.commit(t.root, &ours_files);
    landed(t.land(one, "trunk"));
    match t.land(two, "trunk") {
        Err(ApiError::StaleBase(detail)) => {
            assert_eq!(detail.current_tip, one);
            assert_eq!(detail.paths.len(), 64);
            assert_eq!(detail.paths.first().map(String::as_str), Some("changed-00"));
            assert_eq!(detail.paths.last().map(String::as_str), Some("changed-63"));
            assert!(detail.truncated);
        }
        other => panic!("expected StaleBase, got {other:?}"),
    }
}

fn a_retried_land_returns_the_same_landed<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let one = t.commit(t.root, &[("b", b"2")]);
    let two = t.commit(t.root, &[("c", b"3")]);
    landed(t.land(one, "trunk"));
    let first = t.land(two, "trunk");
    // The line moves on; a retry still answers what happened.
    let three = t.commit(t.tip("trunk").target, &[("d", b"4")]);
    landed(t.land(three, "trunk"));
    let entries = t.r.journal().len();
    assert_eq!(t.land(two, "trunk"), first);
    assert_eq!(t.land(one, "trunk").map(|l| landed(Ok(l)).0), Ok(one));
    // A retry writes nothing.
    assert_eq!(t.r.journal().len(), entries);
}

fn a_stack_lands_draft_by_draft<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let ws = block_on(t.r.e().open_workspace(
        &t.r.cx,
        OpenWorkspace {
            repo: "r".to_owned(),
            name: Some("w".to_owned()),
            line: "trunk".to_owned(),
            at: None,
            lease_ms: 0,
        },
    ))
    .expect("open");
    let commit = |parent: ChangesetId, path: &str, data: &[u8]| {
        t.r.upload(&[data]);
        let mut req = commit_req("r", vec![parent], vec![put(path, data)]);
        req.workspace = Some("w".to_owned());
        block_on(t.r.e().create_changeset(&t.r.cx, req))
            .expect("commit")
            .id
    };
    let low = commit(ws.base, "b", b"2");
    let high = commit(low, "c", b"3");
    // Someone else lands first, so the whole stack is rebased.
    let other = t.commit(t.root, &[("z", b"9")]);
    landed(t.land(other, "trunk"));
    let (top, version, pos, rebased) = landed(t.land(high, "trunk"));
    assert!(rebased);
    assert_eq!((version, pos), (3, 3));
    let top_cs = t.r.changeset(top).changeset;
    let low_new = top_cs.first_parent().expect("parent");
    let low_cs = t.r.changeset(low_new);
    assert_eq!(low_cs.changeset.parents, vec![other]);
    assert_eq!(low_cs.state, DraftState::Landed);
    for (old, new) in [(low, low_new), (high, top)] {
        let view = t.r.changeset(old);
        assert_eq!(view.state, DraftState::Superseded);
        assert_eq!(view.superseded_by, Some(new));
    }
    // The workspace syncs onto the line and its stack is empty.
    let synced = block_on(t.r.e().sync(
        &t.r.cx,
        WorkspaceRef {
            repo: "r".to_owned(),
            name: "w".to_owned(),
        },
        None,
    ))
    .expect("sync");
    assert_eq!(synced.workspace.base, top);
    assert_eq!(synced.workspace.head, None);
}

fn changes_already_on_the_line_land_as_the_tip<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let one = t.commit(t.root, &[("b", b"2")]);
    let same = t.commit(t.root, &[("b", b"2"), ("a", b"1x")]);
    let again = t.commit(t.root, &[("b", b"2")]);
    landed(t.land(one, "trunk"));
    // `again` is `one` by content: it is the same changeset.
    assert_eq!(again, one);
    let three = t.commit(one, &[("a", b"1x")]);
    landed(t.land(three, "trunk"));
    let version = t.tip("trunk").version;
    let (id, v, pos, rebased) = landed(t.land(same, "trunk"));
    assert_eq!((id, v, pos, rebased), (three, version, 2, true));
    assert_eq!(t.tip("trunk").version, version);
    assert_eq!(t.r.changeset(same).superseded_by, Some(three));
}

fn land_needs_the_land_scope_and_write_on_every_path<M: MetaStore>(
    (f, _): (Fixture<M>, Arc<Recorder>),
) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("src/x", b"2"), ("doc", b"3")]);
    let writer = cx(&["repo:r:write"]);
    assert_eq!(
        t.land_as(&writer, draft, "trunk"),
        Err(ApiError::PermissionDenied {
            scope: "repo:r:land:trunk".to_owned(),
            path: None,
        })
    );
    let other_line = cx(&["repo:r:land:dev", "repo:r:write"]);
    assert!(matches!(
        t.land_as(&other_line, draft, "trunk"),
        Err(ApiError::PermissionDenied { .. })
    ));
    let narrow = cx(&["repo:r:land:trunk", "repo:r:write:src"]);
    assert_eq!(
        t.land_as(&narrow, draft, "trunk"),
        Err(ApiError::PermissionDenied {
            scope: "repo:r:write".to_owned(),
            path: Some("doc".to_owned()),
        })
    );
    let lander = cx(&["repo:r:land:trunk", "repo:r:write"]);
    landed(t.land_as(&lander, draft, "trunk"));
}

fn attested_main_needs_passing_checks<M: MetaStore>((f, hooks): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    let (policy, detail) = rejected(t.land(draft, "main"));
    assert_eq!(policy, "attested");
    assert!(detail.contains("ci/build"), "{detail}");
    t.attest(draft, "ci/build", true);
    let (_, detail) = rejected(t.land(draft, "main"));
    assert!(detail.contains("ci/test"), "{detail}");
    t.attest(draft, "ci/test", false);
    let (_, detail) = rejected(t.land(draft, "main"));
    assert!(detail.contains("failed"), "{detail}");
    assert!(detail.contains("https://ci.example/ci/test"), "{detail}");
    // The latest attestation counts.
    t.attest(draft, "ci/test", true);
    let (id, ..) = landed(t.land(draft, "main"));
    assert_eq!(id, draft);
    // Post-land CI is told about the move.
    let events = hooks.landed.lock().expect("landed").clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].landed, draft);
    assert_eq!(events[0].line, "main");
    assert_eq!(events[0].checks, ["ci/build", "ci/test"]);
}

fn attestations_survive_a_file_level_rebase<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    t.attest(draft, "ci/build", true);
    t.attest(draft, "ci/test", true);
    // The line moves; the land rebases without re-attesting.
    let other = t.commit(t.root, &[("c", b"3")]);
    t.attest(other, "ci/build", true);
    t.attest(other, "ci/test", true);
    landed(t.land(other, "main"));
    // A rebased copy of the draft (what a sync makes) inherits them too.
    let rebased = block_on(t.r.e().rebase(
        &t.r.cx,
        Rebase {
            repo: "r".to_owned(),
            changeset: draft,
            onto: other,
            merge: MergeMode::File,
            dry_run: false,
        },
    ))
    .expect("rebase");
    assert_ne!(rebased, draft);
    let (id, _, _, was_rebased) = landed(t.land(rebased, "main"));
    assert_eq!((id, was_rebased), (rebased, false));
}

fn agents_cannot_attest<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    assert_eq!(
        t.attest_as(PrincipalKind::Agent, draft, "ci/test", true),
        Err(ApiError::PermissionDenied {
            scope: "repo:r:attest".to_owned(),
            path: None,
        })
    );
    let reader = cx(&["repo:r:read"]);
    let req = Attest {
        repo: "r".to_owned(),
        changeset: draft,
        check: "ci/test".to_owned(),
        passed: true,
        url: String::new(),
    };
    assert!(matches!(
        block_on(t.r.e().attest(&reader, req.clone())),
        Err(ApiError::PermissionDenied { .. })
    ));
    let mut bad = req.clone();
    bad.check = "ci test".to_owned();
    assert!(matches!(
        block_on(t.r.e().attest(&t.r.cx, bad)),
        Err(ApiError::InvalidChange { .. })
    ));
    let mut long = req.clone();
    long.url = "u".repeat(graft_engine::MAX_ATTEST_URL + 1);
    assert!(matches!(
        block_on(t.r.e().attest(&t.r.cx, long)),
        Err(ApiError::LimitExceeded { .. })
    ));
    let mut unknown = req;
    unknown.changeset = ChangesetId(graft_model::ObjectId([9; 32]));
    assert!(matches!(
        block_on(t.r.e().attest(&t.r.cx, unknown)),
        Err(ApiError::NotFound { .. })
    ));
    for kind in [
        PrincipalKind::Ci,
        PrincipalKind::Service,
        PrincipalKind::Human,
    ] {
        t.attest_as(kind, draft, "ci/test", true).expect("attest");
    }
    let kinds = t.kinds();
    assert_eq!(kinds.iter().filter(|k| *k == "Attested").count(), 3);
}

fn attest_needs_the_attest_scope<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    // `write` used to admit `Attest`; it no longer does (WI-2272), on any
    // prefix, and neither do `read` or `land`.
    for scopes in [
        &["repo:r:write"][..],
        &["repo:r:write:.ci"],
        &["repo:r:read", "repo:r:land:main"],
        &["repo:s:attest"],
    ] {
        assert_eq!(
            t.attest_as_cx(&cx(scopes), draft, "ci/test", true),
            denied_attest(),
            "{scopes:?}"
        );
    }
    // `attest` on its own, or `admin`, does.
    t.attest_as_cx(&cx(&["repo:r:attest"]), draft, "ci/test", true)
        .expect("attest scope");
    t.attest_as_cx(&cx(&["repo:r:admin"]), draft, "ci/test", true)
        .expect("admin");
}

fn attest_only_cannot_commit_or_land<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    let attester = cx(&["repo:r:attest"]);
    let commit = block_on(t.r.e().create_changeset(
        &attester,
        commit_req("r", vec![t.root], vec![put("c", b"3")]),
    ));
    assert!(
        matches!(commit, Err(ApiError::PermissionDenied { .. })),
        "{commit:?}"
    );
    assert!(matches!(
        t.land_as(&attester, draft, "trunk"),
        Err(ApiError::PermissionDenied { .. })
    ));
    // Nor can the CI token, which also reads.
    assert!(matches!(
        t.land_as(&ci_cx(draft), draft, "trunk"),
        Err(ApiError::PermissionDenied { .. })
    ));
    assert_eq!(t.tip("trunk").target, t.root);
}

fn ci_attests_only_its_pinned_changeset<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    let other = t.commit(t.root, &[("c", b"3")]);
    let pinned = ci_cx(draft);
    assert_eq!(
        t.attest_as_cx(&pinned, other, "ci/test", true),
        denied_attest()
    );
    // Not pinned at all: refused, even with admin.
    for subject in ["ci:github:o/r", "ci:github:o/r#", "ci-bot"] {
        let mut unpinned = cx(&["repo:r:admin"]);
        unpinned.principal.kind = PrincipalKind::Ci;
        unpinned.principal.subject = subject.to_owned();
        assert_eq!(
            t.attest_as_cx(&unpinned, draft, "ci/test", true),
            denied_attest(),
            "{subject}"
        );
    }
    // A pin that names a prefix of the id, not the id, is no pin.
    let mut prefix = ci_cx(draft);
    prefix.principal.subject = format!("ci:github:o/r#{}", &draft.to_string()[..13]);
    assert_eq!(
        t.attest_as_cx(&prefix, draft, "ci/test", true),
        denied_attest()
    );
    // The refusal comes before the changeset is looked up, so it says
    // nothing about which changesets exist.
    let missing = ChangesetId(graft_model::ObjectId([9; 32]));
    assert_eq!(
        t.attest_as_cx(&pinned, missing, "ci/test", true),
        denied_attest()
    );
    assert_eq!(t.kinds().iter().filter(|k| *k == "Attested").count(), 0);
    t.attest_as_cx(&pinned, draft, "ci/test", true)
        .expect("pinned changeset");
}

fn path_rules_need_their_checks_and_a_person<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    // Outside every rule: direct.
    let plain = t.commit(t.root, &[("src/x", b"2")]);
    landed(t.land(plain, "trunk"));
    let tip = t.tip("trunk").target;
    let deploy = t.commit(tip, &[("deploy/app.yaml", b"3")]);
    let (policy, detail) = rejected(t.land(deploy, "trunk"));
    assert_eq!(policy, "path-rule");
    assert!(detail.contains("ci/deploy"), "{detail}");
    t.attest(deploy, "ci/deploy", true);
    let (policy, detail) = rejected(t.land(deploy, "trunk"));
    assert_eq!(policy, "human-review");
    assert!(detail.contains(HUMAN_REVIEW), "{detail}");
    // CI cannot stand in for a person.
    t.attest(deploy, HUMAN_REVIEW, true);
    assert_eq!(rejected(t.land(deploy, "trunk")).0, "human-review");
    t.attest_as(PrincipalKind::Human, deploy, HUMAN_REVIEW, true)
        .expect("review");
    landed(t.land(deploy, "trunk"));
    // A rule matches any draft in the stack, not only the head.
    let tip = t.tip("trunk").target;
    let low = t.commit(tip, &[("docs/x.md", b"4")]);
    let high = t.commit(low, &[("src/y", b"5")]);
    assert_eq!(rejected(t.land(high, "trunk")).0, "path-rule");
    t.attest(high, "ci/docs", true);
    landed(t.land(high, "trunk"));
}

fn queued_lines_land_attested_candidates<M: MetaStore>((f, hooks): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let one = t.commit(t.root, &[("b", b"2")]);
    let two = t.commit(t.root, &[("c", b"3")]);
    // A candidate that is the head itself is tested too.
    let (policy, detail) = rejected(t.land(one, "rel"));
    assert_eq!(policy, "queued");
    assert!(detail.contains(&one.to_string()), "{detail}");
    t.attest(one, "ci/test", true);
    landed(t.land(one, "rel"));
    // A rebased candidate: the hook refuses, CI attests the candidate, and
    // the retry (same tip, so the same candidate) lands it.
    let (_, detail) = rejected(t.land(two, "rel"));
    let candidate: ChangesetId = detail
        .strip_prefix("candidate ")
        .and_then(|d| d.split(':').next())
        .and_then(|id| id.parse().ok())
        .expect("candidate id in the detail");
    assert_ne!(candidate, two);
    let tested = hooks.tested.lock().expect("tested").clone();
    let last = tested.last().expect("tested");
    assert_eq!((last.head, last.candidate, last.tip), (two, candidate, one));
    assert_eq!(last.checks, ["ci/test"]);
    // The head's own attestation does not count for its candidate.
    t.attest(two, "ci/test", true);
    assert_eq!(rejected(t.land(two, "rel")).0, "queued");
    t.attest(candidate, "ci/test", true);
    let (id, _, _, rebased) = landed(t.land(two, "rel"));
    assert_eq!((id, rebased), (candidate, true));
    // The Rebase API agrees on the candidate.
    let rebased = block_on(t.r.e().rebase(
        &t.r.cx,
        Rebase {
            repo: "r".to_owned(),
            changeset: two,
            onto: one,
            merge: MergeMode::File,
            dry_run: false,
        },
    ))
    .expect("rebase");
    assert_eq!(rebased, candidate);
}

fn queued_lines_ask_the_ci_hook<M: MetaStore>((f, hooks): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    *hooks.verdict.lock().expect("verdict") = Some(Verdict::Fail("ci/test red".to_owned()));
    let (_, detail) = rejected(t.land(draft, "rel"));
    assert!(detail.ends_with("ci/test red"), "{detail}");
    *hooks.verdict.lock().expect("verdict") = Some(Verdict::Pass);
    landed(t.land(draft, "rel"));
}

fn candidates_are_held_by_a_land_lease<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let one = t.commit(t.root, &[("b", b"2")]);
    let two = t.commit(t.root, &[("c", b"3")]);
    t.attest(one, "ci/test", true);
    landed(t.land(one, "rel"));
    let (_, detail) = rejected(t.land(two, "rel"));
    let candidate: ChangesetId = detail
        .strip_prefix("candidate ")
        .and_then(|d| d.split(':').next())
        .and_then(|id| id.parse().ok())
        .expect("candidate id");
    let record = t.record(candidate);
    assert_eq!(record.state, RecordState::Draft);
    let holder = record.holder.expect("a holder");
    let expires: i64 = holder
        .strip_prefix("land:")
        .and_then(|rest| rest.split_once(':'))
        .and_then(|(ms, _)| ms.parse().ok())
        .expect("land:<expires>:<attempt>");
    assert_eq!(expires, 1_000_000 + 10 * 60 * 1000);
    let created =
        t.r.journal()
            .into_iter()
            .rev()
            .find(|e| e.kind == "ChangesetCreated" && e.body.contains(&candidate.to_string()));
    let body = created.expect("journalled").body;
    assert!(body.contains(&format!("\"holder\":\"{holder}\"")), "{body}");
    // Landing it clears the lease.
    t.attest(candidate, "ci/test", true);
    landed(t.land(two, "rel"));
    let record = t.record(candidate);
    assert_eq!((record.state, record.holder), (RecordState::Landed, None));
}

fn post_land_failures_request_a_revert<M: MetaStore>((f, hooks): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    t.attest(draft, "ci/build", true);
    t.attest(draft, "ci/test", true);
    // A failure before landing asks for nothing.
    let pending = t.commit(t.root, &[("c", b"3")]);
    t.attest(pending, "ci/test", false);
    landed(t.land(draft, "main"));
    // A passing post-land run, or a check that is not post-land, neither.
    t.attest(draft, "ci/test", true);
    t.attest(draft, "ci/lint", false);
    assert!(hooks.reverts.lock().expect("reverts").is_empty());
    assert!(!t.kinds().iter().any(|k| k == "RevertRequested"));
    // A failing post-land check on the landed changeset does.
    t.attest(draft, "ci/test", false);
    let reverts = hooks.reverts.lock().expect("reverts").clone();
    assert_eq!(reverts.len(), 1);
    assert_eq!(reverts[0].line, "main");
    assert_eq!(reverts[0].changeset, draft);
    assert_eq!(reverts[0].check, "ci/test");
    let last = t.r.journal().pop().expect("entry");
    assert_eq!(last.kind, "RevertRequested");
    assert!(last.body.contains(&draft.to_string()), "{}", last.body);
    // `trunk` has no post-land testing: its failures ask for nothing.
    let other = t.commit(t.root, &[("d", b"4")]);
    landed(t.land(other, "trunk"));
    t.attest(other, "ci/test", false);
    assert_eq!(hooks.reverts.lock().expect("reverts").len(), 1);
}

fn refused_lands<M: MetaStore>((f, _): (Fixture<M>, Arc<Recorder>)) {
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    assert_eq!(rejected(t.land(draft, "gp")).0, "authority");
    let mut squash = land_req(draft, "trunk");
    squash.squash = true;
    assert_eq!(
        rejected(block_on(t.r.e().land(&t.r.cx, squash))).0,
        "squash"
    );
    assert!(matches!(
        t.land(draft, "nope"),
        Err(ApiError::NotFound { .. })
    ));
    t.r.update_ref("tag", None, t.root, RefKind::Tag)
        .expect("tag");
    assert_eq!(
        t.land(draft, "tag"),
        Err(ApiError::InvalidChange {
            path: String::new(),
            rule: "not-a-line".to_owned(),
        })
    );
    assert!(matches!(
        t.land(draft, "bad name"),
        Err(ApiError::InvalidChange { .. })
    ));
    // The root is landed already.
    assert_eq!(
        t.land(t.root, "trunk"),
        Err(ApiError::InvalidChange {
            path: String::new(),
            rule: "draft-landed".to_owned(),
        })
    );
    assert!(matches!(
        t.land(ChangesetId(graft_model::ObjectId([4; 32])), "trunk"),
        Err(ApiError::NotFound { .. })
    ));
}

on_both![
    fast_forward_lands_the_head,
    a_moved_tip_is_rebased_and_the_line_ends_with_both,
    same_file_land_reports_the_stale_base,
    stale_base_paths_are_bounded,
    a_retried_land_returns_the_same_landed,
    a_stack_lands_draft_by_draft,
    changes_already_on_the_line_land_as_the_tip,
    land_needs_the_land_scope_and_write_on_every_path,
    attested_main_needs_passing_checks,
    attestations_survive_a_file_level_rebase,
    agents_cannot_attest,
    attest_needs_the_attest_scope,
    attest_only_cannot_commit_or_land,
    ci_attests_only_its_pinned_changeset,
    path_rules_need_their_checks_and_a_person,
    queued_lines_land_attested_candidates,
    queued_lines_ask_the_ci_hook,
    candidates_are_held_by_a_land_lease,
    post_land_failures_request_a_revert,
    refused_lands,
];

/// Decision D22: with the default config every line, `main` included, is
/// `direct`. A draft lands on `main` with no attestations, nothing is
/// queued for a candidate test and no post-land check runs. (D6 is opt-in:
/// `attested_main_needs_passing_checks` configures it and still gates.)
fn the_default_main_lands_directly<M: MetaStore>((f, hooks): (Fixture<M>, Arc<Recorder>)) {
    assert!(EngineConfig::default().lines.is_empty());
    assert_eq!(
        EngineConfig::default().default_line.policy,
        LandPolicy::Direct
    );
    let t = T::new(&f, &[("a", b"1")]);
    let draft = t.commit(t.root, &[("b", b"2")]);
    let (id, version, pos, rebased) = landed(t.land(draft, "main"));
    assert_eq!((id, version, pos, rebased), (draft, 2, 1, false));
    assert_eq!(t.tip("main").target, draft);
    assert!(hooks.landed.lock().expect("landed").is_empty());
    assert!(hooks.tested.lock().expect("tested").is_empty());
    assert!(hooks.reverts.lock().expect("reverts").is_empty());
}

mod default_config {
    #[test]
    fn memory_store() {
        super::the_default_main_lands_directly(super::hooked(super::memory()));
    }
    #[test]
    fn sqlite_store() {
        super::the_default_main_lands_directly(super::hooked(super::sqlite()));
    }
}

/// Two lands of disjoint files, polled in lockstep, so both read the same
/// tip before either commits: the second loses its CAS. With one attempt
/// it is `Contended`; with two it rebases and lands.
#[test]
fn lockstep_racers_lose_one_cas() {
    for (attempts, both_land) in [(1, false), (2, true)] {
        let config = EngineConfig {
            land_attempts: attempts,
            ..EngineConfig::default()
        };
        let f = build(Arc::new(Yielding(MemoryStore::new())), config, None)
            .map_engine(|e| e.with_sleeper(Arc::new(YieldSleeper)));
        let r = Repo::new(&f, "r");
        let root = r.commit(vec![], &[("a", b"1")], &[]).id;
        r.update_ref("trunk", None, root, RefKind::Line)
            .expect("line");
        let one = r.commit(vec![root], &[("b", b"2")], &[]).id;
        let two = r.commit(vec![root], &[("c", b"3")], &[]).id;
        let tasks: Vec<Task<'_, ApiResult<Landed>>> = [one, two]
            .into_iter()
            .map(|head| {
                let (e, cx) = (r.e(), r.cx.clone());
                Box::pin(async move { e.land(&cx, land_req(head, "trunk")).await }) as Task<'_, _>
            })
            .collect();
        let results = run_scheduled(tasks, &[0, 1], || {});
        assert_eq!(landed(results[0].clone()).0, one);
        if both_land {
            let (id, version, pos, rebased) = landed(results[1].clone());
            assert!(rebased);
            assert_eq!((version, pos), (3, 2));
            assert_eq!(r.changeset(id).changeset.parents, vec![one]);
        } else {
            assert_eq!(
                results[1],
                Err(ApiError::Contended {
                    line: "trunk".to_owned(),
                    tip: one,
                })
            );
            // The loser's draft is untouched; it lands on a retry.
            assert_eq!(r.changeset(two).state, DraftState::Abandoned);
            landed(block_on(r.e().land(&r.cx, land_req(two, "trunk"))));
        }
    }
}

/// Two lands of the same head in lockstep: one moves the line, the other
/// loses its CAS, finds the land record and returns the same result.
#[test]
fn lockstep_lands_of_one_head_agree() {
    let f = build(
        Arc::new(Yielding(MemoryStore::new())),
        EngineConfig::default(),
        None,
    )
    .map_engine(|e| e.with_sleeper(Arc::new(YieldSleeper)));
    let r = Repo::new(&f, "r");
    let root = r.commit(vec![], &[("a", b"1")], &[]).id;
    r.update_ref("trunk", None, root, RefKind::Line)
        .expect("line");
    let head = r.commit(vec![root], &[("b", b"2")], &[]).id;
    let tasks: Vec<Task<'_, ApiResult<Landed>>> = (0..2)
        .map(|_| {
            let (e, cx) = (r.e(), r.cx.clone());
            Box::pin(async move { e.land(&cx, land_req(head, "trunk")).await }) as Task<'_, _>
        })
        .collect();
    let results = run_scheduled(tasks, &[0, 1], || {});
    assert_eq!(results[0], results[1]);
    assert_eq!(landed(results[0].clone()), (head, 2, 1, false));
}
