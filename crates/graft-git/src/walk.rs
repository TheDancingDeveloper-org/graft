//! The topological walk: every commit reachable from the tips, parents
//! before children, stopping at commits already imported.

use std::collections::HashMap;

use crate::git::{Kind, Repo, parse_commit};
use crate::{GitError, GitSha};

/// The walk's result.
pub(crate) struct Walk {
    /// Commits to import, oldest first: every parent precedes its children.
    pub order: Vec<GitSha>,
    /// Commits whose object does not have git's usual shape (tree line, then
    /// parent lines). Their parents cannot be known, so the walk stops there.
    pub misshapen: Vec<GitSha>,
}

/// Walks back from `tips` (in order; first parents first, so a line's own
/// history comes out in first-parent order) and returns every commit for
/// which `known` is false, parents before children. `known` commits are
/// boundaries: neither returned nor walked through.
pub(crate) fn topo(
    repo: &Repo,
    tips: &[GitSha],
    mut known: impl FnMut(GitSha) -> Result<bool, GitError>,
) -> Result<Walk, GitError> {
    #[derive(PartialEq)]
    enum Mark {
        Open,
        Done,
    }
    let mut marks: HashMap<GitSha, Mark> = HashMap::new();
    let mut order = Vec::new();
    let mut misshapen = Vec::new();
    let mut stack: Vec<(GitSha, bool)> = tips.iter().rev().map(|t| (*t, false)).collect();
    while let Some((sha, expanded)) = stack.pop() {
        if expanded {
            if marks.insert(sha, Mark::Done) != Some(Mark::Done) {
                order.push(sha);
            }
            continue;
        }
        if marks.contains_key(&sha) {
            continue;
        }
        if known(sha)? {
            marks.insert(sha, Mark::Done);
            continue;
        }
        marks.insert(sha, Mark::Open);
        stack.push((sha, true));
        let Some(commit) = parse_commit(&repo.read(sha, Kind::Commit)?) else {
            misshapen.push(sha);
            continue;
        };
        for p in commit.parents.iter().rev() {
            match marks.get(p) {
                Some(Mark::Done) => {}
                Some(Mark::Open) => {
                    return Err(GitError::Read(format!("commit cycle through {p}")));
                }
                None => stack.push((*p, false)),
            }
        }
    }
    Ok(Walk { order, misshapen })
}
