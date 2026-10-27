//! Accept 3 against the real engine, not the in-process fake.
//!
//! A human reviewer reads `graft_review_queue`, posts a `review` pass with
//! `graft_attest`, and the landed changeset is gone from the queue. The
//! server is the MCP server over `Engine<MemoryStore, MemoryStore>`.

use std::sync::Arc;

use graft_engine::{Engine, EngineConfig};
use graft_mcp::McpServer;
use graft_model::{
    Author, AuthorKind, Change, EntryKind, Metadata, ObjectKind, RepoPath, Timestamp,
};
use graft_proto::types::{CreateChangeset, Land, MergeMode, UpdateRef};
use graft_proto::{AdminApi, CallContext, Frame, Principal, PrincipalKind, RepoApi};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

const REPO: &str = "r";
const NOW_MS: i64 = 1_800_000_000_000;

fn human(scopes: &[&str]) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "reviewer".into(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        },
        request_id: "real-review".into(),
        deadline_ms: None,
    }
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> Value {
    let arguments = args.as_object().expect("object").clone();
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await
        .expect("call");
    assert!(result.is_error != Some(true), "{tool} failed: {result:?}");
    result.structured_content.expect("structured")
}

#[tokio::test]
async fn review_pass_removes_the_land_from_the_real_queue() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(Engine::new(
        Arc::clone(&store),
        store,
        Arc::new(FakeClock::at(NOW_MS)),
        EngineConfig::default(),
    ));
    let cx = human(&[
        "repo:r:admin",
        "repo:r:write",
        "repo:r:land:main",
        "repo:r:attest",
        "repo:r:read",
    ]);
    engine.create_repo(&cx, REPO.into()).await.expect("repo");

    let root_bytes = b"root\n";
    let root = engine
        .create_changeset(
            &cx,
            CreateChangeset {
                repo: REPO.into(),
                parents: Vec::new(),
                changes: vec![Change::Put {
                    path: RepoPath::new("README.md").unwrap(),
                    kind: EntryKind::File,
                    id: graft_model::BlobId::hash(root_bytes).0,
                }],
                author: Author {
                    name: "Ada".into(),
                    email: "ada@example.com".into(),
                    kind: AuthorKind::Human,
                },
                authored_at: Timestamp {
                    ms: NOW_MS,
                    offset_min: 0,
                },
                message: "root".into(),
                metadata: Metadata::new(),
                inline_blobs: vec![Frame::encode(ObjectKind::Blob, root_bytes)],
                workspace: None,
            },
        )
        .await
        .expect("root changeset");
    engine
        .update_ref(
            &cx,
            UpdateRef {
                repo: REPO.into(),
                name: "main".into(),
                expected: None,
                new: root.id,
                kind: graft_proto::types::RefKind::Line,
            },
        )
        .await
        .expect("main");

    let child_bytes = b"root\nmore\n";
    let child = engine
        .create_changeset(
            &cx,
            CreateChangeset {
                repo: REPO.into(),
                parents: vec![root.id],
                changes: vec![Change::Put {
                    path: RepoPath::new("README.md").unwrap(),
                    kind: EntryKind::File,
                    id: graft_model::BlobId::hash(child_bytes).0,
                }],
                author: Author {
                    name: "Ada".into(),
                    email: "ada@example.com".into(),
                    kind: AuthorKind::Human,
                },
                authored_at: Timestamp {
                    ms: NOW_MS + 1,
                    offset_min: 0,
                },
                message: "more".into(),
                metadata: Metadata::new(),
                inline_blobs: vec![Frame::encode(ObjectKind::Blob, child_bytes)],
                workspace: None,
            },
        )
        .await
        .expect("child");
    let landed = engine
        .land(
            &cx,
            Land {
                repo: REPO.into(),
                head: child.id,
                line: "main".into(),
                squash: false,
                merge: MergeMode::File,
                deadline_ms: None,
                merged_from: None,
            },
        )
        .await
        .expect("land");
    let graft_proto::types::Landed::Landed { landed, .. } = landed else {
        panic!("expected a landed changeset, got {landed:?}");
    };

    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = McpServer::new(Arc::clone(&engine), cx.principal.clone());
    tokio::spawn(async move {
        let _ = server.serve_io(server_read, server_write).await;
    });
    let (client_read, client_write) = tokio::io::split(client_io);
    let client = ().serve((client_read, client_write)).await.expect("handshake");

    let before = call(
        &client,
        "graft_review_queue",
        json!({"repo": REPO, "line": "main"}),
    )
    .await;
    let items = before["items"].as_array().expect("items");
    assert!(
        items
            .iter()
            .any(|item| item["changeset"] == landed.to_string()),
        "the landed changeset is queued: {before}"
    );
    assert!(before.get("next_lsn").is_some(), "the page names next_lsn");

    let posted = call(
        &client,
        "graft_attest",
        json!({
            "repo": REPO,
            "changeset": landed.to_string(),
            "kind": "review",
            "check": "model-review",
            "status": "pass",
            "by": "model reviewer"
        }),
    )
    .await;
    assert_eq!(posted["changeset"], landed.to_string());
    assert_eq!(posted["recorded"], true);
    assert!(
        posted.get("status").is_none(),
        "attest does not return the attestation"
    );

    let after = call(
        &client,
        "graft_review_queue",
        json!({"repo": REPO, "line": "main"}),
    )
    .await;
    assert!(
        after["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["changeset"] != landed.to_string()),
        "a review pass removes the land: {after}"
    );
}
