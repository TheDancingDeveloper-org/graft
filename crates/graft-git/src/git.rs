//! Reading a git repository (through `gix`) and the raw git encodings this
//! crate needs: tree entries, commit framing and object hashing.
//!
//! Trees and commits are parsed here from their raw bytes rather than through
//! `gix`'s decoders, because fidelity depends on bytes that a decoder
//! normalizes away (a mode written `040000`, entries out of order).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use graft_model::{EntryKind, ObjectId};

use crate::{GitError, GitSha};

/// A git object kind this crate reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl Kind {
    fn gix(self) -> gix::objs::Kind {
        match self {
            Kind::Blob => gix::objs::Kind::Blob,
            Kind::Tree => gix::objs::Kind::Tree,
            Kind::Commit => gix::objs::Kind::Commit,
            Kind::Tag => gix::objs::Kind::Tag,
        }
    }
}

/// The object a raw tag object names, from its first (`object`) line.
pub(crate) fn tag_target(data: &[u8]) -> Option<GitSha> {
    let line = data.strip_prefix(b"object ")?.get(..40)?;
    GitSha::from_hex(std::str::from_utf8(line).ok()?)
}

/// `sha1("<kind> <len>\0" ‖ data)`.
pub(crate) fn hash(kind: Kind, data: &[u8]) -> GitSha {
    let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, kind.gix(), data)
        .expect("sha1 of in-memory bytes does not fail");
    GitSha(id.as_slice().try_into().expect("sha1 is 20 bytes"))
}

/// One raw tree entry, exactly as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawEntry {
    /// The mode as written (octal ASCII, possibly non-canonical).
    pub mode: Vec<u8>,
    /// The name bytes.
    pub name: Vec<u8>,
    /// The object it points to.
    pub sha: GitSha,
}

impl RawEntry {
    /// Whether the entry is a directory (git's `S_ISDIR`).
    pub(crate) fn is_tree(&self) -> bool {
        mode_bits(&self.mode).is_some_and(|m| m & 0o170000 == 0o040000)
    }

    /// The graft kind, or `None` for a mode graft does not know.
    pub(crate) fn kind(&self) -> Option<EntryKind> {
        entry_kind(&self.mode)
    }
}

fn mode_bits(mode: &[u8]) -> Option<u32> {
    if mode.is_empty() || mode.len() > 7 {
        return None;
    }
    mode.iter().try_fold(0u32, |acc, &b| match b {
        b'0'..=b'7' => Some(acc * 8 + u32::from(b - b'0')),
        _ => None,
    })
}

/// Maps a git mode to a graft kind (design §11.1 step 4): `100644` file,
/// `100755` exec, `120000` symlink, `160000` submodule, `040000` tree. Other
/// regular-file modes (the legacy `100664`) map by the owner-execute bit, as
/// git itself canonicalizes them; the raw tree is kept in the mirror map.
pub(crate) fn entry_kind(mode: &[u8]) -> Option<EntryKind> {
    let m = mode_bits(mode)?;
    match m & 0o170000 {
        0o040000 => Some(EntryKind::Tree),
        0o100000 if m & 0o100 != 0 => Some(EntryKind::Exec),
        0o100000 => Some(EntryKind::File),
        0o120000 => Some(EntryKind::Symlink),
        0o160000 => Some(EntryKind::Submodule),
        _ => None,
    }
}

/// The canonical git mode for a graft kind.
pub(crate) fn canonical_mode(kind: EntryKind) -> &'static [u8] {
    match kind {
        EntryKind::File => b"100644",
        EntryKind::Exec => b"100755",
        EntryKind::Symlink => b"120000",
        EntryKind::Tree => b"40000",
        EntryKind::Submodule => b"160000",
    }
}

/// A submodule's graft id: the commit sha followed by twelve zero bytes.
pub(crate) fn submodule_id(sha: GitSha) -> ObjectId {
    let mut id = [0u8; 32];
    id[..20].copy_from_slice(&sha.0);
    ObjectId(id)
}

/// The commit sha inside a submodule's graft id.
pub(crate) fn submodule_sha(id: ObjectId) -> GitSha {
    GitSha(id.0[..20].try_into().expect("20 of 32 bytes"))
}

/// Parses a raw tree object.
pub(crate) fn parse_tree(data: &[u8]) -> Result<Vec<RawEntry>, String> {
    let mut entries = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let sp = rest
            .iter()
            .position(|&b| b == b' ')
            .ok_or("tree entry without a mode")?;
        let mode = rest[..sp].to_vec();
        rest = &rest[sp + 1..];
        let nul = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or("tree entry without a name")?;
        let name = rest[..nul].to_vec();
        rest = &rest[nul + 1..];
        if rest.len() < 20 {
            return Err("tree entry with a short id".into());
        }
        let sha = GitSha(rest[..20].try_into().expect("20 bytes"));
        rest = &rest[20..];
        entries.push(RawEntry { mode, name, sha });
    }
    Ok(entries)
}

/// Sorts entries in git's order: by name, with directories compared as if
/// their name ended in `/`.
pub(crate) fn sort_entries(entries: &mut [RawEntry]) {
    fn key(e: &RawEntry) -> impl Iterator<Item = u8> + '_ {
        e.name.iter().copied().chain(e.is_tree().then_some(b'/'))
    }
    entries.sort_by(|a, b| key(a).cmp(key(b)));
}

/// Encodes entries, in the order given, as a raw tree object.
pub(crate) fn encode_tree(entries: &[RawEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        out.extend_from_slice(&e.mode);
        out.push(b' ');
        out.extend_from_slice(&e.name);
        out.push(0);
        out.extend_from_slice(&e.sha.0);
    }
    out
}

/// A commit split into the parts graft rebuilds (tree and parents) and the
/// parts kept raw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawCommit {
    pub tree: GitSha,
    pub parents: Vec<GitSha>,
    /// Header lines after the parent lines, through the blank line that ends
    /// them (or to the end, when there is no message).
    pub headers: Vec<u8>,
    /// The message bytes after the blank line.
    pub message: Vec<u8>,
}

fn hex_line<'a>(rest: &'a [u8], tag: &[u8]) -> Option<(GitSha, &'a [u8])> {
    let line = rest.strip_prefix(tag)?;
    if line.len() < 41 || line[40] != b'\n' {
        return None;
    }
    let hex = std::str::from_utf8(&line[..40]).ok()?;
    // Only lowercase hex re-encodes to the same bytes.
    if hex.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    Some((GitSha::from_hex(hex)?, &line[41..]))
}

/// Parses a commit that starts with a tree line and parent lines, as every
/// commit git writes does. `None` for any other shape.
pub(crate) fn parse_commit(data: &[u8]) -> Option<RawCommit> {
    let (tree, mut rest) = hex_line(data, b"tree ")?;
    let mut parents = Vec::new();
    while let Some((p, r)) = hex_line(rest, b"parent ") {
        parents.push(p);
        rest = r;
    }
    let split = if rest.starts_with(b"\n") {
        Some(1)
    } else {
        rest.windows(2).position(|w| w == b"\n\n").map(|i| i + 2)
    };
    let (headers, message) = match split {
        Some(i) => (rest[..i].to_vec(), rest[i..].to_vec()),
        None => (rest.to_vec(), Vec::new()),
    };
    Some(RawCommit {
        tree,
        parents,
        headers,
        message,
    })
}

/// The `author` header's name, email, seconds and offset (`+HHMM`), parsed
/// leniently: whatever does not parse becomes empty or zero. The raw header
/// is kept, so the export never depends on this parse.
pub(crate) fn author(headers: &[u8]) -> (String, String, i64, i16) {
    let line = headers
        .split(|&b| b == b'\n')
        .find_map(|l| l.strip_prefix(b"author "))
        .unwrap_or_default();
    let (lt, gt) = match (
        line.iter().rposition(|&b| b == b'<'),
        line.iter().rposition(|&b| b == b'>'),
    ) {
        (Some(lt), Some(gt)) if lt < gt => (lt, gt),
        _ => {
            return (
                String::from_utf8_lossy(line).into_owned(),
                String::new(),
                0,
                0,
            );
        }
    };
    let name = String::from_utf8_lossy(line[..lt].trim_ascii_end()).into_owned();
    let email = String::from_utf8_lossy(&line[lt + 1..gt]).into_owned();
    let mut when = line[gt + 1..]
        .split(|&b| b == b' ')
        .filter(|t| !t.is_empty());
    let secs = when
        .next()
        .and_then(|t| std::str::from_utf8(t).ok())
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or(0);
    let offset = when.next().map_or(0, parse_offset);
    (name, email, secs, offset)
}

fn parse_offset(t: &[u8]) -> i16 {
    let (sign, digits) = match t.split_first() {
        Some((b'+', d)) => (1, d),
        Some((b'-', d)) => (-1, d),
        _ => return 0,
    };
    if digits.len() != 4 || !digits.iter().all(u8::is_ascii_digit) {
        return 0;
    }
    let n = |i: usize| i16::from(digits[i] - b'0');
    sign * ((n(0) * 10 + n(1)) * 60 + n(2) * 10 + n(3))
}

/// Entries of the trees read recently, shared so lookups do not copy.
const TREE_CACHE: usize = 8192;

/// A git repository opened for reading.
pub(crate) struct Repo {
    inner: gix::Repository,
    trees: HashMap<GitSha, Arc<Vec<RawEntry>>>,
}

fn read_err(e: impl std::fmt::Display) -> GitError {
    GitError::Read(e.to_string())
}

/// Writes `data` as the object `kind` (gix hashes and stores it).
struct Raw<'a>(gix::objs::Kind, &'a [u8]);

impl gix::objs::WriteTo for Raw<'_> {
    fn write_to(&self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        out.write_all(self.1)
    }

    fn kind(&self) -> gix::objs::Kind {
        self.0
    }

    fn size(&self) -> u64 {
        self.1.len() as u64
    }
}

impl Repo {
    /// Opens the repository at `path` (bare or not).
    pub(crate) fn open(path: &Path) -> Result<Repo, GitError> {
        let inner = gix::open(path).map_err(read_err)?;
        Ok(Repo {
            inner,
            trees: HashMap::new(),
        })
    }

    /// Opens the bare repository at `path`, creating it when there is none.
    pub(crate) fn open_or_init(path: &Path) -> Result<Repo, GitError> {
        let inner = match gix::open(path) {
            Ok(inner) => inner,
            Err(_) => gix::init_bare(path).map_err(read_err)?,
        };
        Ok(Repo {
            inner,
            trees: HashMap::new(),
        })
    }

    /// Whether the object is stored here.
    pub(crate) fn has(&self, sha: GitSha) -> bool {
        self.inner
            .has_object(gix::ObjectId::from_bytes_or_panic(&sha.0))
    }

    /// Stores an object and returns its sha, which is checked against the
    /// hash of the bytes.
    pub(crate) fn write(&self, kind: Kind, data: &[u8]) -> Result<GitSha, GitError> {
        let id = self
            .inner
            .write_object(Raw(kind.gix(), data))
            .map_err(|e| GitError::Write(e.to_string()))?
            .detach();
        let sha = to_sha(id);
        if sha != hash(kind, data) {
            return Err(GitError::Write(format!(
                "stored {sha}, not the hash of its bytes"
            )));
        }
        Ok(sha)
    }

    /// Points `name` (a full ref name) at `sha`.
    pub(crate) fn set_ref(&self, name: &str, sha: GitSha) -> Result<(), GitError> {
        let id = gix::ObjectId::from_bytes_or_panic(&sha.0);
        self.inner
            .reference(
                name,
                id,
                gix::refs::transaction::PreviousValue::Any,
                "graft export",
            )
            .map_err(|e| GitError::Write(format!("{name}: {e}")))?;
        Ok(())
    }

    /// The root tree of a stored commit.
    pub(crate) fn commit_tree(&self, sha: GitSha) -> Result<GitSha, GitError> {
        let raw = parse_commit(&self.read(sha, Kind::Commit)?)
            .ok_or_else(|| GitError::Read(format!("commit {sha} has an unexpected shape")))?;
        Ok(raw.tree)
    }

    /// Resolves a full ref name to the commit it (after peeling tags) names.
    /// `None` when the ref does not exist or does not name a commit.
    pub(crate) fn resolve(&self, name: &str) -> Result<Option<GitSha>, GitError> {
        let Some(mut r) = self.inner.try_find_reference(name).map_err(read_err)? else {
            return Ok(None);
        };
        let id = r.peel_to_id().map_err(read_err)?.detach();
        let header = self.inner.find_header(id).map_err(read_err)?;
        Ok((header.kind() == gix::objs::Kind::Commit).then(|| to_sha(id)))
    }

    /// What `name` peels to when that is not a commit: `tree` or `blob`.
    /// `None` for a commit, or when the ref does not exist.
    pub(crate) fn non_commit_kind(&self, name: &str) -> Result<Option<&'static str>, GitError> {
        let Some(mut r) = self.inner.try_find_reference(name).map_err(read_err)? else {
            return Ok(None);
        };
        let id = r.peel_to_id().map_err(read_err)?.detach();
        Ok(match self.inner.find_header(id).map_err(read_err)?.kind() {
            gix::objs::Kind::Tree => Some("tree"),
            gix::objs::Kind::Blob => Some("blob"),
            _ => None,
        })
    }

    /// The tag objects `name` peels through to reach its target, outermost
    /// first: empty for a lightweight ref. Stops at the first object that is
    /// not a tag, or at a tag whose `object` line does not parse.
    pub(crate) fn tag_chain(&self, name: &str) -> Result<Vec<Vec<u8>>, GitError> {
        let Some(r) = self.inner.try_find_reference(name).map_err(read_err)? else {
            return Ok(Vec::new());
        };
        let Some(mut id) = r.target().try_id().map(|id| id.to_owned()) else {
            return Ok(Vec::new());
        };
        let mut chain = Vec::new();
        loop {
            let header = self.inner.find_header(id).map_err(read_err)?;
            if header.kind() != gix::objs::Kind::Tag {
                return Ok(chain);
            }
            let data = self.read(to_sha(id), Kind::Tag)?;
            let next = tag_target(&data);
            chain.push(data);
            match next {
                Some(sha) => id = gix::ObjectId::from_bytes_or_panic(&sha.0),
                None => return Ok(chain),
            }
        }
    }

    /// Every ref under `prefix` (for example `refs/tags/`), sorted by name.
    pub(crate) fn refs_under(&self, prefix: &str) -> Result<Vec<String>, GitError> {
        let platform = self.inner.references().map_err(read_err)?;
        let mut names = Vec::new();
        for r in platform.prefixed(prefix).map_err(read_err)? {
            let r = r.map_err(read_err)?;
            names.push(r.name().as_bstr().to_string());
        }
        names.sort();
        Ok(names)
    }

    /// Reads an object, which must be of `kind`.
    pub(crate) fn read(&self, sha: GitSha, kind: Kind) -> Result<Vec<u8>, GitError> {
        let id = gix::ObjectId::from_bytes_or_panic(&sha.0);
        let obj = self.inner.find_object(id).map_err(read_err)?.detach();
        if obj.kind != kind.gix() {
            return Err(GitError::Read(format!(
                "{sha} is a {}, not a {}",
                obj.kind,
                kind.gix()
            )));
        }
        Ok(obj.data)
    }

    /// A tree's entries, cached.
    pub(crate) fn tree(&mut self, sha: GitSha) -> Result<Arc<Vec<RawEntry>>, GitError> {
        if let Some(t) = self.trees.get(&sha) {
            return Ok(t.clone());
        }
        let data = self.read(sha, Kind::Tree)?;
        let entries =
            Arc::new(parse_tree(&data).map_err(|e| GitError::Read(format!("{sha}: {e}")))?);
        if self.trees.len() >= TREE_CACHE {
            self.trees.clear();
        }
        self.trees.insert(sha, entries.clone());
        Ok(entries)
    }

    /// The entries of `sha`, or none for `None`.
    pub(crate) fn tree_or_empty(
        &mut self,
        sha: Option<GitSha>,
    ) -> Result<Arc<Vec<RawEntry>>, GitError> {
        match sha {
            Some(s) => self.tree(s),
            None => Ok(Arc::new(Vec::new())),
        }
    }
}

fn to_sha(id: gix::ObjectId) -> GitSha {
    GitSha(id.as_slice().try_into().expect("sha1 is 20 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(mode: &str, name: &str) -> RawEntry {
        RawEntry {
            mode: mode.as_bytes().to_vec(),
            name: name.as_bytes().to_vec(),
            sha: GitSha([7; 20]),
        }
    }

    #[test]
    fn modes_map_to_kinds() {
        assert_eq!(entry_kind(b"100644"), Some(EntryKind::File));
        assert_eq!(entry_kind(b"100664"), Some(EntryKind::File));
        assert_eq!(entry_kind(b"100755"), Some(EntryKind::Exec));
        assert_eq!(entry_kind(b"120000"), Some(EntryKind::Symlink));
        assert_eq!(entry_kind(b"160000"), Some(EntryKind::Submodule));
        assert_eq!(entry_kind(b"40000"), Some(EntryKind::Tree));
        assert_eq!(entry_kind(b"040000"), Some(EntryKind::Tree));
        assert_eq!(entry_kind(b"0"), None);
        assert_eq!(entry_kind(b"100648"), None);
        assert_eq!(entry_kind(b""), None);
    }

    #[test]
    fn the_empty_tree_has_its_well_known_sha() {
        assert_eq!(
            hash(Kind::Tree, b"").to_hex(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }

    #[test]
    fn trees_sort_directories_with_a_trailing_slash() {
        let mut entries = vec![
            entry("100644", "a0"),
            entry("40000", "a"),
            entry("100644", "a.b"),
        ];
        sort_entries(&mut entries);
        let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec![b"a.b".to_vec(), b"a".to_vec(), b"a0".to_vec()]);
        let raw = encode_tree(&entries);
        assert_eq!(parse_tree(&raw).unwrap(), entries);
    }

    #[test]
    fn commits_split_into_rebuilt_and_raw_parts() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 0000000000000000000000000000000000000001\n\
author A U Thor <a@example.com> 1700000000 +0530\n\
committer C <c@example.com> 1700000001 -0100\n\
\n\
subject\n\nbody\n";
        let c = parse_commit(raw).unwrap();
        assert_eq!(c.parents.len(), 1);
        assert!(c.headers.starts_with(b"author "));
        assert!(c.headers.ends_with(b"0100\n\n"));
        assert_eq!(c.message, b"subject\n\nbody\n");
        assert_eq!(
            author(&c.headers),
            (
                "A U Thor".into(),
                "a@example.com".into(),
                1_700_000_000,
                330
            )
        );
        let upper = b"tree 4B825DC642CB6EB9A060E54BF8D69288FBEE4904\n";
        assert_eq!(parse_commit(upper), None);
        let no_message = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor x";
        assert_eq!(parse_commit(no_message).unwrap().message, b"");
    }

    #[test]
    fn offsets_parse_leniently() {
        assert_eq!(parse_offset(b"-0130"), -90);
        assert_eq!(parse_offset(b"+0000"), 0);
        assert_eq!(parse_offset(b"0100"), 0);
        assert_eq!(parse_offset(b"+1"), 0);
    }

    #[test]
    fn submodule_ids_round_trip() {
        let sha = GitSha([9; 20]);
        assert_eq!(submodule_sha(submodule_id(sha)), sha);
    }
}
