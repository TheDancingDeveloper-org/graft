//! Line path index rows (`docs/design.md` §4.9, §7.5).

use std::collections::BTreeSet;

use graft_model::{Changeset, ChangesetId, RepoPath};
use graft_store::Write;

/// Every changed path of `cs` and each of its ancestor directories, once
/// each, sorted. At most `changes × depth` entries.
pub fn path_prefixes(cs: &Changeset) -> BTreeSet<RepoPath> {
    let mut out = BTreeSet::new();
    for change in &cs.changes {
        let mut path = Some(change.path().clone());
        while let Some(p) = path {
            let parent = p.parent();
            // A directory already present had its own ancestors added.
            if !out.insert(p) {
                break;
            }
            path = parent;
        }
    }
    out
}

/// The line path index rows for `cs` (whose id is `id`) at `pos` on `line`:
/// one per entry of [`path_prefixes`].
pub fn line_path_rows(line: &str, pos: u64, id: ChangesetId, cs: &Changeset) -> Vec<Write> {
    path_prefixes(cs)
        .into_iter()
        .map(|path| Write::PutLinePath {
            line: line.to_owned(),
            path,
            pos,
            changeset: id,
        })
        .collect()
}
