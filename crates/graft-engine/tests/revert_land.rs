//! D42 (WI-3358): `Revert` with `land = <line>` builds the revert and lands
//! it in one call, needing `read` and a named-repository `land:<line>`.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::counting::Counting;
use common::race::{Task, YieldSleeper, Yielding, run_scheduled};
use common::*;
use graft_engine::policy::Authority;
use graft_engine::{EngineConfig, LineConfig};
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, RepoApi};
use graft_store::MetaStore;
use graft_store::memory::MemoryStore;

fn config() -> EngineConfig {
    let mut gp = LineConfig::direct("gp");
    gp.authority = Authority::GitPrimary;
    EngineConfig {
        lines: vec![gp],
        ..EngineConfig::default()
    }
}

struct T<'f, M> {
    r: Repo<'f, M>,
    root: ChangesetId,
}

impl<'f, M: MetaStore> T<'f, M> {
    fn new(f: &'f Fixture<M>) -> Self {
        let r = Repo::new(f, "r");
        let root = r.commit(vec![], &[("a", b"a0")], &[]).id;
        for line in ["main", "other", "gp"] {
            r.update_ref(line, None, root, RefKind::Line).expect("line");
        }
        Self { r, root }
    }

    fn land_req(head: ChangesetId, line: &str) -> Land {
        Land {
            repo: "r".to_owned(),
            head,
            line: line.to_owned(),
            squash: false,
            merge: MergeMode::Line,
            deadline_ms: None,
            merged_from: None,
        }
    }

    /// Commits `files` on `line`'s tip and lands it.
    fn land(&self, line: &str, files: &[(&str, &[u8])]) -> ChangesetId {
        let id = self.r.commit(vec![self.tip(line)], files, &[]).id;
        match block_on(self.r.e().land(&self.r.cx, Self::land_req(id, line))).expect("land") {
            Landed::Landed { landed, .. } => landed,
            other => panic!("not landed: {other:?}"),
        }
    }

    fn tip(&self, line: &str) -> ChangesetId {
        block_on(self.r.e().resolve_ref(
            &self.r.cx,
            ResolveRef {
                repo: "r".to_owned(),
                name: line.to_owned(),
            },
        ))
        .expect("resolve")
        .target
    }

    fn files(&self, id: ChangesetId) -> BTreeMap<String, Vec<u8>> {
        self.r.fetch(self.r.changeset(id).changeset.root)
    }

    fn revert(
        &self,
        cx: &CallContext,
        changeset: ChangesetId,
        land: Option<&str>,
    ) -> ApiResult<ChangesetId> {
        block_on(self.r.e().revert(
            cx,
            Revert {
                repo: "r".to_owned(),
                changeset,
                // Deliberately wrong: with `land` the tip is used instead.
                onto: self.root,
                workspace: None,
                check_url: None,
                land: land.map(str::to_owned),
            },
        ))
    }

    fn lands_journaled(&self) -> usize {
        self.r
            .journal()
            .iter()
            .filter(|e| e.kind == "LandAttempt")
            .count()
    }
}

fn rule(r: ApiResult<ChangesetId>) -> String {
    match r {
        Err(ApiError::InvalidChange { rule, .. }) => rule,
        other => panic!("not an invalid change: {other:?}"),
    }
}

fn denied<X: std::fmt::Debug>(r: ApiResult<X>) {
    assert!(matches!(r, Err(ApiError::PermissionDenied { .. })), "{r:?}");
}

fn worker(scopes: &[&str]) -> CallContext {
    cx(scopes)
}

macro_rules! on_both {
    ($($name:ident),* $(,)?) => {
        mod memory_store {
            $( #[test] fn $name() { super::$name(&super::memory_with(super::config())) } )*
        }
        mod sqlite_store {
            $( #[test] fn $name() { super::$name(&super::sqlite_with(super::config())) } )*
        }
    };
}

// 1: read + land:L reverts c on L, in one journaled land.
fn reverts_and_lands<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let c = t.land("main", &[("b", b"b1")]);
    let tip = t.land("main", &[("c", b"c1"), ("a", b"a1")]);
    let w = worker(&["repo:r:read", "repo:r:land:main"]);
    let before = t.lands_journaled();
    let r = t.revert(&w, c, Some("main")).expect("revert-land");
    assert_eq!(t.tip("main"), r);
    assert_eq!(t.r.changeset(r).changeset.parents, vec![tip]);
    // `c`'s path is back to the parent's entry (absent); later work stays.
    let files = t.files(r);
    assert!(!files.contains_key("b"));
    assert_eq!(files["c"], b"c1");
    assert_eq!(files["a"], b"a1");
    assert_eq!(t.lands_journaled(), before + 1);
    assert_eq!(
        t.r.changeset(r)
            .changeset
            .metadata
            .get(graft_engine::REVERTS),
        Some(&c.to_string())
    );
}

// 2: the same token without `land` needs path writes.
fn without_land_needs_write<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let c = t.land("main", &[("b", b"b1")]);
    let w = worker(&["repo:r:read", "repo:r:land:main"]);
    denied(t.revert(&w, c, None));
    assert_eq!(t.tip("main"), c);
}

// 3: `c` must be on the line: not on another line, not a draft.
fn must_be_on_the_line<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let on_other = t.land("other", &[("o", b"o1")]);
    let draft = t.r.commit(vec![t.root], &[("d", b"d1")], &[]).id;
    let tip = t.land("main", &[("b", b"b1")]);
    let w = worker(&["repo:r:read", "repo:r:land:main"]);
    for c in [on_other, draft] {
        assert_eq!(rule(t.revert(&w, c, Some("main"))), "revert-not-on-line");
    }
    assert_eq!(t.tip("main"), tip);
}

// 4: land:M does not let a revert land on L; a wildcard repository does not
// authorise it on any line.
fn scope_must_name_the_line_and_repo<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let c = t.land("main", &[("b", b"b1")]);
    denied(t.revert(&worker(&["repo:r:land:other"]), c, Some("main")));
    denied(t.revert(&worker(&["repo:*:land:main"]), c, Some("main")));
    denied(t.revert(&worker(&["repo:*:admin"]), c, Some("main")));
    assert_eq!(t.tip("main"), c);
    // `admin` on the named repository does.
    t.revert(&worker(&["repo:r:admin"]), c, Some("main"))
        .expect("admin");
}

// 5: `graft.reverts` grants nothing: a forged changeset landed with only
// `land:L` is refused outside the write prefix.
fn graft_reverts_grants_nothing<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let c = t.land("main", &[("b", b"b1")]);
    // The reserved metadata cannot be set on create, but a revert built
    // without `land` carries it: store one, then try to land it with only
    // `land:main`.
    let forged = block_on(t.r.e().revert(
        &t.r.cx,
        Revert {
            repo: "r".to_owned(),
            changeset: c,
            onto: c,
            workspace: None,
            check_url: None,
            land: None,
        },
    ))
    .expect("revert without land");
    assert_eq!(
        t.r.changeset(forged)
            .changeset
            .metadata
            .get(graft_engine::REVERTS),
        Some(&c.to_string())
    );
    let w = worker(&["repo:r:land:main", "repo:r:write:docs"]);
    denied(block_on(t.r.e().land(&w, T::<M>::land_req(forged, "main"))));
    assert_eq!(t.tip("main"), c);
}

// 7: a retry after success is `revert-empty` and moves nothing.
fn a_retry_is_empty<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let c = t.land("main", &[("b", b"b1")]);
    t.land("main", &[("c", b"c1")]);
    let w = worker(&["repo:r:read", "repo:r:land:main"]);
    let r = t.revert(&w, c, Some("main")).expect("revert");
    assert_eq!(rule(t.revert(&w, c, Some("main"))), "revert-empty");
    assert_eq!(t.tip("main"), r);
}

// 8: a merge point is inverted against its first parent.
fn a_merge_point_reverts<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    t.land("other", &[("s", b"s1")]);
    t.land("main", &[("m", b"m1")]);
    let merged = match block_on(t.r.e().line_merge(
        &t.r.cx,
        LineMerge {
            repo: "r".to_owned(),
            src: "other".to_owned(),
            dst: "main".to_owned(),
            src_at: None,
            merge: MergeMode::Line,
            deadline_ms: None,
            message: None,
        },
    ))
    .expect("merge")
    {
        LineMergeOutcome::Merged { landed, .. } => landed,
        other => panic!("{other:?}"),
    };
    assert_eq!(t.files(merged)["s"], b"s1");
    t.land("main", &[("x", b"x1")]);
    let w = worker(&["repo:r:read", "repo:r:land:main"]);
    let r = t.revert(&w, merged, Some("main")).expect("revert merge");
    let files = t.files(r);
    assert!(!files.contains_key("s"));
    assert_eq!(files["m"], b"m1");
    assert_eq!(files["x"], b"x1");
    assert_eq!(t.tip("main"), r);
}

// 9: git-primary and followed lines refuse; the line stays put.
fn guards_hold<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let c = t.land("other", &[("b", b"b1")]);
    // `c` must be on `gp` to get past the ancestry check: move `gp` there.
    t.r.update_ref("gp", Some(t.root), c, RefKind::Line)
        .expect("gp");
    let w = worker(&["repo:r:read", "repo:r:land:gp", "repo:r:land:other"]);
    assert!(matches!(
        t.revert(&w, c, Some("gp")),
        Err(ApiError::Rejected { .. })
    ));
    assert_eq!(t.tip("gp"), c);
    t.r.update_ref(&followed_ref("other"), None, c, RefKind::Tool)
        .expect("marker");
    assert!(matches!(
        t.revert(&w, c, Some("other")),
        Err(ApiError::Rejected { .. })
    ));
    assert_eq!(t.tip("other"), c);
}

on_both!(
    reverts_and_lands,
    without_land_needs_write,
    must_be_on_the_line,
    scope_must_name_the_line_and_repo,
    graft_reverts_grants_nothing,
    a_retry_is_empty,
    a_merge_point_reverts,
    guards_hold,
);

// 6: the tip moves between the build and the CAS (a land injected in
// lockstep): the revert retries and the line holds both.
#[test]
fn a_moved_tip_retries() {
    let f = build(Arc::new(Yielding(MemoryStore::new())), config(), None)
        .map_engine(|e| e.with_sleeper(Arc::new(YieldSleeper)));
    let t = T::new(&f);
    let c = t.land("main", &[("b", b"b1")]);
    let other = t.r.commit(vec![c], &[("n", b"n1")], &[]).id;
    let w = worker(&["repo:r:read", "repo:r:land:main"]);
    let tasks: Vec<Task<'_, ApiResult<ChangesetId>>> = vec![
        Box::pin(async {
            match t
                .r
                .e()
                .land(&t.r.cx, T::<MemoryStore>::land_req(other, "main"))
                .await?
            {
                Landed::Landed { landed, .. } => Ok(landed),
                o => panic!("{o:?}"),
            }
        }),
        Box::pin(async {
            t.r.e()
                .revert(
                    &w,
                    Revert {
                        repo: "r".to_owned(),
                        changeset: c,
                        onto: c,
                        workspace: None,
                        check_url: None,
                        land: Some("main".to_owned()),
                    },
                )
                .await
        }),
    ];
    let results = run_scheduled(tasks, &[0, 1], || {});
    let landed_other = *results[0].as_ref().expect("land");
    let reverted = *results[1].as_ref().expect("revert");
    // The revert was built on `c`, lost the CAS, and landed on the other
    // land: both are on the line, in that order.
    assert_eq!(t.tip("main"), reverted);
    assert_eq!(
        t.r.changeset(reverted).changeset.parents,
        vec![landed_other]
    );
    let files = t.files(reverted);
    assert!(!files.contains_key("b"));
    assert_eq!(files["n"], b"n1");
}

// 10: the revert's store calls do not follow history depth.
#[test]
fn ancestry_cost_does_not_follow_depth() {
    let cost = |depth: usize| {
        let meta = Arc::new(Counting::<MemoryStore>::default());
        let f = build(meta.clone(), config(), None);
        let t = T::new(&f);
        let c = t.land("main", &[("b", b"b1")]);
        for i in 0..depth {
            t.land("main", &[("f", format!("{i}").as_bytes())]);
        }
        let w = worker(&["repo:r:read", "repo:r:land:main"]);
        let before = meta.calls.load(Ordering::SeqCst);
        t.revert(&w, c, Some("main")).expect("revert");
        meta.calls.load(Ordering::SeqCst) - before
    };
    let costs: Vec<u64> = [40, 80, 160].into_iter().map(cost).collect();
    // The index's skip lists add at most a call per doubling of depth; a walk
    // would add one per changeset (120 more here).
    assert!(costs[2] <= costs[0] + 4, "{costs:?}");
}
