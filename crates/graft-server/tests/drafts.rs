//! Draft overlap through the real binary (WI-2310, decision D24): two
//! agents' drafts on one file see each other, disjoint drafts don't, a
//! closed workspace drops out, a caller without `repo:<r>:drafts` is
//! refused, and no contents, head ids or tokens appear in an answer.

mod common;

use std::collections::BTreeMap;

use common::{Clients, Site, api, author, changes};
use graft_model::{ChangesetId, RepoPath, Timestamp};
use graft_proto::ApiError;
use graft_proto::pb;
use graft_proto::types::{CreateChangeset, Created, DraftOverlap, OverlappingDrafts, Page};

fn ws(name: &str) -> pb::WorkspaceRef {
    pb::WorkspaceRef {
        repo: "r".into(),
        name: name.into(),
    }
}

/// Opens `name` on `main` and commits `files` into it as one draft.
async fn draft(
    c: &mut Clients,
    name: &str,
    base: ChangesetId,
    before: &BTreeMap<String, Vec<u8>>,
    files: &[(&str, &str)],
) -> ChangesetId {
    draft_leased(c, name, base, before, files, 600_000).await
}

/// [`draft`] with a lease of `lease_ms`.
async fn draft_leased(
    c: &mut Clients,
    name: &str,
    base: ChangesetId,
    before: &BTreeMap<String, Vec<u8>>,
    files: &[(&str, &str)],
    lease_ms: u64,
) -> ChangesetId {
    c.workspaces
        .open_workspace(pb::OpenWorkspaceRequest {
            repo: "r".into(),
            name: Some(name.into()),
            line: "main".into(),
            at: None,
            lease_ms,
        })
        .await
        .expect("open");
    let mut after = before.clone();
    for (p, body) in files {
        after.insert((*p).to_owned(), body.as_bytes().to_vec());
    }
    let blobs: Vec<Vec<u8>> = files.iter().map(|(_, b)| b.as_bytes().to_vec()).collect();
    c.put_blobs("r", &blobs).await.expect("blobs");
    let req = CreateChangeset {
        repo: "r".into(),
        parents: vec![base],
        changes: changes(before, &after),
        author: author(),
        authored_at: Timestamp {
            ms: 1_700_000_000_000,
            offset_min: 0,
        },
        message: format!("draft in {name}"),
        metadata: BTreeMap::new(),
        inline_blobs: Vec::new(),
        workspace: Some(name.into()),
    };
    let created = c
        .repo
        .create_changeset(pb::CreateChangesetRequest::from(req))
        .await
        .expect("commit")
        .into_inner();
    Created::try_from(created).expect("created").id
}

async fn overlap(c: &mut Clients, paths: &[&str]) -> Result<DraftOverlap, ApiError> {
    let req = OverlappingDrafts {
        repo: "r".into(),
        line: "main".into(),
        paths: paths
            .iter()
            .map(|p| RepoPath::new(p).expect("path"))
            .collect(),
        include_own: false,
        page: Page {
            limit: 50,
            token: None,
        },
    };
    let mut out = DraftOverlap::default();
    let mut req = req;
    loop {
        let page = c
            .workspaces
            .overlapping_drafts(pb::OverlappingDraftsRequest::from(req.clone()))
            .await
            .map_err(api)?
            .into_inner();
        let page = DraftOverlap::try_from(page).expect("overlap");
        out.hits.extend(page.hits);
        out.workspaces.extend(page.workspaces);
        match page.next {
            Some(t) => req.page.token = Some(t),
            None => return Ok(out),
        }
    }
}

fn seen(o: &DraftOverlap) -> Vec<(String, String)> {
    let mut v: Vec<_> = o
        .hits
        .iter()
        .map(|h| (h.workspace.clone(), h.path.to_string()))
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drafts_on_one_file_see_each_other() {
    let site = Site::new("server-drafts", "");
    let admin = site.mint(&["repo:r:admin"]);
    let scopes = ["repo:r:write", "repo:r:drafts"];
    let alice = site.mint_as("alice", &scopes, 3600);
    let bob = site.mint_as("bob", &scopes, 3600);
    let carol = site.mint_as("carol", &scopes, 3600);
    let reader = site.mint_as("reader", &["repo:r:read", "repo:r:write"], 3600);
    let server = site.start();

    let mut a = server.clients(Some(&admin)).await;
    a.create_repo("r").await.expect("create");
    let root = BTreeMap::from([
        ("src/a.rs".to_string(), b"a0".to_vec()),
        ("src/b.rs".to_string(), b"b0".to_vec()),
        ("docs/c.md".to_string(), b"c0".to_vec()),
    ]);
    let tip = a
        .commit("r", None, &BTreeMap::new(), &root, 0)
        .await
        .expect("commit")
        .id;
    a.set_line("r", "main", tip).await.expect("line");

    let mut al = server.clients(Some(&alice)).await;
    let mut bo = server.clients(Some(&bob)).await;
    let mut ca = server.clients(Some(&carol)).await;
    let head_a = draft(&mut al, "wa", tip, &root, &[("src/a.rs", "ALICE-SECRET")]).await;
    let head_b = draft(&mut bo, "wb", tip, &root, &[("src/a.rs", "BOB-SECRET")]).await;
    let head_c = draft(&mut ca, "wc", tip, &root, &[("docs/c.md", "CAROL-SECRET")]).await;

    // Same file: each sees the other, never itself.
    let o = overlap(&mut al, &["src/a.rs"]).await.expect("alice asks");
    assert_eq!(seen(&o), [("wb".into(), "src/a.rs".into())]);
    assert_eq!(o.workspaces.len(), 1);
    assert_eq!(o.workspaces[0].owner, "bob");
    let o = overlap(&mut bo, &["src"]).await.expect("bob asks a prefix");
    assert_eq!(seen(&o), [("wa".into(), "src/a.rs".into())]);

    // Disjoint drafts don't see each other: the drafts on src don't
    // answer for docs, and carol's on docs don't answer for src.
    let o = overlap(&mut al, &["docs"]).await.expect("alice asks docs");
    assert_eq!(seen(&o), [("wc".into(), "docs/c.md".into())]);
    let o = overlap(&mut bo, &["src"]).await.expect("bob asks src");
    assert!(seen(&o).iter().all(|(w, _)| w != "wc"), "{o:?}");
    let o = overlap(&mut al, &["src/b.rs"])
        .await
        .expect("untouched file");
    assert!(o.hits.is_empty(), "{o:?}");

    // Without the drafts scope: refused, even with read and write.
    let mut re = server.clients(Some(&reader)).await;
    let e = overlap(&mut re, &["src/a.rs"]).await.expect_err("no scope");
    assert!(matches!(e, ApiError::PermissionDenied { .. }), "{e:?}");

    // The JSON facade answers the same, with no contents, ids or tokens.
    let r = server.get(
        "/v1/repos/r/drafts/overlap?line=main&path=src/a.rs&path=docs",
        Some(&alice),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let body = r.text();
    let v = r.json();
    let names: Vec<&str> = v["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| h["workspace"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["wc", "wb"], "{body}");
    for secret in [
        "ALICE-SECRET",
        "BOB-SECRET",
        "CAROL-SECRET",
        alice.as_str(),
        bob.as_str(),
        carol.as_str(),
    ] {
        assert!(!body.contains(secret), "leaked {secret}: {body}");
    }
    for head in [head_a, head_b, head_c, tip] {
        assert!(!body.contains(&head.to_string()), "head id in {body}");
    }
    let r = server.get(
        "/v1/repos/r/drafts/overlap?line=main&path=src/a.rs",
        Some(&reader),
    );
    assert_eq!(r.status, 403, "{}", r.text());

    // A closed workspace drops out.
    bo.workspaces
        .close_workspace(pb::CloseWorkspaceRequest {
            workspace: Some(ws("wb")),
            keep_drafts: false,
        })
        .await
        .expect("close");
    let o = overlap(&mut al, &["src/a.rs"]).await.expect("after close");
    assert!(o.hits.is_empty(), "{o:?}");

    // The index matches the stacks: fsck is clean.
    drop(server);
    let out = site.admin(&["fsck", "r"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "fsck: {stdout}");
    assert!(stdout.contains("2 workspaces, 0 problems"), "{stdout}");
}

/// A workspace whose lease has lapsed drops out of the answer, though GC
/// has not cleared its rows yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_workspace_drops_out() {
    let site = Site::new("server-drafts-expired", "");
    let admin = site.mint(&["repo:r:admin"]);
    let scopes = ["repo:r:write", "repo:r:drafts"];
    let alice = site.mint_as("alice", &scopes, 3600);
    let bob = site.mint_as("bob", &scopes, 3600);
    let server = site.start();

    let mut a = server.clients(Some(&admin)).await;
    a.create_repo("r").await.expect("create");
    let root = BTreeMap::from([("src/a.rs".to_string(), b"a0".to_vec())]);
    let tip = a
        .commit("r", None, &BTreeMap::new(), &root, 0)
        .await
        .expect("commit")
        .id;
    a.set_line("r", "main", tip).await.expect("line");

    let mut al = server.clients(Some(&alice)).await;
    let mut bo = server.clients(Some(&bob)).await;
    // The server runs in its own process on the real clock, so the lease is
    // long enough to outlast a loaded host before the first query, and expiry
    // is polled for against a bounded deadline rather than slept for.
    let opened = std::time::Instant::now();
    draft_leased(&mut bo, "wb", tip, &root, &[("src/a.rs", "BOB")], 5_000).await;
    let o = overlap(&mut al, &["src/a.rs"]).await.expect("while live");
    assert_eq!(seen(&o), [("wb".into(), "src/a.rs".into())]);

    let deadline = opened + std::time::Duration::from_secs(60);
    loop {
        let o = overlap(&mut al, &["src/a.rs"]).await.expect("after expiry");
        if o.hits.is_empty() && o.workspaces.is_empty() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never expired: {o:?}");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Its rows outlive the lease until GC; fsck takes that as consistent.
    drop(server);
    let out = site.admin(&["fsck", "r"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "fsck: {stdout}");
    assert!(stdout.contains("0 problems"), "{stdout}");
}
