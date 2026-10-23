//! G3.7: line merge (D16) in `Rebase`, `Sync` and `Land`, `attest_merged`,
//! `Revert` and the `Stack` operations, on both stores.

mod common;

use std::collections::BTreeMap;

use common::*;
use graft_engine::policy::LandPolicy;
use graft_engine::{EngineConfig, LineConfig, MERGED_PATHS, REBASED_FROM, REVERTS};
use graft_model::ChangesetId;
use graft_model::merge::{Merge3, merge3};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, PrincipalKind, RepoApi, WorkspaceApi};
use graft_store::{MetaStore, RecordState};

/// `main` is direct; `att` is attested with `attest_merged`; `soft` is
/// attested without it.
fn config() -> EngineConfig {
    let checks = vec!["ci/test".to_owned()];
    let mut att = LineConfig::direct("att");
    att.policy = LandPolicy::Attested {
        checks: checks.clone(),
        attest_merged: true,
    };
    let mut soft = LineConfig::direct("soft");
    soft.policy = LandPolicy::Attested {
        checks,
        attest_merged: false,
    };
    EngineConfig {
        lines: vec![LineConfig::direct("main"), att, soft],
        ..EngineConfig::default()
    }
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

on_both!(
    two_lands_on_different_regions_both_land,
    file_mode_and_overlapping_edits_still_conflict,
    only_small_text_files_merge_by_line,
    the_executable_bit_follows_the_side_that_changed_it,
    sync_merges_by_line,
    attest_merged_forces_re_attesting,
    attestations_survive_a_merge_without_attest_merged,
    revert_of_revert_restores_the_original_tree,
    revert_onto_a_later_tip_merges_by_line,
    revert_into_a_workspace,
    reorder_replays_the_drafts,
    squash_joins_adjacent_drafts,
    split_by_path_groups,
    drop_removes_a_draft_and_rebases_the_rest,
    stack_ops_refuse_bad_requests,
);

const BASE: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
const TOP: &[u8] = b"ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
const BOTTOM: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n";
const BOTH: &[u8] = b"ONE\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n";

/// A repository whose lines all start at a root holding `files`.
struct T<'f, M> {
    r: Repo<'f, M>,
    root: ChangesetId,
}

impl<'f, M: MetaStore> T<'f, M> {
    fn new(f: &'f Fixture<M>, files: &[(&str, &[u8])]) -> Self {
        let r = Repo::new(f, "r");
        let root = r.commit(vec![], files, &[]).id;
        for line in ["main", "att", "soft"] {
            r.update_ref(line, None, root, RefKind::Line).expect("line");
        }
        Self { r, root }
    }

    fn commit(&self, parent: ChangesetId, files: &[(&str, &[u8])]) -> ChangesetId {
        self.r.commit(vec![parent], files, &[]).id
    }

    fn commit_in(&self, ws: &str, parent: ChangesetId, files: &[(&str, &[u8])]) -> ChangesetId {
        self.commit_changes_in(
            ws,
            parent,
            files.iter().map(|(p, b)| put(p, b)).collect(),
            files,
        )
    }

    fn commit_changes_in(
        &self,
        ws: &str,
        parent: ChangesetId,
        mut changes: Vec<graft_model::Change>,
        blobs: &[(&str, &[u8])],
    ) -> ChangesetId {
        let blobs: Vec<&[u8]> = blobs.iter().map(|(_, b)| *b).collect();
        if !blobs.is_empty() {
            self.r.upload(&blobs);
        }
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        let mut req = commit_req("r", vec![parent], changes);
        req.workspace = Some(ws.to_owned());
        block_on(self.r.e().create_changeset(&self.r.cx, req))
            .expect("commit")
            .id
    }

    fn land_with(&self, head: ChangesetId, line: &str, merge: MergeMode) -> ApiResult<Landed> {
        block_on(self.r.e().land(
            &self.r.cx,
            Land {
                repo: "r".to_owned(),
                head,
                line: line.to_owned(),
                squash: false,
                merge,
                deadline_ms: None,
            },
        ))
    }

    fn land(&self, head: ChangesetId, line: &str) -> ApiResult<Landed> {
        self.land_with(head, line, MergeMode::Line)
    }

    fn rebase(
        &self,
        changeset: ChangesetId,
        onto: ChangesetId,
        merge: MergeMode,
    ) -> ApiResult<ChangesetId> {
        block_on(self.r.e().rebase(
            &self.r.cx,
            Rebase {
                repo: "r".to_owned(),
                changeset,
                onto,
                merge,
            },
        ))
    }

    fn revert(
        &self,
        changeset: ChangesetId,
        onto: ChangesetId,
        workspace: Option<&str>,
    ) -> ApiResult<ChangesetId> {
        block_on(self.r.e().revert(
            &self.r.cx,
            Revert {
                repo: "r".to_owned(),
                changeset,
                onto,
                workspace: workspace.map(str::to_owned),
            },
        ))
    }

    fn attest(&self, id: ChangesetId, check: &str) {
        let mut cx = self.r.cx.clone();
        cx.principal.kind = PrincipalKind::Ci;
        cx.principal.subject = "ci".to_owned();
        block_on(self.r.e().attest(
            &cx,
            Attest {
                repo: "r".to_owned(),
                changeset: id,
                check: check.to_owned(),
                passed: true,
                url: "https://ci.example/1".to_owned(),
            },
        ))
        .expect("attest");
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

    fn cs(&self, id: ChangesetId) -> graft_model::Changeset {
        self.r.changeset(id).changeset
    }

    fn wref(&self, name: &str) -> WorkspaceRef {
        WorkspaceRef {
            repo: "r".to_owned(),
            name: name.to_owned(),
        }
    }

    fn open(&self, name: &str, line: &str) -> WorkspaceView {
        block_on(self.r.e().open_workspace(
            &self.r.cx,
            OpenWorkspace {
                repo: "r".to_owned(),
                name: Some(name.to_owned()),
                line: line.to_owned(),
                at: None,
                lease_ms: 0,
            },
        ))
        .expect("open")
    }

    fn ws(&self, name: &str) -> WorkspaceView {
        block_on(self.r.e().get_workspace(&self.r.cx, self.wref(name))).expect("workspace")
    }

    fn sync(&self, name: &str) -> ApiResult<Synced> {
        block_on(self.r.e().sync(&self.r.cx, self.wref(name), None))
    }

    fn stack(&self, name: &str, op: StackOp) -> ApiResult<WorkspaceView> {
        block_on(self.r.e().stack(&self.r.cx, self.wref(name), op))
    }

    /// The workspace's drafts, bottom first.
    fn drafts(&self, name: &str) -> Vec<ChangesetId> {
        let ws = self.ws(name);
        let mut out = Vec::new();
        let mut cur = ws.head;
        while let Some(id) = cur {
            if id == ws.base {
                break;
            }
            out.push(id);
            cur = self.cs(id).first_parent();
        }
        out.reverse();
        out
    }

    fn state(&self, id: ChangesetId) -> RecordState {
        let meta = self.r.e().stores().0;
        let repo = block_on(meta.repo("r")).expect("repo").expect("exists");
        block_on(meta.changeset(repo, id))
            .expect("read")
            .expect("stored")
            .1
            .state
    }
}

fn landed(result: ApiResult<Landed>) -> (ChangesetId, Vec<String>) {
    match result {
        Ok(Landed::Landed {
            landed,
            merged_paths,
            ..
        }) => (landed, merged_paths.iter().map(|p| p.to_string()).collect()),
        other => panic!("expected Landed, got {other:?}"),
    }
}

fn conflict_paths<T: std::fmt::Debug>(result: ApiResult<T>) -> Vec<String> {
    match result {
        Err(ApiError::Conflict { paths, .. }) => paths.into_iter().map(|p| p.path).collect(),
        other => panic!("expected a conflict, got {other:?}"),
    }
}

fn invalid_rule<T: std::fmt::Debug>(result: ApiResult<T>) -> String {
    match result {
        Err(ApiError::InvalidChange { rule, .. }) => rule,
        other => panic!("expected InvalidChange, got {other:?}"),
    }
}

fn snap(list: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    list.iter()
        .map(|(p, b)| ((*p).to_owned(), b.to_vec()))
        .collect()
}

fn two_lands_on_different_regions_both_land<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("b", b"b\n")]);
    let first = t.commit(t.root, &[("a", TOP)]);
    let second = t.commit(t.root, &[("a", BOTTOM)]);
    let (one, merged) = landed(t.land(first, "main"));
    assert_eq!((one, merged.len()), (first, 0));
    let (two, merged) = landed(t.land(second, "main"));
    assert_eq!(merged, ["a"]);
    assert_eq!(t.tip("main"), two);
    // The result equals merge3 of the two edits against the base.
    let Merge3::Clean(want) = merge3(BASE, BOTTOM, TOP) else {
        panic!("clean merge expected");
    };
    assert_eq!(want, BOTH);
    assert_eq!(t.files(two), snap(&[("a", BOTH), ("b", b"b\n")]));
    // The landed changeset records the merge and where it came from.
    let cs = t.cs(two);
    assert_eq!(cs.metadata[MERGED_PATHS], "[\"a\"]");
    assert_eq!(cs.metadata[REBASED_FROM], second.to_string());
    // A retried land returns the same result, merged paths included.
    let (again, merged) = landed(t.land(second, "main"));
    assert_eq!((again, merged), (two, vec!["a".to_owned()]));
    // A rebase that merges nothing records nothing.
    let other = t.commit(t.root, &[("c", b"c\n")]);
    let (three, merged) = landed(t.land(other, "main"));
    assert!(merged.is_empty());
    assert!(!t.cs(three).metadata.contains_key(MERGED_PATHS));
}

fn file_mode_and_overlapping_edits_still_conflict<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE)]);
    landed(t.land(t.commit(t.root, &[("a", TOP)]), "main"));
    // File mode: any both-sides change conflicts.
    let second = t.commit(t.root, &[("a", BOTTOM)]);
    assert_eq!(
        conflict_paths(t.land_with(second, "main", MergeMode::File)),
        ["a"]
    );
    // Line mode, same line: a conflict, and nothing moves.
    let tip = t.tip("main");
    let clash = t.commit(
        t.root,
        &[("a", b"uno\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n")],
    );
    assert_eq!(conflict_paths(t.land(clash, "main")), ["a"]);
    assert_eq!(t.tip("main"), tip);
    // Rebase takes the request's mode.
    assert_eq!(
        conflict_paths(t.rebase(second, tip, MergeMode::File)),
        ["a"]
    );
    let rebased = t.rebase(second, tip, MergeMode::Line).expect("line rebase");
    assert_eq!(t.files(rebased)["a"], BOTH);
    // A delete against an edit is a conflict in either mode.
    let gone = t.r.commit(vec![t.root], &[], &["a"]).id;
    assert_eq!(conflict_paths(t.land(gone, "main")), ["a"]);
}

fn only_small_text_files_merge_by_line<M: MetaStore>(f: &Fixture<M>) {
    let big_base: Vec<u8> = (0..(1 << 20) / 8 + 10)
        .flat_map(|i| format!("{i:07}\n").into_bytes())
        .collect();
    assert!(big_base.len() >= 1 << 20);
    let mut big_top = big_base.clone();
    big_top[..7].copy_from_slice(b"AAAAAAA");
    let mut big_bottom = big_base.clone();
    let n = big_bottom.len();
    big_bottom[n - 8..n - 1].copy_from_slice(b"ZZZZZZZ");
    let bin_base: &[u8] = b"one\n\0two\nthree\n";
    let bin_top: &[u8] = b"ONE\n\0two\nthree\n";
    let bin_bottom: &[u8] = b"one\n\0two\nTHREE\n";
    let t = T::new(f, &[("big", &big_base), ("bin", bin_base)]);
    landed(t.land(
        t.commit(t.root, &[("big", &big_top), ("bin", bin_top)]),
        "main",
    ));
    let big = t.commit(t.root, &[("big", &big_bottom)]);
    assert_eq!(conflict_paths(t.land(big, "main")), ["big"]);
    let bin = t.commit(t.root, &[("bin", bin_bottom)]);
    assert_eq!(conflict_paths(t.land(bin, "main")), ["bin"]);
}

fn the_executable_bit_follows_the_side_that_changed_it<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE)]);
    t.r.upload(&[TOP]);
    let exec = graft_model::Change::Put {
        path: p("a"),
        kind: graft_model::EntryKind::Exec,
        id: blob_id(TOP),
    };
    let first = t.r.try_commit(vec![t.root], vec![exec]).expect("exec").id;
    landed(t.land(first, "main"));
    let second = t.commit(t.root, &[("a", BOTTOM)]);
    let (two, merged) = landed(t.land(second, "main"));
    assert_eq!(merged, ["a"]);
    let entry = block_on(t.r.e().stat(
        &t.r.cx,
        PathAt {
            repo: "r".to_owned(),
            at: two,
            path: p("a"),
        },
    ))
    .expect("stat");
    assert_eq!(entry.kind, graft_model::EntryKind::Exec);
    assert_eq!(entry.id, blob_id(BOTH));
}

fn sync_merges_by_line<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE)]);
    t.open("w", "main");
    let draft = t.commit_in("w", t.root, &[("a", BOTTOM)]);
    landed(t.land(t.commit(t.root, &[("a", TOP)]), "main"));
    let synced = t.sync("w").expect("sync");
    assert_eq!(
        synced
            .changed_paths
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>(),
        ["a"]
    );
    let head = synced.workspace.head.expect("head");
    assert_eq!(t.files(head)["a"], BOTH);
    assert_eq!(t.cs(head).metadata[MERGED_PATHS], "[\"a\"]");
    assert_eq!(t.state(draft), RecordState::Superseded);
    // Landing the synced head is a fast-forward with nothing merged.
    let (id, merged) = landed(t.land(head, "main"));
    assert_eq!(id, head);
    assert!(merged.is_empty());
}

fn attest_merged_forces_re_attesting<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("b", b"b\n")]);
    let first = t.commit(t.root, &[("a", TOP)]);
    t.attest(first, "ci/test");
    landed(t.land(first, "att"));

    // A file-level rebase still inherits: no re-attesting.
    let disjoint = t.commit(t.root, &[("b", b"B\n")]);
    t.attest(disjoint, "ci/test");
    let (_, merged) = landed(t.land(disjoint, "att"));
    assert!(merged.is_empty());

    // A land whose rebase would merge by line is refused, and nothing moves.
    let draft = t.commit(t.root, &[("a", BOTTOM)]);
    t.attest(draft, "ci/test");
    let before = t.tip("att");
    match t.land(draft, "att") {
        Err(ApiError::Rejected { policy, detail }) => {
            assert_eq!(policy, "attested");
            assert!(detail.contains("re-attested"), "{detail}");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert_eq!(t.tip("att"), before);

    // The merged result needs its own attestation: the draft's does not
    // carry over.
    let merged_head = t.rebase(draft, before, MergeMode::Line).expect("rebase");
    assert_eq!(t.cs(merged_head).metadata[MERGED_PATHS], "[\"a\"]");
    match t.land(merged_head, "att") {
        Err(ApiError::Rejected { policy, detail }) => {
            assert_eq!(policy, "attested");
            assert!(detail.contains("ci/test"), "{detail}");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    t.attest(merged_head, "ci/test");
    let (id, merged) = landed(t.land(merged_head, "att"));
    assert_eq!(id, merged_head);
    assert!(merged.is_empty());
    assert_eq!(t.files(id)["a"], BOTH);

    // Through a workspace: the line moves under it, a sync merges, CI
    // attests the merged head, and it lands.
    let ws = t.open("w", "att");
    let mine = t.commit_in(
        "w",
        ws.base,
        &[("a", b"ONE\ntwo\nTHREE\nfour\nfive\nsix\nseven\nEIGHT\n")],
    );
    t.attest(mine, "ci/test");
    let theirs = t.commit(
        ws.base,
        &[("a", b"ONE\ntwo\nthree\nfour\nfive\nSIX\nseven\nEIGHT\n")],
    );
    t.attest(theirs, "ci/test");
    landed(t.land(theirs, "att"));
    assert!(matches!(
        t.land(mine, "att"),
        Err(ApiError::Rejected { .. })
    ));
    let head = t.sync("w").expect("sync").workspace.head.expect("head");
    assert!(matches!(
        t.land(head, "att"),
        Err(ApiError::Rejected { .. })
    ));
    t.attest(head, "ci/test");
    let (id, _) = landed(t.land(head, "att"));
    assert_eq!(id, head);
    assert_eq!(
        t.files(id)["a"],
        b"ONE\ntwo\nTHREE\nfour\nfive\nSIX\nseven\nEIGHT\n"
    );
}

fn attestations_survive_a_merge_without_attest_merged<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE)]);
    let first = t.commit(t.root, &[("a", TOP)]);
    t.attest(first, "ci/test");
    landed(t.land(first, "soft"));
    let second = t.commit(t.root, &[("a", BOTTOM)]);
    t.attest(second, "ci/test");
    let (id, merged) = landed(t.land(second, "soft"));
    assert_eq!(merged, ["a"]);
    assert_eq!(t.files(id)["a"], BOTH);
    // And a merged result inherits through the rebase link.
    let third = t.commit(
        t.root,
        &[("a", b"one\ntwo\nthree\nFOUR\nfive\nsix\nseven\neight\n")],
    );
    t.attest(third, "ci/test");
    let merged_head = t.rebase(third, id, MergeMode::Line).expect("rebase");
    let (landed_id, merged) = landed(t.land(merged_head, "soft"));
    assert_eq!(landed_id, merged_head);
    assert!(merged.is_empty());
}

fn revert_of_revert_restores_the_original_tree<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("d/x", b"x\n"), ("gone", b"g\n")]);
    // A change that edits, adds, deletes, and turns a directory into a file.
    let c =
        t.r.commit(
            vec![t.root],
            &[("a", TOP), ("new/file", b"n\n"), ("d", b"file now\n")],
            &["gone", "d/x"],
        )
        .id;
    let original = t.files(c);
    let r1 = t.revert(c, c, None).expect("revert");
    assert_eq!(t.files(r1), t.files(t.root));
    let cs = t.cs(r1);
    assert_eq!(cs.parents, [c]);
    assert_eq!(cs.metadata[REVERTS], c.to_string());
    assert!(cs.message.contains(&c.to_string()));
    assert_eq!(cs.author.name, t.r.cx.principal.subject);
    // Deterministic: a retry returns the same id.
    assert_eq!(t.revert(c, c, None).expect("retry"), r1);
    assert_eq!(t.state(r1), RecordState::Abandoned);
    // The revert of the revert restores the original tree.
    let r2 = t.revert(r1, r1, None).expect("revert of revert");
    assert_eq!(t.files(r2), original);
    // A root changeset cannot be reverted.
    assert_eq!(invalid_rule(t.revert(t.root, c, None)), "revert-root");
}

fn revert_onto_a_later_tip_merges_by_line<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("b", b"b\n")]);
    let (c, _) = landed(t.land(t.commit(t.root, &[("a", TOP)]), "main"));
    let (later, _) = landed(t.land(t.commit(c, &[("a", BOTH), ("b", b"B\n")]), "main"));
    let r = t.revert(c, later, None).expect("revert");
    assert_eq!(t.files(r), snap(&[("a", BOTTOM), ("b", b"B\n")]));
    let cs = t.cs(r);
    assert_eq!(cs.parents, [later]);
    assert_eq!(cs.metadata[MERGED_PATHS], "[\"a\"]");
    assert!(!cs.metadata.contains_key(REBASED_FROM));
    // It lands as a fast-forward.
    let (id, _) = landed(t.land(r, "main"));
    assert_eq!(id, r);
    // Reverting it again is empty: the line already undid it.
    assert_eq!(invalid_rule(t.revert(c, r, None)), "revert-empty");
    // A revert that overlaps later edits on the same lines conflicts.
    let (edit, _) = landed(t.land(
        t.commit(
            r,
            &[("a", b"1\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n")],
        ),
        "main",
    ));
    assert_eq!(conflict_paths(t.revert(c, edit, None)), ["a"]);
}

fn revert_into_a_workspace<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE)]);
    let (c, _) = landed(t.land(t.commit(t.root, &[("a", TOP)]), "main"));
    let ws = t.open("w", "main");
    assert_eq!(ws.base, c);
    let r = t.revert(c, c, Some("w")).expect("revert");
    assert_eq!(t.ws("w").head, Some(r));
    assert_eq!(t.state(r), RecordState::Draft);
    assert_eq!(t.files(r)["a"], BASE);
    // A retry finds the draft already at the head.
    assert_eq!(t.revert(c, c, Some("w")).expect("retry"), r);
    // The workspace must sit at `onto`.
    let (other, _) = landed(t.land(t.commit(c, &[("b", b"b\n")]), "main"));
    assert!(matches!(
        t.revert(other, other, Some("w")),
        Err(ApiError::RefConflict { .. })
    ));
}

/// A workspace on `main` with three drafts touching `a`, `b`, `c`.
fn three_drafts<'f, M: MetaStore>(f: &'f Fixture<M>) -> (T<'f, M>, [ChangesetId; 3]) {
    let t = T::new(f, &[("a", BASE)]);
    t.open("w", "main");
    let d1 = t.commit_in("w", t.root, &[("b", b"b1\n")]);
    let d2 = t.commit_in("w", d1, &[("c", b"c2\n")]);
    let d3 = t.commit_in("w", d2, &[("a", TOP)]);
    (t, [d1, d2, d3])
}

fn reorder_replays_the_drafts<M: MetaStore>(f: &Fixture<M>) {
    let (t, [d1, d2, d3]) = three_drafts(f);
    let top_files = t.files(d3);
    let ws = t
        .stack("w", StackOp::Reorder(vec![d3, d1, d2]))
        .expect("reorder");
    let drafts = t.drafts("w");
    assert_eq!(drafts.len(), 3);
    assert_eq!(ws.head, Some(drafts[2]));
    assert_eq!(t.files(drafts[2]), top_files);
    assert_eq!(t.cs(drafts[0]).metadata[REBASED_FROM], d3.to_string());
    assert_eq!(t.cs(drafts[1]).metadata[REBASED_FROM], d1.to_string());
    for old in [d1, d2, d3] {
        assert_eq!(t.state(old), RecordState::Superseded);
    }
    for new in &drafts {
        assert_eq!(t.state(*new), RecordState::Draft);
    }
    // The same order again is a no-op.
    let again = t
        .stack("w", StackOp::Reorder(drafts.clone()))
        .expect("same");
    assert_eq!(again.head, ws.head);
    // A retried reorder names drafts no longer in the stack.
    assert_eq!(
        invalid_rule(t.stack("w", StackOp::Reorder(vec![d3, d1, d2]))),
        "not-in-stack"
    );
}

fn squash_joins_adjacent_drafts<M: MetaStore>(f: &Fixture<M>) {
    let (t, [d1, d2, d3]) = three_drafts(f);
    let top_files = t.files(d3);
    let ws = t.stack("w", StackOp::Squash(vec![d1, d2])).expect("squash");
    let drafts = t.drafts("w");
    assert_eq!(drafts.len(), 2);
    assert_eq!(ws.head, Some(drafts[1]));
    let squashed = t.cs(drafts[0]);
    assert_eq!(squashed.parents, [t.root]);
    assert_eq!(
        squashed
            .changes
            .iter()
            .map(|c| c.path().to_string())
            .collect::<Vec<_>>(),
        ["b", "c"]
    );
    assert_eq!(t.files(drafts[1]), top_files);
    assert_eq!(t.state(d1), RecordState::Superseded);
    assert_eq!(t.state(d2), RecordState::Superseded);
    assert_eq!(t.state(d3), RecordState::Superseded);
    // Squash the rest into one.
    t.stack("w", StackOp::Squash(drafts.clone()))
        .expect("squash all");
    let drafts = t.drafts("w");
    assert_eq!(drafts.len(), 1);
    assert_eq!(t.files(drafts[0]), top_files);
    // Drafts that cancel out disappear.
    let undo = t.commit_changes_in(
        "w",
        drafts[0],
        vec![del("b"), del("c"), put("a", BASE)],
        &[],
    );
    let ws = t
        .stack("w", StackOp::Squash(vec![drafts[0], undo]))
        .expect("cancel");
    assert_eq!(ws.head, None);
    assert!(t.drafts("w").is_empty());
}

fn split_by_path_groups<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE)]);
    t.open("w", "main");
    let d1 = t.commit_in(
        "w",
        t.root,
        &[("src/x", b"x\n"), ("src/y", b"y\n"), ("docs/z", b"z\n")],
    );
    let d2 = t.commit_in("w", d1, &[("a", TOP)]);
    let ws = t
        .stack(
            "w",
            StackOp::Split {
                draft: d1,
                groups: vec![vec![p("docs")], vec![p("src")]],
            },
        )
        .expect("split");
    let drafts = t.drafts("w");
    assert_eq!(drafts.len(), 3);
    assert_eq!(ws.head, Some(drafts[2]));
    let paths = |id| {
        t.cs(id)
            .changes
            .iter()
            .map(|c| c.path().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(paths(drafts[0]), ["docs/z"]);
    assert_eq!(paths(drafts[1]), ["src/x", "src/y"]);
    assert_eq!(t.files(drafts[1]), t.files(d1));
    assert_eq!(t.files(drafts[2]), t.files(d2));
    assert_eq!(t.state(d1), RecordState::Superseded);
    // Every change needs a group, and every group a change.
    let top = drafts[0];
    assert_eq!(
        invalid_rule(t.stack(
            "w",
            StackOp::Split {
                draft: drafts[1],
                groups: vec![vec![p("src/x")]],
            }
        )),
        "stack-split-uncovered"
    );
    assert_eq!(
        invalid_rule(t.stack(
            "w",
            StackOp::Split {
                draft: drafts[1],
                groups: vec![vec![p("src")], vec![p("docs")]],
            }
        )),
        "stack-split-empty"
    );
    assert_eq!(t.drafts("w")[0], top);
}

fn drop_removes_a_draft_and_rebases_the_rest<M: MetaStore>(f: &Fixture<M>) {
    let (t, [d1, d2, d3]) = three_drafts(f);
    let ws = t.stack("w", StackOp::Drop(d2)).expect("drop");
    let drafts = t.drafts("w");
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0], d1, "drafts below are kept as they are");
    assert_eq!(ws.head, Some(drafts[1]));
    assert_eq!(t.files(drafts[1]), snap(&[("a", TOP), ("b", b"b1\n")]));
    assert_eq!(t.state(d1), RecordState::Draft);
    assert_eq!(t.state(d2), RecordState::Abandoned);
    assert_eq!(t.state(d3), RecordState::Superseded);
    // Dropping a draft that a later one depends on conflicts, and nothing
    // changes.
    let d4 = t.commit_in("w", drafts[1], &[("b", b"b4\n")]);
    let before = t.ws("w");
    assert_eq!(conflict_paths(t.stack("w", StackOp::Drop(d1))), ["b"]);
    assert_eq!(t.ws("w"), before);
    // Dropping everything empties the stack.
    t.stack("w", StackOp::Drop(d4)).expect("drop top");
    let drafts = t.drafts("w");
    t.stack("w", StackOp::Drop(drafts[0])).expect("drop");
    t.stack("w", StackOp::Drop(t.drafts("w")[0])).expect("drop");
    assert_eq!(t.ws("w").head, None);
}

fn stack_ops_refuse_bad_requests<M: MetaStore>(f: &Fixture<M>) {
    let (t, [d1, d2, d3]) = three_drafts(f);
    assert_eq!(
        invalid_rule(t.stack("w", StackOp::Reorder(vec![d1, d2]))),
        "stack-order"
    );
    assert_eq!(
        invalid_rule(t.stack("w", StackOp::Reorder(vec![d1, d1, d2]))),
        "stack-order"
    );
    assert_eq!(
        invalid_rule(t.stack("w", StackOp::Squash(vec![d1, d3]))),
        "stack-squash"
    );
    assert_eq!(
        invalid_rule(t.stack("w", StackOp::Squash(vec![d2, d1]))),
        "stack-squash"
    );
    assert_eq!(
        invalid_rule(t.stack("w", StackOp::Squash(vec![]))),
        "stack-squash"
    );
    assert_eq!(
        invalid_rule(t.stack("w", StackOp::Drop(t.root))),
        "not-in-stack"
    );
    assert_eq!(t.drafts("w"), [d1, d2, d3]);
    // Stack operations need the workspace scope.
    let reader = cx(&["repo:r:read"]);
    assert!(matches!(
        block_on(t.r.e().stack(&reader, t.wref("w"), StackOp::Drop(d3))),
        Err(ApiError::PermissionDenied { .. })
    ));
}
