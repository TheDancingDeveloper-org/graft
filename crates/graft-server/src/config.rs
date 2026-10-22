//! Server configuration: one TOML file (`docs/design.md` §5.1, §12.2).
//!
//! ```toml
//! listen = "100.64.0.1:7878"         # gRPC, the JSON facade and health
//! metrics = "0.0.0.0:9464"           # optional second listener for scrapes
//! objects_dir = "/var/lib/graft/objects"
//! meta_db = "/var/lib/graft/meta/meta.db"
//! key_file = "/var/lib/graft/meta/signing.key"   # default: beside meta_db
//! min_free_fraction = 0.05           # refuse writes below 5% free
//! min_free_bytes = 0                 # and below this many free bytes
//! json_logs = true
//! max_token_ttl_s = 43200            # MintToken cap (12 h)
//! shutdown_grace_s = 10              # drain time on SIGTERM
//! max_watch_wait_ms = 60000          # longest idle wait of a Watch
//! max_watch_lifetime_s = 3600        # longest Watch stream (and token exp)
//! max_watches_per_principal = 16     # concurrent Watch streams
//! max_watches = 1024
//! max_put_call_bytes = 67108864      # PutObjects buffered per call (64 MiB)
//! put_buffer_bytes = 536870912       # PutObjects buffered by all calls
//! put_wait_ms = 10000                # longest wait for that budget
//! put_idle_ms = 30000                # longest gap between PutObjects messages
//! ```
//!
//! Unknown keys are refused, so a typo fails at start-up rather than being
//! silently ignored.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The config file `serve` and `admin` read when `--config` is not given.
pub(crate) const DEFAULT_CONFIG: &str = "/etc/graft/server.toml";

/// Server configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct ServerConfig {
    /// Listen address for gRPC, the JSON facade, `/metrics` and the health
    /// endpoints. Port 0 picks a free port (tests).
    pub(crate) listen: String,
    /// An optional second listener serving only `/metrics`, `/healthz` and
    /// `/readyz`.
    pub(crate) metrics: Option<String>,
    /// Object store root.
    pub(crate) objects_dir: PathBuf,
    /// SQLite database file.
    pub(crate) meta_db: PathBuf,
    /// The Ed25519 signing key seed (32 bytes, mode 0600). Created on first
    /// use. Defaults to `signing.key` beside `meta_db`.
    pub(crate) key_file: Option<PathBuf>,
    /// Refuse writes when the free fraction of either volume is below this.
    pub(crate) min_free_fraction: f64,
    /// Refuse writes when either volume has fewer free bytes than this.
    pub(crate) min_free_bytes: u64,
    /// JSON logs (for Loki) rather than plain text.
    pub(crate) json_logs: bool,
    /// OTLP/HTTP traces endpoint, if any.
    pub(crate) otlp_endpoint: Option<String>,
    /// The longest TTL `MintToken` grants, seconds.
    pub(crate) max_token_ttl_s: u64,
    /// How long a graceful shutdown waits for in-flight calls, seconds.
    pub(crate) shutdown_grace_s: u64,
    /// The longest idle wait a `Watch` may ask for, ms.
    pub(crate) max_watch_wait_ms: u64,
    /// The longest a `Watch` stream runs, seconds (it also ends at its
    /// token's expiry).
    pub(crate) max_watch_lifetime_s: u64,
    /// Concurrent `Watch` streams per principal.
    pub(crate) max_watches_per_principal: usize,
    /// Concurrent `Watch` streams in total.
    pub(crate) max_watches: usize,
    /// Most frame bytes one `PutObjects` call buffers.
    pub(crate) max_put_call_bytes: u64,
    /// Frame bytes every `PutObjects` call in flight buffers together; past
    /// it calls wait (backpressure).
    pub(crate) put_buffer_bytes: u64,
    /// How long a `PutObjects` message waits for the shared budget before
    /// the call answers `Unavailable`, ms.
    pub(crate) put_wait_ms: u64,
    /// The longest gap between two `PutObjects` messages before the call
    /// answers `Unavailable` and gives its budget back, ms.
    pub(crate) put_idle_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7878".into(),
            metrics: None,
            objects_dir: "/var/lib/graft/objects".into(),
            meta_db: "/var/lib/graft/meta.db".into(),
            key_file: None,
            min_free_fraction: 0.05,
            min_free_bytes: 0,
            json_logs: true,
            otlp_endpoint: None,
            max_token_ttl_s: 12 * 3600,
            shutdown_grace_s: 10,
            max_watch_wait_ms: 60_000,
            max_watch_lifetime_s: 3600,
            max_watches_per_principal: 16,
            max_watches: 1024,
            max_put_call_bytes: 64 << 20,
            put_buffer_bytes: 512 << 20,
            put_wait_ms: 10_000,
            put_idle_ms: 30_000,
        }
    }
}

/// Why the configuration could not be loaded.
#[derive(Debug)]
pub(crate) struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ServerConfig {
    /// Reads and checks the config file at `path`.
    pub(crate) fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("read {}: {e}", path.display())))?;
        Self::parse(&text).map_err(|e| ConfigError(format!("{}: {}", path.display(), e.0)))
    }

    /// Parses and checks config text.
    pub(crate) fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|e| ConfigError(e.message().to_owned()))?;
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> Result<(), ConfigError> {
        if !(0.0..=1.0).contains(&self.min_free_fraction) {
            return Err(ConfigError(
                "min_free_fraction must be between 0 and 1".into(),
            ));
        }
        let positive = [
            ("max_token_ttl_s", self.max_token_ttl_s),
            ("max_watch_lifetime_s", self.max_watch_lifetime_s),
            (
                "max_watches_per_principal",
                self.max_watches_per_principal as u64,
            ),
            ("max_watches", self.max_watches as u64),
            ("max_put_call_bytes", self.max_put_call_bytes),
            ("put_wait_ms", self.put_wait_ms),
            ("put_idle_ms", self.put_idle_ms),
        ];
        if let Some((key, _)) = positive.iter().find(|(_, v)| *v == 0) {
            return Err(ConfigError(format!("{key} must be positive")));
        }
        if self.put_buffer_bytes < self.max_put_call_bytes {
            return Err(ConfigError(
                "put_buffer_bytes must be at least max_put_call_bytes".into(),
            ));
        }
        Ok(())
    }

    /// The `Watch` bounds.
    pub(crate) fn watch_limits(&self) -> crate::watch::WatchLimits {
        crate::watch::WatchLimits {
            max_wait: std::time::Duration::from_millis(self.max_watch_wait_ms),
            max_lifetime: std::time::Duration::from_secs(self.max_watch_lifetime_s),
            per_principal: self.max_watches_per_principal,
            total: self.max_watches,
        }
    }

    /// The signing key file.
    pub(crate) fn key_path(&self) -> PathBuf {
        self.key_file.clone().unwrap_or_else(|| {
            self.meta_db
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("signing.key")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dev_stack_config_parses() {
        // docker/dev/server.toml on the dev-stack branch.
        let c = ServerConfig::parse(
            r#"
listen = "0.0.0.0:7878"
objects_dir = "/var/lib/graft/objects"
meta_db = "/var/lib/graft/meta/meta.db"
min_free_fraction = 0.05
metrics = "0.0.0.0:9464"
"#,
        )
        .expect("parses");
        assert_eq!(c.listen, "0.0.0.0:7878");
        assert_eq!(c.metrics.as_deref(), Some("0.0.0.0:9464"));
        assert_eq!(c.key_path(), Path::new("/var/lib/graft/meta/signing.key"));
        assert_eq!(c.max_token_ttl_s, 12 * 3600);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_refused() {
        assert!(ServerConfig::parse("lisen = \"x\"").is_err());
        assert!(ServerConfig::parse("min_free_fraction = 1.5").is_err());
        assert!(ServerConfig::parse("min_free_fraction = -0.1").is_err());
        assert!(ServerConfig::parse("max_token_ttl_s = 0").is_err());
        assert!(ServerConfig::parse("max_watches = 0").is_err());
        assert!(ServerConfig::parse("max_put_call_bytes = 10\nput_buffer_bytes = 5").is_err());
        assert!(ServerConfig::parse("min_free_fraction = 1.0").is_ok());
    }
}
