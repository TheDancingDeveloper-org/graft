//! The client against a hostile or broken server: lying diffs, symlink
//! routes, repeating tree DAGs, padded answers and missing version headers.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

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
        server: None,
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

/// A clone of `files` with the server's blob for `evil` uploaded.
async fn cloned_with_evil(
    files: &Entries,
    evil: &[u8],
) -> (
    Arc<SnapshotRepo>,
    TestServer,
    Remote,
    tempfile::TempDir,
    graft_client::workcopy::WorkingCopy,
) {
    let repo = Arc::new(SnapshotRepo::new());
    repo.commit_entries("main", files).await;
    let (server, remote) = serve(&repo).await;
    let dest = tempfile::tempdir().unwrap();
    let (wc, _) = clone(
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
    .unwrap();
    repo.fake()
        .put_objects(
            &cx(),
            PutObjects {
                repo: REPO.into(),
                blobs: vec![Frame::encode(ObjectKind::Blob, evil)],
            },
        )
        .await
        .unwrap();
    (repo, server, remote, dest, wc)
}

#[tokio::test]
async fn a_diff_cannot_reach_the_state_directory_in_any_spelling() {
    let mut files: Entries = [("f".to_owned(), (EntryKind::File, b"f".to_vec()))].into();
    let evil = b"repo = attacker\n".to_vec();
    let (repo, _server, remote, dest, mut wc) = cloned_with_evil(&files, &evil).await;
    files.insert("g".into(), (EntryKind::File, b"g".to_vec()));
    let b = repo.commit_entries("main", &files).await;
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let state_before = contents(&dest.path().join(".graft"));

    for path in [
        ".GRAFT/config",
        ".Graft/index",
        // Fullwidth ".graft": NFKC-equal to it.
        "\u{FF0E}\u{FF47}\u{FF52}\u{FF41}\u{FF46}\u{FF54}/state",
        // A zero-width joiner, ignored in names by HFS+ and APFS.
        ".gr\u{200D}aft/sparse",
    ] {
        repo.lie_about_diff(lie(vec![Change::Put {
            path: p(path),
            kind: EntryKind::File,
            id: BlobId::hash(&evil).0,
        }]));
        let err = wc
            .update(&remote, &cache, &cx(), b, &FetchOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Api(ApiError::InvalidPath { path: q, .. }) if q == path),
            "{path:?}: {err:?}"
        );
        let top = path.split('/').next().unwrap();
        assert!(!dest.path().join(top).exists() || top == ".graft");
    }
    assert_eq!(contents(&dest.path().join(".graft")), state_before);
    assert_eq!(wc.state().base, repo.fake().changesets_in_order()[0].id());
}

#[tokio::test]
async fn a_diff_putting_a_symlink_and_a_path_under_it_is_refused_whole() {
    let outside = tempfile::tempdir().unwrap();
    let mut files: Entries = [("a/keep".to_owned(), (EntryKind::File, b"k".to_vec()))].into();
    let evil = b"evil".to_vec();
    let (repo, _server, remote, dest, mut wc) = cloned_with_evil(&files, &evil).await;
    let a = wc.state().base;
    files.insert("z".into(), (EntryKind::File, b"z".to_vec()));
    let b = repo.commit_entries("main", &files).await;
    let target = outside.path().as_os_str().as_bytes().to_vec();
    repo.fake()
        .put_objects(
            &cx(),
            PutObjects {
                repo: REPO.into(),
                blobs: vec![Frame::encode(ObjectKind::Blob, &target)],
            },
        )
        .await
        .unwrap();
    repo.lie_about_diff(lie(vec![
        Change::Put {
            path: p("a/s"),
            kind: EntryKind::Symlink,
            id: BlobId::hash(&target).0,
        },
        Change::Put {
            path: p("a/s/x"),
            kind: EntryKind::File,
            id: BlobId::hash(&evil).0,
        },
    ]));
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let err = wc
        .update(&remote, &cache, &cx(), b, &FetchOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::UnsafePath(q) if *q == p("a/s/x")),
        "{err:?}"
    );
    // Refused before anything was written: no half-applied update.
    assert!(std::fs::symlink_metadata(dest.path().join("a/s")).is_err());
    assert!(contents(outside.path()).is_empty());
    assert_eq!(wc.state().base, a);
    assert!(wc.status().unwrap().is_empty());
}

#[tokio::test]
async fn get_objects_caps_the_bytes_of_one_answer() {
    // 40 incompressible 1 KiB blobs: three pages of frames.
    let mut files = BTreeMap::new();
    for i in 0..40u32 {
        let mut bytes = Vec::new();
        let mut seed = BlobId::hash(&i.to_le_bytes()).0.0;
        while bytes.len() < 1024 {
            bytes.extend_from_slice(&seed);
            seed = BlobId::hash(&seed).0.0;
        }
        files.insert(format!("f{i:02}"), bytes);
    }
    let repo = Arc::new(SnapshotRepo::new());
    repo.commit("main", &files).await;
    let (server, _remote) = serve(&repo).await;
    let ids: Vec<ObjectId> = files.values().map(|b| BlobId::hash(b).0).collect();
    let ask = || GetObjects {
        repo: REPO.into(),
        ids: ids.clone(),
    };

    let capped = Remote::connect(Endpoint::parse(server.url()))
        .await
        .unwrap()
        .with_max_response_bytes(16 * 1024);
    let err = capped.get_objects(&cx(), ask()).await.unwrap_err();
    assert!(
        matches!(&err, ApiError::LimitExceeded { limit, max } if limit == "get-objects-bytes" && *max == 16 * 1024),
        "{err:?}"
    );

    // Under the cap (and at the default) the same answer is whole.
    let roomy = Remote::connect(Endpoint::parse(server.url()))
        .await
        .unwrap()
        .with_max_response_bytes(64 * 1024);
    assert_eq!(
        roomy.get_objects(&cx(), ask()).await.unwrap().found.len(),
        40
    );
    let default = Remote::connect(Endpoint::parse(server.url()))
        .await
        .unwrap();
    assert_eq!(
        default.get_objects(&cx(), ask()).await.unwrap().found.len(),
        40
    );

    assert_eq!(
        graft_client::remote::MAX_GET_OBJECTS_BYTES,
        64 * 1024 * 1024
            + graft_proto::frame::MAX_FRAME_PAYLOAD
            + graft_proto::frame::HEADER_LEN as u64
    );
}
