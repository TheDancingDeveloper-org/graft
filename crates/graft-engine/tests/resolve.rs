//! WI-2312: `GetConflicts` and `Resolve`, conflict resolution without a
//! working copy, on both stores.

mod common;

use std::collections::BTreeMap;

use common::*;
use graft_engine::{EngineConfig, LineConfig, MERGED_PATHS, REBASED_FROM, RESOLVED_PATHS};
use graft_model::{BlobId, ChangesetId, EntryKind};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi, WorkspaceApi};
use graft_store::MetaStore;

fn config() -> EngineConfig {
    EngineConfig {
        lines: vec![LineConfig::direct("main")],
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
    same_line_conflict_shows_markers_and_resolves_to_a_clean_land,
    resolve_is_idempotent_and_cas_guarded,
    delete_modify_resolves_by_delete,
    binary_conflict_has_ids_only_and_resolves_by_blob,
    unresolved_paths_that_merge_cleanly_are_merged,
    a_still_conflicting_unresolved_path_fails_and_writes_nothing,
    resolutions_are_checked,
    conflicts_page_by_path,
    a_clean_stack_has_no_conflicts,
);

const BASE: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
const OURS: &[u8] = b"one\nTWO-ours\nthree\nfour\nfive\nsix\nseven\neight\n";
const THEIRS: &[u8] = b"one\nTWO-theirs\nthree\nfour\nfive\nsix\nseven\neight\n";
const FIXED: &[u8] = b"one\nTWO-both\nthree\nfour\nfive\nsix\nseven\neight\n";
const BOTTOM: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n";
const TOP: &[u8] = b"ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
const BOTH: &[u8] = b"ONE\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n";

struct T<'f, M> {
    r: Repo<'f, M>,
    root: ChangesetId,
}

impl<'f, M: MetaStore> T<'f, M> {
    fn new(f: &'f Fixture<M>, files: &[(&str, &[u8])]) -> Self {
        let r = Repo::new(f, "r");
        let root = r.commit(vec![], files, &[]).id;
        r.update_ref("main", None, root, RefKind::Line)
            .expect("line");
        Self { r, root }
    }

    fn wref(&self, name: &str) -> WorkspaceRef {
        WorkspaceRef {
            repo: "r".to_owned(),
            name: name.to_owned(),
        }
    }

    fn open(&self, name: &str) {
        block_on(self.r.e().open_workspace(
            &self.r.cx,
            OpenWorkspace {
                repo: "r".to_owned(),
                name: Some(name.to_owned()),
                line: "main".to_owned(),
                at: None,
                lease_ms: 0,
            },
        ))
        .expect("open");
    }

    fn ws(&self, name: &str) -> WorkspaceView {
        block_on(self.r.e().get_workspace(&self.r.cx, self.wref(name))).expect("workspace")
    }

    /// Commits `files` and `deletes` as a draft in workspace `ws`.
    fn draft(
        &self,
        ws: &str,
        parent: ChangesetId,
        files: &[(&str, &[u8])],
        deletes: &[&str],
    ) -> ChangesetId {
        let blobs: Vec<&[u8]> = files.iter().map(|(_, b)| *b).collect();
        if !blobs.is_empty() {
            self.r.upload(&blobs);
        }
        let mut changes: Vec<_> = files.iter().map(|(p, b)| put(p, b)).collect();
        changes.extend(deletes.iter().map(|p| del(p)));
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        let mut req = commit_req("r", vec![parent], changes);
        req.workspace = Some(ws.to_owned());
        block_on(self.r.e().create_changeset(&self.r.cx, req))
            .expect("commit")
            .id
    }

    /// Lands a changeset on `main` straight off the root.
    fn land_other(&self, files: &[(&str, &[u8])], deletes: &[&str]) -> ChangesetId {
        let id = self.r.commit(vec![self.root], files, &[]).id;
        let id = if deletes.is_empty() {
            id
        } else {
            let mut changes: Vec<_> = deletes.iter().map(|p| del(p)).collect();
            changes.sort_by(|a, b| a.path().cmp(b.path()));
            self.r.try_commit(vec![id], changes).expect("delete").id
        };
        self.land(id).expect("land other")
    }

    fn land(&self, head: ChangesetId) -> ApiResult<ChangesetId> {
        match block_on(self.r.e().land(
            &self.r.cx,
            Land {
                repo: "r".to_owned(),
                head,
                line: "main".to_owned(),
                squash: false,
                merge: MergeMode::Line,
                deadline_ms: None,
                merged_from: None,
            },
        ))? {
            Landed::Landed { landed, .. } => Ok(landed),
            other => panic!("expected Landed, got {other:?}"),
        }
    }

    fn tip(&self) -> ChangesetId {
        block_on(self.r.e().resolve_ref(
            &self.r.cx,
            ResolveRef {
                repo: "r".to_owned(),
                name: "main".to_owned(),
            },
        ))
        .expect("resolve")
        .target
    }

    fn conflicts(&self, ws: &str, limit: u32, token: Option<String>) -> Conflicts {
        block_on(self.r.e().get_conflicts(
            &self.r.cx,
            GetConflicts {
                workspace: self.wref(ws),
                onto: None,
                page: Page { limit, token },
            },
        ))
        .expect("conflicts")
    }

    fn resolve(
        &self,
        ws: &str,
        onto: ChangesetId,
        expected_head: Option<ChangesetId>,
        resolutions: Vec<PathResolution>,
    ) -> ApiResult<Resolved> {
        block_on(self.r.e().resolve(
            &self.r.cx,
            Resolve {
                workspace: self.wref(ws),
                onto,
                expected_head,
                resolutions,
            },
        ))
    }

    fn files(&self, id: ChangesetId) -> BTreeMap<String, Vec<u8>> {
        self.r.fetch(self.r.changeset(id).changeset.root)
    }

    fn meta(&self, id: ChangesetId) -> BTreeMap<String, String> {
        self.r
            .changeset(id)
            .changeset
            .metadata
            .into_iter()
            .collect()
    }
}

fn inline(path: &str, content: &[u8]) -> PathResolution {
    PathResolution {
        path: p(path),
        resolution: Resolution::Put {
            kind: EntryKind::File,
            content: ResolvedContent::Inline(content.to_vec()),
        },
    }
}

fn delete(path: &str) -> PathResolution {
    PathResolution {
        path: p(path),
        resolution: Resolution::Delete,
    }
}

fn names(paths: &[graft_model::RepoPath]) -> Vec<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

fn snap(list: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    list.iter()
        .map(|(p, b)| ((*p).to_owned(), b.to_vec()))
        .collect()
}

fn same_line_conflict_shows_markers_and_resolves_to_a_clean_land<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("z", b"z\n")]);
    t.open("w");
    let draft = t.draft("w", t.root, &[("a", OURS)], &[]);
    let onto = t.land_other(&[("a", THEIRS)], &[]);

    // The land conflicts, as before.
    assert!(matches!(t.land(draft), Err(ApiError::StaleBase(_))));

    let c = t.conflicts("w", 0, None);
    assert_eq!(
        (c.onto, c.draft, c.next.as_deref()),
        (onto, Some(draft), None)
    );
    assert_eq!(c.paths.len(), 1);
    let v = &c.paths[0];
    assert_eq!(v.path, p("a"));
    assert_eq!(v.base.map(|s| s.id), Some(blob_id(BASE)));
    assert_eq!(v.ours.map(|s| s.id), Some(blob_id(OURS)));
    assert_eq!(v.theirs.map(|s| s.id), Some(blob_id(THEIRS)));
    assert!(v.text_mergeable);
    assert_eq!(v.reason, None);
    let markers = String::from_utf8(v.markers.clone().expect("markers")).expect("utf-8");
    assert_eq!(
        markers,
        String::from_utf8(graft_model::merge::render_conflict_markers(
            BASE, OURS, THEIRS
        ))
        .expect("utf-8")
    );
    assert!(markers.contains("<<<<<<<") && markers.contains("TWO-theirs"));

    let r = t
        .resolve("w", onto, Some(draft), vec![inline("a", FIXED)])
        .expect("resolve");
    assert_eq!(names(&r.resolved_paths), ["a"]);
    assert!(r.merged_paths.is_empty());
    assert_eq!((r.workspace.base, r.workspace.head), (onto, Some(r.head)));
    assert_eq!(t.files(r.head), snap(&[("a", FIXED), ("z", b"z\n")]));
    let meta = t.meta(r.head);
    assert_eq!(meta[RESOLVED_PATHS], "[\"a\"]");
    assert_eq!(meta[REBASED_FROM], draft.to_string());
    assert!(!meta.contains_key(MERGED_PATHS));
    assert_eq!(t.r.changeset(r.head).changeset.parents, vec![onto]);

    // Nothing conflicts now, and the land is a fast-forward.
    let c = t.conflicts("w", 0, None);
    assert!(c.paths.is_empty() && c.draft.is_none());
    assert_eq!(t.land(r.head).expect("land"), r.head);
    assert_eq!(t.files(t.tip()), snap(&[("a", FIXED), ("z", b"z\n")]));
}

fn resolve_is_idempotent_and_cas_guarded<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE)]);
    t.open("w");
    let draft = t.draft("w", t.root, &[("a", OURS)], &[]);
    let onto = t.land_other(&[("a", THEIRS)], &[]);

    // A stale head is a CAS failure and moves nothing.
    match t.resolve("w", onto, Some(t.root), vec![inline("a", FIXED)]) {
        Err(ApiError::RefConflict { name, actual }) => {
            assert_eq!((name.as_str(), actual), ("w", Some(draft)));
        }
        other => panic!("expected RefConflict, got {other:?}"),
    }
    assert_eq!(t.ws("w").head, Some(draft));

    let first = t
        .resolve("w", onto, Some(draft), vec![inline("a", FIXED)])
        .expect("resolve");
    // The same request again succeeds with the same head.
    let again = t
        .resolve("w", onto, Some(draft), vec![inline("a", FIXED)])
        .expect("retry");
    assert_eq!(again.head, first.head);
    assert_eq!(again.workspace, first.workspace);
    assert_eq!(names(&again.resolved_paths), ["a"]);
    // A different resolution against the old head is a CAS failure.
    assert!(matches!(
        t.resolve("w", onto, Some(draft), vec![inline("a", OURS)]),
        Err(ApiError::RefConflict { .. })
    ));
}

fn delete_modify_resolves_by_delete<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("z", b"z\n")]);
    t.open("w");
    let draft = t.draft("w", t.root, &[], &["a"]);
    let onto = t.land_other(&[("a", THEIRS)], &[]);

    let c = t.conflicts("w", 0, None);
    assert_eq!(c.paths.len(), 1);
    let v = &c.paths[0];
    assert_eq!(v.ours, None);
    assert_eq!(v.theirs.map(|s| s.id), Some(blob_id(THEIRS)));
    assert_eq!(
        (v.text_mergeable, v.markers.as_ref(), v.reason),
        (false, None, Some(ConflictReason::Clash))
    );

    let r = t
        .resolve("w", onto, Some(draft), vec![delete("a")])
        .expect("resolve");
    assert_eq!(t.files(r.head), snap(&[("z", b"z\n")]));
    assert_eq!(t.land(r.head).expect("land"), r.head);
    assert_eq!(t.files(t.tip()), snap(&[("z", b"z\n")]));
}

fn binary_conflict_has_ids_only_and_resolves_by_blob<M: MetaStore>(f: &Fixture<M>) {
    let base: &[u8] = b"\x00\x01base";
    let ours: &[u8] = b"\x00\x01ours";
    let theirs: &[u8] = b"\x00\x01theirs";
    let fixed: &[u8] = b"\x00\x01fixed";
    let t = T::new(f, &[("bin", base)]);
    t.open("w");
    let draft = t.draft("w", t.root, &[("bin", ours)], &[]);
    let onto = t.land_other(&[("bin", theirs)], &[]);

    let c = t.conflicts("w", 0, None);
    let v = &c.paths[0];
    assert_eq!(v.ours.map(|s| s.id), Some(blob_id(ours)));
    assert_eq!(
        (v.text_mergeable, v.markers.as_ref(), v.reason),
        (false, None, Some(ConflictReason::Binary))
    );

    let id = t.r.upload(&[fixed])[0];
    let r = t
        .resolve(
            "w",
            onto,
            Some(draft),
            vec![PathResolution {
                path: p("bin"),
                resolution: Resolution::Put {
                    kind: EntryKind::File,
                    content: ResolvedContent::Blob(BlobId(id)),
                },
            }],
        )
        .expect("resolve");
    assert_eq!(t.land(r.head).expect("land"), r.head);
    assert_eq!(t.files(t.tip()), snap(&[("bin", fixed)]));
}

fn unresolved_paths_that_merge_cleanly_are_merged<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("b", BASE)]);
    t.open("w");
    let draft = t.draft("w", t.root, &[("a", OURS), ("b", BOTTOM)], &[]);
    let onto = t.land_other(&[("a", THEIRS), ("b", TOP)], &[]);

    let c = t.conflicts("w", 0, None);
    assert_eq!(
        c.paths
            .iter()
            .map(|v| v.path.to_string())
            .collect::<Vec<_>>(),
        ["a"]
    );

    let r = t
        .resolve("w", onto, Some(draft), vec![inline("a", FIXED)])
        .expect("resolve");
    assert_eq!(names(&r.resolved_paths), ["a"]);
    assert_eq!(names(&r.merged_paths), ["b"]);
    assert_eq!(t.files(r.head), snap(&[("a", FIXED), ("b", BOTH)]));
    let meta = t.meta(r.head);
    assert_eq!(meta[MERGED_PATHS], "[\"b\"]");
    assert_eq!(meta[RESOLVED_PATHS], "[\"a\"]");
}

fn a_still_conflicting_unresolved_path_fails_and_writes_nothing<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("c", BASE)]);
    t.open("w");
    let draft = t.draft("w", t.root, &[("a", OURS), ("c", OURS)], &[]);
    let onto = t.land_other(&[("a", THEIRS), ("c", THEIRS)], &[]);

    match t.resolve("w", onto, Some(draft), vec![inline("a", FIXED)]) {
        Err(ApiError::Conflict { paths, onto: at }) => {
            assert_eq!(at, onto);
            assert_eq!(
                paths.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
                ["c"]
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    let ws = t.ws("w");
    assert_eq!((ws.base, ws.head), (t.root, Some(draft)));
}

fn resolutions_are_checked<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("b", BASE)]);
    t.open("w");
    let draft = t.draft("w", t.root, &[("a", OURS), ("b", BOTTOM)], &[]);
    let onto = t.land_other(&[("a", THEIRS), ("b", TOP)], &[]);
    let rule = |r: ApiResult<Resolved>| match r {
        Err(ApiError::InvalidChange { rule, .. }) => rule,
        other => panic!("expected InvalidChange, got {other:?}"),
    };
    // `b` merges cleanly, so a resolution for it is refused.
    assert_eq!(
        rule(t.resolve(
            "w",
            onto,
            Some(draft),
            vec![inline("a", FIXED), inline("b", FIXED)]
        )),
        "resolution-not-conflicting"
    );
    assert_eq!(
        rule(t.resolve(
            "w",
            onto,
            Some(draft),
            vec![inline("a", FIXED), inline("a", OURS)]
        )),
        "resolution-duplicate"
    );
    assert_eq!(
        rule(t.resolve(
            "w",
            onto,
            Some(draft),
            vec![PathResolution {
                path: p("a"),
                resolution: Resolution::Put {
                    kind: EntryKind::Tree,
                    content: ResolvedContent::Inline(Vec::new()),
                },
            }]
        )),
        "resolution-kind"
    );
    // A blob that is not stored.
    assert!(matches!(
        t.resolve(
            "w",
            onto,
            Some(draft),
            vec![PathResolution {
                path: p("a"),
                resolution: Resolution::Put {
                    kind: EntryKind::File,
                    content: ResolvedContent::Blob(BlobId(blob_id(b"never uploaded"))),
                },
            }]
        ),
        Err(ApiError::MissingObjects { .. })
    ));
    assert_eq!(t.ws("w").head, Some(draft));
}

fn conflicts_page_by_path<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE), ("c", BASE), ("e", BASE)]);
    t.open("w");
    t.draft("w", t.root, &[("a", OURS), ("c", OURS), ("e", OURS)], &[]);
    t.land_other(&[("a", THEIRS), ("c", THEIRS), ("e", THEIRS)], &[]);
    let mut seen = Vec::new();
    let mut token = None;
    loop {
        let c = t.conflicts("w", 2, token);
        assert!(c.paths.len() <= 2);
        seen.extend(c.paths.iter().map(|v| v.path.to_string()));
        token = c.next;
        if token.is_none() {
            break;
        }
    }
    assert_eq!(seen, ["a", "c", "e"]);
}

fn a_clean_stack_has_no_conflicts<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f, &[("a", BASE)]);
    t.open("w");
    let draft = t.draft("w", t.root, &[("a", BOTTOM)], &[]);
    let onto = t.land_other(&[("a", TOP)], &[]);
    let c = t.conflicts("w", 0, None);
    assert_eq!((c.onto, c.draft, c.paths.len()), (onto, None, 0));
    // Resolve with nothing to resolve is a sync.
    let r = t
        .resolve("w", onto, Some(draft), Vec::new())
        .expect("resolve");
    assert_eq!(names(&r.merged_paths), ["a"]);
    assert_eq!(t.files(r.head)["a"], BOTH);
    assert!(!t.meta(r.head).contains_key(RESOLVED_PATHS));
}
