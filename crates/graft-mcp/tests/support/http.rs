//! Plain HTTP/1.1 exchanges with a streamable HTTP MCP endpoint.

use std::net::SocketAddr;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Decodes a `Transfer-Encoding: chunked` body.
pub fn dechunk(mut rest: &str) -> String {
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
pub fn sse_message(body: &str) -> String {
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
pub async fn post(
    addr: SocketAddr,
    session: Option<&str>,
    token: Option<&str>,
    body: &Value,
) -> (u16, Option<String>, String) {
    post_raw(addr, session, token, body.to_string()).await
}

/// [`post`] with a raw body.
pub async fn post_raw(
    addr: SocketAddr,
    session: Option<&str>,
    token: Option<&str>,
    payload: String,
) -> (u16, Option<String>, String) {
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    if let Some(id) = session {
        request.push_str(&format!("Mcp-Session-Id: {id}\r\n"));
    }
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&payload);

    let mut stream = TcpStream::connect(addr)
        .await
        .expect("the listener accepts");
    // A server may answer and close before reading a refused body.
    let _ = stream.write_all(request.as_bytes()).await;
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
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
