//! Server configuration (TOML, work item G1.12).

use std::path::PathBuf;

/// Server configuration.
#[derive(Clone, Debug)]
pub(crate) struct ServerConfig {
    /// gRPC listen address (tailnet only by default).
    pub(crate) listen: String,
    /// Object store root.
    pub(crate) objects_dir: PathBuf,
    /// SQLite database file.
    pub(crate) meta_db: PathBuf,
    /// Refuse writes below this free-space fraction.
    pub(crate) min_free_fraction: f64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7878".into(),
            objects_dir: "/var/lib/graft/objects".into(),
            meta_db: "/var/lib/graft/meta.db".into(),
            min_free_fraction: 0.05,
        }
    }
}
