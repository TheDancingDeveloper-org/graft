//! D41: `repos:admin` creates repositories, and the creator administers
//! what it creates and nothing else. On both stores.

mod common;

use common::*;
use graft_proto::types::*;
use graft_proto::{AdminApi, ApiError, CallContext, RepoApi};
use graft_store::MetaStore;

fn creator(subject: &str) -> CallContext {
    let mut cx = cx(&["repos:admin"]);
    cx.principal.subject = subject.to_owned();
    cx
}

/// The creator's call, as the server runs it: with its creator grant added.
fn granted<M: MetaStore>(f: &Fixture<M>, cx: &CallContext, repo: &str) -> CallContext {
    let mut cx = cx.clone();
    block_on(f.engine.grant_creator(&mut cx, repo)).expect("grant");
    cx
}

fn admin_call<M: MetaStore>(f: &Fixture<M>, cx: &CallContext, repo: &str) -> Result<(), ApiError> {
    // Moving a line is admin-only.
    block_on(f.engine.update_ref(
        cx,
        UpdateRef {
            repo: repo.to_owned(),
            name: "main".to_owned(),
            expected: None,
            new: graft_model::ChangesetId(blob_id(b"x")),
            kind: RefKind::Line,
        },
    ))
    .map(drop)
}

fn the_creator_administers_what_it_creates<M: MetaStore>(f: Fixture<M>) {
    let alice = creator("alice");
    block_on(f.engine.create_repo(&alice, "made".to_owned())).expect("repos:admin creates");
    // Before the grant it is no admin; after it, it is, on that repo only.
    assert!(matches!(
        admin_call(&f, &alice, "made"),
        Err(ApiError::PermissionDenied { .. })
    ));
    let cx = granted(&f, &alice, "made");
    assert!(cx.principal.scopes.iter().any(|s| s == "repo:made:admin"));
    // Admin reaches the ref check: the target does not exist.
    assert!(matches!(
        admin_call(&f, &cx, "made"),
        Err(ApiError::NotFound { .. })
    ));
    // A retry is still a success.
    block_on(f.engine.create_repo(&alice, "made".to_owned())).expect("retry");

    // Another repos:admin holder does not administer it.
    let bob = creator("bob");
    let cx = granted(&f, &bob, "made");
    assert_eq!(cx.principal.scopes, ["repos:admin"]);
    // Nor does alice administer a repository created by someone else.
    Repo::new(&f, "theirs");
    let cx = granted(&f, &alice, "theirs");
    assert_eq!(cx.principal.scopes, ["repos:admin"]);
    // And without repos:admin nothing is created.
    let reader = common::cx(&["repo:made:read"]);
    assert!(matches!(
        block_on(f.engine.create_repo(&reader, "other".to_owned())),
        Err(ApiError::PermissionDenied { .. })
    ));
    assert!(matches!(
        block_on(f.engine.resolve_ref(
            &admin("other"),
            ResolveRef {
                repo: "other".to_owned(),
                name: "main".to_owned(),
            }
        )),
        Err(ApiError::NotFound { .. })
    ));
}

#[test]
fn memory_store() {
    the_creator_administers_what_it_creates(memory());
}

#[test]
fn sqlite_store() {
    the_creator_administers_what_it_creates(sqlite());
}
