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
use graft_proto::types::{RefKind, UpdateRef};
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

    // Land is not built in this release's engine; it answers a typed refusal
    // instead of failing the session.
    let (failed, refused) = write.call("graft_land", json!({"repo": "r", "workspace": ws}));
    assert!(failed);
    assert_eq!(refused["kind"], "rejected");
    assert_eq!(refused["policy"], "unimplemented");
}
