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
        if self.max_token_ttl_s == 0 {
            return Err(ConfigError("max_token_ttl_s must be positive".into()));
        }
        Ok(())
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
        assert!(ServerConfig::parse("min_free_fraction = 1.0").is_ok());
    }
}
