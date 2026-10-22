//! The workspace RPCs through the real binary (`docs/design.md` §7.2, §12.2):
//! a workspace belongs to its owner, leases race to one outcome, and an
//! oversized request is refused before it is buffered.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{Clients, Site, api};
use graft_proto::pb;
use graft_proto::{ApiError, VERSION_HEADER};

fn open(name: &str, lease_ms: u64) -> pb::OpenWorkspaceRequest {
    pb::OpenWorkspaceRequest {
        repo: "r".into(),
        name: Some(name.into()),
        line: "main".into(),
        at: None,
        lease_ms,
    }
}

fn ws(name: &str) -> pb::WorkspaceRef {
    pb::WorkspaceRef {
        repo: "r".into(),
        name: name.into(),
    }
}

fn renew(name: &str, lease_ms: u64) -> pb::RenewLeaseRequest {
    pb::RenewLeaseRequest {
        workspace: Some(ws(name)),
        lease_ms,
    }
}

/// A repository with one changeset on its `main` line. Returns the site
/// (which owns the directory), the running server, and the tokens for an
/// owner, another writer and a reader.
async fn setup(tag: &str) -> (Site, common::Server, [String; 3]) {
    let site = Site::new(tag, "");
    let admin = site.mint(&["repo:r:admin"]);
    let alice = site.mint_as("alice", &["repo:r:write"], 3600);
    let bob = site.mint_as("bob", &["repo:r:write"], 3600);
    let reader = site.mint_as("reader", &["repo:r:read"], 3600);
    let server = site.start();
    let mut a = server.clients(Some(&admin)).await;
    a.create_repo("r").await.expect("create");
    let snap = BTreeMap::from([("a".to_string(), b"1".to_vec())]);
    let tip = a
        .commit("r", None, &BTreeMap::new(), &snap, 0)
        .await
        .expect("commit")
        .id;
    a.set_line("r", "main", tip).await.expect("line");
    (site, server, [alice, bob, reader])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workspace_belongs_to_its_owner() {
    let (_site, server, [alice, bob, reader]) = setup("server-ws-owner").await;
    let mut al = server.clients(Some(&alice)).await;
    let view = al
        .workspaces
        .open_workspace(open("ws-a", 60_000))
        .await
        .expect("alice opens")
        .into_inner();
    assert_eq!(view.owner, "alice");

    let mut bo = server.clients(Some(&bob)).await;
    let refused = |e: tonic::Status| {
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "{e:?}");
        assert!(
            e.metadata().get(VERSION_HEADER).is_some(),
            "errors carry the protocol version too"
        );
        assert!(matches!(api(e), ApiError::PermissionDenied { .. }));
    };
    refused(
        bo.workspaces
            .get_workspace(ws("ws-a"))
            .await
            .expect_err("not bob's"),
    );
    refused(
        bo.workspaces
            .renew_lease(renew("ws-a", 60_000))
            .await
            .expect_err("not bob's"),
    );
    refused(
        bo.workspaces
            .set_head(pb::SetHeadRequest {
                workspace: Some(ws("ws-a")),
                expected: None,
                new: None,
            })
            .await
            .expect_err("not bob's"),
    );
    refused(
        bo.workspaces
            .sync(pb::SyncRequest {
                workspace: Some(ws("ws-a")),
                onto: None,
            })
            .await
            .expect_err("not bob's"),
    );
    refused(
        bo.workspaces
            .stack(pb::StackRequest {
                workspace: Some(ws("ws-a")),
                op: Some(pb::StackOp {
                    op: Some(pb::stack_op::Op::Drop(view.base.clone())),
                }),
            })
            .await
            .expect_err("not bob's"),
    );
    refused(
        bo.workspaces
            .close_workspace(pb::CloseWorkspaceRequest {
                workspace: Some(ws("ws-a")),
                keep_drafts: false,
            })
            .await
            .expect_err("not bob's"),
    );

    // Read access to the repository does not reveal another's workspace.
    let mut rd = server.clients(Some(&reader)).await;
    refused(
        rd.workspaces
            .get_workspace(ws("ws-a"))
            .await
            .expect_err("a reader is not the owner"),
    );

    // Bob's listing holds only his own; the owner reads its workspace.
    let list = bo
        .workspaces
        .list_workspaces(pb::ListWorkspacesRequest {
            repo: "r".into(),
            page: None,
        })
        .await
        .expect("list")
        .into_inner();
    assert!(list.workspaces.iter().all(|w| w.name != "ws-a"));
    let got = al
        .workspaces
        .get_workspace(ws("ws-a"))
        .await
        .expect("owner reads")
        .into_inner();
    assert_eq!(got, view);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_opens_and_renewals_have_one_outcome() {
    let (_site, server, [alice, bob, _reader]) = setup("server-ws-race").await;

    // Eight opens of one name by its owner: each answers the same workspace.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let mut c: Clients = server.clients(Some(&alice)).await;
        tasks.push(tokio::spawn(async move {
            c.workspaces
                .open_workspace(open("ws-o", 60_000))
                .await
                .map(|r| r.into_inner())
        }));
    }
    let mut views = Vec::new();
    for t in tasks {
        views.push(t.await.expect("join").expect("a retried open succeeds"));
    }
    assert!(views.windows(2).all(|p| p[0] == p[1]), "one workspace");

    // Another writer cannot take the name.
    let e = server
        .clients(Some(&bob))
        .await
        .workspaces
        .open_workspace(open("ws-o", 60_000))
        .await
        .expect_err("taken");
    assert!(matches!(api(e), ApiError::RefConflict { .. }));

    // Renewals on a live lease all succeed.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let mut c = server.clients(Some(&alice)).await;
        tasks.push(tokio::spawn(async move {
            c.workspaces.renew_lease(renew("ws-o", 60_000)).await
        }));
    }
    for t in tasks {
        t.await
            .expect("join")
            .expect("a live lease renews under contention");
    }

    // A lapsed lease has no winner: every renewal is refused, and the
    // workspace ends rather than coming back.
    let mut owner = server.clients(Some(&alice)).await;
    owner
        .workspaces
        .open_workspace(open("ws-l", 200))
        .await
        .expect("open a short lease");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let mut c = server.clients(Some(&alice)).await;
        tasks.push(tokio::spawn(async move {
            c.workspaces.renew_lease(renew("ws-l", 60_000)).await
        }));
    }
    for t in tasks {
        let e = t
            .await
            .expect("join")
            .expect_err("a lapsed lease does not renew");
        assert!(
            matches!(
                api(e),
                ApiError::LeaseExpired { .. } | ApiError::NotFound { .. }
            ),
            "a lapsed lease is refused, not contended"
        );
    }
    let e = owner
        .workspaces
        .get_workspace(ws("ws-l"))
        .await
        .expect_err("ended");
    assert!(matches!(api(e), ApiError::NotFound { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_workspace_requests_are_refused_and_the_server_carries_on() {
    let (_site, server, [alice, _bob, _reader]) = setup("server-ws-huge").await;
    let mut c = server.clients(Some(&alice)).await;

    // A name past the workspace limit: refused with a typed error.
    let e = c
        .workspaces
        .open_workspace(open(&"a".repeat(300), 60_000))
        .await
        .expect_err("too long");
    assert!(matches!(api(e), ApiError::InvalidPath { .. }));

    // A request far past the control-message bound: refused before it is
    // buffered, and the connection still answers.
    let huge = open(&"b".repeat(5 << 20), 60_000);
    let e = c
        .workspaces
        .open_workspace(huge)
        .await
        .expect_err("oversized");
    assert_ne!(e.code(), tonic::Code::Ok);

    let ok = c
        .workspaces
        .open_workspace(open("ws-after", 60_000))
        .await
        .expect("still serving")
        .into_inner();
    assert_eq!(ok.name, "ws-after");
}

fn mint_req(subject: &str, session: Option<&str>, work_item: Option<&str>) -> pb::MintTokenRequest {
    pb::MintTokenRequest {
        subject: subject.into(),
        scopes: vec!["repo:r:write".into()],
        ttl_s: 600,
        session: session.map(Into::into),
        work_item: work_item.map(Into::into),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_writer_cannot_mint_its_way_into_another_workspace() {
    let (_site, server, [alice, bob, _reader]) = setup("server-ws-mint-takeover").await;
    let mut al = server.clients(Some(&alice)).await;
    al.workspaces
        .open_workspace(open("ws-a", 60_000))
        .await
        .expect("alice opens");

    // Bob holds the same scopes as alice, but cannot become her, nor stamp
    // his tokens with a session or work item that is not his.
    let mut bo = server.clients(Some(&bob)).await;
    for req in [
        mint_req("alice", None, None),
        mint_req("", Some("ses_alice"), None),
        mint_req("bob", None, Some("WI-1")),
    ] {
        let e = bo
            .admin
            .mint_token(req.clone())
            .await
            .expect_err("another identity");
        assert!(
            matches!(api(e), ApiError::PermissionDenied { .. }),
            "{req:?}"
        );
    }

    // What he can mint is his own: it reaches only his workspaces.
    let own = bo
        .admin
        .mint_token(mint_req("", None, None))
        .await
        .expect("for himself")
        .into_inner()
        .token;
    let mut b2 = server.clients(Some(&own)).await;
    for e in [
        b2.workspaces
            .get_workspace(ws("ws-a"))
            .await
            .expect_err("not bob's"),
        b2.workspaces
            .renew_lease(renew("ws-a", 60_000))
            .await
            .expect_err("not bob's"),
    ] {
        assert!(matches!(api(e), ApiError::PermissionDenied { .. }));
    }
    let view = b2
        .workspaces
        .open_workspace(open("ws-b", 60_000))
        .await
        .expect("bob opens his own")
        .into_inner();
    assert_eq!(view.owner, "bob");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vogt_delegates_a_session_that_owns_its_workspaces() {
    let (site, server, [alice, _bob, _reader]) = setup("server-ws-vogt").await;
    let mut al = server.clients(Some(&alice)).await;
    al.workspaces
        .open_workspace(open("ws-a", 60_000))
        .await
        .expect("alice opens");

    let vogt = site.mint_kind("vogt", "service", &["repo:r:write"], 3600);
    let session = server
        .clients(Some(&vogt))
        .await
        .admin
        .mint_token(mint_req("ses_1", Some("ses_1"), Some("WI-1")))
        .await
        .expect("vogt delegates")
        .into_inner()
        .token;
    let mut s = server.clients(Some(&session)).await;
    let view = s
        .workspaces
        .open_workspace(open("ws-s", 60_000))
        .await
        .expect("the session opens")
        .into_inner();
    assert_eq!(
        (view.owner.as_str(), view.session.as_deref()),
        ("ses_1", Some("ses_1"))
    );
    let e = s
        .workspaces
        .get_workspace(ws("ws-a"))
        .await
        .expect_err("not the session's");
    assert!(matches!(api(e), ApiError::PermissionDenied { .. }));

    // The session token is an agent's: it delegates no further.
    let e = s
        .admin
        .mint_token(mint_req("alice", None, None))
        .await
        .expect_err("agents do not delegate");
    assert!(matches!(api(e), ApiError::PermissionDenied { .. }));
    // A token it mints for itself keeps its identity and its workspaces.
    // Ask for less than the session token's remaining life (it was minted
    // with 600 s a moment ago), so a clock tick between the two mints can't
    // push the request over the caller's expiry.
    let own = s
        .admin
        .mint_token(pb::MintTokenRequest {
            ttl_s: 300,
            ..mint_req("", None, None)
        })
        .await
        .expect("for itself")
        .into_inner()
        .token;
    let got = server
        .clients(Some(&own))
        .await
        .workspaces
        .get_workspace(ws("ws-s"))
        .await
        .expect("same owner")
        .into_inner();
    assert_eq!(got.session.as_deref(), Some("ses_1"));
}
