//! WI-2312 repro: can an agent that has only the server's `/mcp` endpoint
//! (no working copy) finish a land that conflicted?
//!
//! Two agents edit the same line of one file on `main`. A lands first; B's
//! land conflicts. The test then tries every MCP route B has and pins what
//! each one does today. The finding, step by step, is in
//! `docs/notes/mcp-conflict-resolution.md`.

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::Site;
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

    /// `graft_read` of `f.txt` at `at`: the whole (small) file.
    fn read(&self, at: &str) -> String {
        let file = self.ok(
            "graft_read",
            json!({"repo": "r", "at": at, "path": "f.txt"}),
        );
        assert_eq!(file["next_offset"], Value::Null, "one chunk");
        file["content"].as_str().expect("utf8 content").to_owned()
    }

    fn workspace(&self, name: &str) -> Value {
        self.ok(
            "graft_workspace",
            json!({"repo": "r", "action": "get", "name": name}),
        )["workspace"]
            .clone()
    }

    fn commit(&self, ws: &str, parent: Option<&str>, content: &str) -> (bool, Value) {
        let mut args = json!({
            "repo": "r", "workspace": ws, "message": format!("edit by {ws}"),
            "files": [{"path": "f.txt", "content": content}],
        });
        if let Some(parent) = parent {
            args["parent"] = json!(parent);
        }
        self.call("graft_commit", args)
    }
}

/// Asserts a typed `conflict` on `f.txt` against `onto`.
fn assert_conflict(e: &Value, onto: &str) {
    assert_eq!(e["kind"], "conflict", "{e}");
    assert_eq!(e["paths"], json!(["f.txt"]), "{e}");
    assert_eq!(e["onto"], onto, "{e}");
    assert_eq!(e["retryable"], false, "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_only_agent_cannot_resolve_in_place_and_must_abandon_and_recommit() {
    // `main` is `direct` explicitly, so the test does not depend on the
    // default policy of `main` (D6 today).
    let site = Site::new(
        "server-mcp-conflict",
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

    // Both edit line 2 of f.txt.
    let (failed, draft_a) = a.commit("wa", None, THEIRS);
    assert!(!failed, "{draft_a}");
    let (failed, draft_b) = b.commit("wb", None, OURS);
    assert!(!failed, "{draft_b}");
    let draft_b = draft_b["id"].as_str().unwrap().to_owned();

    // A lands first.
    let landed_a = a.ok("graft_land", json!({"repo": "r", "workspace": "wa"}));
    assert_eq!(landed_a["outcome"], "landed");
    let onto = landed_a["landed"].as_str().unwrap().to_owned();

    // Step 1. B's land conflicts. The result is ids only: the path and the
    // tip it failed against. No base/ours/theirs ids, no content, no markers.
    let e = b.err("graft_land", json!({"repo": "r", "workspace": "wb"}));
    assert_conflict(&e, &onto);
    let keys: Vec<&str> = e.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["kind", "onto", "paths", "retryable"]);
    // `merge: file` (the stricter mode) conflicts the same way.
    let e = b.err(
        "graft_land",
        json!({"repo": "r", "workspace": "wb", "merge": "file"}),
    );
    assert_conflict(&e, &onto);

    // Step 2. Sync hits the same conflict and moves nothing.
    let e = b.err("graft_sync", json!({"repo": "r", "workspace": "wb"}));
    assert_conflict(&e, &onto);
    let e = b.err(
        "graft_sync",
        json!({"repo": "r", "workspace": "wb", "onto": onto}),
    );
    assert_conflict(&e, &onto);
    let ws = b.workspace("wb");
    assert_eq!(ws["base"], base, "sync left the base on the old tip");
    assert_eq!(ws["head"], draft_b.as_str());

    // Step 3. The three sides are readable, one `graft_read` each: base is
    // the workspace base, ours the draft, theirs `onto` (or `main`). The
    // agent must merge them itself; nothing renders conflict markers.
    assert_eq!(b.read(&base), BASE);
    assert_eq!(b.read(&draft_b), OURS);
    assert_eq!(b.read(&onto), THEIRS);
    assert_eq!(b.read("main"), THEIRS);
    let diff = b.ok(
        "graft_diff",
        json!({"repo": "r", "from": base, "to": onto, "paths": ["f.txt"]}),
    );
    assert!(
        diff["files"][0]["patch"]
            .as_str()
            .unwrap()
            .contains("+two (from A)"),
        "{diff}"
    );

    // Step 4. Committing the resolution on the workspace stacks it on top
    // of the conflicting draft (the parent defaults to the head) ...
    let (failed, fix) = b.commit("wb", None, RESOLVED);
    assert!(!failed, "{fix}");
    assert_eq!(fix["parent"], draft_b.as_str());
    let fix = fix["id"].as_str().unwrap().to_owned();
    // ... so the land still rebases the bottom draft first, and conflicts.
    let e = b.err("graft_land", json!({"repo": "r", "workspace": "wb"}));
    assert_conflict(&e, &onto);
    let e = b.err("graft_sync", json!({"repo": "r", "workspace": "wb"}));
    assert_conflict(&e, &onto);
    // Landing the fix by `head` is the same stack.
    let e = b.err(
        "graft_land",
        json!({"repo": "r", "workspace": "wb", "head": fix}),
    );
    assert_conflict(&e, &onto);
    // Squash, which would collapse the stack to base -> resolved, is not
    // built; it is refused before any rebase.
    let e = b.err(
        "graft_land",
        json!({"repo": "r", "workspace": "wb", "squash": true}),
    );
    assert_eq!(e["kind"], "rejected", "{e}");
    assert_eq!(e["policy"], "squash", "{e}");

    // Step 5. A resolution parented on `onto` is refused on this workspace:
    // the commit compare-and-swaps the head from `parents[0]`, and the head
    // is the fix, not `onto`.
    let (failed, e) = b.commit("wb", Some(&onto), RESOLVED);
    assert!(failed, "{e}");
    assert_eq!(e["kind"], "ref_conflict", "{e}");
    assert_eq!(e["name"], "wb", "{e}");
    assert_eq!(e["actual"], fix.as_str(), "{e}");
    // Naming the line as the parent is the same refusal.
    let (failed, e) = b.commit("wb", Some("main"), RESOLVED);
    assert!(failed, "{e}");
    assert_eq!(e["kind"], "ref_conflict", "{e}");

    // Step 6. There is no tool to drop or replace drafts, to move the base,
    // or to revert: `graft_revert` is in the design's tool table (§10.1) and
    // in `graft_mcp::TOOLS`, but `/mcp` does not register it.
    let listed = b.rpc("tools/list", json!({}));
    let mut names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "graft_blame",
            "graft_commit",
            "graft_diff",
            "graft_land",
            "graft_log",
            "graft_ls",
            "graft_read",
            "graft_show",
            "graft_status",
            "graft_sync",
            "graft_upstream",
            "graft_workspace",
        ]
    );
    let reply = b.rpc(
        "tools/call",
        json!({"name": "graft_revert", "arguments": {"repo": "r", "changeset": draft_b}}),
    );
    assert!(reply["result"].is_null(), "{reply}");
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    // `graft_workspace` opens on a line, never on a changeset off it: `at`
    // must be on the line, so it cannot re-base a workspace onto a draft.
    let e = b.err(
        "graft_workspace",
        json!({"repo": "r", "action": "open", "name": "wb-at-draft", "line": "main", "at": draft_b}),
    );
    assert_eq!(e["kind"], "invalid_change", "{e}");

    // Step 7. The one route that works: give up the conflicting stack and
    // re-commit the merged content on a fresh base. Close the workspace
    // (its drafts become abandoned), open one on the new tip, commit the
    // resolved file and land it. The result is a new changeset with no link
    // to B's draft (no `graft.rebased-from`), and a stack of several drafts
    // would have to be re-created by hand, draft by draft.
    let closed = b.ok(
        "graft_workspace",
        json!({"repo": "r", "action": "close", "name": "wb", "keep_drafts": false}),
    );
    assert_eq!(closed["kept_drafts"], false);
    let reopened = b.ok(
        "graft_workspace",
        json!({"repo": "r", "action": "open", "name": "wb", "line": "main"}),
    );
    assert_eq!(reopened["workspace"]["base"], onto.as_str());
    assert_eq!(reopened["workspace"]["head"], Value::Null);
    let (failed, redo) = b.commit("wb", None, RESOLVED);
    assert!(!failed, "{redo}");
    assert_eq!(redo["parent"], onto.as_str());
    let landed_b = b.ok("graft_land", json!({"repo": "r", "workspace": "wb"}));
    assert_eq!(landed_b["outcome"], "landed", "{landed_b}");
    assert_eq!(landed_b["rebased"], false);
    assert_eq!(landed_b["landed"], redo["id"]);
    assert_eq!(b.read("main"), RESOLVED);
    let shown = b.ok(
        "graft_show",
        json!({"repo": "r", "changeset": landed_b["landed"]}),
    );
    assert_eq!(shown["changeset"]["parents"], json!([onto]));
    assert_eq!(
        shown["changeset"]["metadata"],
        json!({}),
        "no rebased-from link to the abandoned draft"
    );
    // The original draft is abandoned, not landed.
    let old = b.ok("graft_show", json!({"repo": "r", "changeset": draft_b}));
    assert_eq!(old["changeset"]["state"], "abandoned");
}
