//! First-parent tree diffs that walk only changed subtrees, the path rules
//! check on what a commit introduces, and the git-level changes graft sees.

use std::collections::{BTreeMap, HashMap, HashSet};

use graft_model::path::check_component_portable;
use graft_model::{EntryKind, RepoPath};

use crate::git::Repo;
use crate::{GitError, GitSha};

/// One directory whose tree id differs between a commit and its first
/// parent. `path` is empty for the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirDiff {
    pub path: Vec<u8>,
    pub old: Option<GitSha>,
    pub new: Option<GitSha>,
}

pub(crate) fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        let mut p = Vec::with_capacity(dir.len() + 1 + name.len());
        p.extend_from_slice(dir);
        p.push(b'/');
        p.extend_from_slice(name);
        p
    }
}

/// Every directory that differs between `old` and `new`, parents before
/// children. Subtrees with equal ids are never read; a subtree present on
/// one side only is walked whole (all of it changed).
pub(crate) fn changed_dirs(
    repo: &mut Repo,
    old: Option<GitSha>,
    new: Option<GitSha>,
) -> Result<Vec<DirDiff>, GitError> {
    let mut out = Vec::new();
    let mut stack = vec![DirDiff {
        path: Vec::new(),
        old,
        new,
    }];
    while let Some(d) = stack.pop() {
        if d.old == d.new {
            continue;
        }
        let old_entries = repo.tree_or_empty(d.old)?;
        let new_entries = repo.tree_or_empty(d.new)?;
        let mut subdirs: BTreeMap<&[u8], (Option<GitSha>, Option<GitSha>)> = BTreeMap::new();
        for e in old_entries.iter().filter(|e| e.is_tree()) {
            subdirs.entry(&e.name).or_default().0 = Some(e.sha);
        }
        for e in new_entries.iter().filter(|e| e.is_tree()) {
            subdirs.entry(&e.name).or_default().1 = Some(e.sha);
        }
        // Reverse, so the stack pops them in name order.
        for (name, (o, n)) in subdirs.into_iter().rev() {
            if o != n {
                stack.push(DirDiff {
                    path: join(&d.path, name),
                    old: o,
                    new: n,
                });
            }
        }
        out.push(d);
    }
    Ok(out)
}

/// A change as git sees it: the entry at `path` is now `new` (`None`:
/// removed). Kinds are graft's; a mode change graft cannot see is no change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitChange {
    pub path: RepoPath,
    pub new: Option<(EntryKind, GitSha)>,
}

/// The non-directory changes in `dirs`, sorted by path bytes. The paths must
/// already have passed [`check_paths`].
pub(crate) fn git_changes(repo: &mut Repo, dirs: &[DirDiff]) -> Result<Vec<GitChange>, GitError> {
    let mut out = Vec::new();
    for d in dirs {
        let old_entries = repo.tree_or_empty(d.old)?;
        let new_entries = repo.tree_or_empty(d.new)?;
        let leaf = |e: &crate::git::RawEntry| match e.kind() {
            Some(EntryKind::Tree) | None => None,
            Some(k) => Some((k, e.sha)),
        };
        let old: HashMap<&[u8], (EntryKind, GitSha)> = old_entries
            .iter()
            .filter_map(|e| leaf(e).map(|l| (e.name.as_slice(), l)))
            .collect();
        let new: HashMap<&[u8], (EntryKind, GitSha)> = new_entries
            .iter()
            .filter_map(|e| leaf(e).map(|l| (e.name.as_slice(), l)))
            .collect();
        let path = |name: &[u8]| {
            RepoPath::from_bytes(&join(&d.path, name))
                .map_err(|e| GitError::Read(format!("unchecked path: {e}")))
        };
        for (name, o) in &old {
            if !new.contains_key(name) {
                out.push(GitChange {
                    path: path(name)?,
                    new: None,
                });
            } else if new[name] != *o {
                out.push(GitChange {
                    path: path(name)?,
                    new: Some(new[name]),
                });
            }
        }
        for (name, n) in &new {
            if !old.contains_key(name) {
                out.push(GitChange {
                    path: path(name)?,
                    new: Some(*n),
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.as_str().as_bytes().cmp(b.path.as_str().as_bytes()));
    Ok(out)
}

/// A path rule broken by an entry: (path bytes, reason).
pub(crate) type Offence = (Vec<u8>, String);

/// Checks every entry of every new directory in `dirs` against graft's path
/// rules (decision D11): UTF-8, the component and length rules, portability
/// (including names that differ only in case), unique names, and a mode graft
/// knows. Only directories that changed are read, so the cost follows the
/// change.
pub(crate) fn check_paths(repo: &mut Repo, dirs: &[DirDiff]) -> Result<Vec<Offence>, GitError> {
    let mut out = Vec::new();
    for d in dirs {
        let Some(new) = d.new else { continue };
        let entries = repo.tree(new)?;
        let mut seen = HashSet::new();
        let mut folded: HashMap<String, usize> = HashMap::new();
        for e in entries.iter() {
            let full = join(&d.path, &e.name);
            if !seen.insert(e.name.as_slice()) {
                out.push((full, "duplicate-entry".to_owned()));
                continue;
            }
            if e.kind().is_none() {
                out.push((full.clone(), "unsupported-mode".to_owned()));
            }
            if e.name.contains(&b'/') {
                out.push((full, "slash-in-name".to_owned()));
                continue;
            }
            match RepoPath::from_bytes(&full) {
                Err(err) => out.push((full, err.reason().to_owned())),
                Ok(path) => {
                    let name = path.name();
                    if let Err(err) = check_component_portable(name) {
                        out.push((full, err.to_string().replace("invalid path: ", "")));
                    } else {
                        *folded.entry(name.to_lowercase()).or_default() += 1;
                    }
                }
            }
        }
        for e in entries.iter() {
            let Ok(name) = std::str::from_utf8(&e.name) else {
                continue;
            };
            if folded.get(&name.to_lowercase()).is_some_and(|&n| n > 1)
                && check_component_portable(name).is_ok()
            {
                out.push((
                    join(&d.path, &e.name),
                    "portability (case-clash)".to_owned(),
                ));
            }
        }
    }
    Ok(out)
}
