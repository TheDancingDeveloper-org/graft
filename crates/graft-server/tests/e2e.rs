//! End to end through the binary: import a synthetic repository over the
//! API, `kill -9` the server, restart it, fetch everything back and verify
//! it, then stop it gracefully and run `admin fsck` (G1.12 acceptance).

mod common;

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use common::{Site, SynthSpec, generate};
use graft_model::{ChangesetId, EntryKind, ObjectId, ObjectKind, Tree, TreeId};
use graft_proto::types::{Changesets, ImportChangesets, RefKind, UpdateRef};
use graft_proto::{ApiError, pb};

const REPO: &str = "synth";

/// Every path and blob under `root`, fetched tree by tree, level by level.
async fn fetch_snapshot(
    clients: &mut common::Clients,
    repo: &str,
    root: TreeId,
    seen: &mut HashSet<ObjectId>,
) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut level: Vec<(String, TreeId)> = vec![(String::new(), root)];
    while !level.is_empty() {
        let ids: Vec<ObjectId> = level.iter().map(|(_, t)| t.0).collect();
        let frames: BTreeMap<ObjectId, _> =
            clients.get_objects(repo, &ids).await.into_iter().collect();
        let mut next = Vec::new();
        let mut blobs: Vec<(String, ObjectId)> = Vec::new();
        for (prefix, tree_id) in level {
            let frame = frames.get(&tree_id.0).expect("tree fetched");
            assert_eq!(frame.kind, ObjectKind::Tree);
            let tree = Tree::decode(&frame.decode_verified(tree_id.0).expect("tree verifies"))
                .expect("tree decodes");
            seen.insert(tree_id.0);
            for e in tree.entries {
                let path = if prefix.is_empty() {
                    e.name.clone()
                } else {
                    format!("{prefix}/{}", e.name)
                };
                match e.kind {
                    EntryKind::Tree => next.push((path, TreeId(e.id))),
                    _ => blobs.push((path, e.id)),
                }
            }
        }
        let ids: Vec<ObjectId> = blobs.iter().map(|(_, id)| *id).collect();
        let frames: BTreeMap<ObjectId, _> =
            clients.get_objects(repo, &ids).await.into_iter().collect();
        for (path, id) in blobs {
            let frame = frames.get(&id).expect("blob fetched");
            files.insert(path, frame.decode_verified(id).expect("blob verifies"));
            seen.insert(id);
        }
        level = next;
    }
    files
}

// Quarantine (WI-2467): ~75s kill-9 import/fetch/fsck failed once on an
// unrelated merge-queue run (PR #196) and passed on retry, with no stable
// cause. nextest skips `#[ignore]`d tests, so the required `test` job no
// longer flakes on it. The merge-queue slow step runs it with `--run-ignored
// only-this`; see docs/known-flakes.md.
#[ignore = "slow kill-9 e2e; merge-queue slow set only (WI-2467)"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_kill9_restart_fetch_fsck() {
    let site = Site::new("server-e2e", "");
    let token = site.mint(&[&format!("repo:{REPO}:admin"), "repo:copy:admin"]);
    let synth = generate(&SynthSpec {
        files: 300,
        depth: 3,
        history: 30,
        file_bytes: 1500,
        seed: 12,
    });

    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    c.create_repo(REPO).await.expect("create repo");
    let created = c.import(REPO, &synth.snapshots).await;
    let tip = created.last().expect("history").id;
    c.repo
        .update_ref(pb::UpdateRefRequest::from(UpdateRef {
            repo: REPO.into(),
            name: "main".into(),
            expected: None,
            new: tip,
            kind: RefKind::Line,
        }))
        .await
        .expect("move main");

    // A retried commit is idempotent: same id, nothing new.
    let again = c
        .commit(REPO, None, &BTreeMap::new(), &synth.snapshots[0], 0)
        .await
        .expect("retry");
    assert_eq!(again.id, created[0].id);

    // Bulk import into a second repository, from what the first one serves.
    c.create_repo("copy").await.expect("create copy");
    let all: Vec<Vec<u8>> = synth
        .snapshots
        .iter()
        .flat_map(|s| s.values().cloned())
        .collect();
    c.put_blobs("copy", &all)
        .await
        .expect("put blobs into copy");
    let ids: Vec<ChangesetId> = created.iter().map(|c| c.id).collect();
    let got = c
        .repo
        .get_changesets(pb::GetChangesetsRequest {
            repo: REPO.into(),
            ids: ids.iter().map(|i| i.0.0.to_vec()).collect(),
        })
        .await
        .expect("get changesets")
        .into_inner();
    let got = Changesets::try_from(got).expect("valid");
    let imported = c
        .admin
        .import_changesets(pb::ImportChangesetsRequest::from(ImportChangesets {
            repo: "copy".into(),
            changesets: got.found.iter().map(|v| v.changeset.clone()).collect(),
        }))
        .await
        .expect("import")
        .into_inner();
    assert_eq!(imported.created.len(), created.len());
    drop(c);

    // kill -9: every acknowledged write must survive.
    server.kill9();
    let server = site.start();
    assert!(
        server.startup < Duration::from_secs(2),
        "restart to serving took {:?}",
        server.startup
    );

    let mut c = server.clients(Some(&token)).await;
    let main = c
        .repo
        .resolve_ref(pb::ResolveRefRequest {
            repo: REPO.into(),
            name: "main".into(),
        })
        .await
        .expect("resolve main")
        .into_inner();
    assert_eq!(main.target, tip.0.0.to_vec());

    let got = c
        .repo
        .get_changesets(pb::GetChangesetsRequest {
            repo: REPO.into(),
            ids: ids.iter().map(|i| i.0.0.to_vec()).collect(),
        })
        .await
        .expect("get changesets")
        .into_inner();
    let got = Changesets::try_from(got).expect("valid");
    assert_eq!(got.found.len(), created.len(), "missing: {:?}", got.missing);

    // Fetch every snapshot and compare it with what was committed.
    let mut seen = HashSet::new();
    for (view, expected) in got.found.iter().zip(&synth.snapshots) {
        let files = fetch_snapshot(&mut c, REPO, view.changeset.root, &mut seen).await;
        assert_eq!(&files, expected, "snapshot of {}", view.changeset.id());
    }
    assert!(seen.len() > synth.snapshots.last().expect("tip").len());

    // The journal lists every changeset and the ref move.
    let mut watch = c
        .repo
        .watch(pb::WatchRequest {
            repo: REPO.into(),
            from_lsn: 1,
            kinds: Vec::new(),
            limit: 7,
            wait_ms: 0,
        })
        .await
        .expect("watch")
        .into_inner();
    let mut entries = Vec::new();
    while let Some(page) = watch.message().await.expect("page") {
        entries.extend(page.entries);
    }
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.kind == "ChangesetCreated")
            .count(),
        created.len()
    );
    assert_eq!(entries.iter().filter(|e| e.kind == "RefMoved").count(), 1);
    drop(c);

    let status = server.stop();
    assert!(status.success(), "graceful stop: {status:?}");

    for repo in [REPO, "copy"] {
        let out = site.admin(&["fsck", repo]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "fsck {repo}: {stdout}");
        assert!(stdout.contains(", 0 problems"), "{stdout}");
        assert!(
            stdout.contains(&format!("{} changesets", created.len())),
            "{stdout}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fsck_reports_a_corrupt_object_and_backup_restores_clean() {
    let site = Site::new("server-fsck", "");
    let token = site.mint(&["repo:r:admin"]);
    let synth = generate(&SynthSpec {
        files: 20,
        depth: 2,
        history: 3,
        file_bytes: 600,
        seed: 3,
    });
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    c.create_repo("r").await.expect("create");
    c.import("r", &synth.snapshots).await;
    drop(c);

    // A backup taken while the server runs.
    let backup_dir = site.dir.path().join("backup");
    let out = site.admin(&["backup", backup_dir.to_str().expect("utf8"), "--objects"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(server.stop().success());

    // Damage one stored blob: fsck must find it.
    let blob = synth.snapshots[0].values().next().expect("a file");
    let id = ObjectId::hash(ObjectKind::Blob, blob).to_hex();
    let path = site
        .objects_dir()
        .join("1/objects")
        .join(&id[..2])
        .join(&id[2..]);
    let mut bytes = std::fs::read(&path).expect("read object");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, bytes).expect("write object");
    let out = site.admin(&["fsck", "r"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains("corrupt"), "{stdout}");

    // The backup, restored onto a scratch site, checks clean.
    let restored = Site::new("server-restore", "");
    std::fs::create_dir_all(restored.dir.path().join("meta")).expect("mkdir");
    std::fs::copy(
        backup_dir.join("meta.db"),
        restored.dir.path().join("meta/meta.db"),
    )
    .expect("restore meta");
    copy_dir(&backup_dir.join("objects"), &restored.objects_dir());
    let out = restored.admin(&["fsck", "r"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains(", 0 problems"), "{stdout}");
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read dir") {
        let e = e.expect("entry");
        let dest = to.join(e.file_name());
        if e.file_type().expect("type").is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).expect("copy");
        }
    }
}

/// The server's object store does not verify reads (every reader does), so
/// a damaged blob on disk is still refused by the readers that serve it:
/// GetObjects and a diff with patches both answer `CorruptObject`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damaged_blob_is_refused_by_the_readers() {
    use graft_proto::types::{Diff, Page, PatchOptions};

    let site = Site::new("server-damaged", "");
    let token = site.mint(&["repo:r:admin"]);
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    c.create_repo("r").await.expect("create");
    let snapshots: Vec<BTreeMap<String, Vec<u8>>> = [b"one\n".as_slice(), b"two\n"]
        .iter()
        .map(|text| BTreeMap::from([("f".to_owned(), text.to_vec())]))
        .collect();
    let created = c.import("r", &snapshots).await;

    // Flip the last byte of the old blob, as fsck's test does.
    let id = ObjectId::hash(ObjectKind::Blob, b"one\n");
    let hex = id.to_hex();
    let path = site
        .objects_dir()
        .join("1/objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    let mut bytes = std::fs::read(&path).expect("read object");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, bytes).expect("write object");

    let get = c
        .objects
        .get_objects(pb::GetObjectsRequest {
            repo: "r".into(),
            ids: vec![id.0.to_vec()],
        })
        .await;
    let err = match get {
        Err(status) => common::api(status),
        Ok(stream) => common::api(
            stream
                .into_inner()
                .message()
                .await
                .expect_err("the page is refused"),
        ),
    };
    assert_eq!(err, ApiError::CorruptObject { id });

    let diff = c
        .repo
        .diff(pb::DiffRequest::from(Diff {
            repo: "r".into(),
            from: created[0].id,
            to: created[1].id,
            paths: vec![],
            patch: Some(PatchOptions {
                context: 3,
                max_bytes_per_file: 0,
            }),
            page: Page::default(),
        }))
        .await
        .expect_err("the patch input is refused");
    assert_eq!(common::api(diff), ApiError::CorruptObject { id });
    assert!(server.stop().success());
}
