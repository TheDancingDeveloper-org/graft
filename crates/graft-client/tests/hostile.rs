//! The client against a hostile or broken server: lying diffs, symlink
//! routes, repeating tree DAGs, padded answers and missing version headers.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graft_client::fetch::{Budget, FetchOptions};
use graft_client::workcopy::{DEFAULT_CACHE_BYTES, WcConfig, clone};
use graft_client::{Endpoint, Error, Remote, Sparse};
use graft_model::{
    Author, AuthorKind, BlobId, Change, Changeset, EntryKind, Metadata, ObjectId, ObjectKind,
    RepoPath, Timestamp, Tree, TreeEntry,
};
use graft_proto::api::ObjectApi;
use graft_proto::types::{DiffResult, FileDiff, GetObjects, HasObjects, PutObjects};
use graft_proto::{ApiError, CallContext, Frame};
use graft_testkit::fake_api::admin_cx;
use graft_testkit::server::{SnapshotRepo, TestServer};

type Entries = BTreeMap<String, (EntryKind, Vec<u8>)>;

const REPO: &str = "r";

fn cx() -> CallContext {
    admin_cx()
}

fn config() -> WcConfig {
    WcConfig {
        endpoint: None,
        repo: REPO.to_owned(),
        workspace: None,
    }
}

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

async fn serve(repo: &Arc<SnapshotRepo>) -> (TestServer, Remote) {
    let server = TestServer::tcp(repo.clone()).await;
    let remote = Remote::connect(Endpoint::parse(server.url()))
        .await
        .unwrap();
    (server, remote)
}

/// Every name under `dir`, recursively, as `rel → bytes` (symlinks as their
/// target, never followed).
fn contents(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().into_string().unwrap();
        let ft = e.file_type().unwrap();
        if ft.is_dir() {
            for (k, v) in contents(&e.path()) {
                out.insert(format!("{name}/{k}"), v);
            }
        } else if ft.is_symlink() {
            let t = std::fs::read_link(e.path()).unwrap();
            out.insert(name, t.as_os_str().as_bytes().to_vec());
        } else {
            out.insert(name, std::fs::read(e.path()).unwrap());
        }
    }
    out
}

fn lie(changes: Vec<Change>) -> Option<DiffResult> {
    Some(DiffResult {
        files: changes
            .into_iter()
            .map(|change| FileDiff {
                change,
                patch: None,
                truncated: false,
            })
            .collect(),
        next: None,
    })
}

#[tokio::test]
async fn a_lying_diff_cannot_write_or_delete_through_a_symlink() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x"), b"precious").unwrap();
    let before = contents(outside.path());
    let target = outside.path().as_os_str().as_bytes().to_vec();

    let repo = Arc::new(SnapshotRepo::new());
    let mut files: Entries = [
        ("a/keep".to_owned(), (EntryKind::File, b"k".to_vec())),
        ("a/l".to_owned(), (EntryKind::Symlink, target)),
    ]
    .into();
    let a = repo.commit_entries("main", &files).await;
    files.insert("z".into(), (EntryKind::File, b"z".to_vec()));
    let b = repo.commit_entries("main", &files).await;
    let (_server, remote) = serve(&repo).await;

    let dest = tempfile::tempdir().unwrap();
    let (mut wc, _) = clone(
        &remote,
        &cx(),
        dest.path(),
        config(),
        "main",
        Some(a),
        Sparse::full(),
        &FetchOptions::default(),
    )
    .await
    .unwrap();
    assert!(
        std::fs::symlink_metadata(dest.path().join("a/l"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();

    // The server holds the blob a lying diff names.
    let evil = b"evil".to_vec();
    repo.fake()
        .put_objects(
            &cx(),
            PutObjects {
                repo: REPO.into(),
                blobs: vec![Frame::encode(ObjectKind::Blob, &evil)],
            },
        )
        .await
        .unwrap();
    let put = |path: &str| Change::Put {
        path: p(path),
        kind: EntryKind::File,
        id: BlobId::hash(&evil).0,
    };

    // Through the base's symlink a/l: an overwrite, a new file, a delete.
    for change in [
        put("a/l/x"),
        put("a/l/new"),
        Change::Delete { path: p("a/l/x") },
    ] {
        let path = change.path().clone();
        repo.lie_about_diff(lie(vec![change]));
        let err = wc
            .update(&remote, &cache, &cx(), b, &FetchOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::UnsafePath(q) if *q == path),
            "{err:?}"
        );
        assert_eq!(contents(outside.path()), before);
        assert_eq!(wc.state().base, a);
    }

    // Through an untracked symlink on disk.
    std::os::unix::fs::symlink(outside.path(), dest.path().join("b")).unwrap();
    repo.lie_about_diff(lie(vec![put("b/x")]));
    let err = wc
        .update(&remote, &cache, &cx(), b, &FetchOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::UnsafePath(_)), "{err:?}");
    assert_eq!(contents(outside.path()), before);
    std::fs::remove_file(dest.path().join("b")).unwrap();

    // An honest diff that replaces the symlink with a directory still works.
    repo.lie_about_diff(None);
    files.remove("a/l");
    files.insert("a/l/x".into(), (EntryKind::File, b"inside".to_vec()));
    let c = repo.commit_entries("main", &files).await;
    wc.update(&remote, &cache, &cx(), c, &FetchOptions::default())
        .await
        .unwrap();
    assert!(
        std::fs::symlink_metadata(dest.path().join("a/l"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(std::fs::read(dest.path().join("a/l/x")).unwrap(), b"inside");
    assert_eq!(contents(outside.path()), before);
    assert!(wc.status().unwrap().is_empty());
}

fn tree_frame(t: &Tree) -> (ObjectId, Frame) {
    (t.id().0, Frame::encode(ObjectKind::Tree, &t.encode()))
}

fn dir(name: &str, child: &Tree) -> TreeEntry {
    TreeEntry {
        name: name.to_owned(),
        kind: EntryKind::Tree,
        id: child.id().0,
    }
}

/// A raw changeset rooted at `root` (unvalidated, as a hostile server
/// might serve it).
fn raw_changeset(root: &Tree) -> Changeset {
    Changeset {
        parents: Vec::new(),
        root: root.id(),
        changes: Vec::new(),
        author: Author {
            name: "x".into(),
            email: "x@example.com".into(),
            kind: AuthorKind::Bot,
        },
        authored_at: Timestamp {
            ms: 0,
            offset_min: 0,
        },
        message: "dag".into(),
        metadata: Metadata::new(),
    }
}

#[tokio::test]
async fn a_repeating_tree_dag_is_stopped_by_the_entry_budget() {
    // Depth 24, each tree naming the same child twice: 26 distinct objects,
    // 2^24 paths.
    let blob = b"leaf".to_vec();
    let mut tree = Tree {
        entries: vec![TreeEntry {
            name: "f".into(),
            kind: EntryKind::File,
            id: BlobId::hash(&blob).0,
        }],
    };
    let mut objects = vec![
        (
            BlobId::hash(&blob).0,
            Frame::encode(ObjectKind::Blob, &blob),
        ),
        tree_frame(&tree),
    ];
    for _ in 0..24 {
        tree = Tree {
            entries: vec![dir("a", &tree), dir("b", &tree)],
        };
        objects.push(tree_frame(&tree));
    }
    let repo = Arc::new(SnapshotRepo::new());
    repo.insert_raw("main", raw_changeset(&tree), objects);
    let (server, remote) = serve(&repo).await;

    let budget = Budget {
        max_entries: 4096,
        ..Budget::default()
    };
    let dest = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let err = clone(
        &remote,
        &cx(),
        dest.path(),
        config(),
        "main",
        None,
        Sparse::full(),
        &FetchOptions {
            budget,
            ..FetchOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, Error::Api(ApiError::LimitExceeded { limit, max: 4096 }) if limit == "client.max_entries"),
        "{err:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(10));
    // The fetch read each distinct object once; checkout stopped early.
    let requested = server.requested_ids();
    assert_eq!(requested.len(), 26);
    assert_eq!(requested.iter().collect::<HashSet<_>>().len(), 26);
    assert!(contents(dest.path()).len() <= 4096 + 8);
}

#[tokio::test]
async fn a_deep_chain_is_stopped_by_the_depth_budget() {
    let blob = b"deep".to_vec();
    let mut tree = Tree {
        entries: vec![TreeEntry {
            name: "f".into(),
            kind: EntryKind::File,
            id: BlobId::hash(&blob).0,
        }],
    };
    let mut objects = vec![
        (
            BlobId::hash(&blob).0,
            Frame::encode(ObjectKind::Blob, &blob),
        ),
        tree_frame(&tree),
    ];
    for _ in 0..400 {
        tree = Tree {
            entries: vec![dir("d", &tree)],
        };
        objects.push(tree_frame(&tree));
    }
    let repo = Arc::new(SnapshotRepo::new());
    repo.insert_raw("main", raw_changeset(&tree), objects);
    let (server, remote) = serve(&repo).await;
    let dest = tempfile::tempdir().unwrap();
    let err = clone(
        &remote,
        &cx(),
        dest.path(),
        config(),
        "main",
        None,
        Sparse::full(),
        &FetchOptions::default(),
    )
    .await
    .unwrap_err();
    let max = Budget::default().max_depth as u64;
    assert!(
        matches!(&err, Error::Api(ApiError::LimitExceeded { limit, max: m }) if limit == "client.max_depth" && *m == max),
        "{err:?}"
    );
    assert!(server.requested_ids().len() as u64 <= max + 1);
}

#[tokio::test]
async fn get_objects_drops_unrequested_ids_and_caps_pages() {
    let repo = Arc::new(SnapshotRepo::new());
    let files = BTreeMap::from([("f".to_owned(), b"wanted".to_vec())]);
    repo.commit("main", &files).await;
    let (_server, remote) = serve(&repo).await;
    let want = BlobId::hash(b"wanted").0;
    let ask = || GetObjects {
        repo: REPO.into(),
        ids: vec![want],
    };

    // A few extra objects in the one page: dropped, the answer is exact.
    repo.pad_get_objects(8);
    let got = remote.get_objects(&cx(), ask()).await.unwrap();
    assert_eq!(
        got.found.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![want]
    );

    // Pages beyond one per requested id plus one: refused.
    repo.pad_get_objects(100);
    let err = remote.get_objects(&cx(), ask()).await.unwrap_err();
    assert!(
        matches!(&err, ApiError::LimitExceeded { limit, max: 2 } if limit == "get-objects-pages"),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_response_without_a_version_header_is_refused() {
    let repo = Arc::new(SnapshotRepo::new());
    repo.commit("main", &BTreeMap::from([("f".to_owned(), b"x".to_vec())]))
        .await;
    let (server, remote) = serve(&repo).await;
    assert!(
        remote
            .has_objects(&cx(), HasObjects::default())
            .await
            .is_ok()
    );

    server.advertise_version(false);
    let none = ApiError::IncompatibleProtocol {
        server: "none".into(),
    };
    assert_eq!(
        remote
            .has_objects(&cx(), HasObjects::default())
            .await
            .unwrap_err(),
        none
    );
    assert_eq!(
        remote
            .get_objects(
                &cx(),
                GetObjects {
                    repo: REPO.into(),
                    ids: vec![BlobId::hash(b"x").0],
                },
            )
            .await
            .unwrap_err(),
        none
    );
}
