//! Forwarding (`docs/design.md` §5.1): every call but `GetObjects` goes to
//! the server unchanged, byte for byte, with the caller's headers (its
//! token, protocol version, request id and deadline). Answers stream back
//! the same way. Nothing forwarded is cached, so a mutable answer is never
//! served stale.
//!
//! Only the `graft.v1` methods in [`FORWARDED`] are forwarded; anything else
//! is `Unimplemented` here, so the socket is not a general proxy. A call
//! takes an upstream slot until its answer's headers arrive, so the calls
//! in flight upstream are bounded, and when upstream can't be reached the
//! call gets `Unavailable` at once (see [`crate::upstream`]). Streams that
//! outlive their headers (`Watch`) are bounded by the connection and
//! per-connection stream limits, and by the server's own `Watch` limits.

use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use graft_proto::ApiError;
use tonic::Status;
use tonic::codegen::http;

use crate::metrics::NodeMetrics;
use crate::upstream::Upstream;

/// Every method the node forwards, with its operation name.
pub const FORWARDED: &[(&str, &str)] = &[
    ("/graft.v1.ObjectService/HasObjects", "HasObjects"),
    ("/graft.v1.ObjectService/PutObjects", "PutObjects"),
    ("/graft.v1.RepoService/CreateChangeset", "CreateChangeset"),
    ("/graft.v1.RepoService/GetChangesets", "GetChangesets"),
    ("/graft.v1.RepoService/ResolveRef", "ResolveRef"),
    ("/graft.v1.RepoService/ListRefs", "ListRefs"),
    ("/graft.v1.RepoService/UpdateRef", "UpdateRef"),
    ("/graft.v1.RepoService/Stat", "Stat"),
    ("/graft.v1.RepoService/ListTree", "ListTree"),
    ("/graft.v1.RepoService/ReadFile", "ReadFile"),
    ("/graft.v1.RepoService/Log", "Log"),
    ("/graft.v1.RepoService/IsAncestor", "IsAncestor"),
    ("/graft.v1.RepoService/MergeBase", "MergeBase"),
    ("/graft.v1.RepoService/Diff", "Diff"),
    ("/graft.v1.RepoService/Blame", "Blame"),
    ("/graft.v1.RepoService/Rebase", "Rebase"),
    ("/graft.v1.RepoService/Revert", "Revert"),
    ("/graft.v1.RepoService/Land", "Land"),
    ("/graft.v1.RepoService/Attest", "Attest"),
    ("/graft.v1.RepoService/Watch", "Watch"),
    ("/graft.v1.RepoService/CreateLine", "CreateLine"),
    ("/graft.v1.WorkspaceService/OpenWorkspace", "OpenWorkspace"),
    ("/graft.v1.WorkspaceService/GetWorkspace", "GetWorkspace"),
    (
        "/graft.v1.WorkspaceService/ListWorkspaces",
        "ListWorkspaces",
    ),
    ("/graft.v1.WorkspaceService/RenewLease", "RenewLease"),
    ("/graft.v1.WorkspaceService/SetHead", "SetHead"),
    ("/graft.v1.WorkspaceService/Sync", "Sync"),
    ("/graft.v1.WorkspaceService/Stack", "Stack"),
    (
        "/graft.v1.WorkspaceService/CloseWorkspace",
        "CloseWorkspace",
    ),
    ("/graft.v1.WorkspaceService/GetConflicts", "GetConflicts"),
    ("/graft.v1.WorkspaceService/Resolve", "Resolve"),
    ("/graft.v1.AdminService/CreateRepo", "CreateRepo"),
    (
        "/graft.v1.AdminService/ImportChangesets",
        "ImportChangesets",
    ),
    ("/graft.v1.AdminService/Gc", "Gc"),
    ("/graft.v1.AdminService/MintToken", "MintToken"),
    ("/graft.v1.CiService/ExchangeCiToken", "ExchangeCiToken"),
    ("/graft.v1.MirrorService/RecordExports", "RecordExports"),
];

/// Hop-by-hop headers, which describe one connection and are not passed on.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
];

/// The forwarder.
#[derive(Clone)]
pub(crate) struct Forwarder {
    pub(crate) upstream: Upstream,
    pub(crate) metrics: NodeMetrics,
}

impl Forwarder {
    /// Forwards one call, or answers it with a status.
    pub(crate) async fn forward(self, req: Request) -> Response {
        let started = Instant::now();
        let Some(&(path, op)) = FORWARDED.iter().find(|(p, _)| *p == req.uri().path()) else {
            return status(Status::unimplemented("not a graft method"));
        };
        let peer = req.extensions().get::<crate::socket::Peer>().cloned();
        tracing::debug!(
            op,
            peer_uid = peer.as_ref().map(|p| p.uid),
            peer_pid = peer.as_ref().and_then(|p| p.pid),
            tier = "node",
            "forward"
        );
        let slot = match self.upstream.slot().await {
            Ok(slot) => slot,
            Err(e) => return self.refuse(op, e, started),
        };
        let (parts, body) = req.into_parts();
        let mut out = http::Request::builder()
            .method(parts.method)
            .uri(path)
            .version(http::Version::HTTP_2);
        for (name, value) in &parts.headers {
            if !HOP_BY_HOP.contains(&name.as_str()) {
                out = out.header(name, value);
            }
        }
        let Ok(out) = out.body(tonic::body::Body::new(body)) else {
            return status(Status::invalid_argument("malformed request"));
        };
        self.metrics.forwarded.inc();
        let resp = self.upstream.send(out).await;
        drop(slot);
        match resp {
            Ok(resp) => {
                self.metrics.rpc(op, "forwarded", started);
                resp.map(Body::new)
            }
            Err(e) => self.refuse(op, e, started),
        }
    }

    fn refuse(&self, op: &str, e: ApiError, started: Instant) -> Response {
        self.metrics.rpc(op, "unavailable", started);
        status(Status::from(e))
    }
}

fn status(s: Status) -> Response {
    s.into_http::<Body>()
}
