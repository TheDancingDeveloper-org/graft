//! Scopes and their text forms (`docs/design.md` §12.1), and the check that a
//! principal holds one.

use std::fmt;
use std::str::FromStr;

use graft_model::RepoPath;
use graft_proto::Principal;

use crate::AuthError;

/// A scope, in the text form `repo:<name>:<perm>[:<arg>]`, `objects:read`,
/// `repos:admin`, `mirror:bind` or `mirror:export`. `<perm>` is `read`,
/// `write`, `land`, `attest`, `admin`, `drafts` or (only as
/// `repo:*:mirror`) `mirror`.
///
/// `<name>` may be `*` ([`ANY_REPO`]): the scope then holds on every
/// repository, now and later. A scope that is needed always names one
/// repository.
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
    /// Create, delete and list repositories: `repos:admin`. The holder
    /// administers a repository it creates (D41).
    ReposAdmin,
    /// Bind a repository to an upstream: `mirror:bind`. Operator-only, and
    /// implied by no other scope (D41).
    MirrorBind,
    /// Turn on export to an upstream: `mirror:export`. Operator-only, and
    /// implied by no other scope (D41).
    MirrorExport,
    /// The mirror permission set: held as `repo:*:mirror`, only by the
    /// `mirror` token kind (D41). `admin` does not grant it.
    Mirror {
        /// Repository (`*` when held).
        repo: String,
    },
}

/// The repository name that matches every repository in a held scope.
pub const ANY_REPO: &str = "*";

impl Scope {
    /// Parses the text form.
    pub fn parse(s: &str) -> Result<Self, AuthError> {
        match s {
            "objects:read" => return Ok(Self::ObjectsRead),
            "repos:admin" => return Ok(Self::ReposAdmin),
            "mirror:bind" => return Ok(Self::MirrorBind),
            "mirror:export" => return Ok(Self::MirrorExport),
            _ => {}
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
            ("mirror", None) if repo == ANY_REPO => Ok(Self::Mirror { repo }),
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
            | Self::Admin { repo }
            | Self::Mirror { repo } => Some(repo),
            Self::ObjectsRead | Self::ReposAdmin | Self::MirrorBind | Self::MirrorExport => None,
        }
    }

    /// Whether only the operator token kind may hold this scope
    /// (`mirror:bind`, `mirror:export`).
    pub fn is_operator_only(&self) -> bool {
        matches!(self, Self::MirrorBind | Self::MirrorExport)
    }

    /// Whether holding `self` grants `needed`, for an operation on `path`
    /// (`None` for an operation that is not about one path).
    ///
    /// `admin` grants everything on its repository. `write` and `land` grant
    /// `read` on theirs. A `write` with a prefix grants writes to that prefix
    /// and below only, never a write that names no path. `land` is per line.
    /// `attest`, `drafts` and `objects:read` are granted only by themselves
    /// (and `attest` and `drafts` by `admin`). `repos:admin`, `mirror:bind`,
    /// `mirror:export` and `mirror` are granted only by themselves: `admin`
    /// grants none of them. A held `*` repository matches any repository.
    fn grants(&self, needed: &Scope, path: Option<&RepoPath>) -> bool {
        let same = |held: &str, want: &str| held == ANY_REPO || held == want;
        if let Self::Admin { repo } = self {
            return !matches!(needed, Self::Mirror { .. })
                && needed.repo().is_some_and(|want| same(repo, want));
        }
        match (self, needed) {
            (Self::ObjectsRead, Self::ObjectsRead)
            | (Self::ReposAdmin, Self::ReposAdmin)
            | (Self::MirrorBind, Self::MirrorBind)
            | (Self::MirrorExport, Self::MirrorExport) => true,
            (Self::Mirror { repo }, Self::Mirror { repo: want }) => same(repo, want),
            (
                Self::Read { repo } | Self::Write { repo, .. } | Self::Land { repo, .. },
                Self::Read { repo: want },
            ) => same(repo, want),
            (Self::Write { repo, prefix }, Self::Write { repo: want, .. }) => {
                same(repo, want)
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
            ) => same(repo, want) && line == want_line,
            (Self::Attest { repo }, Self::Attest { repo: want })
            | (Self::Drafts { repo }, Self::Drafts { repo: want }) => same(repo, want),
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
            Self::Mirror { repo } => write!(f, "repo:{repo}:mirror"),
            Self::ObjectsRead => f.write_str("objects:read"),
            Self::ReposAdmin => f.write_str("repos:admin"),
            Self::MirrorBind => f.write_str("mirror:bind"),
            Self::MirrorExport => f.write_str("mirror:export"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use graft_proto::PrincipalKind;

    fn holder(scopes: &[&str]) -> Principal {
        Principal {
            subject: "p".into(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn holds(scopes: &[&str], needed: &str) -> bool {
        let needed = Scope::parse(needed).expect("needed scope parses");
        authorize(&holder(scopes), &needed, None).is_ok()
    }

    #[test]
    fn the_new_forms_round_trip_through_text() {
        for text in [
            "repos:admin",
            "mirror:bind",
            "mirror:export",
            "repo:*:mirror",
            "repo:*:read",
            "repo:*:land:main",
            "repo:*:write:src",
        ] {
            let scope = Scope::parse(text).expect(text);
            assert_eq!(scope.to_string(), text);
        }
        // The mirror set is held only on every repository.
        for text in [
            "repo:r:mirror",
            "repo:*:mirror:x",
            "mirror:all",
            "repos:read",
        ] {
            assert_eq!(Scope::parse(text), Err(AuthError::Malformed), "{text}");
        }
    }

    #[test]
    fn a_wildcard_repository_holds_on_every_repository() {
        assert!(holds(&["repo:*:read"], "repo:a:read"));
        assert!(holds(&["repo:*:land:main"], "repo:b:land:main"));
        assert!(!holds(&["repo:*:land:main"], "repo:b:land:dev"));
        assert!(!holds(&["repo:*:read"], "repo:a:write"));
        assert!(holds(&["repo:*:mirror"], "repo:*:mirror"));
    }

    #[test]
    fn bind_and_export_are_implied_by_nothing_else() {
        for needed in ["mirror:bind", "mirror:export"] {
            for held in [
                "repo:r:admin",
                "repo:*:admin",
                "repos:admin",
                "repo:*:mirror",
                "objects:read",
            ] {
                assert!(!holds(&[held], needed), "{held} grants {needed}");
            }
            assert!(holds(&[needed], needed));
        }
        assert!(!holds(&["mirror:bind"], "mirror:export"));
        assert!(!holds(&["mirror:export"], "mirror:bind"));
        assert!(Scope::MirrorBind.is_operator_only());
        assert!(Scope::MirrorExport.is_operator_only());
        assert!(!Scope::ReposAdmin.is_operator_only());
    }

    #[test]
    fn repos_admin_and_the_mirror_set_grant_only_themselves() {
        assert!(holds(&["repos:admin"], "repos:admin"));
        assert!(!holds(&["repo:r:admin"], "repos:admin"));
        assert!(!holds(&["repo:*:admin"], "repos:admin"));
        assert!(!holds(&["repos:admin"], "repo:r:read"));
        assert!(!holds(&["repos:admin"], "repo:r:admin"));
        // Admin does not grant the mirror set, and the set is not admin.
        assert!(!holds(&["repo:*:admin"], "repo:*:mirror"));
        for needed in ["repo:r:read", "repo:r:write", "repo:r:admin", "repos:admin"] {
            assert!(!holds(&["repo:*:mirror"], needed), "{needed}");
        }
    }
}
