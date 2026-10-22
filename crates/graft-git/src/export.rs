//! Deterministic export of graft changesets as git objects
//! (`docs/design.md` §11.2).
//!
//! G1.8 needs only the sha recomputation, to verify import fidelity:
//!
//! - **blob:** `sha1("blob <len>\0" ‖ bytes)` ([`blob_sha`]);
//! - **tree:** path copying from the parent commit's git tree
//!   ([`export_tree`]). Only directories on changed paths are re-encoded; an
//!   untouched directory keeps its parent's git tree, bytes and all, so legacy
//!   modes and odd orderings survive unchanged. A directory with a recorded
//!   [`RawTree`] uses the raw bytes instead;
//! - **commit:** the tree and parent lines, then the recorded raw headers and
//!   message ([`commit_object`]).
//!
//! Writing the objects into a repository is G2.2.

use std::collections::BTreeMap;

use graft_model::{BlobId, Change, ChangesetId, EntryKind};
use graft_proto::CallContext;
use graft_proto::api::{ObjectApi, RepoApi};

use crate::git::{
    self, Kind, RawEntry, canonical_mode, encode_tree, parse_tree, sort_entries, submodule_sha,
};
use crate::map::{ImportedCommit, MirrorMap, RawTree};
use crate::{GitError, GitSha};

/// Where the export reads git-side data: the parent's git trees and the git
/// sha of each graft blob.
pub trait GitSource {
    /// The raw bytes of a git tree object.
    fn raw_tree(&mut self, sha: GitSha) -> Result<Vec<u8>, GitError>;
    /// The git sha of a graft blob.
    fn blob_sha(&mut self, id: BlobId) -> Result<GitSha, GitError>;
}

/// The git sha of a blob with these bytes.
pub fn blob_sha(bytes: &[u8]) -> GitSha {
    git::hash(Kind::Blob, bytes)
}

/// The git sha of a raw tree object.
pub fn tree_sha(raw: &[u8]) -> GitSha {
    git::hash(Kind::Tree, raw)
}

/// The sha of git's empty tree.
pub fn empty_tree_sha() -> GitSha {
    tree_sha(b"")
}

/// The raw commit object for an imported changeset: `tree`, the `parents`
/// in order, then the recorded headers and message. `message` is the
/// changeset's message, used when the record holds no raw one.
pub fn commit_object(
    tree: GitSha,
    parents: &[GitSha],
    record: &ImportedCommit,
    message: &str,
) -> Vec<u8> {
    let body = record.message.as_deref().unwrap_or(message.as_bytes());
    let mut out = Vec::with_capacity(46 + 48 * parents.len() + record.headers.len() + body.len());
    out.extend_from_slice(b"tree ");
    out.extend_from_slice(tree.to_hex().as_bytes());
    out.push(b'\n');
    for p in parents {
        out.extend_from_slice(b"parent ");
        out.extend_from_slice(p.to_hex().as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(&record.headers);
    out.extend_from_slice(body);
    out
}

/// The sha of [`commit_object`].
pub fn commit_sha(
    tree: GitSha,
    parents: &[GitSha],
    record: &ImportedCommit,
    message: &str,
) -> GitSha {
    git::hash(Kind::Commit, &commit_object(tree, parents, record, message))
}

/// The git root tree of a changeset with `changes` against a first parent
/// whose git root tree is `parent_tree` (`None` for a root changeset), given
/// the commit's recorded `raw_trees`. Reads only the parent's trees on
/// changed paths: O(changes × depth).
pub fn export_tree<S: GitSource>(
    src: &mut S,
    parent_tree: Option<GitSha>,
    changes: &[Change],
    raw_trees: &[RawTree],
) -> Result<GitSha, GitError> {
    let mut root = Node::default();
    root.add_changes(src, changes)?;
    for r in raw_trees {
        root.at(r.dir.as_ref().map(|d| d.as_str())).raw = Some(r.tree.clone());
    }
    let (sha, _) = build(&root, "", parent_tree, src, &mut Learn::default())?;
    Ok(sha.unwrap_or_else(empty_tree_sha))
}

/// The raw trees an import must record so that [`export_tree`] reproduces
/// the commit's tree: `actual` lists every directory whose git tree differs
/// from the first parent's (path, its tree in the commit). Works bottom-up:
/// a directory gets a record only when the path-copied tree (with the
/// records already chosen beneath it) differs from git's.
pub(crate) fn learn_raw_trees<S: GitSource>(
    src: &mut S,
    parent_tree: Option<GitSha>,
    changes: &[Change],
    actual: &[(String, Option<GitSha>)],
) -> Result<Vec<RawTree>, GitError> {
    let mut root = Node::default();
    root.add_changes(src, changes)?;
    for (dir, sha) in actual {
        root.at((!dir.is_empty()).then_some(dir.as_str())).actual = Some(*sha);
    }
    let mut learn = Learn {
        on: true,
        out: Vec::new(),
    };
    build(&root, "", parent_tree, src, &mut learn)?;
    Ok(learn.out)
}

#[derive(Default)]
struct Learn {
    on: bool,
    out: Vec<RawTree>,
}

enum Leaf {
    Delete,
    Put(&'static [u8], GitSha),
}

/// A trie of the dirty paths.
#[derive(Default)]
struct Node {
    children: BTreeMap<String, Node>,
    leaf: Option<Leaf>,
    /// A recorded raw tree for this directory (export).
    raw: Option<Option<Vec<u8>>>,
    /// The directory's tree in the commit (learning).
    actual: Option<Option<GitSha>>,
}

impl Node {
    fn at(&mut self, path: Option<&str>) -> &mut Node {
        let mut node = self;
        for c in path.into_iter().flat_map(|p| p.split('/')) {
            node = node.children.entry(c.to_owned()).or_default();
        }
        node
    }

    fn add_changes<S: GitSource>(
        &mut self,
        src: &mut S,
        changes: &[Change],
    ) -> Result<(), GitError> {
        for c in changes {
            let leaf = match c {
                Change::Delete { .. } => Leaf::Delete,
                Change::Put { kind, id, .. } => {
                    let sha = match kind {
                        EntryKind::Submodule => submodule_sha(*id),
                        _ => src.blob_sha(BlobId(*id))?,
                    };
                    Leaf::Put(canonical_mode(*kind), sha)
                }
            };
            self.at(Some(c.path().as_str())).leaf = Some(leaf);
        }
        Ok(())
    }

    fn is_dir(&self) -> bool {
        !self.children.is_empty() || self.raw.is_some() || self.actual.is_some()
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn is_under(dir: &Option<graft_model::RepoPath>, path: &str) -> bool {
    match dir {
        None => path.is_empty(),
        Some(d) => {
            path.is_empty()
                || d.as_str() == path
                || (d.as_str().starts_with(path) && d.as_str().as_bytes()[path.len()] == b'/')
        }
    }
}

/// Rebuilds the directory at `path` from its parent-side tree `base`.
/// Returns its new tree (`None`: the directory is gone) and whether it
/// differs from `base` in the export's eyes.
fn build<S: GitSource>(
    node: &Node,
    path: &str,
    base: Option<GitSha>,
    src: &mut S,
    learn: &mut Learn,
) -> Result<(Option<GitSha>, bool), GitError> {
    let is_root = path.is_empty();
    if let Some(raw) = &node.raw {
        return Ok((raw.as_deref().map(tree_sha), true));
    }
    let mut entries: Vec<RawEntry> = match base {
        Some(sha) => {
            parse_tree(&src.raw_tree(sha)?).map_err(|e| GitError::Read(format!("{sha}: {e}")))?
        }
        None => Vec::new(),
    };
    let mut dirty = false;
    for (name, child) in &node.children {
        if matches!(child.leaf, Some(Leaf::Delete)) {
            entries.retain(|e| e.name != name.as_bytes());
            dirty = true;
        }
    }
    for (name, child) in &node.children {
        if !child.is_dir() {
            continue;
        }
        let child_base = entries
            .iter()
            .find(|e| e.name == name.as_bytes() && e.is_tree())
            .map(|e| e.sha);
        let (sha, child_dirty) = build(child, &join(path, name), child_base, src, learn)?;
        if !child_dirty {
            continue;
        }
        dirty = true;
        entries.retain(|e| !(e.name == name.as_bytes() && e.is_tree()));
        if let Some(sha) = sha {
            entries.push(RawEntry {
                mode: canonical_mode(EntryKind::Tree).to_vec(),
                name: name.as_bytes().to_vec(),
                sha,
            });
        }
    }
    for (name, child) in &node.children {
        if let Some(Leaf::Put(mode, sha)) = &child.leaf {
            entries.retain(|e| e.name != name.as_bytes());
            entries.push(RawEntry {
                mode: mode.to_vec(),
                name: name.as_bytes().to_vec(),
                sha: *sha,
            });
            dirty = true;
        }
    }
    let mut predicted = if !dirty {
        base
    } else if entries.is_empty() && !is_root {
        None
    } else {
        sort_entries(&mut entries);
        Some(tree_sha(&encode_tree(&entries)))
    };
    if is_root {
        predicted = Some(predicted.unwrap_or_else(empty_tree_sha));
    }
    if learn.on {
        match node.actual {
            Some(actual) if actual != predicted => {
                let tree = match actual {
                    Some(sha) => Some(src.raw_tree(sha)?),
                    None => None,
                };
                learn.out.retain(|r| !is_under(&r.dir, path));
                let dir = (!is_root)
                    .then(|| graft_model::RepoPath::new(path))
                    .transpose()
                    .map_err(|e| GitError::Read(format!("unchecked path: {e}")))?;
                learn.out.push(RawTree { dir, tree });
                return Ok((actual, true));
            }
            Some(_) => {}
            None if dirty => {
                return Err(GitError::Read(format!(
                    "graft changes under {path:?}, which git did not change"
                )));
            }
            None => {}
        }
    }
    Ok((predicted, dirty))
}

/// Computes (and records in `map`) the git commit for `changeset`, writing
/// objects into `git_dir`. Repeatable: the same inputs give the same sha.
pub async fn export<A: RepoApi + ObjectApi, M: MirrorMap>(
    _api: &A,
    _cx: &CallContext,
    _map: &M,
    _repo: &str,
    _changeset: ChangesetId,
    _committer: &Committer,
    _git_dir: &std::path::Path,
) -> Result<GitSha, GitError> {
    todo!("G2.2 (full export)")
}

/// Committer identity for native changesets.
#[derive(Clone, Debug)]
pub struct Committer {
    /// Name.
    pub name: String,
    /// Email.
    pub email: String,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use graft_model::RepoPath;

    use super::*;

    /// Trees and blobs in memory.
    #[derive(Default)]
    struct Mem {
        trees: HashMap<GitSha, Vec<u8>>,
        blobs: HashMap<BlobId, GitSha>,
    }

    impl Mem {
        fn tree(&mut self, entries: &[(&str, &str, GitSha)]) -> GitSha {
            let raw = encode_tree(
                &entries
                    .iter()
                    .map(|(m, n, s)| RawEntry {
                        mode: m.as_bytes().to_vec(),
                        name: n.as_bytes().to_vec(),
                        sha: *s,
                    })
                    .collect::<Vec<_>>(),
            );
            let sha = tree_sha(&raw);
            self.trees.insert(sha, raw);
            sha
        }

        fn blob(&mut self, bytes: &[u8]) -> (BlobId, GitSha) {
            let pair = (BlobId::hash(bytes), blob_sha(bytes));
            self.blobs.insert(pair.0, pair.1);
            pair
        }
    }

    impl GitSource for Mem {
        fn raw_tree(&mut self, sha: GitSha) -> Result<Vec<u8>, GitError> {
            self.trees
                .get(&sha)
                .cloned()
                .ok_or_else(|| GitError::Read(format!("no tree {sha}")))
        }

        fn blob_sha(&mut self, id: BlobId) -> Result<GitSha, GitError> {
            self.blobs
                .get(&id)
                .copied()
                .ok_or_else(|| GitError::Read("no blob".into()))
        }
    }

    fn put(path: &str, kind: EntryKind, id: BlobId) -> Change {
        Change::Put {
            path: RepoPath::new(path).unwrap(),
            kind,
            id: id.0,
        }
    }

    #[test]
    fn blob_shas_match_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(
            blob_sha(b"hello\n").to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn untouched_directories_keep_their_parent_bytes() {
        let mut m = Mem::default();
        let (_, x) = m.blob(b"x");
        let (y_id, y) = m.blob(b"y");
        // A legacy mode in an untouched directory survives.
        let legacy = m.tree(&[("100664", "old", x)]);
        let parent = m.tree(&[("100644", "a", x), ("40000", "keep", legacy)]);
        let changes = vec![put("b", EntryKind::Exec, y_id)];
        let got = export_tree(&mut m, Some(parent), &changes, &[]).unwrap();
        let want = m.tree(&[
            ("100644", "a", x),
            ("100755", "b", y),
            ("40000", "keep", legacy),
        ]);
        assert_eq!(got, want);
    }

    #[test]
    fn emptied_directories_disappear_and_new_ones_appear() {
        let mut m = Mem::default();
        let (_, x) = m.blob(b"x");
        let (y_id, y) = m.blob(b"y");
        let sub = m.tree(&[("100644", "f", x)]);
        let parent = m.tree(&[("40000", "d", sub)]);
        let changes = vec![
            Change::Delete {
                path: RepoPath::new("d/f").unwrap(),
            },
            put("e/g", EntryKind::File, y_id),
        ];
        let got = export_tree(&mut m, Some(parent), &changes, &[]).unwrap();
        let e = m.tree(&[("100644", "g", y)]);
        assert_eq!(got, m.tree(&[("40000", "e", e)]));
        let all_gone = vec![Change::Delete {
            path: RepoPath::new("d/f").unwrap(),
        }];
        assert_eq!(
            export_tree(&mut m, Some(parent), &all_gone, &[]).unwrap(),
            empty_tree_sha()
        );
    }

    #[test]
    fn a_file_can_become_a_directory() {
        let mut m = Mem::default();
        let (_, x) = m.blob(b"x");
        let (y_id, y) = m.blob(b"y");
        let parent = m.tree(&[("100644", "a", x)]);
        let changes = vec![
            Change::Delete {
                path: RepoPath::new("a").unwrap(),
            },
            put("a/b", EntryKind::File, y_id),
        ];
        let got = export_tree(&mut m, Some(parent), &changes, &[]).unwrap();
        let sub = m.tree(&[("100644", "b", y)]);
        assert_eq!(got, m.tree(&[("40000", "a", sub)]));
    }

    #[test]
    fn learning_records_only_what_path_copying_misses() {
        let mut m = Mem::default();
        let (_, x) = m.blob(b"x");
        let d_old = m.tree(&[("100644", "f", x)]);
        let parent = m.tree(&[("40000", "d", d_old)]);
        // git changed f's mode to 100664: graft sees no change.
        let d_new = m.tree(&[("100664", "f", x)]);
        let child = m.tree(&[("40000", "d", d_new)]);
        let actual = vec![(String::new(), Some(child)), ("d".to_owned(), Some(d_new))];
        let raw = learn_raw_trees(&mut m, Some(parent), &[], &actual).unwrap();
        assert_eq!(raw.len(), 1, "only d needs a record: {raw:?}");
        assert_eq!(raw[0].dir.as_ref().unwrap().as_str(), "d");
        assert_eq!(export_tree(&mut m, Some(parent), &[], &raw).unwrap(), child);
    }

    #[test]
    fn learning_records_absent_directories() {
        let mut m = Mem::default();
        let (_, x) = m.blob(b"x");
        let empty = m.tree(&[]);
        let d_old = m.tree(&[("100644", "f", x), ("40000", "e", empty)]);
        let parent = m.tree(&[("40000", "d", d_old), ("100644", "g", x)]);
        let child = m.tree(&[("100644", "g", x)]);
        let changes = vec![Change::Delete {
            path: RepoPath::new("d/f").unwrap(),
        }];
        let actual = vec![
            (String::new(), Some(child)),
            ("d".to_owned(), None),
            ("d/e".to_owned(), None),
        ];
        let raw = learn_raw_trees(&mut m, Some(parent), &changes, &actual).unwrap();
        assert_eq!(
            export_tree(&mut m, Some(parent), &changes, &raw).unwrap(),
            child
        );
    }
}
