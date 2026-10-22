//! Node configuration: one TOML file (`docs/design.md` §5.1, §12.2).
//!
//! ```toml
//! socket = "/run/graft/node.sock"
//! socket_mode = "0660"               # owner and group only; never other users
//! cache_dir = "/var/cache/graft"
//! max_bytes = 21474836480            # LRU bound on cached frames (20 GiB)
//! min_free_bytes = 1073741824        # stop filling the cache below 1 GiB free
//! upstream = "http://100.64.0.1:7878"
//! server_public_key = "<64 hex>"     # the server's Ed25519 verifying key
//! upstream_token_file = "/etc/graft/node.token"   # optional service token
//! max_upstream_calls = 16            # upstream calls in flight
//! max_upstream_waiting = 256         # callers queued for one; past it, shed
//! upstream_wait_ms = 2000            # longest queue wait before shedding
//! upstream_connect_timeout_ms = 1000
//! upstream_timeout_ms = 60000        # one upstream call, headers to end
//! max_connections = 1024             # socket connections at once
//! max_streams_per_connection = 64
//! max_inflight_objects = 65536       # distinct objects being fetched
//! max_repos = 4096                   # repositories the cache tracks
//! metrics = "127.0.0.1:9465"         # optional /metrics and /healthz
//! json_logs = true
//! shutdown_grace_s = 10
//! ```
//!
//! Unknown keys are refused, so a typo fails at start-up. So are a socket
//! mode that opens the socket to other users, and zero bounds.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The config file read when `--config` is not given.
pub const DEFAULT_CONFIG: &str = "/etc/graft/node.toml";

/// Node daemon settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NodeConfig {
    /// The Unix socket every session on the host connects to.
    pub socket: PathBuf,
    /// The socket's permission bits, in octal text. Bits for other users are
    /// refused: give session users the socket's group instead.
    pub socket_mode: String,
    /// Cache directory: `objects/` (the frame store) and `repos` (the
    /// repository index).
    pub cache_dir: PathBuf,
    /// Byte bound of the frame cache (LRU).
    pub max_bytes: u64,
    /// Below this many free bytes on the cache volume, fetched objects are
    /// served but not cached.
    pub min_free_bytes: u64,
    /// The upstream server, `http://host:port`.
    pub upstream: String,
    /// The server's token verifying key, 64 hex characters.
    pub server_public_key: String,
    /// A service token (`objects:read`) for object fetches. Without one, a
    /// fetch carries the token of the caller that started it.
    pub upstream_token_file: Option<PathBuf>,
    /// Upstream calls in flight at once.
    pub max_upstream_calls: usize,
    /// Callers that may queue for an upstream call; past it they are shed.
    pub max_upstream_waiting: usize,
    /// The longest a caller queues for an upstream call, ms.
    pub upstream_wait_ms: u64,
    /// How long to wait for an upstream connection, ms.
    pub upstream_connect_timeout_ms: u64,
    /// The longest one upstream call may take, ms.
    pub upstream_timeout_ms: u64,
    /// Socket connections at once; more are closed on accept.
    pub max_connections: usize,
    /// Concurrent HTTP/2 streams per connection.
    pub max_streams_per_connection: u32,
    /// Distinct objects being fetched at once; past it, misses are shed.
    pub max_inflight_objects: usize,
    /// Repositories the cache tracks.
    pub max_repos: u32,
    /// Address for `/metrics` and `/healthz`, if any.
    pub metrics: Option<String>,
    /// JSON logs (for Loki) rather than plain text.
    pub json_logs: bool,
    /// OTLP/HTTP traces endpoint, if any.
    pub otlp_endpoint: Option<String>,
    /// How long a graceful shutdown waits for in-flight calls, seconds.
    pub shutdown_grace_s: u64,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            socket: "/run/graft/node.sock".into(),
            socket_mode: "0660".into(),
            cache_dir: "/var/cache/graft".into(),
            max_bytes: 20 << 30,
            min_free_bytes: 1 << 30,
            upstream: String::new(),
            server_public_key: String::new(),
            upstream_token_file: None,
            max_upstream_calls: 16,
            max_upstream_waiting: 256,
            upstream_wait_ms: 2_000,
            upstream_connect_timeout_ms: 1_000,
            upstream_timeout_ms: 60_000,
            max_connections: 1024,
            max_streams_per_connection: 64,
            max_inflight_objects: 65_536,
            max_repos: 4096,
            metrics: None,
            json_logs: true,
            otlp_endpoint: None,
            shutdown_grace_s: 10,
        }
    }
}

/// Why the configuration could not be loaded.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl NodeConfig {
    /// Reads and checks the config file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("read {}: {e}", path.display())))?;
        Self::parse(&text).map_err(|e| ConfigError(format!("{}: {}", path.display(), e.0)))
    }

    /// Parses and checks config text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|e| ConfigError(e.message().to_owned()))?;
        config.check()?;
        Ok(config)
    }

    /// Checks the bounds and the values that must be set.
    pub fn check(&self) -> Result<(), ConfigError> {
        if self.upstream.is_empty() {
            return Err(ConfigError("upstream is required".into()));
        }
        self.public_key()?;
        self.mode()?;
        let positive = [
            ("max_bytes", self.max_bytes),
            ("max_upstream_calls", self.max_upstream_calls as u64),
            ("upstream_wait_ms", self.upstream_wait_ms),
            (
                "upstream_connect_timeout_ms",
                self.upstream_connect_timeout_ms,
            ),
            ("upstream_timeout_ms", self.upstream_timeout_ms),
            ("max_connections", self.max_connections as u64),
            (
                "max_streams_per_connection",
                self.max_streams_per_connection.into(),
            ),
            ("max_inflight_objects", self.max_inflight_objects as u64),
            ("max_repos", self.max_repos.into()),
        ];
        if let Some((key, _)) = positive.iter().find(|(_, v)| *v == 0) {
            return Err(ConfigError(format!("{key} must be positive")));
        }
        Ok(())
    }

    /// The socket mode bits.
    pub fn mode(&self) -> Result<u32, ConfigError> {
        let mode = u32::from_str_radix(self.socket_mode.trim_start_matches("0o"), 8)
            .map_err(|_| ConfigError("socket_mode must be octal, like \"0660\"".into()))?;
        if mode > 0o777 {
            return Err(ConfigError("socket_mode must be at most 0777".into()));
        }
        if mode & 0o007 != 0 {
            return Err(ConfigError(
                "socket_mode must not open the socket to other users; use its group".into(),
            ));
        }
        if mode & 0o600 != 0o600 {
            return Err(ConfigError(
                "socket_mode must let the owner read and write".into(),
            ));
        }
        Ok(mode)
    }

    /// The server's verifying key bytes.
    pub fn public_key(&self) -> Result<[u8; 32], ConfigError> {
        let hex = self.server_public_key.trim();
        let bad = || ConfigError("server_public_key must be 64 hex characters".into());
        if hex.len() != 64 || !hex.is_ascii() {
            return Err(bad());
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn base() -> String {
        format!("upstream = \"http://127.0.0.1:1\"\nserver_public_key = \"{KEY}\"\n")
    }

    #[test]
    fn a_minimal_config_parses_with_defaults() {
        let c = NodeConfig::parse(&base()).expect("parses");
        assert_eq!(c.mode().unwrap(), 0o660);
        assert_eq!(c.max_bytes, 20 << 30);
    }

    #[test]
    fn unknown_keys_missing_values_and_open_modes_are_refused() {
        assert!(NodeConfig::parse(&format!("{}typo = 1\n", base())).is_err());
        assert!(NodeConfig::parse(&format!("server_public_key = \"{KEY}\"\n")).is_err());
        assert!(
            NodeConfig::parse("upstream = \"http://x\"\nserver_public_key = \"zz\"\n").is_err()
        );
        for mode in ["0666", "0661", "0400", "9", "01777"] {
            let text = format!("{}socket_mode = \"{mode}\"\n", base());
            assert!(NodeConfig::parse(&text).is_err(), "{mode} accepted");
        }
        assert!(NodeConfig::parse(&format!("{}socket_mode = \"0600\"\n", base())).is_ok());
        assert!(NodeConfig::parse(&format!("{}max_upstream_calls = 0\n", base())).is_err());
    }
}
