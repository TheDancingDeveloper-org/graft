//! `Diff` (`docs/design.md` §7.6): file changes between two snapshots,
//! recursing only into subtrees whose ids differ, optionally restricted to
//! path prefixes, paged by the last path returned, with optional unified
//! patches (histogram diff) capped per file and per response.

use graft_model::{
    Change, EntryKind, ObjectId, ObjectKind, RepoPath, TreeEntry, TreeId, diff_trees_after,
};
use graft_proto::types::{Diff, DiffResult, FileDiff, PatchOptions};
use graft_proto::{ApiError, CallContext, Frame};
use graft_store::{MetaStore, ObjectStore, RepoId};
use std::fmt;

use imara_diff::{
    Algorithm, InternedInput, Interner, Token, UnifiedDiffConfig, UnifiedDiffPrinter,
};

use crate::access::{Need, require};
use crate::source::{Trees, model_root};
use crate::{Engine, errors};

/// The most context lines a patch carries.
pub const MAX_CONTEXT: u32 = 1_000;

/// `Put` at `path` for an entry, or `Delete` when absent.
fn change_to(path: RepoPath, to: Option<&TreeEntry>) -> Change {
    match to {
        Some(e) => Change::Put {
            path,
            kind: e.kind,
            id: e.id,
        },
        None => Change::Delete { path },
    }
}

fn same(a: Option<&TreeEntry>, b: Option<&TreeEntry>) -> bool {
    a.map(|e| (e.kind, e.id)) == b.map(|e| (e.kind, e.id))
}

/// Drops prefixes that lie beneath another prefix, and duplicates.
fn normalize(mut paths: Vec<RepoPath>) -> Vec<RepoPath> {
    paths.sort();
    paths.dedup();
    let all = paths.clone();
    paths.retain(|p| !all.iter().any(|q| q != p && p.starts_with(q)));
    paths
}

/// Whether a kind's blob can be shown as text.
fn textual(kind: EntryKind) -> bool {
    matches!(kind, EntryKind::File | EntryKind::Exec | EntryKind::Symlink)
}

/// Prints hunks as GNU diff does: an empty range names the line before it
/// (`-0,0` for an insertion at the top), and a last line without a newline
/// is followed by `\ No newline at end of file`.
struct Printer<'a>(&'a Interner<&'a str>);

impl Printer<'_> {
    fn line(&self, mut f: impl fmt::Write, sign: char, token: Token) -> fmt::Result {
        let line = self.0[token];
        write!(f, "{sign}{line}")?;
        if !line.ends_with('\n') {
            f.write_str("\n\\ No newline at end of file\n")?;
        }
        Ok(())
    }
}

impl UnifiedDiffPrinter for Printer<'_> {
    fn display_header(
        &self,
        mut f: impl fmt::Write,
        start_before: u32,
        start_after: u32,
        len_before: u32,
        len_after: u32,
    ) -> fmt::Result {
        let start = |s: u32, len: u32| if len == 0 { s } else { s + 1 };
        writeln!(
            f,
            "@@ -{},{len_before} +{},{len_after} @@",
            start(start_before, len_before),
            start(start_after, len_after)
        )
    }

    fn display_context_token(&self, f: impl fmt::Write, token: Token) -> fmt::Result {
        self.line(f, ' ', token)
    }

    fn display_hunk(
        &self,
        mut f: impl fmt::Write,
        before: &[Token],
        after: &[Token],
    ) -> fmt::Result {
        for &t in before {
            self.line(&mut f, '-', t)?;
        }
        for &t in after {
            self.line(&mut f, '+', t)?;
        }
        Ok(())
    }
}

/// A unified diff of one file, with `---`/`+++` headers.
fn unified(path: &RepoPath, old: Option<&str>, new: Option<&str>, context: u32) -> String {
    let mut out = String::new();
    out.push_str(&match old {
        Some(_) => format!("--- a/{path}\n"),
        None => "--- /dev/null\n".to_owned(),
    });
    out.push_str(&match new {
        Some(_) => format!("+++ b/{path}\n"),
        None => "+++ /dev/null\n".to_owned(),
    });
    let input = InternedInput::new(old.unwrap_or(""), new.unwrap_or(""));
    let mut diff = imara_diff::Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let mut config = UnifiedDiffConfig::default();
    config.context_len(context.min(MAX_CONTEXT));
    let printer = Printer(&input.interner);
    out.push_str(&diff.unified_diff(&printer, config, &input).to_string());
    out
}

/// Cuts `patch` to at most `cap` bytes, at a line end where there is one.
fn cap_patch(patch: &mut String, cap: usize) -> bool {
    if patch.len() <= cap {
        return false;
    }
    let mut end = cap;
    while !patch.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(nl) = patch[..end].rfind('\n') {
        end = nl + 1;
    }
    patch.truncate(end);
    true
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    pub(crate) async fn do_diff(
        &self,
        cx: &CallContext,
        req: Diff,
    ) -> Result<DiffResult, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        if req.paths.len() > self.config.max_diff_paths as usize {
            return Err(errors::limit(
                "diff-paths",
                self.config.max_diff_paths.into(),
            ));
        }
        let from = self.root_of(repo, req.from).await?;
        let to = self.root_of(repo, req.to).await?;
        let mut limit = self.page_size(req.page.limit) as usize;
        if req.patch.is_some() {
            limit = limit.min(self.config.max_patch_files as usize);
        }
        let after = req
            .page
            .token
            .as_deref()
            .map(|t| RepoPath::new(t).map_err(|e| errors::path(t, e)))
            .transpose()?;
        let trees = Trees::new(&*self.objects, repo);

        // One more than the page, to learn whether it is the last.
        let mut changes = if req.paths.is_empty() {
            diff_trees_after(
                &trees,
                model_root(from),
                model_root(to),
                after.as_ref(),
                limit + 1,
            )
            .await
            .map_err(errors::source)?
            .changes
        } else {
            let mut all = Vec::new();
            for prefix in normalize(req.paths) {
                all.extend(
                    diff_prefix(&trees, from, to, &prefix, after.as_ref(), limit + 1).await?,
                );
            }
            all.sort_by(|a, b| {
                a.path()
                    .as_str()
                    .as_bytes()
                    .cmp(b.path().as_str().as_bytes())
            });
            all
        };
        let next = if changes.len() > limit {
            changes.truncate(limit);
            changes.last().map(|c| c.path().to_string())
        } else {
            None
        };

        let mut files: Vec<FileDiff> = changes
            .into_iter()
            .map(|change| FileDiff {
                change,
                patch: None,
                truncated: false,
            })
            .collect();
        if let Some(opts) = &req.patch {
            self.patches(repo, &trees, from, &mut files, opts).await?;
        }
        Ok(DiffResult { files, next })
    }

    /// Fills in unified patches for text files, within the per-file cap and
    /// the response budget. A file past the budget gets no patch and is
    /// marked truncated.
    async fn patches(
        &self,
        repo: RepoId,
        trees: &Trees<'_, O>,
        from: TreeId,
        files: &mut [FileDiff],
        opts: &PatchOptions,
    ) -> Result<(), ApiError> {
        let per_file = match opts.max_bytes_per_file {
            0 => self.config.default_patch_file_bytes,
            n => n.min(self.config.max_patch_file_bytes),
        } as usize;
        let mut budget = self.config.max_patch_bytes as usize;
        for file in files {
            if budget == 0 {
                file.truncated = true;
                continue;
            }
            let path = file.change.path();
            let old = trees
                .lookup(from, path)
                .await
                .map_err(errors::source)?
                .filter(|e| !e.kind.is_tree());
            let new = match &file.change {
                Change::Put { kind, id, .. } => Some((*kind, *id)),
                Change::Delete { .. } => None,
            };
            let old_text = match &old {
                None => None,
                Some(e) if textual(e.kind) => match self.text(repo, e.id).await? {
                    Some(t) => Some(t),
                    None => continue,
                },
                Some(_) => continue,
            };
            let new_text = match new {
                None => None,
                Some((kind, id)) if textual(kind) => match self.text(repo, id).await? {
                    Some(t) => Some(t),
                    None => continue,
                },
                Some(_) => continue,
            };
            let mut patch = unified(path, old_text.as_deref(), new_text.as_deref(), opts.context);
            file.truncated = cap_patch(&mut patch, per_file.min(budget));
            budget -= patch.len();
            file.patch = Some(patch);
        }
        Ok(())
    }

    /// A blob as text, or `None` when it is larger than the patch input cap,
    /// not UTF-8, or contains NUL.
    async fn text(&self, repo: RepoId, id: ObjectId) -> Result<Option<String>, ApiError> {
        let raw = self
            .objects
            .get(repo, id)
            .await
            .map_err(errors::store)?
            .ok_or(ApiError::MissingObjects { ids: vec![id] })?;
        let frame = Frame::from_bytes(&raw)
            .ok()
            .filter(|f| f.kind == ObjectKind::Blob)
            .ok_or(ApiError::CorruptObject { id })?;
        if frame.uncompressed_len > self.config.max_patch_input_bytes {
            return Ok(None);
        }
        let payload = frame
            .decode_verified(id)
            .map_err(|_| ApiError::CorruptObject { id })?;
        Ok(String::from_utf8(payload)
            .ok()
            .filter(|s| !s.contains('\0')))
    }
}

/// The changes at and beneath `prefix`, after `after`, at most `limit`.
async fn diff_prefix<O: ObjectStore>(
    trees: &Trees<'_, O>,
    from: TreeId,
    to: TreeId,
    prefix: &RepoPath,
    after: Option<&RepoPath>,
    limit: usize,
) -> Result<Vec<Change>, ApiError> {
    let f = trees.lookup(from, prefix).await.map_err(errors::source)?;
    let t = trees.lookup(to, prefix).await.map_err(errors::source)?;
    let mut out = Vec::new();

    // The non-directory side at the prefix itself.
    let (ff, tf) = (
        f.as_ref().filter(|e| !e.kind.is_tree()),
        t.as_ref().filter(|e| !e.kind.is_tree()),
    );
    let past = |p: &RepoPath| after.is_none_or(|a| p.as_str().as_bytes() > a.as_str().as_bytes());
    if !same(ff, tf) && past(prefix) {
        out.push(change_to(prefix.clone(), tf));
    }

    // The directory side: every path beneath sorts as one contiguous range
    // starting `prefix/`.
    let dir = |e: &Option<TreeEntry>| {
        e.as_ref()
            .filter(|e| e.kind.is_tree())
            .map(|e| TreeId(e.id))
    };
    let (fd, td) = (dir(&f), dir(&t));
    if fd == td {
        return Ok(out);
    }
    let base = format!("{prefix}/");
    let rel_after = match after {
        None => None,
        Some(a) => match a.as_str().strip_prefix(&base) {
            Some(rel) => Some(RepoPath::new(rel).map_err(|e| errors::path(a.as_str(), e))?),
            None if a.as_str().as_bytes() < base.as_bytes() => None,
            // The token is past the whole range.
            None => return Ok(out),
        },
    };
    let page = diff_trees_after(trees, fd, td, rel_after.as_ref(), limit)
        .await
        .map_err(errors::source)?;
    for c in page.changes {
        let full = |p: &RepoPath| {
            RepoPath::new(&format!("{base}{p}")).map_err(|e| errors::path(p.as_str(), e))
        };
        out.push(match c {
            Change::Put { path, kind, id } => Change::Put {
                path: full(&path)?,
                kind,
                id,
            },
            Change::Delete { path } => Change::Delete { path: full(&path)? },
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).expect("valid path")
    }

    #[test]
    fn nested_prefixes_collapse() {
        assert_eq!(
            normalize(vec![p("a/b"), p("a"), p("c"), p("c"), p("ab")]),
            vec![p("a"), p("ab"), p("c")]
        );
    }

    #[test]
    fn unified_patch_has_headers_and_hunks() {
        let patch = unified(&p("f.txt"), Some("a\nb\nc\n"), Some("a\nB\nc\n"), 1);
        assert_eq!(
            patch,
            "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"
        );
        let added = unified(&p("n"), None, Some("x\n"), 3);
        assert_eq!(added, "--- /dev/null\n+++ b/n\n@@ -0,0 +1,1 @@\n+x\n");
        let no_newline = unified(&p("n"), Some("x"), Some("y"), 3);
        assert_eq!(
            no_newline,
            "--- a/n\n+++ b/n\n@@ -1,1 +1,1 @@\n-x\n\\ No newline at end of file\n+y\n\\ No newline at end of file\n"
        );
    }

    #[test]
    fn patches_are_cut_at_a_line_end() {
        let mut s = "line one\nline two\n".to_owned();
        assert!(cap_patch(&mut s, 12));
        assert_eq!(s, "line one\n");
        let mut short = "ok\n".to_owned();
        assert!(!cap_patch(&mut short, 10));
    }
}
