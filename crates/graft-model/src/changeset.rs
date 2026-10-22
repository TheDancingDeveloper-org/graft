//! Changesets: the unit of work (`docs/design.md` §4.5).
//!
//! Canonical payload: `u8 version(1)`, `u32 n ‖ parents`, `root(32)`,
//! `u32 n ‖ changes` (`'p' ‖ str(path) ‖ kind-byte ‖ id` or `'d' ‖ str(path)`),
//! `author` (`str name ‖ str email ‖ u8 kind`), `i64 ms ‖ i16 offset`,
//! `str message`, `u32 n ‖ (str key ‖ str value)…`. Integers are big-endian
//! and `str` is `u32` length ‖ UTF-8. The author kind byte is `0` human,
//! `1` agent, `2` bot, `3` CI.

use std::collections::BTreeMap;
use std::fmt;

use crate::codec::{Reader, put_len, put_str};
use crate::id::{ChangesetId, ObjectId, TreeId};
use crate::path::{PathError, RepoPath};
use crate::time::Timestamp;
use crate::tree::EntryKind;

/// The changeset format version byte.
pub const CHANGESET_VERSION: u8 = 1;

/// The default limit on changes in one changeset (validity rule 8).
pub const DEFAULT_MAX_CHANGES: usize = 100_000;

/// Sorted string map for tools. Keys under `graft.` are reserved.
pub type Metadata = BTreeMap<String, String>;

/// Who wrote a changeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthorKind {
    /// A person.
    Human,
    /// An AI agent session.
    Agent,
    /// An automated tool (revert bot, dependency updater).
    Bot,
    /// A CI job.
    Ci,
}

impl AuthorKind {
    /// The canonical kind byte.
    pub fn byte(self) -> u8 {
        match self {
            AuthorKind::Human => 0,
            AuthorKind::Agent => 1,
            AuthorKind::Bot => 2,
            AuthorKind::Ci => 3,
        }
    }

    /// The kind for a canonical kind byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(AuthorKind::Human),
            1 => Some(AuthorKind::Agent),
            2 => Some(AuthorKind::Bot),
            3 => Some(AuthorKind::Ci),
            _ => None,
        }
    }
}

/// The author of a changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Author {
    /// Display name.
    pub name: String,
    /// Email or other contact.
    pub email: String,
    /// Human, agent, bot or CI.
    pub kind: AuthorKind,
}

/// One file-level change relative to the first parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Create or replace the entry at `path`. `kind` is never `Tree`.
    Put {
        /// Where.
        path: RepoPath,
        /// File, exec, symlink or submodule.
        kind: EntryKind,
        /// The blob (or submodule reference).
        id: ObjectId,
    },
    /// Remove the entry at `path`.
    Delete {
        /// Where.
        path: RepoPath,
    },
}

impl Change {
    /// The path this change touches.
    pub fn path(&self) -> &RepoPath {
        match self {
            Change::Put { path, .. } | Change::Delete { path } => path,
        }
    }
}

/// A changeset. Every field is part of its id; nothing server-assigned is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changeset {
    /// Ordered parents; empty only for a root.
    pub parents: Vec<ChangesetId>,
    /// The root tree of the resulting snapshot.
    pub root: TreeId,
    /// Changes against `parents[0]`, sorted by path.
    pub changes: Vec<Change>,
    /// Who wrote it.
    pub author: Author,
    /// When, as supplied by the client.
    pub authored_at: Timestamp,
    /// Description.
    pub message: String,
    /// Tool metadata.
    pub metadata: Metadata,
}

impl Changeset {
    /// The canonical payload the changeset id hashes. Changes are written in
    /// the order given; [`Changeset::check`] rejects an unsorted list.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(CHANGESET_VERSION);
        put_len(&mut out, self.parents.len());
        for p in &self.parents {
            out.extend_from_slice(&p.0.0);
        }
        out.extend_from_slice(&self.root.0.0);
        put_len(&mut out, self.changes.len());
        for c in &self.changes {
            match c {
                Change::Put { path, kind, id } => {
                    out.push(b'p');
                    put_str(&mut out, path.as_str());
                    out.push(kind.byte());
                    out.extend_from_slice(&id.0);
                }
                Change::Delete { path } => {
                    out.push(b'd');
                    put_str(&mut out, path.as_str());
                }
            }
        }
        put_str(&mut out, &self.author.name);
        put_str(&mut out, &self.author.email);
        out.push(self.author.kind.byte());
        out.extend_from_slice(&self.authored_at.ms.to_be_bytes());
        out.extend_from_slice(&self.authored_at.offset_min.to_be_bytes());
        put_str(&mut out, &self.message);
        put_len(&mut out, self.metadata.len());
        for (k, v) in &self.metadata {
            put_str(&mut out, k);
            put_str(&mut out, v);
        }
        out
    }

    /// Decodes and validates a canonical payload. Besides the framing, it
    /// checks that every path passes the path rules, changes are strictly
    /// sorted by path bytes, `Put` kinds are not `tree`, and metadata keys
    /// are strictly sorted. Trailing bytes are rejected.
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(payload);
        let version = r.u8()?;
        if version != CHANGESET_VERSION {
            return Err(DecodeError::Version(version));
        }
        let n = r.count(32)?;
        let mut parents = Vec::with_capacity(n);
        for _ in 0..n {
            parents.push(ChangesetId(r.id()?));
        }
        let root = TreeId(r.id()?);
        let n = r.count(5)?;
        let mut changes: Vec<Change> = Vec::with_capacity(n);
        for _ in 0..n {
            let tag = r.u8()?;
            let path =
                RepoPath::new(r.str()?).map_err(|_| DecodeError::NotCanonical("change-path"))?;
            if let Some(prev) = changes.last()
                && prev.path().as_str().as_bytes() >= path.as_str().as_bytes()
            {
                return Err(DecodeError::NotCanonical("change-order"));
            }
            let change = match tag {
                b'p' => {
                    let kind = EntryKind::from_byte(r.u8()?)
                        .filter(|k| !k.is_tree())
                        .ok_or(DecodeError::NotCanonical("put-kind"))?;
                    Change::Put {
                        path,
                        kind,
                        id: r.id()?,
                    }
                }
                b'd' => Change::Delete { path },
                _ => return Err(DecodeError::NotCanonical("change-tag")),
            };
            changes.push(change);
        }
        let name = r.str()?.to_owned();
        let email = r.str()?.to_owned();
        let kind =
            AuthorKind::from_byte(r.u8()?).ok_or(DecodeError::NotCanonical("author-kind"))?;
        let authored_at = Timestamp {
            ms: r.i64()?,
            offset_min: r.i16()?,
        };
        let message = r.str()?.to_owned();
        let n = r.count(8)?;
        let mut metadata = Metadata::new();
        let mut last: Option<&str> = None;
        for _ in 0..n {
            let k = r.str()?;
            let v = r.str()?;
            if last.is_some_and(|l| l.as_bytes() >= k.as_bytes()) {
                return Err(DecodeError::NotCanonical("metadata-order"));
            }
            last = Some(k);
            metadata.insert(k.to_owned(), v.to_owned());
        }
        if !r.is_empty() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(Changeset {
            parents,
            root,
            changes,
            author: Author { name, email, kind },
            authored_at,
            message,
            metadata,
        })
    }

    /// The changeset's id.
    pub fn id(&self) -> ChangesetId {
        ChangesetId::hash(&self.encode())
    }

    /// The first parent, if any.
    pub fn first_parent(&self) -> Option<ChangesetId> {
        self.parents.first().copied()
    }

    /// Checks the validity rules that need no store (design §4.5 rules 2, 3,
    /// 8 and 9, plus the `Put` kind and, when enabled, path portability).
    /// Rules 5 to 7 need the parent snapshot and are checked by
    /// [`crate::tree_ops::apply_changes`]; rules 1 and 4 need the store and
    /// are the engine's.
    pub fn check(&self, opts: &CheckOptions) -> Result<(), Violation> {
        if self.parents.is_empty() && !opts.allow_root {
            return Err(Violation::new(None, Rule::RootChangeset));
        }
        if self.changes.is_empty() && self.parents.len() < 2 {
            return Err(Violation::new(None, Rule::EmptyChangeset));
        }
        if self.changes.len() > opts.max_changes {
            return Err(Violation::new(None, Rule::TooManyChanges));
        }
        let mut prev: Option<&RepoPath> = None;
        for c in &self.changes {
            let path = c.path();
            if let Some(p) = prev {
                match p.as_str().as_bytes().cmp(path.as_str().as_bytes()) {
                    std::cmp::Ordering::Equal => {
                        return Err(Violation::new(Some(path), Rule::DuplicatePath));
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(Violation::new(Some(path), Rule::ChangesSorted));
                    }
                    std::cmp::Ordering::Less => {}
                }
            }
            prev = Some(path);
            if let Change::Put { kind, .. } = c
                && kind.is_tree()
            {
                return Err(Violation::new(Some(path), Rule::PutKind));
            }
            if opts.portable_paths {
                path.check_portable()
                    .map_err(|e| Violation::new(Some(path), Rule::Path(e)))?;
            }
        }
        Ok(())
    }
}

/// Repository settings the store-free validity rules depend on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOptions {
    /// Whether a parentless changeset is allowed (import with admin scope, or
    /// an empty repository). Rule 2.
    pub allow_root: bool,
    /// The change-count limit. Rule 8.
    pub max_changes: usize,
    /// Whether the repository enables `portable_paths`.
    pub portable_paths: bool,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions {
            allow_root: false,
            max_changes: DEFAULT_MAX_CHANGES,
            portable_paths: false,
        }
    }
}

/// A validity rule from design §4.5.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// 1: every parent exists in this repository (checked by the engine).
    ParentExists,
    /// 2: a parentless changeset is allowed only for import or an empty
    /// repository.
    RootChangeset,
    /// 3: each path appears at most once.
    DuplicatePath,
    /// 3: changes are sorted by path bytes.
    ChangesSorted,
    /// 3: the path passes the path rules (including portability).
    Path(PathError),
    /// 4: every referenced blob exists (checked by the engine).
    BlobExists,
    /// 5: a `Delete` names an existing non-directory entry of `parents[0]`.
    DeleteMissing,
    /// 6: a `Put` is not a no-op.
    PutNoOp,
    /// 7: the result is a well-formed tree: no path is both a file and a
    /// directory.
    FileAndDirectory,
    /// 8: the change count is within the limit.
    TooManyChanges,
    /// 9: a changeset with no changes needs more than one parent.
    EmptyChangeset,
    /// A `Put` names the `tree` kind, which only the server writes.
    PutKind,
}

impl Rule {
    /// A stable machine-readable name (the `rule` of `InvalidChange`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Rule::ParentExists => "parent-exists",
            Rule::RootChangeset => "root-changeset",
            Rule::DuplicatePath => "duplicate-path",
            Rule::ChangesSorted => "changes-sorted",
            Rule::Path(e) => e.reason(),
            Rule::BlobExists => "blob-exists",
            Rule::DeleteMissing => "delete-missing",
            Rule::PutNoOp => "put-no-op",
            Rule::FileAndDirectory => "file-and-directory",
            Rule::TooManyChanges => "too-many-changes",
            Rule::EmptyChangeset => "empty-changeset",
            Rule::PutKind => "put-kind",
        }
    }
}

/// A broken validity rule, at a path where one applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The offending path, when the rule is about one.
    pub path: Option<RepoPath>,
    /// The rule broken.
    pub rule: Rule,
}

impl Violation {
    pub(crate) fn new(path: Option<&RepoPath>, rule: Rule) -> Self {
        Violation {
            path: path.cloned(),
            rule,
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => write!(f, "invalid change at {p}: {}", self.rule.as_str()),
            None => write!(f, "invalid changeset: {}", self.rule.as_str()),
        }
    }
}

impl std::error::Error for Violation {}

/// Why a canonical payload failed to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Unknown format version byte.
    Version(u8),
    /// The payload ended early.
    Truncated,
    /// Bytes left over after the last field.
    TrailingBytes,
    /// A field broke a canonical-form rule (ordering, uniqueness, UTF-8).
    NotCanonical(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Version(v) => write!(f, "unknown format version {v}"),
            DecodeError::Truncated => f.write_str("payload truncated"),
            DecodeError::TrailingBytes => f.write_str("trailing bytes after payload"),
            DecodeError::NotCanonical(what) => write!(f, "not canonical: {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}
