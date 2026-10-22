//! The streamable HTTP service, mounted on a loopback listener with an
//! ephemeral port and driven with plain HTTP requests: initialize, the
//! initialized notification, a tool list, a tool call, then shutdown.

mod support;

use std::net::SocketAddr;

use graft_mcp::streamable_http_service;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use serde_json::{Value, json};
use support::{Fake, principal};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Decodes a `Transfer-Encoding: chunked` body.
fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    loop {
        let (size, after) = rest.split_once("\r\n").expect("a chunk size");
        let n = usize::from_str_radix(size.trim(), 16).expect("a hex chunk size");
        if n == 0 {
            return out;
        }
        out.push_str(&after[..n]);
        rest = &after[n + 2..];
    }
}

/// The JSON-RPC message in a reply. The server frames replies as server-sent
/// events even with JSON responses on, so the message is the `data:` line.
/// A body that is not SSE is returned as it is.
fn sse_message(body: &str) -> String {
    if !body.starts_with("data:") {
        return body.to_owned();
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .find(|data| data.starts_with('{'))
        .unwrap_or_default()
        .to_owned()
}

/// One HTTP exchange: a POST of `body` to `/mcp`, with the session id when
/// there is one. Returns the status, the session id the server names, and the
/// response body.
async fn post(
    addr: SocketAddr,
    session: Option<&str>,
    body: &Value,
) -> (u16, Option<String>, String) {
    let payload = body.to_string();
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    if let Some(id) = session {
        request.push_str(&format!("Mcp-Session-Id: {id}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&payload);

    let mut stream = TcpStream::connect(addr)
        .await
        .expect("the listener accepts");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8(raw).expect("UTF-8 response");
    let (head, rest) = raw.split_once("\r\n\r\n").expect("an HTTP response");

    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status code");
    let mut session_id = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("mcp-session-id") {
            session_id = Some(value.trim().to_owned());
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.trim().eq_ignore_ascii_case("chunked");
        }
    }
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_owned()
    };
    (status, session_id, sse_message(&body))
}

#[tokio::test]
async fn the_service_serves_tools_over_loopback_http() {
    let config = StreamableHttpServerConfig::default().with_json_response(true);
    let service = streamable_http_service(Fake::with_files(), principal(), config);
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });

    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "graft-mcp-test", "version": "0"}
        }
    });
    let (status, session, body) = post(addr, None, &init).await;
    assert_eq!(status, 200, "initialize: {body}");
    let session = session.expect("the server names a session");
    let reply: Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("a JSON reply ({e}): {body:?}"));
    assert_eq!(reply["result"]["serverInfo"]["name"], "graft-mcp");
    assert!(reply["result"]["capabilities"]["resources"].is_object());

    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let (status, _, _) = post(addr, Some(&session), &initialized).await;
    assert_eq!(status, 202, "the notification is accepted");

    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let (status, _, body) = post(addr, Some(&session), &list).await;
    assert_eq!(status, 200);
    let tools: Vec<Value> = serde_json::from_str::<Value>(&body).unwrap()["result"]["tools"]
        .as_array()
        .unwrap()
        .clone();
    // The server lists tools by name, not in design order.
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "graft_blame",
            "graft_diff",
            "graft_log",
            "graft_ls",
            "graft_read",
            "graft_show",
            "graft_status"
        ]
    );
    assert!(
        tools
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == true)
    );

    let call = json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {
            "name": "graft_read",
            "arguments": {"repo": "r", "at": "main", "path": "README.md", "limit": 4}
        }
    });
    let (status, _, body) = post(addr, Some(&session), &call).await;
    assert_eq!(status, 200);
    let read: Value =
        serde_json::from_str::<Value>(&body).unwrap()["result"]["structuredContent"].clone();
    assert_eq!(read["content"], "# fi");
    assert_eq!(read["next_offset"], 4);

    stop.send(()).expect("the server is listening");
    server
        .await
        .expect("the server task finishes")
        .expect("the server shuts down cleanly");
}
