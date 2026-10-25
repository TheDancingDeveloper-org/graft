//! The client against an in-process server on a loopback socket (G1.11).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use graft_client::fetch::{CACHE_REPO, FetchOptions, fetch_slice, resolve_root};
use graft_client::workcopy::{
    DEFAULT_CACHE_BYTES, FileStatus, WcConfig, WcState, WorkingCopy, clone,
};
use graft_client::{Endpoint, Error, Remote, Sparse};
use graft_model::{BlobId, ObjectId, RepoPath, Tree, TreeId};
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{GetObjects, HasObjects, PutObjects, ResolveRef};
use graft_proto::{ApiError, CallContext, Frame, ProtocolVersion};
use graft_store::ObjectStore;
use graft_testkit::fake_api::admin_cx;
use graft_testkit::server::{Rpc, SnapshotRepo, TestServer};
use graft_testkit::synth::{SynthSpec, generate};

type Files = BTreeMap<String, Vec<u8>>;

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

fn synth(files: usize, seed: u64) -> Files {
    generate(&SynthSpec {
        files,
        depth: 3,
        history: 1,
        file_bytes: 200,
        seed,
    })
    .snapshots
    .pop()
    .unwrap()
}

async fn serve(files: &Files) -> (Arc<SnapshotRepo>, TestServer, Remote) {
    let repo = Arc::new(SnapshotRepo::new());
    repo.commit("main", files).await;
    let server = TestServer::tcp(repo.clone()).await;
    let remote = Remote::connect(Endpoint::parse(server.url()))
        .await
        .unwrap();
    (repo, server, remote)
}

/// Every tree of a snapshot by directory ("" is the root), read from the
/// fake directly (not through the server).
async fn trees(repo: &SnapshotRepo, root: TreeId) -> BTreeMap<String, (TreeId, Tree)> {
    let mut out = BTreeMap::new();
    let mut todo = vec![(String::new(), root)];
    while let Some((dir, id)) = todo.pop() {
        let got = repo
            .fake()
            .get_objects(
                &cx(),
                GetObjects {
                    repo: REPO.into(),
                    ids: vec![id.0],
                },
            )
            .await
            .unwrap();
        let tree = Tree::decode(&got.found[0].1.decode().unwrap()).unwrap();
        for e in &tree.entries {
            if e.kind.is_tree() {
                let path = if dir.is_empty() {
                    e.name.clone()
                } else {
                    format!("{dir}/{}", e.name)
                };
                todo.push((path, TreeId(e.id)));
            }
        }
        out.insert(dir, (id, tree));
    }
    out
}

/// The ids a fetch of `sparse` needs: the trees it must read and the blobs
/// of the files in it.
async fn slice_ids(
    repo: &SnapshotRepo,
    root: TreeId,
    files: &Files,
    sparse: &Sparse,
) -> HashSet<ObjectId> {
    let mut ids: HashSet<ObjectId> = trees(repo, root)
        .await
        .into_iter()
        .filter(|(dir, _)| dir.is_empty() || sparse.descends(&p(dir)))
        .map(|(_, (id, _))| id.0)
        .collect();
    for (path, bytes) in files {
        if sparse.includes(&p(path)) {
            ids.insert(BlobId::hash(bytes).0);
        }
    }
    ids
}

/// The working files under `root`, excluding `.graft`.
fn disk(root: &Path) -> Files {
    fn walk(dir: &Path, prefix: &str, out: &mut Files) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if e.file_type().unwrap().is_dir() {
                if path != ".graft" {
                    walk(&e.path(), &path, out);
                }
            } else {
                out.insert(path, std::fs::read(e.path()).unwrap());
            }
        }
    }
    let mut out = Files::new();
    walk(root, "", &mut out);
    out
}

fn in_slice(files: &Files, sparse: &Sparse) -> Files {
    files
        .iter()
        .filter(|(path, _)| sparse.includes(&p(path)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[tokio::test]
async fn remote_speaks_the_api_over_tcp_and_unix() {
    let files = synth(20, 1);
    let repo = Arc::new(SnapshotRepo::new());
    let tip = repo.commit("main", &files).await;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("node.sock");
    let tcp = TestServer::tcp(repo.clone()).await;
    let unix = TestServer::unix(repo.clone(), &sock).await;

    for (server, endpoint) in [
        (&tcp, Endpoint::parse(tcp.url())),
        (&unix, Endpoint::NodeSocket(sock.clone())),
    ] {
        let remote = Remote::connect(endpoint)
            .await
            .unwrap()
            .with_token("secret-token");
        assert!(!format!("{remote:?}").contains("secret-token"));
        let r = remote
            .resolve_ref(
                &cx(),
                ResolveRef {
                    repo: REPO.into(),
                    name: "main".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(r.target, tip);
        let root = resolve_root(&remote, &cx(), REPO, tip).await.unwrap();
        assert_eq!(Some(root), repo.root(tip));

        // Typed errors survive the wire.
        let err = remote
            .resolve_ref(
                &cx(),
                ResolveRef {
                    repo: REPO.into(),
                    name: "nope".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::NotFound { .. }), "{err:?}");

        // Upload through the client stream, then see it stored.
        let blob = format!("uploaded through {}", server.url()).into_bytes();
        let id = BlobId::hash(&blob).0;
        let stored = remote
            .put_objects(
                &cx(),
                PutObjects {
                    repo: REPO.into(),
                    blobs: vec![Frame::encode(graft_model::ObjectKind::Blob, &blob)],
                },
            )
            .await
            .unwrap();
        assert_eq!(stored, vec![id]);
        let missing = remote
            .has_objects(
                &cx(),
                HasObjects {
                    repo: REPO.into(),
                    ids: vec![id, BlobId::hash(b"absent").0],
                },
            )
            .await
            .unwrap();
        assert_eq!(missing, vec![BlobId::hash(b"absent").0]);

        // A deadline already past fails before sending.
        let mut late = cx();
        late.deadline_ms = Some(1);
        let calls = server.calls().len();
        let err = remote
            .has_objects(&late, HasObjects::default())
            .await
            .unwrap_err();
        assert_eq!(err, ApiError::Unavailable { retry_after_ms: 0 });
        assert_eq!(server.calls().len(), calls);
    }

    // A server that can't bind the client's version refuses it, by name.
    tcp.set_protocol_version(ProtocolVersion { major: 1, minor: 0 });
    let remote = Remote::connect(Endpoint::parse(tcp.url())).await.unwrap();
    let err = remote
        .has_objects(&cx(), HasObjects::default())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ApiError::IncompatibleProtocol {
            server: "1.0".into()
        }
    );
    // A newer-minor client is refused by an older server too.
    tcp.set_protocol_version(graft_proto::PROTOCOL_VERSION);
    let newer = Remote::connect(Endpoint::parse(tcp.url()))
        .await
        .unwrap()
        .with_protocol_version(ProtocolVersion {
            major: graft_proto::PROTOCOL_VERSION.major,
            minor: graft_proto::PROTOCOL_VERSION.minor + 1,
        });
    assert!(matches!(
        newer.has_objects(&cx(), HasObjects::default()).await,
        Err(ApiError::IncompatibleProtocol { .. })
    ));
}

#[tokio::test]
async fn unreachable_endpoints_are_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let err = Remote::connect(Endpoint::NodeSocket(dir.path().join("none.sock")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Api(ApiError::Unavailable { .. })),
        "{err:?}"
    );
    let err = Remote::connect(Endpoint::Server("not a url".into()))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Endpoint(_)), "{err:?}");
}

#[tokio::test]
async fn a_three_prefix_clone_fetches_only_objects_under_the_prefixes() {
    let files = synth(400, 2);
    let (repo, server, remote) = serve(&files).await;
    let tip = repo.tip("main").unwrap();
    let root = repo.root(tip).unwrap();

    // Three prefixes at different depths, from the snapshot itself.
    let dirs: BTreeSet<String> = files
        .keys()
        .filter_map(|f| f.rsplit_once('/').map(|(d, _)| d.to_owned()))
        .collect();
    let mut chosen: Vec<RepoPath> = Vec::new();
    for depth in 1..=3 {
        let d = dirs
            .iter()
            .map(|d| p(d))
            .find(|d| {
                d.components().count() == depth
                    && chosen
                        .iter()
                        .all(|c| !d.starts_with(c) && !c.starts_with(d))
            })
            .unwrap();
        chosen.push(d);
    }
    let sparse = Sparse::new(chosen, false);
    assert_eq!(sparse.prefixes.len(), 3);

    let wanted = slice_ids(&repo, root, &files, &sparse).await;
    let dest = tempfile::tempdir().unwrap();
    server.take_calls();
    let (wc, stats) = clone(
        &remote,
        &cx(),
        dest.path(),
        config(),
        "main",
        None,
        sparse.clone(),
        &FetchOptions {
            batch: 32,
            window: 4,
            ..FetchOptions::default()
        },
    )
    .await
    .unwrap();

    // Exactly the slice's objects were requested, each once.
    let requested = server.requested_ids();
    let unique: HashSet<ObjectId> = requested.iter().copied().collect();
    assert_eq!(unique.len(), requested.len(), "an id was requested twice");
    assert_eq!(unique, wanted);
    assert!(unique.len() < slice_ids(&repo, root, &files, &Sparse::full()).await.len() / 2);
    assert_eq!(stats.trees + stats.blobs, wanted.len() as u64);
    assert_eq!(stats.requests as usize, server.get_objects_calls());

    // Only the slice is on disk, and it is clean.
    assert_eq!(disk(dest.path()), in_slice(&files, &sparse));
    assert!(wc.status().unwrap().is_empty());
    assert_eq!(wc.state().base, tip);
}

#[tokio::test]
async fn an_interrupted_fetch_resumes_asking_only_for_missing_ids() {
    let files = synth(300, 3);
    let (repo, server, remote) = serve(&files).await;
    let tip = repo.tip("main").unwrap();
    let root = repo.root(tip).unwrap();
    let sparse = Sparse::full();
    let all = slice_ids(&repo, root, &files, &sparse).await;

    let dest = tempfile::tempdir().unwrap();
    let state = WcState {
        line: "main".into(),
        base: tip,
        head: None,
        lsn: 0,
        moving: None,
    };
    let mut wc = WorkingCopy::init(dest.path(), config(), state, sparse.clone()).unwrap();
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let opts = FetchOptions {
        batch: 8,
        window: 2,
        ..FetchOptions::default()
    };

    // Drop the connection part way through the stream.
    server.take_calls();
    server.drop_connections_after(100);
    let err = fetch_slice(&remote, &cache, &cx(), REPO, root, &sparse, &opts)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Api(ApiError::Unavailable { .. })),
        "{err:?}"
    );
    assert_eq!(server.drops(), 1);
    let ids: Vec<ObjectId> = all.iter().copied().collect();
    let cached: HashSet<ObjectId> = cache
        .has(CACHE_REPO, &ids)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert!(
        !cached.is_empty() && cached.len() < all.len(),
        "{} of {}",
        cached.len(),
        all.len()
    );

    // Resume on the same remote (it reconnects): only the missing ids.
    server.take_calls();
    fetch_slice(&remote, &cache, &cx(), REPO, root, &sparse, &opts)
        .await
        .unwrap();
    let again = server.requested_ids();
    let again_set: HashSet<ObjectId> = again.iter().copied().collect();
    assert_eq!(again_set.len(), again.len(), "an id was requested twice");
    assert!(again_set.is_disjoint(&cached), "asked again for cached ids");
    assert_eq!(&again_set | &cached, all);

    // Checkout now needs nothing from the server but the changeset.
    server.take_calls();
    wc.checkout(&remote, &cache, &cx(), &opts).await.unwrap();
    assert_eq!(server.calls(), vec![Rpc::Repo("GetChangesets")]);
    assert_eq!(disk(dest.path()), files);
}

#[tokio::test]
async fn checkout_refetches_blobs_torn_in_the_cache_once() {
    let files = synth(300, 4);
    let (repo, server, remote) = serve(&files).await;
    let tip = repo.tip("main").unwrap();
    let root = repo.root(tip).unwrap();
    let sparse = Sparse::full();

    let dest = tempfile::tempdir().unwrap();
    let state = WcState {
        line: "main".into(),
        base: tip,
        head: None,
        lsn: 0,
        moving: None,
    };
    let mut wc = WorkingCopy::init(dest.path(), config(), state, sparse.clone()).unwrap();
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let opts = FetchOptions {
        batch: 8,
        window: 2,
        ..FetchOptions::default()
    };
    fetch_slice(&remote, &cache, &cx(), REPO, root, &sparse, &opts)
        .await
        .unwrap();

    // What a crash mid-write leaves: of the blobs, one in three is empty,
    // one in three is cut to half length, one in three is intact.
    let blobs: Vec<ObjectId> = files.values().map(|b| BlobId::hash(b).0).collect();
    let mut torn = HashSet::new();
    for (i, id) in blobs.iter().enumerate() {
        let path = cache.object_path(CACHE_REPO, *id);
        let len = std::fs::metadata(&path).unwrap().len();
        let new_len = match i % 3 {
            0 => 0,
            1 => len / 2,
            _ => continue,
        };
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(new_len)
            .unwrap();
        torn.insert(*id);
    }
    assert!(torn.len() > 10, "{} torn", torn.len());

    // Checkout must succeed, asking the server for exactly the torn blobs,
    // each once, in no more than ceil(torn / batch) requests.
    server.take_calls();
    wc.checkout(&remote, &cache, &cx(), &opts).await.unwrap();
    let asked = server.requested_ids();
    let unique: HashSet<ObjectId> = asked.iter().copied().collect();
    assert_eq!(unique.len(), asked.len(), "an id was requested twice");
    assert_eq!(unique, torn);
    // Refetches go out per directory, in batches of `opts.batch`, so there
    // is at most one request per torn blob.
    assert!(server.get_objects_calls() <= torn.len());
    assert_eq!(disk(dest.path()), files);
}

#[tokio::test]
async fn fifty_same_size_edits_within_a_second_are_all_detected() {
    let files: Files = (0..50)
        .map(|i| (format!("src/f{i:02}.txt"), vec![b'a' + (i % 26) as u8; 64]))
        .collect();
    let (_repo, _server, remote) = serve(&files).await;
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
    assert!(wc.status().unwrap().is_empty());

    // 50 back-to-back edits, each the same size, in place (same inode), with
    // the mtime put back as a coarse-timestamp filesystem would leave it.
    // The pinned mtime stands in for "within one second", so the outcome
    // does not depend on how fast this loop runs on a loaded host.
    for (i, path) in files.keys().enumerate() {
        let file = dest.path().join(path);
        let before = std::fs::symlink_metadata(&file).unwrap();
        let mtime = before.modified().unwrap();
        std::fs::write(&file, vec![b'A' + (i % 26) as u8; 64]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let after = std::fs::symlink_metadata(&file).unwrap();
        assert_eq!(after.modified().unwrap(), mtime, "{path}: mtime moved");
        assert_eq!(after.len(), before.len(), "{path}: size changed");
    }
    let modified = |wc: &WorkingCopy| {
        wc.status()
            .unwrap()
            .into_iter()
            .filter(|s| matches!(s, FileStatus::Modified(_)))
            .count()
    };
    assert_eq!(modified(&wc), 50);

    // The racy case proper: the index records each edited file's current
    // stat (as if written in the same tick as the edits), so only the
    // racily-clean rule can catch them. Every file and the index share one
    // mtime.
    let tick = SystemTime::now();
    for path in files.keys() {
        let f = std::fs::File::options()
            .write(true)
            .open(dest.path().join(path))
            .unwrap();
        f.set_modified(tick).unwrap();
    }
    let index_path = dest.path().join(".graft/index");
    let text = std::fs::read_to_string(&index_path).unwrap();
    let mut out = String::new();
    for (n, line) in text.lines().enumerate() {
        if n == 0 {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let f: Vec<&str> = line.splitn(8, ' ').collect();
        let meta = std::fs::symlink_metadata(dest.path().join(f[7])).unwrap();
        let st = graft_client::workcopy::Stat::of(&meta);
        out.push_str(&format!(
            "{} {} {} {} {} {} {} {}\n",
            f[0], f[1], st.size, st.mtime_ns, st.ctime_ns, st.ino, st.mode, f[7]
        ));
    }
    std::fs::write(&index_path, out).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&index_path)
        .unwrap()
        .set_modified(tick)
        .unwrap();
    let reopened = WorkingCopy::open(dest.path()).unwrap();
    assert_eq!(modified(&reopened), 50);

    // Racily clean but unchanged is still clean.
    for path in files.keys() {
        std::fs::write(dest.path().join(path), &files[path]).unwrap();
    }
    assert!(reopened.status().unwrap().is_empty());
}

#[tokio::test]
async fn status_reports_added_and_deleted_within_the_slice_only() {
    let files: Files = [("a/x", "1"), ("a/y", "2"), ("b/z", "3"), ("top", "4")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect();
    let (_repo, _server, remote) = serve(&files).await;
    let dest = tempfile::tempdir().unwrap();
    let sparse = Sparse::new([p("a")], true);
    let (wc, _) = clone(
        &remote,
        &cx(),
        dest.path(),
        config(),
        "main",
        None,
        sparse,
        &FetchOptions::default(),
    )
    .await
    .unwrap();
    assert!(!dest.path().join("b").exists());
    std::fs::remove_file(dest.path().join("a/x")).unwrap();
    std::fs::write(dest.path().join("a/new"), b"n").unwrap();
    std::fs::write(dest.path().join("new-top"), b"n").unwrap();
    // Outside the slice: never reported.
    std::fs::create_dir(dest.path().join("b")).unwrap();
    std::fs::write(dest.path().join("b/ignored"), b"n").unwrap();
    assert_eq!(
        wc.status().unwrap(),
        vec![
            FileStatus::Added(p("a/new")),
            FileStatus::Deleted(p("a/x")),
            FileStatus::Added(p("new-top")),
        ]
    );
}

#[tokio::test]
async fn update_from_a_to_b_fetches_only_the_blobs_diff_names() {
    let a_files = synth(300, 4);
    let (repo, server, remote) = serve(&a_files).await;
    let top: BTreeSet<String> = a_files
        .keys()
        .filter_map(|f| f.split_once('/').map(|(d, _)| d.to_owned()))
        .collect();
    let mut top = top.into_iter();
    let sparse = Sparse::new([p(&top.next().unwrap()), p(&top.next().unwrap())], false);

    let dest = tempfile::tempdir().unwrap();
    let (mut wc, _) = clone(
        &remote,
        &cx(),
        dest.path(),
        config(),
        "main",
        None,
        sparse.clone(),
        &FetchOptions::default(),
    )
    .await
    .unwrap();

    // B: rewrite every 7th file, delete every 11th, add files in and out of
    // the slice.
    let mut b_files = a_files.clone();
    for (i, path) in a_files.keys().enumerate() {
        if i % 11 == 0 {
            b_files.remove(path);
        } else if i % 7 == 0 {
            b_files.insert(path.clone(), format!("rewritten {i}").into_bytes());
        }
    }
    let inside = sparse.prefixes[0].as_str().to_owned();
    b_files.insert(format!("{inside}/added.txt"), b"added".to_vec());
    b_files.insert("elsewhere/added.txt".into(), b"outside".to_vec());
    let b = repo.commit("main", &b_files).await;

    let expected: HashSet<ObjectId> = b_files
        .iter()
        .filter(|(path, bytes)| sparse.includes(&p(path)) && a_files.get(*path) != Some(*bytes))
        .map(|(_, bytes)| BlobId::hash(bytes).0)
        .collect();
    assert!(!expected.is_empty());

    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    server.take_calls();
    let stats = wc
        .update(&remote, &cache, &cx(), b, &FetchOptions::default())
        .await
        .unwrap();
    let calls = server.calls();
    assert_eq!(calls[0], Rpc::Repo("Diff"));
    assert!(
        calls[1..].iter().all(|c| matches!(c, Rpc::GetObjects(_))),
        "{calls:?}"
    );
    let requested = server.requested_ids();
    let unique: HashSet<ObjectId> = requested.iter().copied().collect();
    assert_eq!(unique.len(), requested.len());
    assert_eq!(unique, expected, "fetched something Diff did not name");
    assert_eq!(stats.fetch.trees, 0);

    assert_eq!(disk(dest.path()), in_slice(&b_files, &sparse));
    assert!(wc.status().unwrap().is_empty());
    assert_eq!(WorkingCopy::open(dest.path()).unwrap().state().base, b);
}

#[tokio::test]
async fn update_refuses_to_overwrite_local_changes() {
    let a_files: Files = [("d/x", "1"), ("d/y", "2")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect();
    let (repo, _server, remote) = serve(&a_files).await;
    let dest = tempfile::tempdir().unwrap();
    let (mut wc, _) = clone(
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
    let mut b_files = a_files.clone();
    b_files.insert("d/x".into(), b"server".to_vec());
    let b = repo.commit("main", &b_files).await;

    std::fs::write(dest.path().join("d/x"), b"local edit").unwrap();
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let err = wc
        .update(&remote, &cache, &cx(), b, &FetchOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Dirty(paths) if paths == &vec![p("d/x")]),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read(dest.path().join("d/x")).unwrap(),
        b"local edit"
    );
    assert_ne!(wc.state().base, b);
}
