//! `graft status --upstream` (WI-2309): what the line did since the base,
//! and whether the author's work would merge onto its tip. Read-only: it
//! writes nothing on the server or on disk and moves no base or head.
//!
//! The author's paths are the working copy's changes (uncommitted edits
//! count) and the stack's (`Diff` base..head). Each is checked against the
//! line with `Diff` base..tip restricted to those paths, so the server's
//! id-pruned tree diff touches only them, never history (AGENTS.md rule 2).
//! A path both sides changed is predicted with the decision D16 line merge
//! (`merge3`) on the base, the local file (or the head's entry outside the
//! slice) and the tip. With a stack, the server's `GetConflicts` (the sync
//! planner, run in memory) is asked too, since drafts rebase one by one: a
//! path it reports conflicts even when the net content would merge.
//!
//! Every list is bounded by the caller's limit and says when it was cut.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;

use graft_model::merge::{MAX_TEXT_BYTES, Merge3, is_text, merge3};
use graft_model::{Change, ChangesetId, EntryKind, RepoPath};
use graft_proto::api::{RepoApi, WorkspaceApi};
use graft_proto::types::{Diff, GetConflicts, Log, Page, PathAt};
use graft_proto::{ApiError, CallContext};

use super::WorkingCopy;
use crate::{Error, Result};

/// Paths per `Diff` request (the server's default `max_diff_paths`).
const DIFF_PATHS: usize = 64;

/// The most of the author's own paths checked against the line; beyond
/// this the overlap check is cut and says so.
pub const MAX_OWN_PATHS: usize = 1024;

/// Entries per page when listing the stack's changes.
const OWN_PAGE: u32 = 256;

/// How an overlapping path would merge onto the tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The line merge is clean (or both sides made the same change).
    Clean,
    /// The line merge conflicts. `hunks` are 1-based, inclusive line ranges
    /// in the author's version; empty when there is no line merge to
    /// locate them (`reason` says why).
    Conflict {
        /// Conflicting line ranges in the author's file.
        hunks: Vec<(usize, usize)>,
        /// Why no line merge applied, when none did.
        reason: Option<&'static str>,
    },
}

/// A path the line changed since the base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamChange {
    /// The path.
    pub path: RepoPath,
    /// True when the line deleted it.
    pub deleted: bool,
}

/// A path both the line and the author changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    /// The path.
    pub path: RepoPath,
    /// True when the working copy has uncommitted edits to it.
    pub uncommitted: bool,
    /// The prediction.
    pub outcome: Outcome,
}

/// What [`WorkingCopy::upstream`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    /// The line.
    pub line: String,
    /// The working copy's base.
    pub base: ChangesetId,
    /// The head draft.
    pub head: Option<ChangesetId>,
    /// The line tip now.
    pub tip: ChangesetId,
    /// Changesets on the line after the base, counted up to the limit.
    pub behind: u64,
    /// More than `behind` changesets are on the line after the base.
    pub behind_more: bool,
    /// This page of the paths the line changed since the base.
    pub changed: Vec<UpstreamChange>,
    /// Resume token for the next page of `changed`.
    pub next: Option<String>,
    /// The author's paths the line also changed, with their predictions.
    pub overlaps: Vec<Overlap>,
    /// The overlap list was cut (by the limit or [`MAX_OWN_PATHS`]).
    pub overlaps_truncated: bool,
    /// The bottom-most draft the server's sync planner reports conflicting.
    pub stack_conflict: Option<ChangesetId>,
}

impl Upstream {
    /// True when no overlapping path conflicts.
    pub fn clean(&self) -> bool {
        self.stack_conflict.is_none()
            && self
                .overlaps
                .iter()
                .all(|o| matches!(o.outcome, Outcome::Clean))
    }
}

/// One side of a three-way merge: the entry kind and its bytes, or absent.
type Side = Option<(EntryKind, Vec<u8>)>;

impl WorkingCopy {
    /// The line's changes since the base and how the author's work would
    /// merge onto its tip. `limit` bounds every list; `token` resumes the
    /// `changed` page. Reads only.
    pub async fn upstream<A: RepoApi + WorkspaceApi>(
        &self,
        api: &A,
        cx: &CallContext,
        limit: u32,
        token: Option<String>,
    ) -> Result<Upstream> {
        let limit = limit.max(1);
        let base = self.state.base;
        let head = self.state.head;
        let tip = self.tip(api, cx).await?;
        let mut out = Upstream {
            line: self.state.line.clone(),
            base,
            head,
            tip,
            behind: 0,
            behind_more: false,
            changed: Vec::new(),
            next: None,
            overlaps: Vec::new(),
            overlaps_truncated: false,
            stack_conflict: None,
        };
        if tip == base {
            return Ok(out);
        }
        let history = api
            .log(
                cx,
                Log {
                    repo: self.config.repo.clone(),
                    from: tip,
                    until: Some(base),
                    paths: Vec::new(),
                    page: Page { limit, token: None },
                },
            )
            .await?;
        out.behind = history.changesets.len() as u64;
        out.behind_more = history.next.is_some();
        let page = api
            .diff(cx, self.diff_req(base, tip, Vec::new(), limit, token))
            .await?;
        out.changed = page
            .files
            .into_iter()
            .map(|f| UpstreamChange {
                deleted: matches!(f.change, Change::Delete { .. }),
                path: f.change.path().clone(),
            })
            .collect();
        out.next = page.next;

        let uncommitted: BTreeSet<RepoPath> =
            self.status()?.iter().map(|s| s.path().clone()).collect();
        let mut own = uncommitted.clone();
        if let Some(head) = head {
            self.stack_paths(api, cx, base, head, &mut own).await?;
        }
        if own.len() > MAX_OWN_PATHS {
            out.overlaps_truncated = true;
        }
        let own: Vec<RepoPath> = own.into_iter().take(MAX_OWN_PATHS).collect();
        let theirs = self
            .theirs(api, cx, base, tip, &own, limit as usize, &mut out)
            .await?;
        let mut stack = BTreeSet::new();
        if head.is_some()
            && let Some(c) = self.stack_conflicts(api, cx, tip, limit).await?
        {
            out.stack_conflict = c.draft;
            stack = c.paths;
        }
        for change in theirs {
            let path = change.path().clone();
            let base_side = self.read(api, cx, base, &path).await?;
            let ours = self.ours_side(api, cx, &path).await?;
            if ours == base_side {
                // The author's net change is nothing: the tip's wins.
                continue;
            }
            let theirs_side = match &change {
                Change::Delete { .. } => None,
                Change::Put { .. } => self.read(api, cx, tip, &path).await?,
            };
            let mut outcome = predict(&base_side, &ours, &theirs_side);
            if stack.contains(&path) && outcome == Outcome::Clean {
                outcome = Outcome::Conflict {
                    hunks: Vec::new(),
                    reason: Some("stack"),
                };
            }
            out.overlaps.push(Overlap {
                uncommitted: uncommitted.contains(&path),
                path,
                outcome,
            });
        }
        Ok(out)
    }

    fn diff_req(
        &self,
        from: ChangesetId,
        to: ChangesetId,
        paths: Vec<RepoPath>,
        limit: u32,
        token: Option<String>,
    ) -> Diff {
        Diff {
            repo: self.config.repo.clone(),
            from,
            to,
            paths,
            patch: None,
            page: Page { limit, token },
        }
    }

    /// Adds the stack's changed paths to `own`, up to [`MAX_OWN_PATHS`]
    /// beyond which the caller reports the cut.
    async fn stack_paths<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        base: ChangesetId,
        head: ChangesetId,
        own: &mut BTreeSet<RepoPath>,
    ) -> Result<()> {
        let mut token = None;
        loop {
            let page = api
                .diff(cx, self.diff_req(base, head, Vec::new(), OWN_PAGE, token))
                .await?;
            own.extend(page.files.into_iter().map(|f| f.change.path().clone()));
            match page.next {
                Some(next) if own.len() <= MAX_OWN_PATHS => token = Some(next),
                _ => return Ok(()),
            }
        }
    }

    /// The line's changes base..tip to the author's paths, `DIFF_PATHS` at
    /// a time, up to `limit` of them.
    #[allow(clippy::too_many_arguments)]
    async fn theirs<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        base: ChangesetId,
        tip: ChangesetId,
        own: &[RepoPath],
        limit: usize,
        out: &mut Upstream,
    ) -> Result<Vec<Change>> {
        let mut found = Vec::new();
        for chunk in own.chunks(DIFF_PATHS) {
            let wanted: BTreeSet<&RepoPath> = chunk.iter().collect();
            let mut token = None;
            loop {
                let page = api
                    .diff(
                        cx,
                        self.diff_req(base, tip, chunk.to_vec(), DIFF_PATHS as u32, token),
                    )
                    .await?;
                for f in page.files {
                    // A path filter is a prefix: keep only the exact paths.
                    if !wanted.contains(f.change.path()) {
                        continue;
                    }
                    if found.len() == limit {
                        out.overlaps_truncated = true;
                        return Ok(found);
                    }
                    found.push(f.change);
                }
                match page.next {
                    Some(next) => token = Some(next),
                    None => break,
                }
            }
        }
        Ok(found)
    }

    /// The paths the server's sync planner reports conflicting, or `None`
    /// when the server cannot say (no workspace, or no `GetConflicts`).
    async fn stack_conflicts<A: WorkspaceApi>(
        &self,
        api: &A,
        cx: &CallContext,
        tip: ChangesetId,
        limit: u32,
    ) -> Result<Option<StackConflicts>> {
        let Some(name) = self.config.workspace.as_deref() else {
            return Ok(None);
        };
        let req = GetConflicts {
            workspace: self.workspace_ref(name),
            onto: Some(tip),
            page: Page { limit, token: None },
        };
        match api.get_conflicts(cx, req).await {
            Ok(c) => Ok(Some(StackConflicts {
                draft: c.draft,
                paths: c.paths.into_iter().map(|v| v.path).collect(),
            })),
            Err(ApiError::NotFound { .. }) => Ok(None),
            Err(ApiError::Rejected { policy, .. }) if policy == "unimplemented" => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The entry at `path` in `at`, or `None` when there is none.
    async fn read<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        at: ChangesetId,
        path: &RepoPath,
    ) -> Result<Side> {
        let req = PathAt {
            repo: self.config.repo.clone(),
            at,
            path: path.clone(),
        };
        match api.read_file(cx, req).await {
            Ok(f) => Ok(Some((f.kind, f.data))),
            Err(ApiError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The author's entry at `path`: the file on disk within the slice,
    /// else the head's.
    async fn ours_side<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        path: &RepoPath,
    ) -> Result<Side> {
        if !self.sparse.includes(path) {
            let at = self.state.head.unwrap_or(self.state.base);
            return self.read(api, cx, at, path).await;
        }
        let file = self.root.join(path.as_str());
        let meta = match fs::symlink_metadata(&file) {
            Ok(m) if m.is_dir() => return Ok(None),
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::from(e)),
        };
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&file)?;
            return Ok(Some((
                EntryKind::Symlink,
                target.as_os_str().as_bytes().to_vec(),
            )));
        }
        let kind = if meta.mode() & 0o100 != 0 {
            EntryKind::Exec
        } else {
            EntryKind::File
        };
        Ok(Some((kind, fs::read(&file)?)))
    }
}

struct StackConflicts {
    draft: Option<ChangesetId>,
    paths: BTreeSet<RepoPath>,
}

/// The D16 prediction for one path both sides changed, with the same
/// reasons `GetConflicts` gives when no line merge applies.
pub fn predict(base: &Side, ours: &Side, theirs: &Side) -> Outcome {
    let conflict = |reason| Outcome::Conflict {
        hunks: Vec::new(),
        reason: Some(reason),
    };
    if ours == theirs {
        return Outcome::Clean;
    }
    let (Some(b), Some(o), Some(t)) = (base, ours, theirs) else {
        return conflict("clash");
    };
    let file = |k: EntryKind| matches!(k, EntryKind::File | EntryKind::Exec);
    if !(file(b.0) && file(o.0) && file(t.0)) {
        return conflict("kind");
    }
    if [&b.1, &o.1, &t.1].iter().any(|d| d.len() >= MAX_TEXT_BYTES) {
        return conflict("too_large");
    }
    if ![&b.1, &o.1, &t.1].iter().all(|d| is_text(d)) {
        return conflict("binary");
    }
    match merge3(&b.1, &o.1, &t.1) {
        // A kind change on one side only rides along with a clean merge.
        Merge3::Clean(_) => Outcome::Clean,
        Merge3::Conflicts(regions) => Outcome::Conflict {
            hunks: regions
                .iter()
                .map(|r| (r.start + 1, r.end.max(r.start + 1)))
                .collect(),
            reason: None,
        },
        Merge3::NotText => conflict("binary"),
        Merge3::TooComplex => conflict("too_large"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> Side {
        Some((EntryKind::File, s.as_bytes().to_vec()))
    }

    #[test]
    fn predicts_clean_conflict_and_clash() {
        let base = f("a\nb\nc\nd\ne\n");
        assert_eq!(
            predict(&base, &f("A\nb\nc\nd\ne\n"), &f("a\nb\nc\nd\nE\n")),
            Outcome::Clean
        );
        assert_eq!(
            predict(&base, &f("a\nB\nc\nd\ne\n"), &f("a\nX\nc\nd\ne\n")),
            Outcome::Conflict {
                hunks: vec![(2, 2)],
                reason: None
            }
        );
        assert_eq!(
            predict(&base, &None, &f("a\n")),
            Outcome::Conflict {
                hunks: Vec::new(),
                reason: Some("clash")
            }
        );
        assert_eq!(predict(&base, &f("same\n"), &f("same\n")), Outcome::Clean);
        let bin = Some((EntryKind::File, vec![0u8, 1, 2]));
        assert!(matches!(
            predict(&bin, &Some((EntryKind::File, vec![0u8, 3])), &f("x\n")),
            Outcome::Conflict {
                reason: Some("binary"),
                ..
            }
        ));
    }
}
