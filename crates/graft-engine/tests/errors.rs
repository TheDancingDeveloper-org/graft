//! Every error a phase 1 operation can return, each produced by a request
//! (`docs/design.md` §7.10). `Conflict`, `Contended`, `LeaseExpired` and
//! `Obliterated` belong to later operations (rebase, land, workspaces,
//! obliterate); `Unauthenticated` and `IncompatibleProtocol` are raised by
//! the transport before a call reaches the engine.

mod common;

use std::fmt::Debug;
use std::sync::atomic::Ordering;

use common::*;
use graft_engine::EngineConfig;
use graft_model::{Change, ChangesetId, EntryKind, ObjectId, ObjectKind, TreeId};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{
    AdminApi, ApiError, CallContext, Codec, Frame, ObjectApi, PrincipalKind, RepoApi,
};
use graft_store::{MetaStore, MetaTxn, RecordState, RepoId, Write};

fn err<T: Debug>(r: ApiResult<T>) -> ApiError {
    r.expect_err("the request fails")
}

fn rule(r: ApiResult<impl Debug>) -> String {
    match err(r) {
        ApiError::InvalidChange { rule, .. } => rule,
        other => panic!("expected InvalidChange, got {other:?}"),
    }
}

fn reason(r: ApiResult<impl Debug>) -> String {
    match err(r) {
        ApiError::InvalidPath { reason, .. } => reason,
        other => panic!("expected InvalidPath, got {other:?}"),
    }
}

fn denied(r: ApiResult<impl Debug>) -> (String, Option<String>) {
    match err(r) {
        ApiError::PermissionDenied { scope, path } => (scope, path),
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

fn limit(r: ApiResult<impl Debug>) -> (String, u64) {
    match err(r) {
        ApiError::LimitExceeded { limit, max } => (limit, max),
        other => panic!("expected LimitExceeded, got {other:?}"),
    }
}

fn rid<M: MetaStore>(repo: &Repo<'_, M>) -> RepoId {
    let (meta, _) = repo.e().stores();
    block_on(meta.repo(&repo.name))
        .expect("repo")
        .expect("exists")
}

fn path_at(repo: &str, at: ChangesetId, path: &str) -> PathAt {
    PathAt {
        repo: repo.to_owned(),
        at,
        path: p(path),
    }
}

#[test]
fn not_found() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("f", b"a")], &[]).id;
    let e = &f.engine;

    let unknown_repo = block_on(e.resolve_ref(
        &admin("nope"),
        ResolveRef {
            repo: "nope".to_owned(),
            name: "main".to_owned(),
        },
    ));
    assert!(matches!(err(unknown_repo), ApiError::NotFound { what } if what == "repository nope"));
    let unknown_ref = block_on(e.resolve_ref(
        &repo.cx,
        ResolveRef {
            repo: "r".to_owned(),
            name: "main".to_owned(),
        },
    ));
    assert!(matches!(err(unknown_ref), ApiError::NotFound { .. }));
    let ghost = ChangesetId(ObjectId([7; 32]));
    let unknown_cs = block_on(e.stat(&repo.cx, path_at("r", ghost, "f")));
    assert!(matches!(err(unknown_cs), ApiError::NotFound { .. }));
    let unknown_path = block_on(e.read_file(&repo.cx, path_at("r", a, "g")));
    assert!(matches!(err(unknown_path), ApiError::NotFound { .. }));
    let ref_to_ghost = repo.update_ref("main", None, ghost, RefKind::Line);
    assert!(matches!(err(ref_to_ghost), ApiError::NotFound { .. }));
    let missing = block_on(e.get_changesets(
        &repo.cx,
        GetChangesets {
            repo: "r".to_owned(),
            ids: vec![ghost, a],
        },
    ))
    .expect("get changesets");
    assert_eq!(missing.missing, vec![ghost], "GetChangesets lists misses");
    assert_eq!(missing.found.len(), 1);
}

#[test]
fn collected() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("f", b"a")], &[]).id;
    let (meta, _) = f.engine.stores();
    let (_, mut record) = block_on(meta.changeset(rid(&repo), a))
        .expect("read")
        .expect("stored");
    record.state = RecordState::Collected;
    block_on(meta.commit(
        rid(&repo),
        MetaTxn {
            preconditions: Vec::new(),
            writes: vec![Write::SetRecord { id: a, record }],
        },
    ))
    .expect("collect");

    let read = block_on(f.engine.read_file(&repo.cx, path_at("r", a, "f")));
    assert_eq!(err(read), ApiError::Collected { id: a });
    let child = repo.try_commit(vec![a], vec![del("f")]);
    assert_eq!(err(child), ApiError::Collected { id: a });
    let listed = block_on(f.engine.get_changesets(
        &repo.cx,
        GetChangesets {
            repo: "r".to_owned(),
            ids: vec![a],
        },
    ))
    .expect("get changesets");
    assert_eq!(listed.collected, vec![a]);
}

#[test]
fn invalid_path() {
    let f = memory_with(EngineConfig {
        portable_paths: true,
        ..EngineConfig::default()
    });
    let e = &f.engine;
    assert_eq!(
        reason(block_on(e.create_repo(&admin("a/b"), "a/b".to_owned()))),
        "repo-name"
    );

    let repo = Repo::new(&f, "r");
    let a = repo
        .commit(vec![], &[("dir/f", b"a"), ("Readme", b"r")], &[])
        .id;
    assert_eq!(
        reason(block_on(e.read_file(&repo.cx, path_at("r", a, "dir")))),
        "not-a-file"
    );
    let list = |path: &str, recursive, token: &str| {
        block_on(e.list_tree(
            &repo.cx,
            ListTree {
                repo: "r".to_owned(),
                at: a,
                path: Some(p(path)),
                recursive,
                page: Page {
                    limit: 1,
                    token: (!token.is_empty()).then(|| token.to_owned()),
                },
            },
        ))
    };
    assert_eq!(reason(list("dir/f", false, "")), "not-a-directory");
    assert_eq!(rule(list("dir", false, "a/b")), "page-token");
    assert_eq!(rule(list("dir", true, "/x")), "page-token");

    repo.upload(&[b"x"]);
    assert_eq!(
        reason(repo.try_commit(vec![a], vec![put("aux.rs", b"x")])),
        "portability"
    );
    assert_eq!(
        reason(repo.try_commit(vec![a], vec![put("README", b"x")])),
        "portability"
    );
}

#[test]
fn invalid_change() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("d/f", b"a"), ("g", b"g")], &[]).id;
    repo.upload(&[b"b"]);

    assert_eq!(
        rule(repo.try_commit(vec![a], vec![put("z", b"b"), put("y", b"b")])),
        "changes-sorted"
    );
    assert_eq!(
        rule(repo.try_commit(vec![a], vec![put("y", b"b"), put("y", b"b")])),
        "duplicate-path"
    );
    assert_eq!(
        rule(repo.try_commit(vec![a], vec![del("nope")])),
        "delete-missing"
    );
    assert_eq!(
        rule(repo.try_commit(vec![a], vec![del("d")])),
        "delete-missing"
    );
    assert_eq!(
        rule(repo.try_commit(vec![a], vec![put("g", b"g")])),
        "put-no-op"
    );
    assert_eq!(
        rule(repo.try_commit(vec![a], vec![put("d", b"b")])),
        "file-and-directory"
    );
    assert_eq!(
        rule(repo.try_commit(vec![a], vec![put("g/h", b"b")])),
        "file-and-directory"
    );
    assert_eq!(rule(repo.try_commit(vec![a], vec![])), "empty-changeset");
    assert_eq!(
        rule(repo.try_commit(vec![], vec![put("new", b"b")])),
        "root-changeset"
    );
    let tree_put = Change::Put {
        path: p("t"),
        kind: EntryKind::Tree,
        id: blob_id(b"b"),
    };
    assert_eq!(rule(repo.try_commit(vec![a], vec![tree_put])), "put-kind");

    let mut reserved = commit_req("r", vec![a], vec![put("y", b"b")]);
    reserved
        .metadata
        .insert("graft.rebased-from".to_owned(), "x".to_owned());
    assert_eq!(
        rule(block_on(f.engine.create_changeset(&repo.cx, reserved))),
        "reserved-metadata"
    );

    // An import whose root does not match its changes.
    let mut wrong = repo.changeset(a).changeset;
    wrong.root = TreeId(ObjectId([9; 32]));
    let import = block_on(f.engine.import_changesets(
        &repo.cx,
        ImportChangesets {
            repo: "r".to_owned(),
            changesets: vec![wrong],
        },
    ));
    assert_eq!(rule(import), "root-mismatch");

    // Uploads are blobs in well-formed frames.
    let upload = |frame: Frame| {
        block_on(f.engine.put_objects(
            &repo.cx,
            PutObjects {
                repo: "r".to_owned(),
                blobs: vec![frame],
            },
        ))
    };
    assert_eq!(
        rule(upload(Frame::encode(ObjectKind::Tree, b"\x01"))),
        "blob-only"
    );
    let garbage = Frame {
        kind: ObjectKind::Blob,
        codec: Codec::Gzip,
        uncompressed_len: 10,
        body: b"not gzip".to_vec(),
    };
    assert_eq!(rule(upload(garbage)), "frame");

    // Refs.
    assert_eq!(
        rule(repo.update_ref("bad name", None, a, RefKind::Tool)),
        "ref-name"
    );
    assert_eq!(
        rule(repo.update_ref("", None, a, RefKind::Tool)),
        "ref-name"
    );
    repo.update_ref("t", None, a, RefKind::Tag).expect("tag");
    assert_eq!(
        rule(repo.update_ref("t", Some(a), a, RefKind::Line)),
        "ref-kind"
    );
    let b = repo.commit(vec![a], &[("y", b"b")], &[]).id;
    assert_eq!(
        rule(repo.update_ref("t", Some(a), b, RefKind::Tag)),
        "tag-immutable"
    );
}

/// A token's session and work item bind the draft's `vogt.*` metadata;
/// a service token may attribute to anyone.
#[test]
fn identity_metadata() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("a", b"a")], &[]).id;
    repo.upload(&[b"b"]);
    let mut agent = repo.cx.clone();
    agent.principal.kind = PrincipalKind::Agent;
    agent.principal.session = Some("ses_1".to_owned());
    agent.principal.work_item = Some("WI-1".to_owned());
    let req = |n: &str, session: &str, item: &str| {
        let mut r = commit_req("r", vec![a], vec![put(n, b"b")]);
        r.metadata
            .insert("vogt.session".to_owned(), session.to_owned());
        r.metadata
            .insert("vogt.work_item".to_owned(), item.to_owned());
        r
    };
    let create = |cx: &CallContext, r| block_on(f.engine.create_changeset(cx, r));

    assert_eq!(
        rule(create(&agent, req("x", "ses_2", "WI-1"))),
        "session-metadata"
    );
    assert_eq!(
        rule(create(&agent, req("x", "ses_1", "WI-2"))),
        "work-item-metadata"
    );
    create(&agent, req("x", "ses_1", "WI-1")).expect("own identity");
    // Unset metadata, or a token without a session, is not checked.
    create(&agent, commit_req("r", vec![a], vec![put("y", b"b")])).expect("no metadata");
    create(&repo.cx, req("z", "ses_2", "WI-2")).expect("token has no session");
    let mut service = agent.clone();
    service.principal.kind = PrincipalKind::Service;
    create(&service, req("w", "ses_2", "WI-2")).expect("service acts for others");
}

#[test]
fn unknown_parent() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("f", b"a")], &[]).id;
    let ghost = ChangesetId(ObjectId([3; 32]));
    assert_eq!(
        err(repo.try_commit(vec![a, ghost], vec![del("f")])),
        ApiError::UnknownParent { id: ghost }
    );
}

#[test]
fn missing_objects() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("f", b"a")], &[]).id;
    let changes = vec![put("x", b"never"), put("y", b"uploaded")];
    repo.upload(&[b"uploaded"]);
    assert_eq!(
        err(repo.try_commit(vec![a], changes)),
        ApiError::MissingObjects {
            ids: vec![blob_id(b"never")]
        }
    );
}

#[test]
fn ref_conflict_and_rejected() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("f", b"a")], &[]).id;
    let b = repo.commit(vec![a], &[("f", b"b")], &[]).id;
    let c = repo.commit(vec![a], &[("f", b"c")], &[]).id;
    repo.update_ref("main", None, b, RefKind::Line)
        .expect("line");
    assert_eq!(
        err(repo.update_ref("main", None, c, RefKind::Line)),
        ApiError::RefConflict {
            name: "main".to_owned(),
            actual: Some(b),
        }
    );
    assert!(matches!(
        err(repo.update_ref("main", Some(b), c, RefKind::Line)),
        ApiError::Rejected { policy, .. } if policy == "line-fast-forward"
    ));
}

#[test]
fn permission_denied() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("src/f", b"a")], &[]).id;
    let e = &f.engine;
    let (scope, _) = denied(block_on(e.create_repo(&cx(&[]), "s".to_owned())));
    assert_eq!(scope, "repo:s:admin");
    let (scope, _) = denied(block_on(
        e.stat(&cx(&["repo:other:read"]), path_at("r", a, "src/f")),
    ));
    assert_eq!(scope, "repo:r:read");

    let reader = cx(&["repo:r:read"]);
    let (scope, path) = denied(block_on(
        e.create_changeset(&reader, commit_req("r", vec![a], vec![del("src/f")])),
    ));
    assert_eq!(
        (scope.as_str(), path.as_deref()),
        ("repo:r:write", Some("src/f"))
    );

    let docs_writer = cx(&["repo:r:write:docs"]);
    let (_, path) = denied(block_on(
        e.create_changeset(&docs_writer, commit_req("r", vec![a], vec![del("src/f")])),
    ));
    assert_eq!(path.as_deref(), Some("src/f"));

    // Lines move by admin only; tool refs by any writer.
    let writer = cx(&["repo:r:write"]);
    let line = block_on(e.update_ref(
        &writer,
        UpdateRef {
            repo: "r".to_owned(),
            name: "main".to_owned(),
            expected: None,
            new: a,
            kind: RefKind::Line,
        },
    ));
    assert_eq!(denied(line).0, "repo:r:admin");
    block_on(e.update_ref(
        &writer,
        UpdateRef {
            repo: "r".to_owned(),
            name: "tool/ci".to_owned(),
            expected: None,
            new: a,
            kind: RefKind::Tool,
        },
    ))
    .expect("a writer moves a tool ref");
    let import = block_on(e.import_changesets(
        &writer,
        ImportChangesets {
            repo: "r".to_owned(),
            changesets: Vec::new(),
        },
    ));
    assert_eq!(denied(import).0, "repo:r:admin");
    // objects:read reads objects of any repository, and nothing else.
    let node = cx(&["objects:read"]);
    block_on(e.has_objects(
        &node,
        HasObjects {
            repo: "r".to_owned(),
            ids: vec![blob_id(b"a")],
        },
    ))
    .expect("objects:read may ask");
    denied(block_on(e.stat(&node, path_at("r", a, "src/f"))));
}

/// The history reads check the read scope before touching the index.
#[test]
fn history_reads_are_scoped() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("src/f", b"a")], &[]).id;
    let e = &f.engine;
    for who in [cx(&[]), cx(&["repo:other:read"]), cx(&["objects:read"])] {
        let log = block_on(e.log(
            &who,
            Log {
                repo: "r".to_owned(),
                from: a,
                until: None,
                paths: Vec::new(),
                page: Page {
                    limit: 10,
                    token: None,
                },
            },
        ));
        assert_eq!(denied(log).0, "repo:r:read");
        let blame = block_on(e.blame(
            &who,
            Blame {
                repo: "r".to_owned(),
                at: a,
                path: p("src/f"),
                lines: None,
            },
        ));
        assert_eq!(denied(blame).0, "repo:r:read");
        let pair = Pair {
            repo: "r".to_owned(),
            a,
            b: a,
        };
        assert_eq!(
            denied(block_on(e.is_ancestor(&who, pair.clone()))).0,
            "repo:r:read"
        );
        assert_eq!(denied(block_on(e.merge_base(&who, pair))).0, "repo:r:read");
    }
    let reader = cx(&["repo:r:read"]);
    let page = Page {
        limit: 10,
        token: None,
    };
    let log = Log {
        repo: "r".to_owned(),
        from: a,
        until: None,
        paths: Vec::new(),
        page,
    };
    block_on(e.log(&reader, log)).expect("a reader reads the log");
}

#[test]
fn limit_exceeded() {
    let f = memory_with(EngineConfig {
        max_changes: 2,
        max_inline_bytes: 8,
        max_blob_bytes: 16,
        max_ids: 2,
        max_import: 1,
        max_diff_paths: 1,
        max_put_bytes: 20,
        ..EngineConfig::default()
    });
    let repo = Repo::new(&f, "r");
    let e = &f.engine;
    let a = repo.commit(vec![], &[("f", b"a")], &[]).id;
    repo.upload(&[b"x"]);
    let three = vec![put("x1", b"x"), put("x2", b"x"), put("x3", b"x")];
    assert_eq!(
        limit(repo.try_commit(vec![a], three)),
        ("changes".to_owned(), 2)
    );

    let mut inline = commit_req("r", vec![a], vec![put("big", b"0123456789")]);
    inline.inline_blobs = vec![Frame::encode(ObjectKind::Blob, b"0123456789")];
    assert_eq!(
        limit(block_on(e.create_changeset(&repo.cx, inline))).0,
        "inline-bytes"
    );

    let upload = |blobs: Vec<&[u8]>| {
        block_on(
            e.put_objects(
                &repo.cx,
                PutObjects {
                    repo: "r".to_owned(),
                    blobs: blobs
                        .into_iter()
                        .map(|b| Frame::encode(ObjectKind::Blob, b))
                        .collect(),
                },
            ),
        )
    };
    assert_eq!(limit(upload(vec![&[0; 17]])), ("blob-bytes".to_owned(), 16));
    assert_eq!(limit(upload(vec![b"1", b"2", b"3"])).0, "ids");
    assert_eq!(limit(upload(vec![&[1; 11], &[2; 11]])).0, "put-bytes");
    let ids = vec![blob_id(b"1"), blob_id(b"2"), blob_id(b"3")];
    let has = block_on(e.has_objects(
        &repo.cx,
        HasObjects {
            repo: "r".to_owned(),
            ids: ids.clone(),
        },
    ));
    assert_eq!(limit(has).0, "ids");
    let gets = block_on(e.get_changesets(
        &repo.cx,
        GetChangesets {
            repo: "r".to_owned(),
            ids: vec![a, a, a],
        },
    ));
    assert_eq!(limit(gets).0, "ids");
    // GetObjects defers what it does not answer instead of failing.
    let got = repo.get_objects(ids.clone());
    assert_eq!(got.deferred, vec![ids[2]]);

    let cs = repo.changeset(a).changeset;
    let import = block_on(e.import_changesets(
        &repo.cx,
        ImportChangesets {
            repo: "r".to_owned(),
            changesets: vec![cs.clone(), cs],
        },
    ));
    assert_eq!(limit(import), ("import".to_owned(), 1));
    let diff = block_on(e.diff(
        &repo.cx,
        Diff {
            repo: "r".to_owned(),
            from: a,
            to: a,
            paths: vec![p("x"), p("y")],
            patch: None,
            page: Page::default(),
        },
    ));
    assert_eq!(limit(diff), ("diff-paths".to_owned(), 1));
}

#[test]
fn a_bad_page_token_is_never_echoed() {
    let f = memory();
    let e = &f.engine;
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("dir/f", b"a")], &[]).id;
    let secret = "graft_tok_SECRET//value";
    let page = || Page {
        limit: 1,
        token: Some(secret.to_owned()),
    };
    let check = |r: ApiResult<String>| {
        let e = err(r);
        let shown = format!("{e:?} {e}");
        assert!(!shown.contains("SECRET"), "cursor echoed: {shown}");
        assert!(
            matches!(&e, ApiError::InvalidChange { path, rule } if path.is_empty() && rule == "page-token"),
            "{e:?}"
        );
    };
    for recursive in [false, true] {
        check(
            block_on(e.list_tree(
                &repo.cx,
                ListTree {
                    repo: "r".to_owned(),
                    at: a,
                    path: Some(p("dir")),
                    recursive,
                    page: page(),
                },
            ))
            .map(|r| format!("{r:?}")),
        );
    }
    check(
        block_on(e.diff(
            &repo.cx,
            Diff {
                repo: "r".to_owned(),
                from: a,
                to: a,
                paths: vec![],
                patch: None,
                page: page(),
            },
        ))
        .map(|r| format!("{r:?}")),
    );
}

#[test]
fn get_objects_defers_past_the_byte_budget() {
    let f = memory_with(EngineConfig {
        max_get_bytes: 40,
        ..EngineConfig::default()
    });
    let repo = Repo::new(&f, "r");
    let ids = repo.upload(&[&[1; 30], &[2; 30], &[3; 5]]);
    let got = repo.get_objects(ids.clone());
    assert_eq!(got.found.len(), 1, "the first frame always fits");
    assert_eq!(got.deferred, vec![ids[1], ids[2]]);
    let rest = repo.get_objects(got.deferred);
    assert_eq!(rest.found.len(), 1);
    assert_eq!(rest.deferred, vec![ids[2]]);
}

#[test]
fn corrupt_object() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    let a = repo.commit(vec![], &[("f", b"good bytes")], &[]).id;
    let id = blob_id(b"good bytes");
    f.objects.corrupt(
        rid(&repo),
        id,
        Frame::encode(ObjectKind::Blob, b"bad bytes").to_bytes(),
    );
    let get = block_on(f.engine.get_objects(
        &repo.cx,
        GetObjects {
            repo: "r".to_owned(),
            ids: vec![id],
        },
    ));
    assert_eq!(err(get), ApiError::CorruptObject { id });
    let read = block_on(f.engine.read_file(&repo.cx, path_at("r", a, "f")));
    assert_eq!(err(read), ApiError::CorruptObject { id });

    // A corrupt tree fails reads that walk it.
    let root = repo.changeset(a).changeset.root;
    f.objects.corrupt(rid(&repo), root.0, b"GRF1junk".to_vec());
    let stat = block_on(f.engine.stat(&repo.cx, path_at("r", a, "f")));
    assert_eq!(err(stat), ApiError::CorruptObject { id: root.0 });
}

#[test]
fn unavailable() {
    let f = memory();
    let repo = Repo::new(&f, "r");
    f.objects.fail.store(true, Ordering::SeqCst);
    let put = block_on(f.engine.put_objects(
        &repo.cx,
        PutObjects {
            repo: "r".to_owned(),
            blobs: vec![Frame::encode(ObjectKind::Blob, b"x")],
        },
    ));
    assert_eq!(err(put), ApiError::Unavailable { retry_after_ms: 7 });
}
