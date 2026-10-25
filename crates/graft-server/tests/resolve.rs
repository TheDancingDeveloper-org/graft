//! `GetConflicts` and `Resolve` over gRPC through the real binary
//! (WI-2312): a same-line conflict is shown with markers, resolved without
//! a working copy, and lands; a stranger can't read another's conflicts.

mod common;

use std::collections::BTreeMap;

use common::{Site, api, author, changes};
use graft_model::{ChangesetId, EntryKind, RepoPath, Timestamp};
use graft_proto::ApiError;
use graft_proto::pb;
use graft_proto::types::{
    Conflicts, CreateChangeset, Created, GetConflicts, Landed, Page, PathResolution, Resolution,
    Resolve, Resolved, ResolvedContent, WorkspaceRef,
};

const BASE: &[u8] = b"one\ntwo\nthree\n";
const OURS: &[u8] = b"one\nTWO-ours\nthree\n";
const THEIRS: &[u8] = b"one\nTWO-theirs\nthree\n";
const FIXED: &[u8] = b"one\nTWO-both\nthree\n";

fn snap(content: &[u8]) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([("f.txt".to_string(), content.to_vec())])
}

fn wref(name: &str) -> WorkspaceRef {
    WorkspaceRef {
        repo: "r".into(),
        name: name.into(),
    }
}

async fn commit_in(
    c: &mut common::Clients,
    ws: Option<&str>,
    parent: ChangesetId,
    content: &[u8],
) -> ChangesetId {
    c.put_blobs("r", &[content.to_vec()]).await.expect("blobs");
    let req = CreateChangeset {
        repo: "r".into(),
        parents: vec![parent],
        changes: changes(&snap(BASE), &snap(content)),
        author: author(),
        authored_at: Timestamp {
            ms: 1_700_000_000_000,
            offset_min: 0,
        },
        message: "edit".into(),
        metadata: BTreeMap::new(),
        inline_blobs: Vec::new(),
        workspace: ws.map(str::to_owned),
    };
    let created = c
        .repo
        .create_changeset(pb::CreateChangesetRequest::from(req))
        .await
        .expect("commit")
        .into_inner();
    Created::try_from(created).expect("created").id
}

async fn land(c: &mut common::Clients, head: ChangesetId) -> Result<ChangesetId, ApiError> {
    let r = c
        .repo
        .land(pb::LandRequest::from(graft_proto::types::Land {
            repo: "r".into(),
            head,
            line: "main".into(),
            squash: false,
            merge: graft_proto::types::MergeMode::Line,
            deadline_ms: None,
            merged_from: None,
        }))
        .await
        .map_err(api)?
        .into_inner();
    match Landed::try_from(r).expect("landed") {
        Landed::Landed { landed, .. } => Ok(landed),
        other => panic!("expected Landed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflict_resolves_over_grpc_and_lands() {
    let site = Site::new("server-resolve", "");
    let admin = site.mint(&["repo:r:admin"]);
    let alice = site.mint_as("alice", &["repo:r:write", "repo:r:land:main"], 3600);
    let bob = site.mint_as("bob", &["repo:r:write"], 3600);
    let server = site.start();
    let mut a = server.clients(Some(&admin)).await;
    a.create_repo("r").await.expect("create");
    let root = a
        .commit("r", None, &BTreeMap::new(), &snap(BASE), 0)
        .await
        .expect("root")
        .id;
    a.set_line("r", "main", root).await.expect("line");

    let mut al = server.clients(Some(&alice)).await;
    al.workspaces
        .open_workspace(pb::OpenWorkspaceRequest {
            repo: "r".into(),
            name: Some("w".into()),
            line: "main".into(),
            at: None,
            lease_ms: 60_000,
        })
        .await
        .expect("open");
    let draft = commit_in(&mut al, Some("w"), root, OURS).await;
    let other = commit_in(&mut a, None, root, THEIRS).await;
    let onto = land(&mut a, other).await.expect("land theirs");
    // An unchecked land of a draft behind the tip is a stale base: the
    // current tip and the path, never the side object ids.
    match land(&mut al, draft).await {
        Err(ApiError::StaleBase(detail)) => {
            assert_eq!(detail.current_tip, onto);
            assert_eq!(detail.paths, vec!["f.txt".to_owned()]);
            assert!(!detail.truncated);
        }
        other => panic!("expected StaleBase, got {other:?}"),
    }

    let req = GetConflicts {
        workspace: wref("w"),
        onto: None,
        page: Page::default(),
    };
    // Someone else's workspace is refused before anything is read.
    let mut bo = server.clients(Some(&bob)).await;
    let refused = bo
        .workspaces
        .get_conflicts(pb::GetConflictsRequest::from(req.clone()))
        .await
        .expect_err("not bob's");
    assert!(matches!(api(refused), ApiError::PermissionDenied { .. }));

    let got = al
        .workspaces
        .get_conflicts(pb::GetConflictsRequest::from(req))
        .await
        .expect("conflicts")
        .into_inner();
    let got = Conflicts::try_from(got).expect("valid");
    assert_eq!((got.onto, got.draft), (onto, Some(draft)));
    assert_eq!(got.paths.len(), 1);
    let markers = got.paths[0].markers.clone().expect("markers");
    assert_eq!(
        markers,
        graft_model::merge::render_conflict_markers(BASE, OURS, THEIRS)
    );

    let resolved = al
        .workspaces
        .resolve(pb::ResolveRequest::from(Resolve {
            workspace: wref("w"),
            onto,
            expected_head: Some(draft),
            resolutions: vec![PathResolution {
                path: RepoPath::new("f.txt").expect("path"),
                resolution: Resolution::Put {
                    kind: EntryKind::File,
                    content: ResolvedContent::Inline(FIXED.to_vec()),
                },
            }],
        }))
        .await
        .expect("resolve")
        .into_inner();
    let resolved = Resolved::try_from(resolved).expect("valid");
    assert_eq!(resolved.workspace.base, onto);
    assert_eq!(
        resolved
            .resolved_paths
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        ["f.txt"]
    );

    // The resolved head lands as a fast-forward and holds the resolution.
    assert_eq!(
        land(&mut al, resolved.head).await.expect("land"),
        resolved.head
    );
    let file = a
        .repo
        .read_file(pb::PathAt {
            repo: "r".into(),
            at: resolved.head.0.0.to_vec(),
            path: "f.txt".into(),
        })
        .await
        .expect("read")
        .into_inner();
    assert_eq!(file.data, FIXED);
}
