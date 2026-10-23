//! Scopes and their text forms (`docs/design.md` §12.1), and the check that a
//! principal holds one.

use std::fmt;
use std::str::FromStr;

use graft_model::RepoPath;
use graft_proto::Principal;

use crate::AuthError;

/// A scope, in the text form `repo:<name>:<perm>[:<arg>]` or `objects:read`.
/// `<perm>` is `read`, `write`, `land`, `attest`, `admin` or `drafts`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Read a repository: `repo:<name>:read`.
    Read {
        /// Repository.
        repo: String,
    },
    /// Write paths, optionally under a prefix: `repo:<name>:write[:<prefix>]`.
    Write {
        /// Repository.
        repo: String,
        /// Prefix restriction.
        prefix: Option<RepoPath>,
    },
    /// Land on a line: `repo:<name>:land:<line>`.
    Land {
        /// Repository.
        repo: String,
        /// Line.
        line: String,
    },
    /// Post attestations (`Attest`) and nothing else: `repo:<name>:attest`.
    /// It does not grant `read`, and `write` does not grant it.
    Attest {
        /// Repository.
        repo: String,
    },
    /// See other agents' open drafts by path (`OverlappingDrafts`):
    /// `repo:<name>:drafts`. Only `admin` grants it, and it grants nothing
    /// else (`docs/notes/draft-overlap.md` §2).
    Drafts {
        /// Repository.
        repo: String,
    },
    /// Administer a repository: `repo:<name>:admin`.
    Admin {
        /// Repository.
        repo: String,
    },
    /// Read objects for any repository the caller can read (node daemon):
    /// `objects:read`.
    ObjectsRead,
}

impl Scope {
    /// Parses the text form.
    pub fn parse(s: &str) -> Result<Self, AuthError> {
        if s == "objects:read" {
            return Ok(Self::ObjectsRead);
        }
        let rest = s.strip_prefix("repo:").ok_or(AuthError::Malformed)?;
        let (repo, rest) = rest.split_once(':').ok_or(AuthError::Malformed)?;
        if !is_name(repo) {
            return Err(AuthError::Malformed);
        }
        let repo = repo.to_owned();
        let (perm, arg) = match rest.split_once(':') {
            Some((perm, arg)) => (perm, Some(arg)),
            None => (rest, None),
        };
        match (perm, arg) {
            ("read", None) => Ok(Self::Read { repo }),
            ("write", None) => Ok(Self::Write { repo, prefix: None }),
            ("write", Some(prefix)) => {
                let prefix = RepoPath::new(prefix).map_err(|_| AuthError::Malformed)?;
                Ok(Self::Write {
                    repo,
                    prefix: Some(prefix),
                })
            }
            ("land", Some(line)) if is_name(line) => Ok(Self::Land {
                repo,
                line: line.to_owned(),
            }),
            ("attest", None) => Ok(Self::Attest { repo }),
            ("admin", None) => Ok(Self::Admin { repo }),
            ("drafts", None) => Ok(Self::Drafts { repo }),
            _ => Err(AuthError::Malformed),
        }
    }

    /// The repository the scope applies to, if it names one.
    pub fn repo(&self) -> Option<&str> {
        match self {
            Self::Read { repo }
            | Self::Write { repo, .. }
            | Self::Land { repo, .. }
            | Self::Attest { repo }
            | Self::Drafts { repo }
            | Self::Admin { repo } => Some(repo),
            Self::ObjectsRead => None,
        }
    }

    /// Whether holding `self` grants `needed`, for an operation on `path`
    /// (`None` for an operation that is not about one path).
    ///
    /// `admin` grants everything on its repository. `write` and `land` grant
    /// `read` on theirs. A `write` with a prefix grants writes to that prefix
    /// and below only, never a write that names no path. `land` is per line.
    /// `attest`, `drafts` and `objects:read` are granted only by themselves
    /// (and `attest` and `drafts` by `admin`).
    fn grants(&self, needed: &Scope, path: Option<&RepoPath>) -> bool {
        if let Self::Admin { repo } = self {
            return needed.repo() == Some(repo.as_str());
        }
        match (self, needed) {
            (Self::ObjectsRead, Self::ObjectsRead) => true,
            (
                Self::Read { repo } | Self::Write { repo, .. } | Self::Land { repo, .. },
                Self::Read { repo: want },
            ) => repo == want,
            (Self::Write { repo, prefix }, Self::Write { repo: want, .. }) => {
                repo == want
                    && match (prefix, path) {
                        (None, _) => true,
                        (Some(prefix), Some(path)) => path.starts_with(prefix),
                        (Some(_), None) => false,
                    }
            }
            (
                Self::Land { repo, line },
                Self::Land {
                    repo: want,
                    line: want_line,
                },
            ) => repo == want && line == want_line,
            (Self::Attest { repo }, Self::Attest { repo: want })
            | (Self::Drafts { repo }, Self::Drafts { repo: want }) => repo == want,
            _ => false,
        }
    }
}

impl FromStr for Scope {
    type Err = AuthError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { repo } => write!(f, "repo:{repo}:read"),
            Self::Write { repo, prefix: None } => write!(f, "repo:{repo}:write"),
            Self::Write {
                repo,
                prefix: Some(prefix),
            } => write!(f, "repo:{repo}:write:{prefix}"),
            Self::Land { repo, line } => write!(f, "repo:{repo}:land:{line}"),
            Self::Attest { repo } => write!(f, "repo:{repo}:attest"),
            Self::Drafts { repo } => write!(f, "repo:{repo}:drafts"),
            Self::Admin { repo } => write!(f, "repo:{repo}:admin"),
            Self::ObjectsRead => f.write_str("objects:read"),
        }
    }
}

/// A repository or line name inside a scope: non-empty, no `:`, no
/// whitespace or control characters.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| c == ':' || c.is_whitespace() || c.is_control())
}

/// Checks that `principal` holds `needed`, applying path prefixes.
///
/// `path` is the path a write touches; callers check every changed path.
/// Scopes were validated when the token was verified, so a scope string that
/// does not parse here simply grants nothing.
pub fn authorize(
    principal: &Principal,
    needed: &Scope,
    path: Option<&RepoPath>,
) -> Result<(), AuthError> {
    let held = principal
        .scopes
        .iter()
        .filter_map(|s| Scope::parse(s).ok())
        .any(|scope| scope.grants(needed, path));
    if held {
        Ok(())
    } else {
        Err(AuthError::Denied {
            scope: needed.to_string(),
            path: path.map(|p| p.as_str().to_owned()),
        })
    }
}
