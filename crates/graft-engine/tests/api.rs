//! Phase 1 API scenarios, each run against the memory and SQLite stores.

mod common;

use std::collections::BTreeMap;

use common::*;
use graft_model::{ChangesetId, EntryKind, ObjectKind};
use graft_proto::types::*;
use graft_proto::{AdminApi, ApiError, Frame, ObjectApi, RepoApi};
use graft_store::{MetaStore, ObjectStore};
use graft_testkit::synth::{SynthSpec, generate};

on_both_stores!(
    round_trip_reproduces_a_synthetic_repository,
    create_changeset_retry_returns_the_same_id_and_writes_nothing,
    get_objects_with_misses_returns_the_rest,
    non_fast_forward_line_moves_are_refused,
    tool_and_tag_refs_follow_their_rules,
    snapshot_reads_stat_list_and_read,
    watch_pages_and_filters_the_journal,
    import_allows_roots_and_merges,
    diff_names_and_patches,
);

/// Upload, create, ref, resolve, fetch: every file of every snapshot comes
/// back byte for byte.
fn round_trip_reproduces_a_synthetic_repository<M: MetaStore>(f: &Fixture<M>) {
    let repo = Repo::new(f, "synth");
    let synth = generate(&SynthSpec {
        files: 60,
        depth: 3,
        history: 6,
        file_bytes: 700,
        seed: 42,
    });
    let mut prev = BTreeMap::new();
    let mut parent: Option<ChangesetId> = None;
    let mut ids = Vec::new();
    for snapshot in &synth.snapshots {
        let changes = changes_between(&prev, snapshot);
        // Upload only what the server lacks.
        let wanted: Vec<_> = snapshot.values().map(|b| blob_id(b)).collect();
        let missing = block_on(repo.e().has_objects(
            &repo.cx,
            HasObjects {
                repo: repo.name.clone(),
                ids: wanted,
            },
        ))
        .expect("has objects");
        let blobs: Vec<&[u8]> = snapshot
            .values()
            .filter(|b| missing.contains(&blob_id(b)))
            .map(Vec::as_slice)
            .collect();
        if !blobs.is_empty() {
            repo.upload(&blobs);
        }
        let created = repo
            .try_commit(parent.into_iter().collect(), changes)
            .expect("commit");
        // Nothing holds a changeset created without a workspace.
        assert_eq!(created.state, DraftState::Abandoned);
        let moved = repo
            .update_ref("main", parent, created.id, RefKind::Line)
            .expect("fast-forward main");
        assert_eq!(moved.target, created.id);
        parent = Some(created.id);
        ids.push(created.id);
        prev = snapshot.clone();
    }

    let main = block_on(repo.e().resolve_ref(
        &repo.cx,
        ResolveRef {
            repo: repo.name.clone(),
            name: "main".to_owned(),
        },
    ))
    .expect("resolve main");
    assert_eq!(main.target, *ids.last().expect("history"));
    assert_eq!(main.version, synth.snapshots.len() as u64);
    assert_eq!(main.kind, RefKind::Line);

    for (id, snapshot) in ids.iter().zip(&synth.snapshots) {
        let view = repo.changeset(*id);
        assert_eq!(view.state, DraftState::Landed);
        assert_eq!(&repo.fetch(view.changeset.root), snapshot);
    }

    // The line index covers every changeset the moves added.
    let (meta, _) = repo.e().stores();
    let rid = block_on(meta.repo("synth")).expect("repo").expect("exists");
    for (pos, id) in ids.iter().enumerate() {
        assert_eq!(
            block_on(meta.line_pos(rid, "main", *id)).expect("line pos"),
            Some(pos as u64)
        );
    }

    // ReadFile and a paged recursive ListTree agree with the snapshot.
    let tip = main.target;
    let last = synth.snapshots.last().expect("snapshot");
    for (path, data) in last.iter().take(10) {
        let file = block_on(repo.e().read_file(
            &repo.cx,
            PathAt {
                repo: repo.name.clone(),
                at: tip,
                path: p(path),
            },
        ))
        .expect("read file");
        assert_eq!(&file.data, data);
        assert_eq!(file.kind, EntryKind::File);
    }
    let mut listed = Vec::new();
    let mut token = None;
    loop {
        let page = block_on(repo.e().list_tree(
            &repo.cx,
            ListTree {
                repo: repo.name.clone(),
                at: tip,
                path: None,
                recursive: true,
                page: Page { limit: 7, token },
            },
        ))
        .expect("list tree");
        assert!(page.entries.len() <= 7);
        listed.extend(page.entries);
        match page.next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    let files: Vec<String> = listed
        .iter()
        .filter(|e| !e.kind.is_tree())
        .map(|e| e.path.to_string())
        .collect();
    let mut sorted = files.clone();
    sorted.sort();
    assert_eq!(sorted, last.keys().cloned().collect::<Vec<_>>());
    let mut unique = listed.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
    unique.dedup();
    assert_eq!(unique.len(), listed.len(), "no entry is listed twice");
}

fn create_changeset_retry_returns_the_same_id_and_writes_nothing<M: MetaStore>(f: &Fixture<M>) {
    let repo = Repo::new(f, "retry");
    let base = repo.commit(vec![], &[("a/one", b"1")], &[]);
    repo.upload(&[b"2"]);
    let req = commit_req(
        "retry",
        vec![base.id],
        vec![put("a/two", b"2"), put("b/three", b"2")],
    );
    let first = block_on(repo.e().create_changeset(&repo.cx, req.clone())).expect("create");
    let journal = repo.journal();
    let objects = block_on(f.objects.stats()).expect("stats").objects;

    let again = block_on(repo.e().create_changeset(&repo.cx, req)).expect("retry");
    assert_eq!(again, first, "same id, root, lsn and state");
    assert_eq!(repo.journal(), journal, "no journal entry");
    assert_eq!(
        block_on(f.objects.stats()).expect("stats").objects,
        objects,
        "no new objects"
    );

    // A retried root changeset into a now non-empty repository still works.
    let root_again = repo
        .try_commit(vec![], vec![put("a/one", b"1")])
        .expect("retry root");
    assert_eq!(root_again.id, base.id);
}

fn get_objects_with_misses_returns_the_rest<M: MetaStore>(f: &Fixture<M>) {
    let repo = Repo::new(f, "objs");
    let ids = repo.upload(&[b"alpha", b"beta"]);
    let unknown = blob_id(b"never uploaded");
    let got = repo.get_objects(vec![ids[0], unknown, ids[1], ids[0]]);
    assert_eq!(got.missing, vec![unknown]);
    assert!(got.deferred.is_empty());
    let found: Vec<_> = got.found.iter().map(|(id, _)| *id).collect();
    assert_eq!(found, vec![ids[0], ids[1]], "duplicates are answered once");
    for (id, frame) in &got.found {
        frame.decode_verified(*id).expect("verified");
    }
    // Uploading again is idempotent.
    assert_eq!(repo.upload(&[b"alpha"]), vec![ids[0]]);
}

fn non_fast_forward_line_moves_are_refused<M: MetaStore>(f: &Fixture<M>) {
    let repo = Repo::new(f, "line");
    let a = repo.commit(vec![], &[("f", b"a")], &[]).id;
    let b = repo.commit(vec![a], &[("f", b"b")], &[]).id;
    let c = repo.commit(vec![a], &[("f", b"c")], &[]).id;
    let d = repo.commit(vec![b], &[("g", b"d")], &[]).id;
    repo.update_ref("main", None, b, RefKind::Line)
        .expect("create line");

    let sideways = repo.update_ref("main", Some(b), c, RefKind::Line);
    assert!(
        matches!(sideways, Err(ApiError::Rejected { .. })),
        "{sideways:?}"
    );
    let backwards = repo.update_ref("main", Some(b), a, RefKind::Line);
    assert!(
        matches!(backwards, Err(ApiError::Rejected { .. })),
        "{backwards:?}"
    );
    // A merge whose first parent is not the tip is not a fast-forward.
    let merge = repo.commit(vec![c, b], &[("f", b"m")], &[]).id;
    let merged = repo.update_ref("main", Some(b), merge, RefKind::Line);
    assert!(
        matches!(merged, Err(ApiError::Rejected { .. })),
        "{merged:?}"
    );

    let tip = repo
        .update_ref("main", Some(b), d, RefKind::Line)
        .expect("ff");
    assert_eq!((tip.target, tip.version), (d, 2));
    // A retry after success reports success.
    let retry = repo
        .update_ref("main", Some(b), d, RefKind::Line)
        .expect("retry");
    assert_eq!(retry, tip);
    // A stale expectation is a conflict naming the actual target.
    let stale = repo.update_ref("main", Some(a), b, RefKind::Line);
    assert_eq!(
        stale,
        Err(ApiError::RefConflict {
            name: "main".to_owned(),
            actual: Some(d),
        })
    );
    assert_eq!(repo.changeset(c).state, DraftState::Abandoned);
    assert_eq!(repo.changeset(d).state, DraftState::Landed);
}

fn tool_and_tag_refs_follow_their_rules<M: MetaStore>(f: &Fixture<M>) {
    let repo = Repo::new(f, "refs");
    let a = repo.commit(vec![], &[("f", b"a")], &[]).id;
    let b = repo.commit(vec![a], &[("f", b"b")], &[]).id;
    repo.update_ref("tool/x", None, b, RefKind::Tool)
        .expect("create tool ref");
    let back = repo
        .update_ref("tool/x", Some(b), a, RefKind::Tool)
        .expect("move back");
    assert_eq!(back.version, 2);
    repo.update_ref("tags/v1", None, a, RefKind::Tag)
        .expect("create tag");
    let moved = repo.update_ref("tags/v1", Some(a), b, RefKind::Tag);
    assert!(
        matches!(moved, Err(ApiError::InvalidChange { ref rule, .. }) if rule == "tag-immutable")
    );

    let all = block_on(repo.e().list_refs(
        &repo.cx,
        ListRefs {
            repo: repo.name.clone(),
            prefix: None,
            page: Page::default(),
        },
    ))
    .expect("list refs");
    let names: Vec<_> = all.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["tags/v1", "tool/x"]);
    let page = block_on(repo.e().list_refs(
        &repo.cx,
        ListRefs {
            repo: repo.name.clone(),
            prefix: Some("t".to_owned()),
            page: Page {
                limit: 1,
                token: Some("tags/v1".to_owned()),
            },
        },
    ))
    .expect("list refs page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].name, "tool/x");
}

fn snapshot_reads_stat_list_and_read<M: MetaStore>(f: &Fixture<M>) {
    let repo = Repo::new(f, "reads");
    let cs = repo
        .commit(
            vec![],
            &[
                ("a", b"top"),
                ("d/x", b"x"),
                ("d/y", b"y"),
                ("d/z/w", b"w"),
                ("d.e", b"de"),
            ],
            &[],
        )
        .id;
    let stat = |path: &str| {
        block_on(repo.e().stat(
            &repo.cx,
            PathAt {
                repo: repo.name.clone(),
                at: cs,
                path: p(path),
            },
        ))
    };
    assert_eq!(stat("d/x").expect("stat").id, blob_id(b"x"));
    assert_eq!(stat("d/z").expect("stat").kind, EntryKind::Tree);
    assert!(matches!(stat("d/q"), Err(ApiError::NotFound { .. })));
    assert!(matches!(stat("a/b"), Err(ApiError::NotFound { .. })));

    let list = |path: Option<&str>, recursive, limit, token: Option<String>| {
        block_on(repo.e().list_tree(
            &repo.cx,
            ListTree {
                repo: repo.name.clone(),
                at: cs,
                path: path.map(p),
                recursive,
                page: Page { limit, token },
            },
        ))
        .expect("list tree")
    };
    let first = list(Some("d"), false, 2, None);
    let names: Vec<_> = first.entries.iter().map(|e| e.path.to_string()).collect();
    assert_eq!(names, ["d/x", "d/y"]);
    assert_eq!(first.next.as_deref(), Some("y"));
    let second = list(Some("d"), false, 2, first.next);
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].path, p("d/z"));
    assert_eq!(second.next, None);

    // Pre-order: a directory's entries follow it directly.
    let all = list(None, true, 0, None);
    let paths: Vec<_> = all.entries.iter().map(|e| e.path.to_string()).collect();
    assert_eq!(paths, ["a", "d", "d/x", "d/y", "d/z", "d/z/w", "d.e"]);
    // Resuming inside a directory skips what came before.
    let resumed = list(None, true, 2, Some("d/y".to_owned()));
    let paths: Vec<_> = resumed.entries.iter().map(|e| e.path.to_string()).collect();
    assert_eq!(paths, ["d/z", "d/z/w"]);
    assert_eq!(resumed.next.as_deref(), Some("d/z/w"));

    let file = block_on(repo.e().read_file(
        &repo.cx,
        PathAt {
            repo: repo.name.clone(),
            at: cs,
            path: p("d/z/w"),
        },
    ))
    .expect("read");
    assert_eq!(file.data, b"w");
}

fn watch_pages_and_filters_the_journal<M: MetaStore>(f: &Fixture<M>) {
    let repo = Repo::new(f, "watch");
    let a = repo.commit(vec![], &[("f", b"a")], &[]);
    let b = repo.commit(vec![a.id], &[("f", b"b")], &[]);
    repo.update_ref("main", None, b.id, RefKind::Line)
        .expect("line");
    let all = repo.journal();
    let kinds: Vec<_> = all.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["ChangesetCreated", "ChangesetCreated", "RefMoved"]);
    assert_eq!(all.iter().map(|e| e.lsn).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(a.lsn, 1);
    assert_eq!(b.lsn, 2);
    assert_eq!(
        all[0].body,
        format!("{{\"id\":\"{}\",\"state\":\"abandoned\"}}", a.id)
    );
    assert!(
        all[2].body.contains(&format!("\"new\":\"{}\"", b.id)),
        "{}",
        all[2].body
    );
    assert!(all[2].body.contains("\"old\":null"));

    let refs_only = block_on(repo.e().watch(
        &repo.cx,
        Watch {
            repo: repo.name.clone(),
            from_lsn: 0,
            kinds: vec!["RefMoved".to_owned()],
            limit: 10,
            wait_ms: 0,
        },
    ))
    .expect("watch");
    assert_eq!(refs_only.entries.len(), 1);
    assert_eq!(refs_only.next_lsn, 4);
    let tail = block_on(repo.e().watch(
        &repo.cx,
        Watch {
            repo: repo.name.clone(),
            from_lsn: 4,
            kinds: Vec::new(),
            limit: 10,
            wait_ms: 0,
        },
    ))
    .expect("watch tail");
    assert!(tail.entries.is_empty());
    assert_eq!(tail.next_lsn, 4, "an empty page resumes where it asked");
}

fn import_allows_roots_and_merges<M: MetaStore>(f: &Fixture<M>) {
    let repo = Repo::new(f, "import");
    repo.upload(&[b"one", b"two"]);
    // Build the history in a scratch repository to learn the roots.
    let (s1, s2) = (Repo::new(f, "scratch1"), Repo::new(f, "scratch2"));
    let r1 = s1.commit(vec![], &[("x", b"one")], &[]);
    let r2 = s2.commit(vec![], &[("y", b"two")], &[]);
    let roots = vec![s1.changeset(r1.id).changeset, s2.changeset(r2.id).changeset];
    let mut merge = roots[0].clone();
    merge.parents = vec![r1.id, r2.id];
    merge.changes = Vec::new();
    merge.message = "merge".to_owned();
    let mut batch = roots.clone();
    batch.push(merge);
    let imported = block_on(repo.e().import_changesets(
        &repo.cx,
        ImportChangesets {
            repo: repo.name.clone(),
            changesets: batch.clone(),
        },
    ))
    .expect("import");
    assert_eq!(imported.len(), 3);
    assert_eq!(imported[0].id, r1.id);
    assert_eq!(imported[1].id, r2.id);
    assert_eq!(
        imported[2].root, roots[0].root,
        "an empty merge keeps the first parent's root"
    );
    let again = block_on(repo.e().import_changesets(
        &repo.cx,
        ImportChangesets {
            repo: repo.name.clone(),
            changesets: batch,
        },
    ))
    .expect("re-import");
    assert_eq!(again, imported);
    repo.update_ref("main", None, imported[2].id, RefKind::Line)
        .expect("line over the import");
}

fn diff_names_and_patches<M: MetaStore>(f: &Fixture<M>) {
    let repo = Repo::new(f, "diff");
    let a = repo
        .commit(
            vec![],
            &[
                ("src/lib.rs", b"one\ntwo\nthree\n"),
                ("bin", b"\x00\x01"),
                ("gone", b"bye\n"),
            ],
            &[],
        )
        .id;
    let b = repo
        .commit(
            vec![a],
            &[
                ("src/lib.rs", b"one\nTWO\nthree\n"),
                ("bin", b"\x00\x02"),
                ("new", b"hi\n"),
            ],
            &["gone"],
        )
        .id;
    let diff = |paths: Vec<&str>, patch: Option<PatchOptions>| {
        block_on(repo.e().diff(
            &repo.cx,
            Diff {
                repo: repo.name.clone(),
                from: a,
                to: b,
                paths: paths.into_iter().map(p).collect(),
                patch,
                page: Page::default(),
            },
        ))
        .expect("diff")
    };
    let names = diff(vec![], None);
    let paths: Vec<_> = names
        .files
        .iter()
        .map(|f| f.change.path().to_string())
        .collect();
    assert_eq!(paths, ["bin", "gone", "new", "src/lib.rs"]);
    assert!(names.files.iter().all(|f| f.patch.is_none()));

    let patched = diff(
        vec![],
        Some(PatchOptions {
            context: 1,
            max_bytes_per_file: 0,
        }),
    );
    let by_path: BTreeMap<_, _> = patched
        .files
        .iter()
        .map(|f| (f.change.path().to_string(), f))
        .collect();
    assert_eq!(
        by_path["src/lib.rs"].patch.as_deref(),
        Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n")
    );
    assert_eq!(by_path["bin"].patch, None, "binary files get no patch");
    assert_eq!(
        by_path["gone"].patch.as_deref(),
        Some("--- a/gone\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-bye\n")
    );
    assert!(
        by_path["new"]
            .patch
            .as_deref()
            .is_some_and(|p| p.starts_with("--- /dev/null\n"))
    );

    let capped = diff(
        vec!["src"],
        Some(PatchOptions {
            context: 3,
            max_bytes_per_file: 40,
        }),
    );
    assert_eq!(capped.files.len(), 1);
    let file = &capped.files[0];
    assert!(file.truncated);
    assert!(
        file.patch
            .as_ref()
            .is_some_and(|p| p.len() <= 40 && p.ends_with('\n'))
    );

    // Inline blobs commit in one call.
    let mut req = commit_req("diff", vec![b], vec![put("inline.txt", b"inline\n")]);
    req.inline_blobs = vec![Frame::encode(ObjectKind::Blob, b"inline\n")];
    let c = block_on(repo.e().create_changeset(&repo.cx, req)).expect("inline commit");
    assert_eq!(
        repo.fetch(repo.changeset(c.id).changeset.root)["inline.txt"],
        b"inline\n"
    );
}
