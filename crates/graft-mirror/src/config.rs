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
//! tags = "*"                           # upstream tags to follow; default
//!                                      # "*" (all), "" turns tags off
//!
//! [[lines]]
//! branch = "main"                      # upstream refs/heads/main
//! line = "main"                        # graft line; default: the branch
//!
//! [[check_refs]]                       # optional; follow keeps per-land
//! line = "backlog-1"                   # check refs for a post-land line
//! prefix = "graft/check/backlog-1/"    # default graft/check/<line>/
//! ```
//!
//! The token is never in the file or in process arguments: it comes from
//! `token_file` or the `GRAFT_TOKEN` environment variable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use graft_git::GitSha;
use serde::Deserialize;

use crate::MirrorError;
use crate::check_refs::{CheckRefLine, CheckRefsConfig, REQUIRED_PREFIX, default_prefix};
use crate::credentials::StaticToken;
use crate::export::ExportConfig;
use crate::follow::{FollowConfig, FollowLine, POLL_INTERVAL};
use crate::git::Mirror;

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
    /// The upstream tags follow imports, as a pattern under `refs/tags/`.
    /// Default `*`; empty: none.
    #[serde(default)]
    pub tags: Option<String>,
    /// The followed branches.
    pub lines: Vec<LineConfig>,
    /// The GitHub repository and credential for batch PRs, PR import and the
    /// authority switch. Optional: without it the mirror follows and imports
    /// only.
    #[serde(default)]
    pub github: Option<GithubFile>,
    /// Graft-primary export settings (`graft-mirror export --config`).
    #[serde(default)]
    pub export: Option<ExportFile>,
    /// Post-land checked lines that `follow` keeps per-land check refs for.
    #[serde(default)]
    pub check_refs: Vec<CheckRefsFile>,
}

/// One `[[check_refs]]` line.
///
/// ```toml
/// [[check_refs]]
/// line = "backlog-1"
/// prefix = "graft/check/backlog-1/"   # default graft/check/<line>/
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRefsFile {
    /// The graft line; its server config must be `post-land`.
    pub line: String,
    /// The upstream branch prefix, under `graft/` and ending in `/`.
    #[serde(default)]
    pub prefix: Option<String>,
}

/// Graft-primary export settings.
///
/// ```toml
/// [export]
/// import_pulls = true   # import GitHub PRs as drafts; needs [github]
///
/// # principal (authenticated token subject) -> git identity
/// [export.authors]
/// ada = { name = "Ada Lovelace", email = "ada@example.com" }
///
/// # used when the principal has no mapping above
/// default_author = "Graft Export <export@example.com>"
/// ```
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportFile {
    /// Import open GitHub PRs as drafts in `github-pr/<n>` workspaces.
    #[serde(default)]
    pub import_pulls: bool,
    /// Git identities for authenticated principals (`RefMoved.actor`). The
    /// bot committer is used only when a principal is absent here and
    /// [`default_author`](Self::default_author) is unset.
    #[serde(default)]
    pub authors: BTreeMap<String, AuthorFile>,
    /// `Name <email>` used when the authenticated principal has no mapping.
    #[serde(default)]
    pub default_author: Option<String>,
}

/// A git identity in `[export.authors]`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorFile {
    /// Display name.
    pub name: String,
    /// Email. Never a secret: it is written on the exported commit.
    pub email: String,
}

/// The mirror's GitHub repository and how it authenticates.
///
/// ```toml
/// [github]
/// owner = "indexarr"
/// repo = "rusttorrent"
/// # exactly one of these two (decision D8; there is no default):
/// token_file = "/run/secrets/github-token"
/// [github.github_app]
/// app_id = 12345
/// installation_id = 67890
/// private_key_file = "/run/secrets/github-app.pem"
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubFile {
    /// The API root (default `https://api.github.com`).
    #[serde(default)]
    pub api: Option<String>,
    /// The owner (user or organisation).
    pub owner: String,
    /// The repository name.
    pub repo: String,
    /// A file holding a token to use as is.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// A GitHub App to mint installation tokens from.
    #[serde(default)]
    pub github_app: Option<AppFile>,
}

/// A GitHub App's identity and key.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppFile {
    /// The App's id.
    pub app_id: u64,
    /// The installation of the App on the repository's owner.
    pub installation_id: u64,
    /// The PEM private key file.
    pub private_key_file: PathBuf,
}

/// The one credential the config chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthChoice {
    /// A token file.
    TokenFile(PathBuf),
    /// A GitHub App (its key file is checked when it is loaded).
    App {
        /// The App id.
        app_id: u64,
        /// The installation id.
        installation_id: u64,
        /// The key file.
        private_key_file: PathBuf,
    },
}

impl GithubFile {
    /// The credential the file chose. Exactly one of `token_file` and
    /// `github_app` must be set; neither or both is a config error.
    pub fn auth(&self) -> Result<AuthChoice, MirrorError> {
        match (&self.token_file, &self.github_app) {
            (Some(path), None) => Ok(AuthChoice::TokenFile(path.clone())),
            (None, Some(app)) => Ok(AuthChoice::App {
                app_id: app.app_id,
                installation_id: app.installation_id,
                private_key_file: app.private_key_file.clone(),
            }),
            (None, None) => Err(bad(
                "[github] needs a credential: set token_file or github_app (there is no default)",
            )),
            (Some(_), Some(_)) => Err(bad(
                "[github] must choose one credential, not both token_file and github_app",
            )),
        }
    }
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
        if let Some(github) = &config.github {
            github.auth()?;
            if github.owner.is_empty() || github.repo.is_empty() {
                return Err(bad("[github] needs an owner and a repo"));
            }
        }
        if config.export.as_ref().is_some_and(|e| e.import_pulls) && config.github.is_none() {
            return Err(bad("[export] import_pulls needs a [github] section"));
        }
        let mut checked = Vec::new();
        for c in config.check_refs() {
            check_branch(&c.line)?;
            if !c.prefix.starts_with(REQUIRED_PREFIX) || !c.prefix.ends_with('/') {
                return Err(bad(format!(
                    "[[check_refs]] prefix {:?} must start with {REQUIRED_PREFIX:?} and end in \"/\"",
                    c.prefix
                )));
            }
            check_branch(&c.prefix[..c.prefix.len() - 1])?;
            if checked.contains(&c.line) {
                return Err(bad(format!(
                    "[[check_refs]] line {:?} is listed twice",
                    c.line
                )));
            }
            checked.push(c.line);
        }
        Ok(config)
    }

    /// Reads and checks the file at `path`.
    pub fn load(path: &Path) -> Result<FileConfig, MirrorError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| bad(format!("reading {}: {e}", path.display())))?;
        FileConfig::parse(&text)
    }

    /// The followed (or exported) lines: each upstream branch and its graft
    /// line.
    pub fn lines(&self) -> Vec<FollowLine> {
        self.lines
            .iter()
            .map(|l| FollowLine {
                branch: l.branch.clone(),
                line: l.line.clone().unwrap_or_else(|| l.branch.clone()),
            })
            .collect()
    }

    /// The `[export] default_author` identity, when set.
    pub fn default_author(&self) -> Result<Option<crate::batch::AuthorIdentity>, MirrorError> {
        let Some(raw) = self
            .export
            .as_ref()
            .and_then(|e| e.default_author.as_deref())
        else {
            return Ok(None);
        };
        parse_author_identity(raw).map(Some)
    }

    /// Git identities from `[export.authors]`, keyed by principal.
    pub fn author_map(&self) -> BTreeMap<String, crate::batch::AuthorIdentity> {
        let Some(export) = &self.export else {
            return BTreeMap::new();
        };
        export
            .authors
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    crate::batch::AuthorIdentity {
                        name: v.name.clone(),
                        email: v.email.clone(),
                    },
                )
            })
            .collect()
    }

    /// The local mirror of upstream. With `[github] token_file` set, that
    /// token goes with every git network command the mirror runs (follow's
    /// polls, export, pr-batch), so each works however it is launched
    /// (WI-2571). A token file that cannot be read or is empty logs one
    /// warning and the mirror reaches upstream without a credential.
    pub fn mirror(&self) -> Mirror {
        let credential = self
            .github
            .as_ref()
            .and_then(|gh| gh.token_file.as_deref())
            .and_then(|path| match StaticToken::load(path) {
                Ok(token) => Some(token.into_secret()),
                Err(e) => {
                    tracing::warn!(error = %e, "git reaches upstream without a credential");
                    None
                }
            });
        Mirror::new(&self.mirror_dir, &self.upstream).with_credential(credential)
    }

    /// The `[[check_refs]]` lines, each with its prefix.
    pub fn check_refs(&self) -> Vec<CheckRefLine> {
        self.check_refs
            .iter()
            .map(|c| CheckRefLine {
                line: c.line.clone(),
                prefix: c.prefix.clone().unwrap_or_else(|| default_prefix(&c.line)),
            })
            .collect()
    }

    /// The check refs settings.
    pub fn check_refs_config(&self) -> CheckRefsConfig {
        let mut config = CheckRefsConfig::new(self.repo.clone(), self.check_refs());
        if let Some(secs) = self.poll_interval_secs {
            config.interval = Duration::from_secs(secs);
        }
        config
    }

    /// The export settings.
    pub fn export(&self) -> ExportConfig {
        let mut config = ExportConfig::new(self.repo.clone(), self.lines());
        if let Some(secs) = self.poll_interval_secs {
            config.interval = Duration::from_secs(secs);
        }
        config
    }

    /// The follow settings.
    pub fn follow(&self) -> Result<FollowConfig, MirrorError> {
        Ok(FollowConfig {
            repo: self.repo.clone(),
            lines: self.lines(),
            allow_lossy: parse_shas(&self.allow_lossy)?,
            batch: self.batch.unwrap_or(256).max(1),
            interval: self
                .poll_interval_secs
                .map_or(POLL_INTERVAL, Duration::from_secs),
            tags: match self.tags.as_deref() {
                None => Some("*".to_owned()),
                Some("") => None,
                Some(p) => Some(p.to_owned()),
            },
        })
    }
}

/// `Name <email>` as configured for `[export] default_author`.
fn parse_author_identity(raw: &str) -> Result<crate::batch::AuthorIdentity, MirrorError> {
    let raw = raw.trim();
    let Some(open) = raw.rfind('<') else {
        return Err(bad("[export] default_author must be \"Name <email>\""));
    };
    let Some(close) = raw.rfind('>') else {
        return Err(bad("[export] default_author must be \"Name <email>\""));
    };
    if close != raw.len() - 1 || close <= open {
        return Err(bad("[export] default_author must be \"Name <email>\""));
    }
    let name = raw[..open].trim();
    let email = raw[open + 1..close].trim();
    if name.is_empty() || email.is_empty() || email.contains('<') || email.contains('>') {
        return Err(bad("[export] default_author must be \"Name <email>\""));
    }
    Ok(crate::batch::AuthorIdentity {
        name: name.to_owned(),
        email: email.to_owned(),
    })
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
    fn default_author_is_name_and_email() {
        let ok = format!(
            "{BASE}[export]\ndefault_author = \"Export Bot <bot@example.com>\"\n[[lines]]\nbranch = \"main\"\n"
        );
        let c = FileConfig::parse(&ok).unwrap();
        let id = c.default_author().unwrap().unwrap();
        assert_eq!(id.name, "Export Bot");
        assert_eq!(id.email, "bot@example.com");
        let bad = format!(
            "{BASE}[export]\ndefault_author = \"no-angles\"\n[[lines]]\nbranch = \"main\"\n"
        );
        assert!(FileConfig::parse(&bad).unwrap().default_author().is_err());
    }

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
    fn the_github_credential_has_no_default() {
        let gh = |extra: &str| {
            format!(
                "{BASE}[[lines]]\nbranch = \"main\"\n[github]\nowner = \"o\"\nrepo = \"r\"\n{extra}"
            )
        };
        assert!(FileConfig::parse(&gh("")).is_err(), "no credential chosen");
        assert!(
            FileConfig::parse(&gh("token_file = \"/t\"\n[github.github_app]\napp_id = 1\ninstallation_id = 2\nprivate_key_file = \"/k\"\n")).is_err(),
            "both chosen"
        );
        let token = FileConfig::parse(&gh("token_file = \"/t\"\n")).unwrap();
        assert_eq!(
            token.github.unwrap().auth().unwrap(),
            AuthChoice::TokenFile("/t".into())
        );
        let app = FileConfig::parse(&gh(
            "[github.github_app]\napp_id = 1\ninstallation_id = 2\nprivate_key_file = \"/k\"\n",
        ))
        .unwrap();
        assert_eq!(
            app.github.unwrap().auth().unwrap(),
            AuthChoice::App {
                app_id: 1,
                installation_id: 2,
                private_key_file: "/k".into()
            }
        );
    }

    #[test]
    fn check_refs_default_their_prefix_and_refuse_bad_ones() {
        let lines = format!("{BASE}[[lines]]\nbranch = \"main\"\n");
        let c = FileConfig::parse(&format!(
            "{lines}[[check_refs]]\nline = \"backlog-1\"\n\
             [[check_refs]]\nline = \"b2\"\nprefix = \"graft/c/b2/\"\n"
        ))
        .unwrap();
        assert_eq!(
            c.check_refs(),
            [
                CheckRefLine {
                    line: "backlog-1".into(),
                    prefix: "graft/check/backlog-1/".into()
                },
                CheckRefLine {
                    line: "b2".into(),
                    prefix: "graft/c/b2/".into()
                }
            ]
        );
        assert!(FileConfig::parse(&lines).unwrap().check_refs().is_empty());
        let one = |extra: &str| format!("{lines}[[check_refs]]\nline = \"l\"\n{extra}");
        for bad_prefix in ["check/l/", "graft/check/l", "graft/a..b/", "graft//"] {
            assert!(
                FileConfig::parse(&one(&format!("prefix = \"{bad_prefix}\"\n"))).is_err(),
                "{bad_prefix}"
            );
        }
        let twice = format!("{}[[check_refs]]\nline = \"l\"\n", one(""));
        assert!(FileConfig::parse(&twice).is_err(), "a line listed twice");
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
        let pulls = format!("{}[export]\nimport_pulls = true\n", line("main"));
        assert!(
            FileConfig::parse(&pulls).is_err(),
            "PR import without [github]"
        );
        let unknown = format!("{BASE}token = \"secret\"\n[[lines]]\nbranch = \"main\"\n");
        assert!(FileConfig::parse(&unknown).is_err(), "no inline token");
    }
}
