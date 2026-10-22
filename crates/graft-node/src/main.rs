//! `graft-node`: one cache daemon per host (`docs/design.md` §5.1, work item
//! G2.3). Serves the object API from a bounded disk cache over a Unix socket,
//! single-flights identical fetches, and forwards repository calls upstream.

use std::process::ExitCode;

/// Node daemon settings.
#[derive(Clone, Debug)]
struct NodeConfig {
    /// Socket path.
    socket: String,
    /// Cache directory.
    cache_dir: String,
    /// Cache byte bound.
    max_bytes: u64,
    /// Upstream server.
    upstream: String,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            socket: "/run/graft/node.sock".into(),
            cache_dir: "/var/cache/graft".into(),
            max_bytes: 20 << 30,
            upstream: String::new(),
        }
    }
}

fn main() -> ExitCode {
    let config = NodeConfig::default();
    eprintln!(
        "graft-node: not implemented yet (see docs/plan.md, G2.3); would serve {} from {} ({} bytes) via {:?}",
        config.socket, config.cache_dir, config.max_bytes, config.upstream
    );
    ExitCode::from(2)
}
