//! A `/mcp` client for the tests: one initialized MCP session over plain
//! HTTP/1.1, with the bearer token of one principal.

#![allow(dead_code, unreachable_pub)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::{Value, json};

/// One POST to `/mcp`: the status, the session id the server names, and the
/// JSON-RPC message (unwrapped from SSE framing when there is one).
pub fn post(
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
pub struct Agent {
    addr: SocketAddr,
    token: String,
    id: String,
}

impl Agent {
    pub fn open(addr: SocketAddr, token: &str) -> Self {
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
    pub fn rpc(&self, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let (status, _, body) = post(self.addr, Some(&self.id), &self.token, &request);
        assert_eq!(status, 200, "{method}: {body}");
        serde_json::from_str(&body).expect("JSON")
    }

    /// Calls a tool; returns whether it failed and its structured content.
    pub fn call(&self, tool: &str, args: Value) -> (bool, Value) {
        let reply = self.rpc("tools/call", json!({"name": tool, "arguments": args}));
        let result = &reply["result"];
        assert!(result.is_object(), "{tool}: no result: {reply}");
        (
            result["isError"] == true,
            result["structuredContent"].clone(),
        )
    }

    /// Calls a tool that must succeed.
    pub fn ok(&self, tool: &str, args: Value) -> Value {
        let (failed, value) = self.call(tool, args);
        assert!(!failed, "{tool} failed: {value}");
        value
    }

    /// Calls a tool that must fail; returns the typed error.
    pub fn err(&self, tool: &str, args: Value) -> Value {
        let (failed, value) = self.call(tool, args);
        assert!(failed, "{tool} unexpectedly succeeded: {value}");
        value
    }

    /// `graft_read` of `path` at `at`: the whole (small) file, as its
    /// encoding and content.
    pub fn read(&self, at: &str, path: &str) -> (String, String) {
        let file = self.ok("graft_read", json!({"repo": "r", "at": at, "path": path}));
        assert_eq!(file["next_offset"], Value::Null, "one chunk");
        (
            file["encoding"].as_str().expect("encoding").to_owned(),
            file["content"].as_str().expect("content").to_owned(),
        )
    }

    /// Commits one `graft_commit` file entry on the workspace head.
    pub fn commit(&self, ws: &str, file: Value) -> String {
        let draft = self.ok(
            "graft_commit",
            json!({"repo": "r", "workspace": ws, "message": format!("edit by {ws}"), "files": [file]}),
        );
        draft["id"].as_str().unwrap().to_owned()
    }
}
