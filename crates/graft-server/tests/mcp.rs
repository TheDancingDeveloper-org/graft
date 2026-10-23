//! `/mcp` on the running server (G2.8): the same bearer token as gRPC, and
//! tool calls that reach the engine through the same operations and scope
//! checks.

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::Site;
use graft_proto::pb;
use graft_proto::types::{Attest, RefKind, UpdateRef};
use serde_json::{Value, json};

/// One POST to `/mcp`. Returns the status, the session id the server names,
/// and the JSON-RPC message (unwrapped from SSE framing when there is one).
fn post(
    addr: SocketAddr,
    session: Option<&str>,
    token: Option<&str>,
    body: &Value,
) -> (u16, Option<String>, String) {
    let payload = body.to_string();
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: graft\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    if let Some(id) = session {
        head.push_str(&format!("Mcp-Session-Id: {id}\r\n"));
    }
    if let Some(t) = token {
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    write!(s, "{head}\r\n{payload}").expect("write");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).expect("read");
    let raw = String::from_utf8(raw).expect("utf8");
    let (head, rest) = raw.split_once("\r\n\r\n").expect("header end");
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status");
    let mut session_id = None;
    let mut chunked = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.eq_ignore_ascii_case("mcp-session-id") {
            session_id = Some(v.trim().to_owned());
        }
        if k.eq_ignore_ascii_case("transfer-encoding") {
            chunked = v.trim().eq_ignore_ascii_case("chunked");
        }
    }
    let mut body = String::new();
    if chunked {
        let mut rest = rest;
        loop {
            let (size, after) = rest.split_once("\r\n").expect("chunk size");
            let n = usize::from_str_radix(size.trim(), 16).expect("hex size");
            if n == 0 {
                break;
            }
            body.push_str(&after[..n]);
            rest = &after[n + 2..];
        }
    } else {
        body = rest.to_owned();
    }
    let message = if body.starts_with("data:") {
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .find(|d| d.starts_with('{'))
            .unwrap_or_default()
            .to_owned()
    } else {
        body
    };
    (status, session_id, message)
}

/// An initialized MCP session.
struct Session {
    addr: SocketAddr,
    token: String,
    id: String,
}

impl Session {
    fn open(addr: SocketAddr, token: &str) -> Self {
        let init = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "server-test", "version": "0"}
            }
        });
        let (status, id, body) = post(addr, None, Some(token), &init);
        assert_eq!(status, 200, "initialize: {body}");
        let id = id.expect("a session id");
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(post(addr, Some(&id), Some(token), &note).0, 202);
        Self {
            addr,
            token: token.to_owned(),
            id,
        }
    }

    /// Calls a tool; returns whether it failed and its structured content.
    fn call(&self, tool: &str, args: Value) -> (bool, Value) {
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        });
        let (status, _, body) = post(self.addr, Some(&self.id), Some(&self.token), &request);
        assert_eq!(status, 200, "{tool}: {body}");
        let reply: Value = serde_json::from_str(&body).expect("JSON");
        let result = reply["result"].clone();
        (
            result["isError"] == true,
            result["structuredContent"].clone(),
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_takes_the_bearer_token_and_the_engine_scopes() {
    let site = Site::new("server-mcp", "");
    let admin = site.mint(&["repo:r:admin"]);
    let writer = site.mint(&["repo:r:write"]);
    let reader = site.mint(&["repo:r:read"]);
    let lander = site.mint(&["repo:r:write", "repo:r:land:main"]);
    let server = site.start();

    let mut c = server.clients(Some(&admin)).await;
    c.create_repo("r").await.expect("create");
    let files = BTreeMap::from([("README.md".to_owned(), b"hello\n".to_vec())]);
    let root = c
        .commit("r", None, &BTreeMap::new(), &files, 0)
        .await
        .expect("root");
    c.repo
        .update_ref(pb::UpdateRefRequest::from(UpdateRef {
            repo: "r".into(),
            name: "main".into(),
            expected: None,
            new: root.id,
            kind: RefKind::Line,
        }))
        .await
        .expect("main");

    // No token, a bad token: refused before MCP sees the request.
    let init = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}});
    assert_eq!(post(server.addr, None, None, &init).0, 401);
    assert_eq!(
        post(server.addr, None, Some("v4.public.nope"), &init).0,
        401
    );

    // A read token reads, and the engine refuses its writes.
    let read = Session::open(server.addr, &reader);
    let (failed, file) = read.call(
        "graft_read",
        json!({"repo": "r", "at": "main", "path": "README.md"}),
    );
    assert!(!failed, "{file}");
    assert_eq!(file["content"], "hello\n");
    let (failed, refused) = read.call(
        "graft_workspace",
        json!({"repo": "r", "action": "open", "line": "main"}),
    );
    assert!(failed);
    assert_eq!(refused["kind"], "permission_denied");

    // A write token opens a workspace and commits inline content.
    let write = Session::open(server.addr, &writer);
    let (failed, opened) = write.call(
        "graft_workspace",
        json!({"repo": "r", "action": "open", "line": "main"}),
    );
    assert!(!failed, "{opened}");
    let ws = opened["workspace"]["name"].as_str().unwrap().to_owned();
    let (failed, committed) = write.call(
        "graft_commit",
        json!({
            "repo": "r", "workspace": ws, "message": "two files",
            "files": [
                {"path": "README.md", "content": "hello again\n"},
                {"path": "src/lib.rs", "content": "pub fn f() {}\n"},
            ],
        }),
    );
    assert!(!failed, "{committed}");
    assert_eq!(committed["state"], "draft");
    let (_, file) = write.call(
        "graft_read",
        json!({"repo": "r", "at": committed["id"], "path": "src/lib.rs"}),
    );
    assert_eq!(file["content"], "pub fn f() {}\n");

    // Landing needs `land:<line>` as well as write: the write token is
    // refused, naming the scope.
    let (failed, refused) = write.call("graft_land", json!({"repo": "r", "workspace": ws}));
    assert!(failed);
    assert_eq!(refused["kind"], "permission_denied");
    assert_eq!(refused["scope"], "repo:r:land:main");

    // With the land scope, `main`'s default gate (decision D6, `attested`)
    // refuses a draft that CI has not attested, as a typed policy refusal.
    let land = Session::open(server.addr, &lander);
    let (failed, refused) = land.call("graft_land", json!({"repo": "r", "workspace": ws}));
    assert!(failed);
    assert_eq!(refused["kind"], "rejected");
    assert_eq!(refused["policy"], "attested");
    assert!(
        refused["detail"].as_str().unwrap().contains("ci/build"),
        "{refused}"
    );

    // CI attests both checks on the draft; the land then succeeds.
    let draft: graft_model::ChangesetId = committed["id"].as_str().unwrap().parse().unwrap();
    let attest = |check: &str| {
        pb::AttestRequest::from(Attest {
            repo: "r".into(),
            changeset: draft,
            check: check.into(),
            passed: true,
            url: "https://ci.example/run/1".into(),
        })
    };
    // `write` does not attest (WI-2272): CI holds the dedicated `attest`
    // scope, and its token is pinned to the changeset it attests, as the
    // OIDC exchange mints it.
    let mut by_writer = server.clients(Some(&writer)).await;
    let refused = by_writer.repo.attest(attest("ci/build")).await;
    assert_eq!(
        refused.expect_err("write cannot attest").code(),
        tonic::Code::PermissionDenied
    );
    let ci = site.mint_kind(
        &format!("ci:github:o/r#{draft}"),
        "ci",
        &["repo:r:read", "repo:r:attest"],
        3600,
    );
    let mut by_ci = server.clients(Some(&ci)).await;
    for check in ["ci/build", "ci/test"] {
        by_ci.repo.attest(attest(check)).await.expect("attest");
    }
    let (failed, landed) = land.call("graft_land", json!({"repo": "r", "workspace": ws}));
    assert!(!failed, "{landed}");
    assert_eq!(landed["outcome"], "landed");
    assert_eq!(landed["landed"], committed["id"]);
    assert_eq!(landed["rebased"], false);
    assert_eq!(landed["line_version"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_configured_non_main_line_is_attested() {
    // WI-2288: `[[lines]]` gives `trial-1` the attested policy.
    let site = Site::new(
        "server-line-policy",
        "[[lines]]\nname = \"trial-1\"\npolicy = \"attested\"\nchecks = [\"ci/fmt\", \"ci/test\"]\n",
    );
    let admin = site.mint(&["repo:r:admin"]);
    let lander = site.mint(&["repo:r:write", "repo:r:land:trial-1"]);
    let server = site.start();

    let mut c = server.clients(Some(&admin)).await;
    c.create_repo("r").await.expect("create");
    let files = BTreeMap::from([("README.md".to_owned(), b"hello\n".to_vec())]);
    let root = c
        .commit("r", None, &BTreeMap::new(), &files, 0)
        .await
        .expect("root");
    c.set_line("r", "trial-1", root.id).await.expect("line");

    let land = Session::open(server.addr, &lander);
    let (failed, opened) = land.call(
        "graft_workspace",
        json!({"repo": "r", "action": "open", "line": "trial-1"}),
    );
    assert!(!failed, "{opened}");
    let ws = opened["workspace"]["name"].as_str().unwrap().to_owned();
    let (failed, committed) = land.call(
        "graft_commit",
        json!({
            "repo": "r", "workspace": ws, "message": "change",
            "files": [{"path": "README.md", "content": "changed\n"}],
        }),
    );
    assert!(!failed, "{committed}");

    // Unattested: refused by the line's policy, naming the first check.
    let (failed, refused) = land.call("graft_land", json!({"repo": "r", "workspace": ws}));
    assert!(failed);
    assert_eq!(refused["kind"], "rejected");
    assert_eq!(refused["policy"], "attested");
    assert!(
        refused["detail"].as_str().unwrap().contains("ci/fmt"),
        "{refused}"
    );

    // Attested on the configured checks: the land succeeds.
    let draft: graft_model::ChangesetId = committed["id"].as_str().unwrap().parse().unwrap();
    let ci = site.mint_kind(
        &format!("ci:github:o/r#{draft}"),
        "ci",
        &["repo:r:read", "repo:r:attest"],
        3600,
    );
    let mut by_ci = server.clients(Some(&ci)).await;
    for check in ["ci/fmt", "ci/test"] {
        by_ci
            .repo
            .attest(pb::AttestRequest::from(Attest {
                repo: "r".into(),
                changeset: draft,
                check: check.into(),
                passed: true,
                url: "https://ci.example/run/1".into(),
            }))
            .await
            .expect("attest");
    }
    let (failed, landed) = land.call("graft_land", json!({"repo": "r", "workspace": ws}));
    assert!(!failed, "{landed}");
    assert_eq!(landed["outcome"], "landed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_refuses_an_oversized_body_from_an_authenticated_caller() {
    let site = Site::new("server-mcp-cap", "");
    let writer = site.mint(&["repo:r:write"]);
    let server = site.start();

    // Over the 4 MiB `/mcp` body cap, from a valid session: rmcp's limit
    // answers 413 before any tool runs. This pins the cap through the
    // server's mount, not only in graft-mcp's own tests.
    let session = Session::open(server.addr, &writer);
    let padding = "x".repeat(4 * 1024 * 1024 + 1);
    let body = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "graft_read", "arguments": {"pad": padding}}
    });
    let (status, _, _) = post(server.addr, Some(&session.id), Some(&writer), &body);
    assert_eq!(status, 413);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_refuses_writes_by_a_revoked_token_and_still_serves_its_reads() {
    let site = Site::new("server-mcp-revoke", "");
    let admin = site.mint(&["repo:r:admin"]);
    let target = site.mint_as("ses_target", &["repo:r:write"], 3600);
    let server = site.start();

    let mut c = server.clients(Some(&admin)).await;
    c.create_repo("r").await.expect("create");
    let files = BTreeMap::from([("README.md".to_owned(), b"hello\n".to_vec())]);
    let root = c
        .commit("r", None, &BTreeMap::new(), &files, 0)
        .await
        .expect("root");
    c.repo
        .update_ref(pb::UpdateRefRequest::from(UpdateRef {
            repo: "r".into(),
            name: "main".into(),
            expected: None,
            new: root.id,
            kind: RefKind::Line,
        }))
        .await
        .expect("main");

    let session = Session::open(server.addr, &target);
    let (failed, opened) = session.call(
        "graft_workspace",
        json!({"repo": "r", "action": "open", "line": "main"}),
    );
    assert!(!failed, "writes before revocation: {opened}");
    let ws = opened["workspace"]["name"].as_str().unwrap().to_owned();

    let out = site.admin_stdin(&["token", "revoke"], &target);
    assert!(
        out.status.success(),
        "revoke: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Refused on the next write, in the same MCP session, as on gRPC.
    let (failed, refused) = session.call(
        "graft_commit",
        json!({
            "repo": "r", "workspace": ws, "message": "after revocation",
            "files": [{"path": "README.md", "content": "revoked\n"}],
        }),
    );
    assert!(failed);
    assert_eq!(refused["kind"], "unauthenticated", "{refused}");
    let (failed, refused) = session.call(
        "graft_workspace",
        json!({"repo": "r", "action": "open", "line": "main"}),
    );
    assert!(failed);
    assert_eq!(refused["kind"], "unauthenticated", "{refused}");

    // Reads are not on the denylist's path, as on gRPC.
    let (failed, file) = session.call(
        "graft_read",
        json!({"repo": "r", "at": "main", "path": "README.md"}),
    );
    assert!(!failed, "{file}");
    assert_eq!(file["content"], "hello\n");
}
