//! A local repository is the engine over the server's stores, in-process
//! (D32): create, a changeset, log, and a second opener that finds the lock
//! held.

use std::sync::Arc;

use graft_client::{Error, LocalRepo};
use graft_model::{Author, AuthorKind, Change, EntryKind, ObjectKind, RepoPath, Timestamp};
use graft_proto::Frame;
use graft_proto::api::RepoApi as _;
use graft_proto::types::{CreateChangeset, Log, Page};
use std::collections::BTreeMap;

#[test]
fn create_commit_and_log_need_no_server() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let repo = LocalRepo::create(&repo_dir, "demo").unwrap();
    assert_eq!(repo.name(), "demo");
    assert!(repo_dir.join("meta.sqlite").is_file());
    assert!(repo_dir.join("objects").is_dir());
    assert!(repo_dir.join("repo.lock").is_file());

    let body = b"hello\n";
    let cx = LocalRepo::context("demo", "req-1");
    let created = poll(repo.engine().create_changeset(
        &cx,
        CreateChangeset {
            repo: "demo".into(),
            parents: vec![],
            changes: vec![Change::Put {
                path: RepoPath::new("README.md").unwrap(),
                kind: EntryKind::File,
                id: graft_model::ObjectId::hash(ObjectKind::Blob, body),
            }],
            message: "init".into(),
            author: Author {
                name: "Local".into(),
                email: "local@example.com".into(),
                kind: AuthorKind::Human,
            },
            authored_at: Timestamp {
                ms: 1_700_000_000_000,
                offset_min: 0,
            },
            metadata: BTreeMap::new(),
            inline_blobs: vec![Frame::encode(ObjectKind::Blob, body)],
            workspace: None,
        },
    ))
    .unwrap();

    let history = poll(repo.engine().log(
        &cx,
        Log {
            repo: "demo".into(),
            from: created.id,
            until: None,
            paths: vec![],
            page: Page {
                limit: 10,
                token: None,
            },
        },
    ))
    .unwrap();
    assert_eq!(history.changesets.len(), 1);
    assert_eq!(history.changesets[0].id, created.id);
    assert_eq!(history.changesets[0].subject, "init");

    // The lock is held: a second open in this process fails the same way a
    // second process would.
    match LocalRepo::open(&repo_dir) {
        Err(Error::Busy) => {}
        other => panic!("expected Busy, got {other:?}"),
    }

    drop(repo);
    let again = LocalRepo::open(&repo_dir).unwrap();
    let history = poll(again.engine().log(
        &cx,
        Log {
            repo: "demo".into(),
            from: created.id,
            until: None,
            paths: vec![],
            page: Page {
                limit: 10,
                token: None,
            },
        },
    ))
    .unwrap();
    assert_eq!(history.changesets[0].subject, "init");
    let _shared: Arc<_> = again.engine_handle();
}

fn poll<T>(
    fut: impl std::future::Future<Output = Result<T, graft_proto::ApiError>>,
) -> Result<T, graft_proto::ApiError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(fut)
}
