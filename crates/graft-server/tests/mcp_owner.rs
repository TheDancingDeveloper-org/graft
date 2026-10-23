//! WI-2315: a workspace is its owner's on `/mcp` as on gRPC
//! (`docs/design.md` §7.2).
//!
//! Each test has agent A open two workspaces with one draft each. Agent B,
//! with the same repository scopes, is refused every operation on A's
//! workspace with `permission_denied`, learns nothing of it, and leaves it
//! as it was. A may act on its own workspace, and an admin on A's other.

mod common;
#[path = "common/mcp_agent.rs"]
mod mcp_agent;

use std::collections::BTreeMap;

use common::{Server, Site};
use graft_model::{ChangesetId, Timestamp};
use graft_proto::types::{CreateChangeset, RefKind, UpdateRef};
use graft_proto::{ApiError, pb};
use mcp_agent::Agent;
use serde_json::{Value, json};

/// The repository, A's workspaces and the three agents.
struct Fixture {
    /// `main`'s tip, one changeset past the workspaces' base.
    tip: String,
    /// A's draft in `wa` (A acts on it) and in `wb` (the admin acts on it).
    drafts: [String; 2],
    a: Agent,
    b: Agent,
    admin: Agent,
    token_b: String,
    server: Server,
    _site: Site,
}

impl Fixture {
    async fn new(name: &str) -> Self {
        let site = Site::new(name, "[[lines]]\nname = \"main\"\npolicy = \"direct\"\n");
        let admin = site.mint(&["repo:r:admin"]);
        let scopes = ["repo:r:write", "repo:r:land:main"];
        let token_a = site.mint_as("agent-a", &scopes, 3600);
        let token_b = site.mint_as("agent-b", &scopes, 3600);
        let server = site.start();

        let mut c = server.clients(Some(&admin)).await;
        c.create_repo("r").await.expect("create");
        let files = BTreeMap::from([("f.txt".to_owned(), b"one\n".to_vec())]);
        let root = c
            .commit("r", None, &BTreeMap::new(), &files, 0)
            .await
            .expect("root");
        c.set_line("r", "main", root.id).await.expect("main");

        let a = Agent::open(server.addr, &token_a);
        let mut drafts = Vec::new();
        for ws in ["wa", "wb"] {
            a.ok(
                "graft_workspace",
                json!({"repo": "r", "action": "open", "name": ws, "line": "main"}),
            );
            drafts.push(a.commit(ws, json!({"path": format!("{ws}.txt"), "content": "x\n"})));
        }
        // `main` moves on after the workspaces open, so a resolve or a
        // revert has a non-root target that is not their base.
        let next = BTreeMap::from([("f.txt".to_owned(), b"two\n".to_vec())]);
        let tip = c
            .commit("r", Some(root.id), &files, &next, 1)
            .await
            .expect("tip");
        c.repo
            .update_ref(pb::UpdateRefRequest::from(UpdateRef {
                repo: "r".into(),
                name: "main".into(),
                expected: Some(root.id),
                new: tip.id,
                kind: RefKind::Line,
            }))
            .await
            .expect("main moves");
        Self {
            tip: tip.id.to_string(),
            drafts: drafts.try_into().unwrap(),
            b: Agent::open(server.addr, &token_b),
            admin: Agent::open(server.addr, &admin),
            a,
            token_b,
            server,
            _site: site,
        }
    }

    /// A's draft in `ws`.
    fn draft(&self, ws: &str) -> &str {
        &self.drafts[usize::from(ws == "wb")]
    }

    /// A's view of its workspace `ws`.
    fn view(&self, ws: &str) -> Value {
        self.a.ok(
            "graft_workspace",
            json!({"repo": "r", "action": "get", "name": ws}),
        )["workspace"]
            .clone()
    }

    /// B calls `tool` with `args(ws)` on each of A's workspaces and is
    /// refused, without a word of the workspace, which stays as it was.
    fn refused(&self, tool: &str, args: impl Fn(&str) -> Value) {
        for (ws, draft) in ["wa", "wb"].into_iter().zip(&self.drafts) {
            let before = self.view(ws);
            let e = self.b.err(tool, args(ws));
            assert_eq!(e["kind"], "permission_denied", "{tool}: {e}");
            let text = e.to_string();
            assert!(!text.contains(draft.as_str()), "{tool} leaks: {text}");
            assert!(!text.contains("agent-a"), "{tool} leaks: {text}");
            assert_eq!(self.view(ws), before, "{tool}: A's workspace moved");
        }
    }

    /// A calls `tool` on `wa` and the admin on `wb`; both succeed.
    fn allowed(&self, tool: &str, args: impl Fn(&str) -> Value) -> [Value; 2] {
        [self.a.ok(tool, args("wa")), self.admin.ok(tool, args("wb"))]
    }

    /// Both cases, refused first.
    fn owner_only(&self, tool: &str, args: impl Fn(&str) -> Value) -> [Value; 2] {
        self.refused(tool, &args);
        self.allowed(tool, args)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_workspace_is_for_its_owner_only() {
    let f = Fixture::new("server-mcp-owner-get").await;
    let [a, admin] = f.owner_only(
        "graft_workspace",
        |ws| json!({"repo": "r", "action": "get", "name": ws}),
    );
    assert_eq!(a["workspace"]["head"], f.drafts[0].as_str());
    assert_eq!(admin["workspace"]["head"], f.drafts[1].as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renew_is_for_its_owner_only() {
    let f = Fixture::new("server-mcp-owner-renew").await;
    f.owner_only(
        "graft_workspace",
        |ws| json!({"repo": "r", "action": "renew", "name": ws, "lease_ms": 120_000}),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_is_for_its_owner_only() {
    let f = Fixture::new("server-mcp-owner-close").await;
    let [a, admin] = f.owner_only(
        "graft_workspace",
        |ws| json!({"repo": "r", "action": "close", "name": ws, "keep_drafts": false}),
    );
    assert_eq!(a["closed"], "wa");
    assert_eq!(admin["closed"], "wb");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_is_for_its_owner_only() {
    let f = Fixture::new("server-mcp-owner-commit").await;
    let args = |ws: &str| {
        json!({"repo": "r", "workspace": ws, "message": "m",
               "files": [{"path": "c.txt", "content": "c\n"}]})
    };
    let [a, admin] = f.owner_only("graft_commit", args);
    assert_eq!(f.view("wa")["head"], a["id"]);
    assert_eq!(f.view("wb")["head"], admin["id"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_is_for_its_owner_only() {
    let f = Fixture::new("server-mcp-owner-sync").await;
    f.owner_only("graft_sync", |ws| json!({"repo": "r", "workspace": ws}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn land_is_for_its_owner_only() {
    let f = Fixture::new("server-mcp-owner-land").await;
    f.owner_only("graft_land", |ws| json!({"repo": "r", "workspace": ws}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflicts_are_for_its_owner_only() {
    let f = Fixture::new("server-mcp-owner-conflicts").await;
    f.owner_only(
        "graft_conflicts",
        |ws| json!({"repo": "r", "workspace": ws}),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_is_for_its_owner_only() {
    let f = Fixture::new("server-mcp-owner-resolve").await;
    // Each stack edits the line `main` moved on, so it conflicts there.
    let heads: Vec<String> = ["wa", "wb"]
        .into_iter()
        .map(|ws| {
            f.a.commit(ws, json!({"path": "f.txt", "content": format!("{ws}\n")}))
        })
        .collect();
    f.owner_only("graft_resolve", |ws| {
        json!({"repo": "r", "workspace": ws, "onto": f.tip,
               "expected_head": heads[usize::from(ws == "wb")],
               "resolutions": [{"path": "f.txt", "content": "y\n"}]})
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_into_a_workspace_is_for_its_owner_only() {
    let f = Fixture::new("server-mcp-owner-revert").await;
    f.owner_only(
        "graft_revert",
        |ws| json!({"repo": "r", "changeset": f.draft(ws), "workspace": ws}),
    );
}

/// gRPC regression: a commit naming another principal's workspace is
/// refused before it moves that workspace's head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_commit_into_a_foreign_workspace_is_refused() {
    let f = Fixture::new("server-mcp-owner-grpc-commit").await;
    let before = f.view("wa");
    let parent = ChangesetId::parse(&f.drafts[0]).unwrap();
    let req = CreateChangeset {
        repo: "r".into(),
        parents: vec![parent],
        changes: Vec::new(),
        author: common::author(),
        authored_at: Timestamp {
            ms: 1_700_000_000_000,
            offset_min: 0,
        },
        message: "foreign".into(),
        metadata: BTreeMap::new(),
        inline_blobs: Vec::new(),
        workspace: Some("wa".into()),
    };
    let mut cb = f.server.clients(Some(&f.token_b)).await;
    let e = cb
        .repo
        .create_changeset(pb::CreateChangesetRequest::from(req))
        .await
        .expect_err("foreign commit");
    assert!(
        matches!(common::api(e), ApiError::PermissionDenied { .. }),
        "gRPC refuses"
    );
    assert_eq!(f.view("wa"), before, "A's workspace is untouched");
}
