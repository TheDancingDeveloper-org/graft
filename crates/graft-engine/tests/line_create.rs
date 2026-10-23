//! WI-2301: `CreateLine` forks a line by CAS from absent, journalled, and
//! never overwrites a ref; on both stores.

mod common;

use common::*;
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, RepoApi};
use graft_store::MetaStore;

on_both_stores!(
    create_then_list_then_land_on_the_new_line,
    creating_an_existing_line_is_refused_and_nothing_moves,
    bad_names_and_unknown_revs_are_typed_errors,
    land_or_admin_scope_may_create_the_line,
);

fn create<M: MetaStore>(
    r: &Repo<'_, M>,
    cx: &CallContext,
    name: &str,
    from: ChangesetId,
) -> ApiResult<RefView> {
    block_on(r.e().create_line(
        cx,
        CreateLine {
            repo: r.name.clone(),
            name: name.to_owned(),
            from,
        },
    ))
}

fn lines<M: MetaStore>(r: &Repo<'_, M>) -> Vec<(String, ChangesetId, u64)> {
    block_on(r.e().list_refs(
        &r.cx,
        ListRefs {
            repo: r.name.clone(),
            prefix: None,
            page: Page::default(),
        },
    ))
    .expect("list")
    .into_iter()
    .map(|v| (v.name, v.target, v.version))
    .collect()
}

fn ref_moves<M: MetaStore>(r: &Repo<'_, M>) -> usize {
    r.journal().iter().filter(|e| e.kind == "RefMoved").count()
}

fn create_then_list_then_land_on_the_new_line<M: MetaStore>(f: &Fixture<M>) {
    let r = Repo::new(f, "r");
    let root = r.commit(vec![], &[("a", b"1")], &[]).id;
    let next = r.commit(vec![root], &[("a", b"2")], &[]).id;
    r.update_ref("main", None, next, RefKind::Line)
        .expect("main");

    let made = create(&r, &r.cx, "batch-1", next).expect("create");
    assert_eq!(
        made,
        RefView {
            name: "batch-1".to_owned(),
            target: next,
            version: 1,
            kind: RefKind::Line,
        }
    );
    assert_eq!(
        lines(&r),
        vec![
            ("batch-1".to_owned(), next, 1),
            ("main".to_owned(), next, 1)
        ]
    );
    let entry = r.journal().pop().expect("journalled");
    assert_eq!(entry.kind, "RefMoved");
    assert!(
        entry.body.contains("\"name\":\"batch-1\""),
        "{}",
        entry.body
    );
    assert!(entry.body.contains("\"old\":null"), "{}", entry.body);

    // A land on the new line takes the default (direct) policy and moves
    // only that line.
    let draft = r.commit(vec![next], &[("b", b"3")], &[]).id;
    let landed = block_on(r.e().land(
        &cx(&["repo:r:land:batch-1", "repo:r:write"]),
        Land {
            repo: "r".to_owned(),
            head: draft,
            line: "batch-1".to_owned(),
            squash: false,
            merge: MergeMode::File,
            deadline_ms: None,
        },
    ));
    match landed {
        Ok(Landed::Landed {
            landed,
            line_version,
            ..
        }) => {
            assert_eq!((landed, line_version), (draft, 2));
        }
        other => panic!("expected Landed, got {other:?}"),
    }
    assert_eq!(
        lines(&r),
        vec![
            ("batch-1".to_owned(), draft, 2),
            ("main".to_owned(), next, 1)
        ]
    );
}

fn creating_an_existing_line_is_refused_and_nothing_moves<M: MetaStore>(f: &Fixture<M>) {
    let r = Repo::new(f, "r");
    let a = r.commit(vec![], &[("a", b"1")], &[]).id;
    let b = r.commit(vec![a], &[("a", b"2")], &[]).id;
    create(&r, &r.cx, "main", a).expect("create");
    let before = (lines(&r), ref_moves(&r));

    for from in [b, a] {
        assert_eq!(
            create(&r, &r.cx, "main", from),
            Err(ApiError::RefConflict {
                name: "main".to_owned(),
                actual: Some(a),
            })
        );
    }
    // Nor does it overwrite a ref of another kind.
    r.update_ref("pin", None, a, RefKind::Tool).expect("tool");
    let before_tool = (lines(&r), ref_moves(&r));
    assert_eq!(
        create(&r, &r.cx, "pin", b),
        Err(ApiError::RefConflict {
            name: "pin".to_owned(),
            actual: Some(a),
        })
    );
    assert_eq!((lines(&r), ref_moves(&r)), before_tool);
    assert_eq!(before.1 + 1, before_tool.1);
}

fn bad_names_and_unknown_revs_are_typed_errors<M: MetaStore>(f: &Fixture<M>) {
    let r = Repo::new(f, "r");
    let a = r.commit(vec![], &[("a", b"1")], &[]).id;
    for name in ["", "has space", "tab\there", &"x".repeat(256)] {
        assert_eq!(
            create(&r, &r.cx, name, a),
            Err(ApiError::InvalidChange {
                path: name.to_owned(),
                rule: "ref-name".to_owned(),
            }),
            "{name:?}"
        );
    }
    let unknown = r.commit(vec![a], &[("z", b"9")], &[]).id;
    let other = Repo::new(f, "other");
    assert!(
        matches!(
            create(&other, &other.cx, "line", unknown),
            Err(ApiError::NotFound { .. })
        ),
        "a changeset of another repository is unknown here"
    );
    assert!(lines(&r).is_empty());
    assert!(lines(&other).is_empty());
}

fn land_or_admin_scope_may_create_the_line<M: MetaStore>(f: &Fixture<M>) {
    let r = Repo::new(f, "r");
    let a = r.commit(vec![], &[("a", b"1")], &[]).id;
    let denied = |scopes: &[&str]| {
        assert_eq!(
            create(&r, &cx(scopes), "dev", a),
            Err(ApiError::PermissionDenied {
                scope: "repo:r:land:dev".to_owned(),
                path: None,
            }),
            "{scopes:?}"
        );
    };
    denied(&["repo:r:read"]);
    denied(&["repo:r:write"]);
    denied(&["repo:r:land:main"]);
    denied(&["repo:other:admin", "repo:r:read"]);
    // No scope on the repository at all says nothing about it.
    assert!(matches!(
        create(&r, &cx(&["repo:other:admin"]), "dev", a),
        Err(ApiError::PermissionDenied { .. })
    ));
    assert!(lines(&r).is_empty());

    create(&r, &cx(&["repo:r:land:dev"]), "dev", a).expect("land scope");
    create(&r, &cx(&["repo:r:admin"]), "rel", a).expect("admin");
    assert_eq!(lines(&r).len(), 2);
}
