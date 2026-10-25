//! JSON-RPC 2.0 over the LSP base protocol: messages framed by a
//! `Content-Length` header. Headers and bodies are bounded, so a client
//! can't make the server buffer without limit.

use std::io;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest message body the server reads. A larger one is skipped.
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// The longest header line.
const MAX_HEADER_LINE: u64 = 1024;

/// The most header lines in one message.
const MAX_HEADERS: usize = 16;

/// JSON-RPC and LSP error codes.
pub(crate) mod code {
    /// The body is not JSON.
    pub(crate) const PARSE_ERROR: i64 = -32700;
    /// The message is not a valid request.
    pub(crate) const INVALID_REQUEST: i64 = -32600;
    /// No such method.
    pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
    /// The parameters are wrong.
    pub(crate) const INVALID_PARAMS: i64 = -32602;
    /// A request arrived before `initialize`.
    pub(crate) const SERVER_NOT_INITIALIZED: i64 = -32002;
    /// The request was valid but failed.
    pub(crate) const REQUEST_FAILED: i64 = -32803;
}

/// One framed message as read.
pub(crate) enum Frame {
    /// The body.
    Body(Vec<u8>),
    /// A body over [`MAX_MESSAGE_BYTES`], skipped unread.
    TooLarge,
}

/// Reads one message. `None` at the end of the input.
pub(crate) async fn read_frame<R: AsyncBufRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut length: Option<usize> = None;
    let mut lines = 0;
    loop {
        let mut line = String::new();
        let n = (&mut *r).take(MAX_HEADER_LINE).read_line(&mut line).await?;
        if n == 0 {
            return if lines == 0 {
                Ok(None)
            } else {
                Err(bad("the input ended inside a header"))
            };
        }
        if !line.ends_with('\n') {
            return Err(bad("a header line is too long"));
        }
        lines += 1;
        if lines > MAX_HEADERS {
            return Err(bad("too many header lines"));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(bad("a header line has no colon"));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let n = value
                .trim()
                .parse()
                .map_err(|_| bad("Content-Length is not a number"))?;
            length = Some(n);
        }
    }
    let length = length.ok_or_else(|| bad("a message has no Content-Length"))?;
    if length > MAX_MESSAGE_BYTES {
        let skipped =
            tokio::io::copy(&mut (&mut *r).take(length as u64), &mut tokio::io::sink()).await?;
        if skipped < length as u64 {
            return Err(bad("the input ended inside a message"));
        }
        return Ok(Some(Frame::TooLarge));
    }
    let mut body = vec![0; length];
    r.read_exact(&mut body).await?;
    Ok(Some(Frame::Body(body)))
}

/// Writes one message.
pub(crate) async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    w.write_all(&body).await?;
    w.flush().await
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

/// A failed request.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RpcError {
    pub(crate) code: i64,
    pub(crate) message: String,
    pub(crate) data: Option<Value>,
}

impl RpcError {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, message)
    }

    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::new(code::REQUEST_FAILED, message)
    }

    pub(crate) fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// A response to request `id`.
pub(crate) fn response(id: Value, result: Result<Value, RpcError>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => {
            let mut error = json!({"code": e.code, "message": e.message});
            if let Some(data) = e.data {
                error["data"] = data;
            }
            json!({"jsonrpc": "2.0", "id": id, "error": error})
        }
    }
}

/// A notification.
pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// A request from the server to the client.
pub(crate) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": format!("graft-{id}"), "method": method, "params": params})
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn frames(input: &[u8]) -> Vec<io::Result<Option<Frame>>> {
        let mut r = tokio::io::BufReader::new(input);
        let mut out = Vec::new();
        loop {
            let f = read_frame(&mut r).await;
            let stop = !matches!(f, Ok(Some(_)));
            out.push(f);
            if stop {
                return out;
            }
        }
    }

    #[tokio::test]
    async fn reads_framed_bodies_and_ends_cleanly() {
        let got =
            frames(b"Content-Length: 2\r\nContent-Type: x\r\n\r\n{}content-length: 4\r\n\r\nnull")
                .await;
        assert_eq!(got.len(), 3);
        assert!(matches!(&got[0], Ok(Some(Frame::Body(b))) if b == b"{}"));
        assert!(matches!(&got[1], Ok(Some(Frame::Body(b))) if b == b"null"));
        assert!(matches!(got[2], Ok(None)));
    }

    #[tokio::test]
    async fn refuses_broken_headers() {
        for input in [
            &b"Content-Length: x\r\n\r\n"[..],
            b"no colon\r\n\r\n",
            b"Content-Type: y\r\n\r\n",
            b"Content-Length: 3\r\n",
        ] {
            let got = frames(input).await;
            assert!(got.last().unwrap().is_err(), "{input:?}");
        }
        let long = format!("X: {}\r\n\r\n", "a".repeat(2000));
        assert!(frames(long.as_bytes()).await.last().unwrap().is_err());
    }

    #[tokio::test]
    async fn skips_a_body_over_the_limit() {
        let mut input = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_BYTES + 1).into_bytes();
        input.resize(input.len() + MAX_MESSAGE_BYTES + 1, b' ');
        input.extend_from_slice(b"Content-Length: 2\r\n\r\n[]");
        let got = frames(&input).await;
        assert!(matches!(got[0], Ok(Some(Frame::TooLarge))));
        assert!(matches!(&got[1], Ok(Some(Frame::Body(b))) if b == b"[]"));
    }

    #[test]
    fn response_shape_preserves_typed_limit_errors() {
        let value = response(
            json!(1),
            Err(RpcError::failed("too large")
                .with_data(json!({"error": "limit_exceeded", "max_bytes": 8}))),
        );
        assert_eq!(value["error"]["data"]["error"], "limit_exceeded");
        assert_eq!(value["error"]["data"]["max_bytes"], 8);
    }
}
