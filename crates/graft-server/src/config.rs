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
//! put_wait_ms = 10000                # longest wait for that budget (≤ 1 h)
//! put_idle_ms = 30000                # longest gap between PutObjects messages (≤ 1 h)
//!
//! [ci]                               # CI token exchange: see `ci.rs`
//!
//! [gc]                               # the sweep scheduler (design §8.6)
//! enabled = false                    # off by default
//! interval_s = 3600                  # between passes
//! repos = ["graft"]                  # the repositories it sweeps
//! grace_s = 900                      # doomed-to-deleted grace
//! retention_s = 604800               # abandoned and superseded drafts kept
//! verify = true                      # check the plan before each pass
//!
//! [[lines]]                          # per-line land policy: see `lines.rs`
//! name = "trial-1"
//! policy = "attested"
//! checks = ["ci/fmt", "ci/clippy", "ci/test"]
//! ```
//!
//! Unknown keys are refused, so a typo fails at start-up rather than being
//! silently ignored. So are values out of bounds: `max_put_call_bytes` above
//! 80 MiB (the largest `PutObjects` message) and the put timeouts above one
//! hour, which would otherwise mean no timeout at all.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::grpc::MAX_MESSAGE;

/// The config file `serve` and `admin` read when `--config` is not given.
pub(crate) const DEFAULT_CONFIG: &str = "/etc/graft/server.toml";

/// The longest `put_wait_ms` or `put_idle_ms` a config may name: one hour.
/// Past it a put is in effect unbounded, so a larger value is refused.
pub(crate) const MAX_PUT_TIMEOUT_MS: u64 = 60 * 60 * 1000;

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
    /// CI token exchange: trusted issuers and repository bindings.
    pub(crate) ci: crate::ci::CiConfig,
    /// Per-line land policy; absent means the engine defaults (every line,
    /// `main` included, is `direct`: decision D22). An entry opts its line
    /// into `attested`, `queued` or `post_land` (phase 2; D6 for `main` is
    /// one). Global to the server, not per repository.
    pub(crate) lines: Vec<crate::lines::LineSpec>,
    /// The policy of lines `lines` does not name.
    pub(crate) default_line: Option<crate::lines::LineSpec>,
    /// The GC sweep scheduler.
    pub(crate) gc: GcSection,
}

/// `[gc]`: the sweep scheduler. It is off by default, and even when it is
/// on a sweep deletes nothing in a repository until the operator turns
/// deletion on there (`admin gc deletion <repo> on`); until then each pass
/// is refused and counted as `refused`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct GcSection {
    /// Whether `serve` runs the scheduler.
    pub(crate) enabled: bool,
    /// Seconds between passes.
    pub(crate) interval_s: u64,
    /// The repositories it sweeps.
    pub(crate) repos: Vec<String>,
    /// Doomed-to-deleted grace, seconds; longer than any request deadline.
    pub(crate) grace_s: u64,
    /// How long abandoned and superseded drafts are kept, seconds.
    pub(crate) retention_s: u64,
    /// Check the plan against the ancestry walk before each pass, and
    /// sweep nothing on a mismatch.
    pub(crate) verify: bool,
}

impl Default for GcSection {
    fn default() -> Self {
        let d = graft_gc::GcConfig::default();
        Self {
            enabled: false,
            interval_s: 3600,
            repos: Vec::new(),
            grace_s: d.delete_grace.as_secs(),
            retention_s: d.draft_retention.as_secs(),
            verify: true,
        }
    }
}

/// The shortest `grace_s`: a minute, which is still longer than the
/// engine's request deadlines. A shorter grace could delete an object a
/// write is about to reference.
pub(crate) const MIN_GC_GRACE_S: u64 = 60;

impl GcSection {
    /// The collector's timings: the defaults with this section applied.
    pub(crate) fn gc_config(&self) -> graft_gc::GcConfig {
        graft_gc::GcConfig {
            delete_grace: std::time::Duration::from_secs(self.grace_s),
            draft_retention: std::time::Duration::from_secs(self.retention_s),
            ..graft_gc::GcConfig::default()
        }
    }

    fn check(&self) -> Result<(), ConfigError> {
        if self.interval_s == 0 {
            return Err(ConfigError("gc.interval_s must be positive".into()));
        }
        if self.grace_s < MIN_GC_GRACE_S {
            return Err(ConfigError(format!(
                "gc.grace_s must be at least {MIN_GC_GRACE_S}"
            )));
        }
        if self.enabled && self.repos.is_empty() {
            return Err(ConfigError(
                "gc.enabled needs at least one repository in gc.repos".into(),
            ));
        }
        Ok(())
    }
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
            ci: crate::ci::CiConfig::default(),
            lines: Vec::new(),
            default_line: None,
            gc: GcSection::default(),
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
        // The decode limit of `PutObjects` clamps at MAX_MESSAGE, so a larger
        // per-call cap would not be honoured for a single message: refuse it.
        if self.max_put_call_bytes > MAX_MESSAGE as u64 {
            return Err(ConfigError(format!(
                "max_put_call_bytes must be at most {MAX_MESSAGE} (80 MiB, the largest PutObjects message)"
            )));
        }
        for (key, value) in [
            ("put_wait_ms", self.put_wait_ms),
            ("put_idle_ms", self.put_idle_ms),
        ] {
            if value > MAX_PUT_TIMEOUT_MS {
                return Err(ConfigError(format!(
                    "{key} must be at most {MAX_PUT_TIMEOUT_MS} (one hour)"
                )));
            }
        }
        crate::lines::check(&self.lines, self.default_line.as_ref()).map_err(ConfigError)?;
        self.gc.check()?;
        self.ci.check().map_err(ConfigError)
    }

    /// The engine configuration: the defaults with the line sections applied.
    pub(crate) fn engine_config(&self) -> graft_engine::EngineConfig {
        crate::lines::engine_config(&self.lines, self.default_line.as_ref())
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

    #[test]
    fn the_gc_scheduler_is_off_by_default() {
        let c = ServerConfig::parse("").expect("parses");
        assert!(!c.gc.enabled);
        let g = c.gc.gc_config();
        assert_eq!(g.delete_grace, graft_gc::GcConfig::default().delete_grace);
        assert_eq!(
            g.draft_retention,
            graft_gc::GcConfig::default().draft_retention
        );
        let c = ServerConfig::parse(
            "[gc]\nenabled = true\nrepos = [\"r\"]\ninterval_s = 60\ngrace_s = 120\nretention_s = 3600\nverify = false\n",
        )
        .expect("parses");
        assert!(c.gc.enabled && !c.gc.verify);
        assert_eq!(c.gc.repos, ["r"]);
        assert_eq!(c.gc.gc_config().delete_grace.as_secs(), 120);
        assert_eq!(c.gc.gc_config().draft_retention.as_secs(), 3600);
    }

    #[test]
    fn bad_gc_sections_are_refused() {
        assert!(ServerConfig::parse("[gc]\ninterval_s = 0").is_err());
        assert!(ServerConfig::parse("[gc]\ngrace_s = 59").is_err());
        assert!(
            ServerConfig::parse("[gc]\nenabled = true").is_err(),
            "no repos"
        );
        assert!(ServerConfig::parse("[gc]\nenabld = true").is_err(), "typo");
    }

    #[test]
    fn put_timeouts_are_bounded_at_one_hour() {
        let at = |key: &str, v: u64| ServerConfig::parse(&format!("{key} = {v}"));
        // The maximum is accepted; one more is refused, and so is u64::MAX.
        for key in ["put_wait_ms", "put_idle_ms"] {
            assert!(at(key, MAX_PUT_TIMEOUT_MS).is_ok(), "{key} at the maximum");
            assert!(at(key, MAX_PUT_TIMEOUT_MS + 1).is_err(), "{key} past it");
            assert!(at(key, i64::MAX as u64).is_err(), "{key} at i64::MAX");
            assert!(at(key, 0).is_err(), "{key} at zero");
        }
        // A value past i64::MAX is not even an integer the TOML reader takes.
        let huge = format!("put_wait_ms = {}", u64::MAX);
        assert!(ServerConfig::parse(&huge).is_err());
    }

    #[test]
    fn max_put_call_bytes_is_bounded_by_the_decode_clamp() {
        let cap = |v: u64| {
            ServerConfig::parse(&format!("max_put_call_bytes = {v}\nput_buffer_bytes = {v}"))
        };
        let clamp = MAX_MESSAGE as u64;
        assert!(cap(clamp).is_ok(), "exactly the clamp is honoured");
        assert!(cap(clamp + 1).is_err(), "one past the clamp is refused");
        // The largest integer TOML takes; the clamp check is what refuses it.
        assert!(cap(i64::MAX as u64).is_err(), "i64::MAX is refused");
    }

    fn lines_of(text: &str) -> Vec<graft_engine::policy::LineConfig> {
        ServerConfig::parse(text)
            .expect("parses")
            .engine_config()
            .lines
    }

    #[test]
    fn without_line_sections_the_engine_defaults_stand() {
        let c = ServerConfig::parse("").expect("parses");
        let want = graft_engine::EngineConfig::default();
        let got = c.engine_config();
        assert_eq!(got.lines, want.lines);
        assert_eq!(got.default_line, want.default_line);
    }

    #[test]
    fn a_trial_line_maps_into_the_engine_config() {
        use graft_engine::policy::{Authority, LandPolicy};
        let c = ServerConfig::parse(
            r#"
[[lines]]
name = "trial-1"
policy = "attested"
checks = ["ci/fmt", "ci/clippy", "ci/test"]
attest_merged = false
authority = "graft-primary"

[[lines]]
name = "release"
policy = "queued"
checks = ["ci/build"]
authority = "git-primary"
path_rules = [{ prefix = "deploy", checks = ["ci/deploy"], human_review = true }]
post_land = { checks = ["ci/build"], auto_revert = true }

[default_line]
policy = "attested"
checks = ["ci/test"]
"#,
        )
        .expect("parses");
        let e = c.engine_config();
        // Only the configured lines: `main` is not implicit (D22).
        let names: Vec<_> = e.lines.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["trial-1", "release"]);
        let trial = &e.lines[0];
        assert_eq!(
            trial.policy,
            LandPolicy::Attested {
                checks: vec!["ci/fmt".into(), "ci/clippy".into(), "ci/test".into()],
                attest_merged: false
            }
        );
        assert_eq!(trial.authority, Authority::GraftPrimary);
        let release = &e.lines[1];
        assert!(matches!(release.policy, LandPolicy::Queued { .. }));
        assert_eq!(release.authority, Authority::GitPrimary);
        assert_eq!(release.path_rules.len(), 1);
        assert!(release.path_rules[0].human_review);
        assert!(release.post_land.as_ref().unwrap().auto_revert);
        assert!(matches!(e.default_line.policy, LandPolicy::Attested { .. }));
    }

    #[test]
    fn without_line_sections_main_is_direct() {
        // Decision D22: no implicit D6 for `main`; every line lands directly.
        let e = ServerConfig::parse("").expect("parses").engine_config();
        assert!(e.lines.is_empty());
        assert_eq!(
            e.default_line.policy,
            graft_engine::policy::LandPolicy::Direct
        );
        assert!(e.default_line.post_land.is_none());
    }

    #[test]
    fn a_listed_main_opts_into_d6() {
        let lines = lines_of(
            r#"
[[lines]]
name = "main"
policy = "attested"
checks = ["ci/build", "ci/test"]
post_land = { checks = ["ci/build", "ci/test"], auto_revert = true }
"#,
        );
        assert_eq!(lines, [graft_engine::policy::LineConfig::d6("main")]);
    }

    #[test]
    fn a_listed_main_with_no_policy_is_direct() {
        let lines = lines_of("[[lines]]\nname = \"main\"\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].policy, graft_engine::policy::LandPolicy::Direct);
    }

    #[test]
    fn the_deploy_config_parses_and_lands_directly() {
        use graft_engine::policy::{Authority, LandPolicy};
        // deploy/server.toml as the graft-dev VM runs it: direct (D22).
        let c = ServerConfig::parse(include_str!("../../../deploy/server.toml"))
            .expect("deploy/server.toml parses");
        let e = c.engine_config();
        let names: Vec<_> = e.lines.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["main", "trial-1"]);
        for line in &e.lines {
            assert_eq!(line.policy, LandPolicy::Direct, "{}", line.name);
            assert!(line.post_land.is_none(), "{}", line.name);
            assert!(line.path_rules.is_empty(), "{}", line.name);
        }
        assert_eq!(e.lines[0].authority, Authority::GitPrimary);
        assert_eq!(e.lines[1].authority, Authority::GraftPrimary);
        assert_eq!(e.default_line.policy, LandPolicy::Direct);
    }

    #[test]
    fn bad_line_sections_are_refused() {
        let bad = [
            // Unknown keys, at every level.
            "[[lines]]\nname = \"a\"\nchek = []",
            "[[lines]]\nname = \"a\"\npost_land = { checks = [\"c\"], revert = true }",
            "[[lines]]\nname = \"a\"\npath_rules = [{ prefix = \"x\", checks = [\"c\"], nope = 1 }]",
            "[default_line]\nbogus = 1",
            "[[line]]\nname = \"a\"",
            // Unknown values.
            "[[lines]]\nname = \"a\"\npolicy = \"fast\"",
            "[[lines]]\nname = \"a\"\nauthority = \"both\"",
            // Names.
            "[[lines]]\npolicy = \"direct\"",
            "[[lines]]\nname = \"\"",
            "[[lines]]\nname = \"a b\"",
            "[[lines]]\nname = \"a\"\n[[lines]]\nname = \"a\"",
            "[default_line]\nname = \"a\"",
            // Invalid combinations.
            "[[lines]]\nname = \"a\"\npolicy = \"attested\"",
            "[[lines]]\nname = \"a\"\npolicy = \"queued\"",
            "[[lines]]\nname = \"a\"\npolicy = \"direct\"\nchecks = [\"c\"]",
            "[[lines]]\nname = \"a\"\nattest_merged = true",
            "[[lines]]\nname = \"a\"\npolicy = \"queued\"\nchecks = [\"c\"]\nattest_merged = true",
            "[[lines]]\nname = \"a\"\npolicy = \"attested\"\nchecks = [\"c\", \"c\"]",
            "[[lines]]\nname = \"a\"\npolicy = \"attested\"\nchecks = [\"\"]",
            "[[lines]]\nname = \"a\"\npath_rules = [{ prefix = \"x\" }]",
            "[[lines]]\nname = \"a\"\npath_rules = [{ prefix = \"/x\", human_review = true }]",
            "[[lines]]\nname = \"a\"\npost_land = { auto_revert = true }",
            "[default_line]\npolicy = \"attested\"",
        ];
        for text in bad {
            assert!(ServerConfig::parse(text).is_err(), "accepted: {text}");
        }
    }
}
