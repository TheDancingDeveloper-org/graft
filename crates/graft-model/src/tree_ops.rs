//! Pure tree algorithms: path-copying apply and id-pruned diff
//! (`docs/design.md` §4.9 root trees, §7.6 diff).
//!
//! Both read trees through [`ObjectSource`] and never hold a whole snapshot:
//! [`apply_changes`] reads and rewrites only the directories on the changed
//! paths, and [`diff_trees`] descends only into entries whose ids differ.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::changeset::{Change, Rule, Violation};
use crate::id::{ObjectId, TreeId};
use crate::path::{self, PathError, Portability, RepoPath};
use crate::tree::{EntryKind, Tree, TreeEntry};

/// Read access to trees, supplied by the caller (engine, client, git export).
pub trait ObjectSource: Send + Sync {
    /// Loads and verifies a tree.
    fn tree(&self, id: TreeId) -> impl Future<Output = Result<Tree, SourceError>> + Send;
}

impl<S: ObjectSource> ObjectSource for &S {
    fn tree(&self, id: TreeId) -> impl Future<Output = Result<Tree, SourceError>> + Send {
        (**self).tree(id)
    }
}

/// A failure reading from an [`ObjectSource`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// The object is not available.
    Missing(TreeId),
    /// The bytes did not hash to the id.
    Corrupt(TreeId),
    /// The tree decoded but breaks a model rule (for example, a path under
    /// it is longer than 4,096 bytes).
    Malformed(TreeId),
    /// Any other backend failure, described.
    Backend(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Missing(id) => write!(f, "tree {id} is missing"),
            SourceError::Corrupt(id) => write!(f, "tree {id} is corrupt"),
            SourceError::Malformed(id) => write!(f, "tree {id} is malformed"),
            SourceError::Backend(what) => write!(f, "object source failed: {what}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// The result of applying changes: the new root and the trees that must be
/// written (only those on the changed paths).
#[derive(Clone, Debug, Default)]
pub struct Applied {
    /// The new root tree id, or `None` when the snapshot is now empty.
    pub root: Option<TreeId>,
    /// Newly created trees, children before parents, each once.
    pub new_trees: Vec<(TreeId, Tree)>,
}

/// Why changes could not be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// A rule from design §4.5 was broken.
    Invalid(Violation),
    /// Reading a tree failed.
    Source(SourceError),
}

impl From<SourceError> for ApplyError {
    fn from(e: SourceError) -> Self {
        ApplyError::Source(e)
    }
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::Invalid(v) => v.fmt(f),
            ApplyError::Source(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Options for [`apply_changes_with`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyOptions {
    /// Reject a new name that differs only in case from another name in the
    /// same directory (`InvalidPath { reason: "portability" }`). Only names
    /// this apply adds are checked, so existing history never blocks an
    /// unrelated change.
    pub case_clash: bool,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        ApplyOptions { case_clash: true }
    }
}

/// Applies `changes` to the snapshot rooted at `root` by path copying, with
/// the default [`ApplyOptions`]. Costs O(changes × depth) reads and writes;
/// untouched subtrees keep their ids.
pub async fn apply_changes<S: ObjectSource>(
    source: &S,
    root: Option<TreeId>,
    changes: &[Change],
) -> Result<Applied, ApplyError> {
    apply_changes_with(source, root, changes, &ApplyOptions::default()).await
}

/// Applies `changes` to the snapshot rooted at `root` by path copying.
///
/// Checks validity rules 3 (each path once), 5 (a `Delete` names an existing
/// non-directory entry), 6 (no no-op `Put`) and 7 (no path both a file and a
/// directory). A directory whose last entry is removed is removed. A file may
/// be replaced by a directory, or a directory by a file, in one changeset by
/// deleting the old entries and putting the new ones.
pub async fn apply_changes_with<S: ObjectSource>(
    source: &S,
    root: Option<TreeId>,
    changes: &[Change],
    opts: &ApplyOptions,
) -> Result<Applied, ApplyError> {
    let mut trie = Node::default();
    for change in changes {
        trie.insert(change)?;
    }
    let mut out = Out::default();
    let root = apply_dir(source, root, &trie, None, opts, &mut out).await?;
    Ok(Applied {
        root,
        new_trees: out.trees,
    })
}

/// The changes under one directory, keyed by component.
#[derive(Default)]
struct Node<'c> {
    /// The change at exactly this path.
    leaf: Option<&'c Change>,
    /// Changes beneath this path, by next component.
    children: BTreeMap<&'c str, Node<'c>>,
}

impl<'c> Node<'c> {
    fn insert(&mut self, change: &'c Change) -> Result<(), ApplyError> {
        let mut node = self;
        for component in change.path().components() {
            node = node.children.entry(component).or_default();
        }
        if node.leaf.is_some() {
            return Err(invalid(change.path(), Rule::DuplicatePath));
        }
        node.leaf = Some(change);
        Ok(())
    }
}

#[derive(Default)]
struct Out {
    trees: Vec<(TreeId, Tree)>,
    seen: HashSet<TreeId>,
}

fn invalid(path: &RepoPath, rule: Rule) -> ApplyError {
    ApplyError::Invalid(Violation::new(Some(path), rule))
}

fn child_path(dir: Option<&RepoPath>, name: &str) -> Result<RepoPath, ApplyError> {
    path::child(dir, name).map_err(|e| {
        let at = dir
            .cloned()
            .unwrap_or_else(|| RepoPath::new(name).expect("valid name"));
        invalid(&at, Rule::Path(e))
    })
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Rewrites one directory. Returns the new tree id, or `None` when the
/// directory ends up empty.
fn apply_dir<'a, S: ObjectSource>(
    source: &'a S,
    existing: Option<TreeId>,
    node: &'a Node<'a>,
    dir: Option<RepoPath>,
    opts: &'a ApplyOptions,
    out: &'a mut Out,
) -> BoxFuture<'a, Result<Option<TreeId>, ApplyError>> {
    Box::pin(async move {
        let base = match existing {
            Some(id) => source.tree(id).await?.entries,
            None => Vec::new(),
        };
        let mut entries = Vec::with_capacity(base.len() + node.children.len());
        let mut added: Vec<usize> = Vec::new();
        let mut base_iter = base.into_iter().peekable();
        for (&name, child) in &node.children {
            while let Some(e) = base_iter.next_if(|e| e.name.as_bytes() < name.as_bytes()) {
                entries.push(e);
            }
            let old = base_iter.next_if(|e| e.name == name);
            let was_present = old.is_some();
            let new = apply_entry(source, old, name, child, dir.as_ref(), opts, out).await?;
            if let Some(e) = new {
                if !was_present {
                    added.push(entries.len());
                }
                entries.push(e);
            }
        }
        entries.extend(base_iter);

        if opts.case_clash && !added.is_empty() {
            check_case_clash(&entries, &added, dir.as_ref())?;
        }
        if entries.is_empty() {
            return Ok(None);
        }
        let tree = Tree { entries };
        let id = tree.id();
        if Some(id) != existing && out.seen.insert(id) {
            out.trees.push((id, tree));
        }
        Ok(Some(id))
    })
}

/// Resolves one name in a directory: the existing entry `old` (if any) and
/// the changes at or beneath it.
async fn apply_entry<S: ObjectSource>(
    source: &S,
    old: Option<TreeEntry>,
    name: &str,
    node: &Node<'_>,
    dir: Option<&RepoPath>,
    opts: &ApplyOptions,
    out: &mut Out,
) -> Result<Option<TreeEntry>, ApplyError> {
    let path = match node.leaf {
        Some(c) => c.path().clone(),
        None => child_path(dir, name)?,
    };
    let old_is_tree = old.as_ref().is_some_and(|e| e.kind.is_tree());

    // Changes beneath this name: rebuild the subdirectory first, so that
    // emptying a directory frees the name for a file.
    let mut sub = None;
    if !node.children.is_empty() {
        let base = match &old {
            Some(e) if e.kind.is_tree() => Some(TreeId(e.id)),
            Some(_) if matches!(node.leaf, Some(Change::Delete { .. })) => None,
            Some(_) => return Err(invalid(&path, Rule::FileAndDirectory)),
            None => None,
        };
        sub = apply_dir(source, base, node, Some(path.clone()), opts, out).await?;
    }
    let sub_entry = |id: TreeId| TreeEntry {
        name: name.to_owned(),
        kind: EntryKind::Tree,
        id: id.0,
    };

    match node.leaf {
        None => Ok(sub.map(sub_entry)),
        Some(Change::Delete { .. }) => match &old {
            Some(e) if !e.kind.is_tree() => Ok(sub.map(sub_entry)),
            _ => Err(invalid(&path, Rule::DeleteMissing)),
        },
        Some(&Change::Put { kind, id, .. }) => {
            if kind.is_tree() {
                return Err(invalid(&path, Rule::PutKind));
            }
            if sub.is_some() || (old_is_tree && node.children.is_empty()) {
                return Err(invalid(&path, Rule::FileAndDirectory));
            }
            if old.as_ref().is_some_and(|e| e.kind == kind && e.id == id) {
                return Err(invalid(&path, Rule::PutNoOp));
            }
            Ok(Some(TreeEntry {
                name: name.to_owned(),
                kind,
                id,
            }))
        }
    }
}

/// Rejects an added name that case-folds to another name in `entries`.
fn check_case_clash(
    entries: &[TreeEntry],
    added: &[usize],
    dir: Option<&RepoPath>,
) -> Result<(), ApplyError> {
    let mut folded: HashMap<String, usize> = HashMap::with_capacity(entries.len());
    for e in entries {
        *folded.entry(path::case_fold(&e.name)).or_default() += 1;
    }
    for &i in added {
        let name = &entries[i].name;
        if folded[&path::case_fold(name)] > 1 {
            let at = child_path(dir, name)?;
            return Err(invalid(
                &at,
                Rule::Path(PathError::NotPortable(Portability::CaseClash)),
            ));
        }
    }
    Ok(())
}

/// One page of a diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffPage {
    /// Changes that turn `from` into `to`, sorted by path bytes. A removed
    /// directory appears as a `Delete` of every file under it.
    pub changes: Vec<Change>,
    /// When the page was truncated, the last path in it: pass it as `after`
    /// to [`diff_trees_after`] for the next page.
    pub next: Option<RepoPath>,
}

/// Diffs two snapshots, recursing only into entries whose ids differ.
/// Returns at most `limit` changes (at least one), with a resume cursor when
/// truncated.
pub async fn diff_trees<S: ObjectSource>(
    source: &S,
    from: Option<TreeId>,
    to: Option<TreeId>,
    limit: usize,
) -> Result<DiffPage, SourceError> {
    diff_trees_after(source, from, to, None, limit).await
}

/// Like [`diff_trees`], but returns only changes at paths after `after`.
/// Subtrees wholly at or before `after` are skipped without being read.
pub async fn diff_trees_after<S: ObjectSource>(
    source: &S,
    from: Option<TreeId>,
    to: Option<TreeId>,
    after: Option<&RepoPath>,
    limit: usize,
) -> Result<DiffPage, SourceError> {
    let limit = limit.max(1);
    let mut ctx = DiffCtx {
        after,
        // One extra change tells us whether the page is truncated.
        want: limit + 1,
        out: Vec::new(),
    };
    diff_dir(source, from, to, None, &mut ctx).await?;
    let mut changes = ctx.out;
    let next = if changes.len() > limit {
        changes.truncate(limit);
        changes.last().map(|c| c.path().clone())
    } else {
        None
    };
    Ok(DiffPage { changes, next })
}

struct DiffCtx<'a> {
    after: Option<&'a RepoPath>,
    want: usize,
    out: Vec<Change>,
}

impl DiffCtx<'_> {
    fn full(&self) -> bool {
        self.out.len() >= self.want
    }
}

/// One thing to visit in a directory, in full-path byte order: a name's
/// non-directory side (key `name`) or its directory side (key `name/`).
struct Item<'t> {
    key: String,
    name: &'t str,
    from: Option<&'t TreeEntry>,
    to: Option<&'t TreeEntry>,
    is_dir: bool,
}

fn diff_dir<'a, S: ObjectSource>(
    source: &'a S,
    from: Option<TreeId>,
    to: Option<TreeId>,
    dir: Option<RepoPath>,
    ctx: &'a mut DiffCtx<'_>,
) -> BoxFuture<'a, Result<(), SourceError>> {
    Box::pin(async move {
        if from == to || ctx.full() {
            return Ok(());
        }
        let from_tree = match from {
            Some(id) => source.tree(id).await?,
            None => Tree::default(),
        };
        let to_tree = match to {
            Some(id) => source.tree(id).await?,
            None => Tree::default(),
        };
        let malformed = || SourceError::Malformed(to.or(from).expect("one side is a tree"));

        let mut items = Vec::new();
        let (mut fi, mut ti) = (
            from_tree.entries.iter().peekable(),
            to_tree.entries.iter().peekable(),
        );
        loop {
            let (f, t) = match (fi.peek(), ti.peek()) {
                (None, None) => break,
                (Some(f), Some(t)) if f.name == t.name => (fi.next(), ti.next()),
                (Some(f), Some(t)) if f.name.as_bytes() < t.name.as_bytes() => (fi.next(), None),
                (Some(_), None) => (fi.next(), None),
                (_, Some(_)) => (None, ti.next()),
            };
            push_items(&mut items, f, t);
        }
        items.sort_by(|a, b| a.key.as_bytes().cmp(b.key.as_bytes()));

        for item in items {
            if ctx.full() {
                break;
            }
            let path = path::child(dir.as_ref(), item.name).map_err(|_| malformed())?;
            if item.is_dir {
                let (f, t) = (
                    item.from.map(|e| TreeId(e.id)),
                    item.to.map(|e| TreeId(e.id)),
                );
                if f == t || dir_before(&path, ctx.after) {
                    continue;
                }
                diff_dir(source, f, t, Some(path), ctx).await?;
            } else {
                if ctx
                    .after
                    .is_some_and(|a| path.as_str().as_bytes() <= a.as_str().as_bytes())
                {
                    continue;
                }
                match (item.from, item.to) {
                    (Some(f), Some(t)) if f.kind == t.kind && f.id == t.id => {}
                    (_, Some(t)) => ctx.out.push(put(path, t.kind, t.id)),
                    (Some(_), None) => ctx.out.push(Change::Delete { path }),
                    (None, None) => {}
                }
            }
        }
        Ok(())
    })
}

/// Splits one name's two sides into a non-directory item and a directory
/// item, as present.
fn push_items<'t>(items: &mut Vec<Item<'t>>, f: Option<&'t TreeEntry>, t: Option<&'t TreeEntry>) {
    let name = f.or(t).map(|e| e.name.as_str()).expect("one side");
    let file = |e: Option<&'t TreeEntry>| e.filter(|e| !e.kind.is_tree());
    let tree = |e: Option<&'t TreeEntry>| e.filter(|e| e.kind.is_tree());
    if file(f).is_some() || file(t).is_some() {
        items.push(Item {
            key: name.to_owned(),
            name,
            from: file(f),
            to: file(t),
            is_dir: false,
        });
    }
    if tree(f).is_some() || tree(t).is_some() {
        items.push(Item {
            key: format!("{name}/"),
            name,
            from: tree(f),
            to: tree(t),
            is_dir: true,
        });
    }
}

/// Whether every path under directory `dir` sorts at or before `after`.
fn dir_before(dir: &RepoPath, after: Option<&RepoPath>) -> bool {
    let Some(after) = after else { return false };
    let prefix = format!("{dir}/");
    !after.as_str().starts_with(&prefix) && prefix.as_bytes() < after.as_str().as_bytes()
}

fn put(path: RepoPath, kind: EntryKind, id: ObjectId) -> Change {
    Change::Put { path, kind, id }
}
