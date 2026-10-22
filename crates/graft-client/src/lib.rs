//! The graft client library (`docs/design.md` §5.1, §7.3, §9).
//!
//! [`Remote`] implements the API traits over the wire (TCP or the node
//! socket); [`workcopy`] is the working copy; [`fetch`] is the named-object
//! fetch protocol. Work items G1.11, G2.5, G3.5.

use std::path::PathBuf;

pub mod fetch;
pub mod remote;
pub mod workcopy;

pub use remote::Remote;

/// Where the client sends requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// The per-host node daemon's socket.
    NodeSocket(PathBuf),
    /// A server over TCP (`http://host:port` or `https://`).
    Server(String),
}

/// The default node socket path.
pub const NODE_SOCKET: &str = "/run/graft/node.sock";

/// Picks an endpoint: `GRAFT_ENDPOINT`, else the node socket if present,
/// else the configured server.
pub fn discover(_configured: Option<&str>) -> Option<Endpoint> {
    todo!("G1.11")
}
