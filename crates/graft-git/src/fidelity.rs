//! The reusable fidelity check: does git see exactly the changes graft
//! recorded? (`docs/design.md` §11.1, §11.2.)
//!
//! [`check_changeset`] reads one commit from a repository and compares its
//! diff against its first parent with the changeset's `changes`. It costs
//! O(change): it never reads a whole snapshot. The chain makes it a full
//! check: if every commit's diff matches graft's changes, and the root
//! commit's diff is the whole of its tree, each commit's git tree equals
//! graft's snapshot. Tests use it on every commit of an export.

use std::path::Path;

use graft_model::{BlobId, Change, Changeset, ChangesetId, EntryKind, RepoPath};

use crate::diff::{changed_dirs, git_changes};
use crate::git::{Kind, Repo, parse_commit, submodule_sha};
use crate::map::MirrorMap;
use crate::{GitError, GitSha};

/// Checks changeset `id`, with the contents `changeset`, against its commit
/// in `git_dir`. The git shas come from `map`.
///
/// The commit's parents must be the git shas of `changeset.parents`, in
/// order. Its diff against the first parent must list the same paths, kinds
/// and blob shas as `changeset.changes`. A mismatch is
/// [`GitError::Fidelity`] with the commit's sha.
pub fn check_changeset<M: MirrorMap>(
    git_dir: &Path,
    map: &M,
    id: ChangesetId,
    changeset: &Changeset,
) -> Result<(), GitError> {
    let sha = map
        .sha_of(id)?
        .ok_or_else(|| GitError::Map(format!("changeset {id} has no git commit")))?;
    let mut repo = Repo::open(git_dir)?;
    let commit = parse_commit(&repo.read(sha, Kind::Commit)?)
        .ok_or_else(|| GitError::Read(format!("commit {sha} has an unexpected shape")))?;

    let mut parents = Vec::with_capacity(changeset.parents.len());
    for p in &changeset.parents {
        parents.push(
            map.sha_of(*p)?
                .ok_or_else(|| GitError::Map(format!("parent {p} of {id} has no git commit")))?,
        );
    }
    if parents != commit.parents {
        return Err(GitError::Fidelity(vec![sha]));
    }

    let parent_tree = match parents.first() {
        Some(p) => Some(repo.commit_tree(*p)?),
        None => None,
    };
    let dirs = changed_dirs(&mut repo, parent_tree, Some(commit.tree))?;
    let mut actual: Vec<(RepoPath, Option<(EntryKind, GitSha)>)> = git_changes(&mut repo, &dirs)?
        .into_iter()
        .map(|c| (c.path, c.new))
        .collect();
    actual.sort_by(|a, b| a.0.as_str().as_bytes().cmp(b.0.as_str().as_bytes()));

    let mut expected = Vec::with_capacity(changeset.changes.len());
    for change in &changeset.changes {
        expected.push(match change {
            Change::Delete { path } => (path.clone(), None),
            Change::Put { path, kind, id } => {
                let blob = match kind {
                    EntryKind::Submodule => submodule_sha(*id),
                    _ => blob_sha(map, BlobId(*id))?,
                };
                (path.clone(), Some((*kind, blob)))
            }
        });
    }
    expected.sort_by(|a, b| a.0.as_str().as_bytes().cmp(b.0.as_str().as_bytes()));

    if actual != expected {
        return Err(GitError::Fidelity(vec![sha]));
    }
    Ok(())
}

fn blob_sha<M: MirrorMap>(map: &M, id: BlobId) -> Result<GitSha, GitError> {
    map.blob_sha(id)?
        .ok_or_else(|| GitError::Map(format!("no git sha for blob {id}")))
}
