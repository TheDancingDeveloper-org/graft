//! Local token verification (`docs/design.md` §12.2): the node holds the
//! server's verifying key and checks the caller's token itself before it
//! serves a cached object, with no upstream round trip. Forwarded calls are
//! not checked here: the server checks them, and the node passes the token
//! through unchanged.

use std::time::{SystemTime, UNIX_EPOCH};

use graft_auth::{NoDenylist, Scope, Token, VerifyingKey};
use graft_proto::{ApiError, CallContext, PROTOCOL_VERSION, VERSION_HEADER};
use tonic::metadata::MetadataMap;

/// The request id header (as the client sends it).
pub(crate) const REQUEST_ID: &str = "graft-request-id";

/// A verified caller, with the token it presented.
pub(crate) struct Caller {
    /// Who is calling.
    pub(crate) cx: CallContext,
    /// The bearer token, for upstream calls made on its behalf.
    pub(crate) token: Token,
}

/// Verifies callers against the server's key.
#[derive(Clone)]
pub(crate) struct Verifier {
    key: VerifyingKey,
}

impl Verifier {
    pub(crate) fn new(key: VerifyingKey) -> Self {
        Self { key }
    }

    /// Binds the protocol version and verifies the bearer token.
    pub(crate) fn verify(&self, md: &MetadataMap) -> Result<Caller, ApiError> {
        let header = |k: &str| md.get(k).and_then(|v| v.to_str().ok());
        PROTOCOL_VERSION.accept(header(VERSION_HEADER))?;
        let token = header("authorization")
            .and_then(bearer)
            .ok_or(ApiError::Unauthenticated)?;
        let token = Token::from_encoded(token.to_owned());
        let claims = graft_auth::verify(&self.key, &token, now_s(), &NoDenylist)?;
        let request_id = header(REQUEST_ID)
            .filter(|id| is_request_id(id))
            .unwrap_or("")
            .to_owned();
        Ok(Caller {
            cx: CallContext {
                principal: claims.principal,
                request_id,
                deadline_ms: None,
            },
            token,
        })
    }
}

/// Whether `caller` may read objects of `repo`: any scope that grants
/// `repo:<repo>:read`, or `objects:read`.
pub(crate) fn may_read_objects(cx: &CallContext, repo: &str) -> Result<(), ApiError> {
    let read = Scope::Read {
        repo: repo.to_owned(),
    };
    graft_auth::authorize(&cx.principal, &read, None)
        .or_else(|_| graft_auth::authorize(&cx.principal, &Scope::ObjectsRead, None))
        .map_err(|_| ApiError::PermissionDenied {
            scope: read.to_string(),
            path: None,
        })
}

fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer")
        && !token.is_empty()
        && token.len() <= graft_auth::MAX_TOKEN_LEN)
        .then_some(token)
}

/// A request id worth logging: short, printable ASCII.
fn is_request_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_graphic())
}

fn now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
