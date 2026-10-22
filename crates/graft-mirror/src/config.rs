//! The `follow` configuration file (TOML).
//!
//! ```toml
//! server = "http://127.0.0.1:7420"     # the graft server
//! repo = "rusttorrent"                 # the graft repository
//! upstream = "https://github.com/org/repo.git"
//! mirror_dir = "/var/lib/graft-mirror/rusttorrent.git"
//! db = "/var/lib/graft-mirror/rusttorrent.sqlite"
//! token_file = "/run/secrets/graft-mirror-token"  # or GRAFT_TOKEN
//! poll_interval_secs = 15              # default 15 (decision D9)
//! metrics_addr = "127.0.0.1:9465"      # serves /metrics; optional
//!
//! [[lines]]
//! branch = "main"                      # upstream refs/heads/main
//! line = "main"                        # graft line; default: the branch
//! ```
//!
//! The token is never in the file or in process arguments: it comes from
//! `token_file` or the `GRAFT_TOKEN` environment variable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use graft_git::GitSha;
use serde::Deserialize;

use crate::MirrorError;
use crate::follow::{FollowConfig, FollowLine, POLL_INTERVAL};

/// The parsed file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// The graft server URL.
    pub server: String,
    /// The graft repository.
    pub repo: String,
    /// The upstream git URL or path.
    pub upstream: String,
    /// The local bare mirror.
    pub mirror_dir: PathBuf,
    /// The mirror DB.
    pub db: PathBuf,
    /// A file holding the bearer token.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// Seconds between polls.
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
    /// Where to serve `/metrics`.
    #[serde(default)]
    pub metrics_addr: Option<String>,
    /// Emit JSON logs.
    #[serde(default)]
    pub json_logs: bool,
    /// Changesets per import batch.
    #[serde(default)]
    pub batch: Option<usize>,
    /// Commits allowed to fail the fidelity check (40-hex shas).
    #[serde(default)]
    pub allow_lossy: Vec<String>,
    /// The followed branches.
    pub lines: Vec<LineConfig>,
}

/// One followed branch.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineConfig {
    /// The upstream branch.
    pub branch: String,
    /// The graft line (default: the branch name).
    #[serde(default)]
    pub line: Option<String>,
}

fn bad(what: impl Into<String>) -> MirrorError {
    MirrorError::Config(what.into())
}

/// Parses 40-hex shas.
pub fn parse_shas(shas: &[String]) -> Result<Vec<GitSha>, MirrorError> {
    shas.iter()
        .map(|s| GitSha::from_hex(s).ok_or_else(|| bad(format!("not a commit sha: {s:?}"))))
        .collect()
}

fn check_branch(b: &str) -> Result<(), MirrorError> {
    let ok = !b.is_empty()
        && !b.starts_with('-')
        && !b.contains("..")
        && b.split('/').all(|part| !part.is_empty())
        && b.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if ok {
        Ok(())
    } else {
        Err(bad(format!("not a branch name: {b:?}")))
    }
}

impl FileConfig {
    /// Parses and checks `text`.
    pub fn parse(text: &str) -> Result<FileConfig, MirrorError> {
        let config: FileConfig = toml::from_str(text).map_err(|e| bad(e.message().to_owned()))?;
        if config.upstream.is_empty() || config.upstream.starts_with('-') {
            return Err(bad("upstream must be a URL or a path"));
        }
        if config.lines.is_empty() {
            return Err(bad("no [[lines]] to follow"));
        }
        let mut seen = Vec::new();
        for l in &config.lines {
            check_branch(&l.branch)?;
            let line = l.line.as_deref().unwrap_or(&l.branch);
            if seen.contains(&line) {
                return Err(bad(format!("line {line:?} is followed twice")));
            }
            seen.push(line);
        }
        if config.poll_interval_secs == Some(0) {
            return Err(bad("poll_interval_secs must be at least 1"));
        }
        parse_shas(&config.allow_lossy)?;
        Ok(config)
    }

    /// Reads and checks the file at `path`.
    pub fn load(path: &Path) -> Result<FileConfig, MirrorError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| bad(format!("reading {}: {e}", path.display())))?;
        FileConfig::parse(&text)
    }

    /// The follow settings.
    pub fn follow(&self) -> Result<FollowConfig, MirrorError> {
        Ok(FollowConfig {
            repo: self.repo.clone(),
            lines: self
                .lines
                .iter()
                .map(|l| FollowLine {
                    branch: l.branch.clone(),
                    line: l.line.clone().unwrap_or_else(|| l.branch.clone()),
                })
                .collect(),
            allow_lossy: parse_shas(&self.allow_lossy)?,
            batch: self.batch.unwrap_or(256).max(1),
            interval: self
                .poll_interval_secs
                .map_or(POLL_INTERVAL, Duration::from_secs),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
server = "http://127.0.0.1:7420"
repo = "r"
upstream = "/srv/upstream.git"
mirror_dir = "/var/lib/m.git"
db = "/var/lib/m.sqlite"
"#;

    #[test]
    fn defaults_follow_d9() {
        let c = FileConfig::parse(&format!("{BASE}[[lines]]\nbranch = \"main\"\n")).unwrap();
        let f = c.follow().unwrap();
        assert_eq!(f.interval, Duration::from_secs(15));
        assert_eq!(
            f.lines,
            [FollowLine {
                branch: "main".into(),
                line: "main".into()
            }]
        );
        assert_eq!(f.batch, 256);
    }

    #[test]
    fn refuses_bad_files() {
        assert!(FileConfig::parse(BASE).is_err(), "no lines");
        let line = |b: &str| format!("{BASE}[[lines]]\nbranch = \"{b}\"\n");
        assert!(FileConfig::parse(&line("-x")).is_err());
        assert!(FileConfig::parse(&line("a..b")).is_err());
        assert!(FileConfig::parse(&line("a b")).is_err());
        let twice = format!("{}{}", line("main"), "[[lines]]\nbranch = \"main\"\n");
        assert!(FileConfig::parse(&twice).is_err());
        let unknown = format!("{BASE}token = \"secret\"\n[[lines]]\nbranch = \"main\"\n");
        assert!(FileConfig::parse(&unknown).is_err(), "no inline token");
    }
}
