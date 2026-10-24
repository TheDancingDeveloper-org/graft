//! git interop (`docs/design.md` §11). The only crate that knows git.
//!
//! Work items G1.8 (import and fidelity) and G2.2 (export).
//!
//! - [`import::import`] reads a bare repository with `gix`, walks every commit
//!   reachable from the configured refs oldest first, and sends blobs and
//!   `ImportChangesets` batches through the API traits. It checks every path
//!   first (decision D11) and verifies per commit that the export reproduces
//!   the git sha.
//! - [`export`] holds the deterministic git encoders: blob, tree (by path
//!   copying from the parent's git tree) and commit shas.
//! - [`exporter::Exporter`] writes changesets as git commits into a bare
//!   repository, parents first, and exports whole refs (G2.2).
//! - [`fidelity::check_changeset`] checks that git sees the changes graft
//!   recorded, in O(change).
//! - [`map`] is the mirror map: graft ids ↔ git shas, raw commit headers and
//!   the raw git trees graft's model cannot express.

use std::fmt;

use graft_model::ChangesetId;

mod diff;
pub mod export;
pub mod exporter;
pub mod fidelity;
mod git;
pub mod import;
pub mod map;
mod walk;

/// A git object id (SHA-1, 20 bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitSha(pub [u8; 20]);

impl GitSha {
    /// Lowercase hex, as git prints it.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(40);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Parses 40 hex digits (either case).
    pub fn from_hex(s: &str) -> Option<GitSha> {
        if s.len() != 40 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 20];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(GitSha(out))
    }
}

impl fmt::Display for GitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for GitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GitSha({})", self.to_hex())
    }
}

/// A path that breaks graft's path rules, found before anything is written.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BadPath {
    /// The path, with bytes that are not UTF-8 shown as U+FFFD.
    pub path: String,
    /// The rule broken: a [`graft_model::PathError::reason`], the
    /// portability rule (`portability (case-clash)`), `duplicate-entry` or
    /// `unsupported-mode`; or `blob-bytes` for a blob over
    /// [`import::MAX_BLOB_BYTES`](crate::import::MAX_BLOB_BYTES).
    pub reason: String,
    /// The first commit (in import order) that introduced it.
    pub commit: GitSha,
}

/// A commit graft's model cannot hold, found before anything is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unrepresentable {
    /// The commit.
    pub commit: GitSha,
    /// Why: `empty-changeset` (no change against a single or absent parent,
    /// which validity rule 9 forbids) or `commit-shape` (the object does not
    /// start with a tree line and parent lines).
    pub reason: &'static str,
}

/// A tag import skipped: it names a tree or a blob, directly or through tag
/// objects. The mirror map records tags of commits only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonCommitTag {
    /// The tag name, without `refs/tags/`.
    pub name: String,
    /// What it peels to: `tree` or `blob`.
    pub kind: &'static str,
}

/// A git interop failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitError {
    /// Reading the git repository failed.
    Read(String),
    /// Paths that break graft's path rules (decision D11), every one of them.
    /// Nothing was imported.
    BadPaths(Vec<BadPath>),
    /// Commits graft cannot represent. Nothing was imported.
    Unrepresentable(Vec<Unrepresentable>),
    /// Commits whose export does not reproduce their sha and that
    /// `allow_lossy` does not name. Changesets before the first of them were
    /// imported; none after.
    Fidelity(Vec<GitSha>),
    /// Pairs of commits (first imported, second) that map to the same
    /// changeset, so one export cannot reproduce both. The
    /// `mirror.git-commit` metadata key makes this unreachable; it is kept
    /// as a guard.
    /// Changesets before the first pair were imported; none after.
    Indistinct(Vec<(GitSha, GitSha)>),
    /// A commit's changes broke a validity rule while deriving its root.
    Invalid {
        /// The commit.
        commit: GitSha,
        /// The rule (`graft_model::Rule::as_str`) or source failure.
        rule: String,
    },
    /// The graft API refused a call.
    Api(graft_proto::ApiError),
    /// The mirror map failed.
    Map(String),
    /// Writing objects or a ref into the destination repository failed.
    Write(String),
    /// The export was configured with something it cannot write.
    Config(String),
    /// A native changeset whose metadata git cannot hold in a commit (a
    /// newline or NUL where a header or trailer goes). Nothing was written
    /// for it.
    Unexportable {
        /// The changeset.
        changeset: ChangesetId,
        /// Which field: `author-name`, `author-email`, `committer`,
        /// `message` or `trailer`.
        reason: &'static str,
    },
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Read(what) => write!(f, "reading the git repository failed: {what}"),
            GitError::BadPaths(paths) => {
                write!(f, "{} paths break graft's path rules", paths.len())
            }
            GitError::Unrepresentable(commits) => {
                write!(f, "{} commits cannot be represented", commits.len())
            }
            GitError::Fidelity(shas) => {
                write!(f, "{} commits do not export to their sha", shas.len())
            }
            GitError::Indistinct(pairs) => {
                write!(
                    f,
                    "{} commits map to a changeset another commit has",
                    pairs.len()
                )
            }
            GitError::Invalid { commit, rule } => write!(f, "commit {commit} breaks rule {rule}"),
            GitError::Api(e) => write!(f, "the graft API refused: {e:?}"),
            GitError::Map(what) => write!(f, "mirror map failed: {what}"),
            GitError::Write(what) => write!(f, "writing to the git repository failed: {what}"),
            GitError::Config(what) => write!(f, "export configuration: {what}"),
            GitError::Unexportable { changeset, reason } => {
                write!(f, "changeset {changeset} cannot be exported: bad {reason}")
            }
        }
    }
}

impl std::error::Error for GitError {}

impl From<graft_proto::ApiError> for GitError {
    fn from(e: graft_proto::ApiError) -> Self {
        GitError::Api(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_hex_round_trips() {
        let hex = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let sha = GitSha::from_hex(hex).unwrap();
        assert_eq!(sha.to_hex(), hex);
        assert_eq!(GitSha::from_hex(&hex.to_uppercase()), Some(sha));
        assert_eq!(GitSha::from_hex("4b82"), None);
        assert_eq!(GitSha::from_hex(&"g".repeat(40)), None);
    }
}
