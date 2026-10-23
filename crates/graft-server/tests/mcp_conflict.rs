//! WI-2312: an agent that has only the server's `/mcp` endpoint (no working
//! copy) finishes a land that conflicted, with `graft_conflicts` and
//! `graft_resolve`.
//!
//! Two agents change one file on `main`. A lands first; B's land conflicts.
//! B reads the conflict, resolves it, and lands. The cases are a same-line
//! edit, delete/modify, and a binary file. The original finding is in
//! `docs/notes/mcp-conflict-resolution.md`.

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::{Server, Site};
use serde_json::{Value, json};

const BASE: &str = "one\ntwo\nthree\n";
const OURS: &str = "one\ntwo (from B)\nthree\n";
const THEIRS: &str = "one\ntwo (from A)\nthree\n";
const RESOLVED: &str = "one\ntwo (from A and B)\nthree\n";

/// One POST to `/mcp`: the status, the session id the server names, and the
/// JSON-RPC message (unwrapped from SSE framing when there is one).
fn post(
    addr: SocketAddr,
    session: Option<&str>,
    token: &str,
    body: &Value,
) -> (u16, Option<String>, String) {
    let payload = body.to_string();
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: graft\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\nAuthorization: Bearer {token}\r\n",
        payload.len()
    );
    if let Some(id) = session {
        head.push_str(&format!("Mcp-Session-Id: {id}\r\n"));
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

/// An initialized MCP session: one agent with only `/mcp`.
struct Agent {
    addr: SocketAddr,
    token: String,
    id: String,
}

impl Agent {
    fn open(addr: SocketAddr, token: &str) -> Self {
        let init = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "conflict-repro", "version": "0"}
            }
        });
        let (status, id, body) = post(addr, None, token, &init);
        assert_eq!(status, 200, "initialize: {body}");
        let id = id.expect("a session id");
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(post(addr, Some(&id), token, &note).0, 202);
        Self {
            addr,
            token: token.to_owned(),
            id,
        }
    }

    /// The whole JSON-RPC reply to one `method` call.
    fn rpc(&self, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let (status, _, body) = post(self.addr, Some(&self.id), &self.token, &request);
        assert_eq!(status, 200, "{method}: {body}");
        serde_json::from_str(&body).expect("JSON")
    }

    /// Calls a tool; returns whether it failed and its structured content.
    fn call(&self, tool: &str, args: Value) -> (bool, Value) {
        let reply = self.rpc("tools/call", json!({"name": tool, "arguments": args}));
        let result = &reply["result"];
        assert!(result.is_object(), "{tool}: no result: {reply}");
        (
            result["isError"] == true,
            result["structuredContent"].clone(),
        )
    }

    /// Calls a tool that must succeed.
    fn ok(&self, tool: &str, args: Value) -> Value {
        let (failed, value) = self.call(tool, args);
        assert!(!failed, "{tool} failed: {value}");
        value
    }

    /// Calls a tool that must fail; returns the typed error.
    fn err(&self, tool: &str, args: Value) -> Value {
        let (failed, value) = self.call(tool, args);
        assert!(failed, "{tool} unexpectedly succeeded: {value}");
        value
    }

    /// `graft_read` of `path` at `at`: the whole (small) file, as its
    /// encoding and content.
    fn read(&self, at: &str, path: &str) -> (String, String) {
        let file = self.ok("graft_read", json!({"repo": "r", "at": at, "path": path}));
        assert_eq!(file["next_offset"], Value::Null, "one chunk");
        (
            file["encoding"].as_str().expect("encoding").to_owned(),
            file["content"].as_str().expect("content").to_owned(),
        )
    }

    /// Commits one `graft_commit` file entry on the workspace head.
    fn commit(&self, ws: &str, file: Value) -> String {
        let draft = self.ok(
            "graft_commit",
            json!({"repo": "r", "workspace": ws, "message": format!("edit by {ws}"), "files": [file]}),
        );
        draft["id"].as_str().unwrap().to_owned()
    }
}

/// What one case changes: the root file, A's change, B's change, and B's
/// resolution, each a `graft_commit` file entry (or resolution) for `path`.
struct Case {
    path: &'static str,
    root: Vec<u8>,
    theirs: Value,
    ours: Value,
    resolution: Value,
}

/// The run after B's conflicting land: what B saw and what landed.
struct Outcome {
    onto: String,
    draft_b: String,
    conflict_error: Value,
    conflicts: Value,
    resolved: Value,
    landed: Value,
    agent: Agent,
    secrets: Vec<String>,
    // Kept alive until the case's last call.
    _server: Server,
    _site: Site,
}

/// Plays one case: A and B open workspaces on `main`, commit, A lands, B's
/// land conflicts, then B resolves with only MCP tools and lands.
async fn play(name: &str, case: Case) -> Outcome {
    let site = Site::new(name, "[[lines]]\nname = \"main\"\npolicy = \"direct\"\n");
    let admin = site.mint(&["repo:r:admin"]);
    let scopes = ["repo:r:write", "repo:r:land:main"];
    let token_a = site.mint_as("agent-a", &scopes, 3600);
    let token_b = site.mint_as("agent-b", &scopes, 3600);
    let server = site.start();

    let mut c = server.clients(Some(&admin)).await;
    c.create_repo("r").await.expect("create");
    let files = BTreeMap::from([(case.path.to_owned(), case.root.clone())]);
    let root = c
        .commit("r", None, &BTreeMap::new(), &files, 0)
        .await
        .expect("root");
    c.set_line("r", "main", root.id).await.expect("main");
    let base = root.id.to_string();

    let a = Agent::open(server.addr, &token_a);
    let b = Agent::open(server.addr, &token_b);
    for (agent, ws) in [(&a, "wa"), (&b, "wb")] {
        let opened = agent.ok(
            "graft_workspace",
            json!({"repo": "r", "action": "open", "name": ws, "line": "main"}),
        );
        assert_eq!(opened["workspace"]["base"], base);
    }
    a.commit("wa", case.theirs);
    let draft_b = b.commit("wb", case.ours);
    let landed_a = a.ok("graft_land", json!({"repo": "r", "workspace": "wa"}));
    assert_eq!(landed_a["outcome"], "landed");
    let onto = landed_a["landed"].as_str().unwrap().to_owned();

    // B's land conflicts, with ids only.
    let conflict_error = b.err("graft_land", json!({"repo": "r", "workspace": "wb"}));
    assert_eq!(conflict_error["kind"], "conflict", "{conflict_error}");
    assert_eq!(conflict_error["paths"], json!([case.path]));
    assert_eq!(conflict_error["onto"], onto.as_str());
    assert_eq!(conflict_error["retryable"], false);
    assert_eq!(conflict_error["sides"][0]["path"], case.path);

    // B reads the conflict ...
    let conflicts = b.ok("graft_conflicts", json!({"repo": "r", "workspace": "wb"}));
    assert_eq!(conflicts["onto"], onto.as_str(), "{conflicts}");
    assert_eq!(conflicts["draft"], draft_b.as_str(), "{conflicts}");
    assert_eq!(conflicts["next"], Value::Null);
    assert_eq!(
        conflicts["paths"].as_array().unwrap().len(),
        1,
        "{conflicts}"
    );
    let side = &conflicts["paths"][0];
    assert_eq!(side["path"], case.path);
    // The error and the conflict view name the same ids.
    for k in ["base", "ours", "theirs"] {
        assert_eq!(
            conflict_error["sides"][0][k], side[k]["id"],
            "{k}: {conflict_error} vs {side}"
        );
    }

    // ... resolves it against `onto`, under CAS on its head ...
    let mut resolution = case.resolution;
    resolution["path"] = json!(case.path);
    let resolved = b.ok(
        "graft_resolve",
        json!({
            "repo": "r", "workspace": "wb", "onto": onto,
            "expected_head": draft_b, "resolutions": [resolution.clone()],
        }),
    );
    assert_eq!(resolved["workspace"]["base"], onto.as_str(), "{resolved}");
    assert_eq!(resolved["workspace"]["head"], resolved["head"]);
    assert_eq!(resolved["resolved_paths"], json!([case.path]));
    // A retry of the same request is a no-op success with the same head.
    let again = b.ok(
        "graft_resolve",
        json!({
            "repo": "r", "workspace": "wb", "onto": onto,
            "expected_head": draft_b, "resolutions": [resolution],
        }),
    );
    assert_eq!(again["head"], resolved["head"], "deterministic: {again}");

    // ... and lands: a fast-forward of the resolved draft.
    let landed = b.ok("graft_land", json!({"repo": "r", "workspace": "wb"}));
    assert_eq!(landed["outcome"], "landed", "{landed}");
    assert_eq!(landed["landed"], resolved["head"]);
    assert_eq!(landed["rebased"], false);

    let secrets = vec![token_a, token_b, admin];
    Outcome {
        onto,
        draft_b,
        conflict_error,
        conflicts,
        resolved,
        landed,
        agent: b,
        secrets,
        _server: server,
        _site: site,
    }
}

/// The landed changeset is B's draft rebased onto `onto`, with lineage.
fn assert_lineage(o: &Outcome, path: &str) {
    let shown = o.agent.ok(
        "graft_show",
        json!({"repo": "r", "changeset": o.landed["landed"]}),
    );
    let cs = &shown["changeset"];
    assert_eq!(cs["parents"], json!([o.onto]), "{shown}");
    assert_eq!(
        cs["metadata"]["graft.rebased-from"],
        o.draft_b.as_str(),
        "{shown}"
    );
    assert_eq!(
        cs["metadata"]["graft.resolved-paths"],
        json!([path]).to_string(),
        "{shown}"
    );
}

/// No error payload carries file contents or a token.
fn assert_clean(o: &Outcome, contents: &[&str]) {
    let text = o.conflict_error.to_string();
    for c in contents {
        assert!(!text.contains(c), "error carries content {c:?}: {text}");
    }
    for t in &o.secrets {
        for v in [&o.conflict_error, &o.conflicts, &o.resolved, &o.landed] {
            assert!(!v.to_string().contains(t.as_str()), "a token leaked");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_only_agent_resolves_a_same_line_conflict() {
    let o = play(
        "server-mcp-conflict-line",
        Case {
            path: "f.txt",
            root: BASE.as_bytes().to_vec(),
            theirs: json!({"path": "f.txt", "content": THEIRS}),
            ours: json!({"path": "f.txt", "content": OURS}),
            resolution: json!({"content": RESOLVED}),
        },
    )
    .await;
    let side = &o.conflicts["paths"][0];
    assert_eq!(side["text_mergeable"], true, "{side}");
    assert_eq!(side["reason"], Value::Null);
    assert_eq!(side["markers_truncated"], false);
    let markers = side["markers"].as_str().expect("markers");
    for needle in ["<<<<<<<", "two (from B)", "two (from A)", ">>>>>>>"] {
        assert!(markers.contains(needle), "{needle} in {markers}");
    }
    assert_eq!(o.resolved["merged_paths"], json!([]));
    // The oracle: exactly the resolved file is on `main`.
    assert_eq!(
        o.agent.read("main", "f.txt"),
        ("utf8".into(), RESOLVED.into())
    );
    assert_lineage(&o, "f.txt");
    assert_clean(&o, &["(from A)", "(from B)", "three"]);
    // The old draft is superseded by the rebase, not landed.
    let old = o
        .agent
        .ok("graft_show", json!({"repo": "r", "changeset": o.draft_b}));
    assert_ne!(old["changeset"]["state"], "landed");

    // `graft_revert` is served now: invert the landed change on the
    // workspace and the line's file goes back to A's version.
    let revert = o.agent.ok(
        "graft_revert",
        json!({"repo": "r", "changeset": o.landed["landed"], "workspace": "wb"}),
    );
    assert_eq!(revert["onto"], o.landed["landed"], "{revert}");
    let landed = o
        .agent
        .ok("graft_land", json!({"repo": "r", "workspace": "wb"}));
    assert_eq!(landed["landed"], revert["id"], "{landed}");
    assert_eq!(
        o.agent.read("main", "f.txt"),
        ("utf8".into(), THEIRS.into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_only_agent_resolves_modify_delete_by_deleting() {
    let o = play(
        "server-mcp-conflict-delete",
        Case {
            path: "f.txt",
            root: BASE.as_bytes().to_vec(),
            theirs: json!({"path": "f.txt", "content": THEIRS}),
            ours: json!({"path": "f.txt", "op": "delete"}),
            resolution: json!({"op": "delete"}),
        },
    )
    .await;
    let side = &o.conflicts["paths"][0];
    assert_eq!(side["ours"], Value::Null, "{side}");
    assert_eq!(side["markers"], Value::Null);
    assert_eq!(side["reason"], "clash");
    assert_eq!(o.conflict_error["sides"][0]["ours"], Value::Null);
    // The oracle: the path is gone from `main`.
    let e = o.agent.err(
        "graft_read",
        json!({"repo": "r", "at": "main", "path": "f.txt"}),
    );
    assert_eq!(e["kind"], "not_found", "{e}");
    assert_lineage(&o, "f.txt");
    assert_clean(&o, &["(from A)", "three"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_only_agent_resolves_a_binary_conflict_by_id_only() {
    let o = play(
        "server-mcp-conflict-binary",
        Case {
            path: "blob.bin",
            root: vec![0xff, 0x00, 0x01],
            theirs: json!({"path": "blob.bin", "content": "ff0002", "encoding": "hex"}),
            ours: json!({"path": "blob.bin", "content": "ff0003", "encoding": "hex"}),
            resolution: json!({"content": "ff0004", "encoding": "hex"}),
        },
    )
    .await;
    let side = &o.conflicts["paths"][0];
    assert_eq!(side["text_mergeable"], false, "{side}");
    assert_eq!(side["markers"], Value::Null);
    assert_eq!(side["reason"], "binary");
    assert_eq!(
        o.agent.read("main", "blob.bin"),
        ("hex".into(), "ff0004".into())
    );
    assert_lineage(&o, "blob.bin");
    assert_clean(&o, &["ff0002", "ff0003"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_into_a_workspace_is_for_its_owner_only() {
    let site = Site::new(
        "server-mcp-revert-owner",
        "[[lines]]\nname = \"main\"\npolicy = \"direct\"\n",
    );
    let admin = site.mint(&["repo:r:admin"]);
    let scopes = ["repo:r:write", "repo:r:land:main"];
    let token_a = site.mint_as("agent-a", &scopes, 3600);
    let token_b = site.mint_as("agent-b", &scopes, 3600);
    let server = site.start();

    let mut c = server.clients(Some(&admin)).await;
    c.create_repo("r").await.expect("create");
    let files = BTreeMap::from([("f.txt".to_owned(), BASE.as_bytes().to_vec())]);
    let root = c
        .commit("r", None, &BTreeMap::new(), &files, 0)
        .await
        .expect("root");
    let files = BTreeMap::from([("f.txt".to_owned(), THEIRS.as_bytes().to_vec())]);
    let tip = c
        .commit("r", Some(root.id), &BTreeMap::new(), &files, 1)
        .await
        .expect("tip");
    c.set_line("r", "main", tip.id).await.expect("main");
    let tip = tip.id.to_string();

    let a = Agent::open(server.addr, &token_a);
    let b = Agent::open(server.addr, &token_b);
    a.ok(
        "graft_workspace",
        json!({"repo": "r", "action": "open", "name": "wa", "line": "main"}),
    );
    let draft = a.commit("wa", json!({"path": "g.txt", "content": "g\n"}));

    // B may not add a draft to A's workspace, by default `onto` or not.
    for args in [
        json!({"repo": "r", "changeset": tip, "workspace": "wa"}),
        json!({"repo": "r", "changeset": tip, "workspace": "wa", "onto": tip}),
    ] {
        let e = b.err("graft_revert", args);
        assert_eq!(e["kind"], "permission_denied", "{e}");
    }
    // Nor over gRPC.
    let mut cb = server.clients(Some(&token_b)).await;
    let tip_id = graft_model::ChangesetId::parse(&tip).unwrap();
    let e = cb
        .repo
        .revert(graft_proto::pb::RevertRequest {
            repo: "r".into(),
            changeset: tip_id.0.0.to_vec(),
            onto: tip_id.0.0.to_vec(),
            workspace: Some("wa".into()),
        })
        .await
        .expect_err("foreign revert");
    assert!(
        matches!(
            common::api(e),
            graft_proto::ApiError::PermissionDenied { .. }
        ),
        "gRPC refuses too"
    );
    let ws = a.ok(
        "graft_workspace",
        json!({"repo": "r", "action": "get", "name": "wa"}),
    );
    assert_eq!(
        ws["workspace"]["head"],
        draft.as_str(),
        "A's stack is untouched"
    );

    // The owner may revert onto a changeset that is not the workspace head.
    let reverted = a.ok(
        "graft_revert",
        json!({"repo": "r", "changeset": tip, "workspace": "wa", "onto": draft}),
    );
    assert_eq!(reverted["onto"], draft.as_str(), "{reverted}");
    assert_eq!(
        a.read(reverted["id"].as_str().unwrap(), "f.txt"),
        ("utf8".into(), BASE.into())
    );
}
