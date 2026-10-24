//! Repository paths (`docs/design.md` §4.2).
//!
//! A [`RepoPath`] is UTF-8 with `/` separators: no leading or trailing `/`,
//! no empty, `.` or `..` component, no NUL, each component at most 255 bytes
//! and the whole path at most 4,096 bytes. Paths are case-sensitive and sort
//! by bytes. The root is not a path.
//!
//! Portability ([`RepoPath::check_portable`]) is a separate, opt-in check for
//! names a common filesystem cannot hold. Case clashes depend on a directory's
//! other entries, so they are checked where trees are built
//! ([`crate::tree_ops`]).

use std::fmt;

/// The longest component, in bytes.
pub const MAX_COMPONENT_BYTES: usize = 255;

/// The longest path, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;

/// A validated path relative to the repository root. Never the root itself.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(String);

impl RepoPath {
    /// Validates `s` against the path rules.
    pub fn new(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if s.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        for component in s.split('/') {
            check_component(component)?;
        }
        Ok(RepoPath(s.to_owned()))
    }

    /// Validates raw bytes: they must be UTF-8 and pass [`RepoPath::new`].
    pub fn from_bytes(b: &[u8]) -> Result<Self, PathError> {
        let s = std::str::from_utf8(b).map_err(|_| PathError::NotUtf8)?;
        Self::new(s)
    }

    /// Checks the portability rules on every component: no Windows-reserved
    /// device name (`CON`, `aux.rs`, `com1.txt`, ...), no character Windows
    /// forbids (`<>:"\|?*` and control characters), and no trailing `.` or
    /// space, and no name for git's metadata directory
    /// ([`is_git_metadata_name`]). Case clashes are checked per directory by
    /// [`crate::tree_ops::apply_changes_with`].
    pub fn check_portable(&self) -> Result<(), PathError> {
        self.components().try_for_each(check_component_portable)
    }

    /// The path as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The components, split on `/`.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The last component.
    pub fn name(&self) -> &str {
        self.0.rsplit_once('/').map_or(self.0.as_str(), |(_, n)| n)
    }

    /// The parent directory, or `None` for a top-level entry.
    pub fn parent(&self) -> Option<RepoPath> {
        self.0.rsplit_once('/').map(|(p, _)| RepoPath(p.to_owned()))
    }

    /// `self/name`, where `name` is one valid component.
    pub fn join(&self, name: &str) -> Result<RepoPath, PathError> {
        check_component(name)?;
        let joined = format!("{}/{}", self.0, name);
        if joined.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        Ok(RepoPath(joined))
    }

    /// Whether `self` equals `prefix` or lies beneath it.
    pub fn starts_with(&self, prefix: &RepoPath) -> bool {
        self.0 == prefix.0
            || (self.0.starts_with(&prefix.0)
                && self.0.as_bytes().get(prefix.0.len()) == Some(&b'/'))
    }
}

/// `parent/name` with `parent` absent meaning the root. `name` must already
/// be a valid component; the length is checked.
pub(crate) fn child(parent: Option<&RepoPath>, name: &str) -> Result<RepoPath, PathError> {
    match parent {
        None => RepoPath::new(name),
        Some(p) => p.join(name),
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RepoPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Checks one component against the path rules (not portability).
pub fn check_component(c: &str) -> Result<(), PathError> {
    if c.is_empty() {
        return Err(PathError::EmptyComponent);
    }
    if c == "." || c == ".." {
        return Err(PathError::DotComponent);
    }
    if c.contains('\0') {
        return Err(PathError::Nul);
    }
    if c.contains('/') {
        return Err(PathError::EmptyComponent);
    }
    if c.len() > MAX_COMPONENT_BYTES {
        return Err(PathError::ComponentTooLong);
    }
    Ok(())
}

/// Device names Windows reserves, with or without an extension.
const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "conin$", "conout$", "com0", "com1", "com2", "com3", "com4",
    "com5", "com6", "com7", "com8", "com9", "com¹", "com²", "com³", "lpt0", "lpt1", "lpt2", "lpt3",
    "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "lpt¹", "lpt²", "lpt³",
];

/// Checks one component against the portability rules.
pub fn check_component_portable(c: &str) -> Result<(), PathError> {
    if is_git_metadata_name(c) {
        return Err(PathError::NotPortable(Portability::GitMetadata));
    }
    let stem = c.split('.').next().unwrap_or(c).trim_end_matches(' ');
    if WINDOWS_RESERVED.contains(&stem.to_lowercase().as_str()) {
        return Err(PathError::NotPortable(Portability::ReservedName));
    }
    if c.chars()
        .any(|ch| matches!(ch, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*') || ch < ' ')
    {
        return Err(PathError::NotPortable(Portability::ReservedChar));
    }
    if c.ends_with('.') || c.ends_with(' ') {
        return Err(PathError::NotPortable(Portability::TrailingDotOrSpace));
    }
    Ok(())
}

/// Code points HFS+ ignores when it compares names (git's `is_hfs_dotgit`).
fn hfs_ignorable(ch: char) -> bool {
    matches!(ch,
        '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}')
}

/// Whether a component names git's metadata directory on some filesystem,
/// following git's `verify_path`: `.git` in any case, `.git` with code points
/// HFS+ ignores, and the NTFS forms (the short name `git~1`, and either name
/// followed by trailing dots or spaces, an alternate data stream `:...` or a
/// `\`). Checking such a name out can overwrite repository metadata
/// (CVE-2014-9390).
pub fn is_git_metadata_name(c: &str) -> bool {
    let hfs: String = c.chars().filter(|&ch| !hfs_ignorable(ch)).collect();
    if hfs.eq_ignore_ascii_case(".git") {
        return true;
    }
    let lower = c.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix(".git")
        .or_else(|| lower.strip_prefix("git~1"))
    else {
        return false;
    };
    let rest = rest.trim_start_matches(['.', ' ']);
    rest.is_empty() || rest.starts_with(':') || rest.starts_with('\\')
}

/// The case-folded form two names clash on.
pub(crate) fn case_fold(name: &str) -> String {
    name.to_lowercase()
}

/// Why a path was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// Empty string (the root is not a path).
    Empty,
    /// Leading or trailing `/`, or an empty component.
    EmptyComponent,
    /// A `.` or `..` component.
    DotComponent,
    /// A NUL byte.
    Nul,
    /// A component over 255 bytes.
    ComponentTooLong,
    /// A path over 4,096 bytes.
    TooLong,
    /// Not valid UTF-8.
    NotUtf8,
    /// Fails a portability rule.
    NotPortable(Portability),
}

impl PathError {
    /// The rule broken, as a stable machine-readable name (the `reason` of
    /// `InvalidPath`).
    pub fn reason(&self) -> &'static str {
        match self {
            PathError::Empty => "empty",
            PathError::EmptyComponent => "empty-component",
            PathError::DotComponent => "dot-component",
            PathError::Nul => "nul",
            PathError::ComponentTooLong => "component-too-long",
            PathError::TooLong => "too-long",
            PathError::NotUtf8 => "not-utf8",
            PathError::NotPortable(_) => "portability",
        }
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::NotPortable(p) => write!(f, "invalid path: portability ({})", p.as_str()),
            other => write!(f, "invalid path: {}", other.reason()),
        }
    }
}

impl std::error::Error for PathError {}

/// Which portability rule a path broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Portability {
    /// A Windows device name such as `CON` or `aux.rs`.
    ReservedName,
    /// A character Windows forbids in names.
    ReservedChar,
    /// A trailing `.` or space, which Windows strips.
    TrailingDotOrSpace,
    /// Differs only in case from another name in the same directory.
    CaseClash,
    /// Names git's metadata directory (`.git`, `.GIT`, `git~1`, ...).
    GitMetadata,
}

impl Portability {
    /// A stable machine-readable name.
    pub fn as_str(self) -> &'static str {
        match self {
            Portability::ReservedName => "reserved-name",
            Portability::ReservedChar => "reserved-char",
            Portability::TrailingDotOrSpace => "trailing-dot-or-space",
            Portability::CaseClash => "case-clash",
            Portability::GitMetadata => "git-metadata",
        }
    }
}
