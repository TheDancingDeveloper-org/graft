//! Pure tree algorithms: path-copying apply and id-pruned diff
//! (`docs/design.md` §4.9 root trees, §7.6 diff).

use std::future::Future;

use crate::changeset::Change;
use crate::id::TreeId;
use crate::tree::Tree;

/// Read access to trees, supplied by the caller (engine, client, git export).
pub trait ObjectSource: Send + Sync {
    /// Loads and verifies a tree.
    fn tree(&self, id: TreeId) -> impl Future<Output = Result<Tree, SourceError>> + Send;
}

/// A failure reading from an [`ObjectSource`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// The object is not available.
    Missing(TreeId),
    /// The bytes did not hash to the id.
    Corrupt(TreeId),
    /// Any other backend failure, described.
    Backend(String),
}

/// The result of applying changes: the new root and the trees that must be
/// written (only those on the changed paths).
#[derive(Clone, Debug, Default)]
pub struct Applied {
    /// The new root tree id.
    pub root: Option<TreeId>,
    /// Newly created trees, children before parents.
    pub new_trees: Vec<(TreeId, Tree)>,
}

/// Why changes could not be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// A rule from design §4.5 was broken at this path.
    Invalid {
        /// The offending path.
        path: String,
        /// The rule, by name.
        rule: &'static str,
    },
    /// Reading a tree failed.
    Source(SourceError),
}

/// Applies `changes` to the snapshot rooted at `root` by path copying.
/// Costs O(changes × depth); untouched subtrees keep their ids.
pub async fn apply_changes<S: ObjectSource>(
    _source: &S,
    _root: Option<TreeId>,
    _changes: &[Change],
) -> Result<Applied, ApplyError> {
    todo!("G1.1: path-copying apply")
}

/// Diffs two snapshots, recursing only into entries whose ids differ.
/// Returns changes that turn `from` into `to`, sorted by path, up to `limit`,
/// plus a resume cursor when truncated.
pub async fn diff_trees<S: ObjectSource>(
    _source: &S,
    _from: Option<TreeId>,
    _to: Option<TreeId>,
    _limit: usize,
) -> Result<(Vec<Change>, Option<String>), SourceError> {
    todo!("G1.1: id-pruned tree diff")
}
