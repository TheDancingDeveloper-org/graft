//! The streamable HTTP service, mounted on a loopback listener with an
//! ephemeral port and driven with plain HTTP requests: initialize, the
//! initialized notification, a tool list, a tool call, then shutdown.

mod support;

use std::net::SocketAddr;

use graft_mcp::{Caller, streamable_http_service};
use graft_proto::CallContext;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use serde_json::{Value, json};
use support::http::post;
use support::{Fake, principal};

/// Serves `/mcp` on a loopback port. With `authenticated`, a layer stands in
/// for the mounting server's token check and gives every request a caller.
async fn serve(
    authenticated: bool,
) -> (
    SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let config = StreamableHttpServerConfig::default().with_json_response(true);
    let service = streamable_http_service(Fake::with_files(), config);
    let mut app = axum::Router::new().nest_service("/mcp", service);
    if authenticated {
        app = app.layer(axum::middleware::from_fn(
            |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
                request.extensions_mut().insert(Caller(CallContext {
                    principal: principal(),
                    request_id: "http-test".into(),
                    deadline_ms: None,
                }));
                next.run(request).await
            },
        ));
    }
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
    (addr, stop, server)
}

/// Initializes a session and returns its id.
async fn initialize(addr: SocketAddr) -> String {
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "graft-mcp-test", "version": "0"}
        }
    });
    let (status, session, body) = post(addr, None, None, &init).await;
    assert_eq!(status, 200, "initialize: {body}");
    let session = session.expect("the server names a session");
    let reply: Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("a JSON reply ({e}): {body:?}"));
    assert_eq!(reply["result"]["serverInfo"]["name"], "graft-mcp");
    assert!(reply["result"]["capabilities"]["resources"].is_object());

    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let (status, _, _) = post(addr, Some(&session), None, &initialized).await;
    assert_eq!(status, 202, "the notification is accepted");
    session
}

#[tokio::test]
async fn the_service_serves_tools_over_loopback_http() {
    let (addr, stop, server) = serve(true).await;
    let session = initialize(addr).await;

    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let (status, _, body) = post(addr, Some(&session), None, &list).await;
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
            "graft_workspace"
        ]
    );
    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        let writes = [
            "graft_commit",
            "graft_land",
            "graft_sync",
            "graft_workspace",
        ];
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            !writes.contains(&name),
            "{name}"
        );
    }

    let call = json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {
            "name": "graft_read",
            "arguments": {"repo": "r", "at": "main", "path": "README.md", "limit": 4}
        }
    });
    let (status, _, body) = post(addr, Some(&session), None, &call).await;
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

#[tokio::test]
async fn a_request_without_a_caller_is_unauthenticated() {
    let (addr, stop, server) = serve(false).await;
    let session = initialize(addr).await;
    let call = json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {
            "name": "graft_read",
            "arguments": {"repo": "r", "at": "main", "path": "README.md"}
        }
    });
    let (status, _, body) = post(addr, Some(&session), None, &call).await;
    assert_eq!(status, 200);
    let result = serde_json::from_str::<Value>(&body).unwrap()["result"].clone();
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["kind"], "unauthenticated");
    stop.send(()).expect("the server is listening");
    server.await.unwrap().unwrap();
}
