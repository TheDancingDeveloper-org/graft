//! `GetConflicts` and `Resolve`: conflict resolution without a working copy
//! (WI-2312, design note `docs/notes/mcp-conflict-resolution.md`, option a).
//!
//! `GetConflicts` plans the workspace's sync onto the target in memory,
//! exactly as `Sync` would (decision D16 line merge included), and reports
//! the first draft that conflicts: per path the base, ours and theirs
//! entries and, when all three are text within the D16 limits, the merge3
//! rendering with diff3 markers, capped at [`MAX_CONFLICT_MARKER_BYTES`].
//! It writes nothing.
//!
//! `Resolve` is `Sync` with the caller's entries for the paths that
//! conflict. The planner applies a resolution only where the line merge
//! fails, at the first draft that conflicts on that path; a resolution for
//! a path that does not conflict is refused, and a path that still
//! conflicts fails the call with `Conflict`, before anything is written.
//! The rewritten drafts keep author, time, message and metadata, add
//! `graft.rebased-from`, and the draft that took resolutions records
//! [`RESOLVED_PATHS`](crate::RESOLVED_PATHS). Their ids are content-only,
//! so a repeated call computes the same head; a retry after success finds
//! the workspace already there and returns it. The workspace moves by one
//! CAS on `expected_head`.
//!
//! Cost: the same as `Sync` (O(changes × depth) tree reads per draft), plus
//! the three sides of each conflicting path on the page for the markers.

use std::collections::HashMap;

use graft_model::merge::{MAX_TEXT_BYTES, is_text, render_conflict_markers};
use graft_model::{Changeset, ChangesetId, EntryKind, ObjectId, ObjectKind};
use graft_proto::types::{
    ConflictReason, ConflictSide, ConflictView, Conflicts, GetConflicts, MAX_CONFLICT_MARKER_BYTES,
    MergeMode, Resolution, Resolve, Resolved, ResolvedContent,
};
use graft_proto::{ApiError, CallContext};
use graft_store::{MetaStore, ObjectStore, RepoId, TxnOutcome, Write};

use crate::access::{Need, require, require_changes};
use crate::rebase::{Failed, Overrides, Sides};
use crate::workspace::{CAS_RETRIES, check_name, contended, guarded, head_conflict, view};
use crate::{Engine, errors, journal};

/// A resolution's entry kind must be one a file change can put.
fn resolvable(kind: EntryKind) -> bool {
    matches!(kind, EntryKind::File | EntryKind::Exec | EntryKind::Symlink)
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// The workspace's stack over its base, bottom first, after checking
    /// the caller may write every path it changes.
    async fn drafts(
        &self,
        cx: &CallContext,
        repo_name: &str,
        repo: RepoId,
        ws: &graft_store::WorkspaceRecord,
        writes: &mut Vec<Write>,
    ) -> Result<
        (
            Vec<(ChangesetId, Changeset)>,
            HashMap<ChangesetId, graft_store::ChangesetRecord>,
        ),
        ApiError,
    > {
        let mut stack = self.stack_content(repo, ws.base, ws.head, writes).await?;
        stack.reverse();
        let mut records = HashMap::with_capacity(stack.len());
        let mut drafts = Vec::with_capacity(stack.len());
        for (id, cs, record) in stack {
            require_changes(cx, repo_name, &cs.changes)?;
            records.insert(id, record);
            drafts.push((id, cs));
        }
        Ok((drafts, records))
    }

    pub(crate) async fn do_get_conflicts(
        &self,
        cx: &CallContext,
        req: GetConflicts,
    ) -> Result<Conflicts, ApiError> {
        let wref = &req.workspace;
        require(cx, &wref.repo, Need::Workspace)?;
        check_name(&wref.name)?;
        let after = req
            .page
            .token
            .as_deref()
            .map(|t| graft_model::RepoPath::new(t).map_err(|_| errors::invalid(None, "page-token")))
            .transpose()?;
        let limit = self.page_size(req.page.limit) as usize;
        let repo = self.repo_id(&wref.repo).await?;
        let ws = self
            .live_workspace(cx, &wref.repo, repo, &wref.name)
            .await?;
        let target = self.sync_target(repo, &ws.line, req.onto).await?;
        let clean = Conflicts {
            onto: target,
            draft: None,
            paths: Vec::new(),
            next: None,
        };
        if target == ws.base {
            return Ok(clean);
        }
        let (drafts, _) = self
            .drafts(cx, &wref.repo, repo, &ws, &mut Vec::new())
            .await?;
        let onto_root = self.root_of(repo, target).await?;
        let failed = match self
            .plan_with(
                repo,
                &drafts,
                target,
                onto_root,
                MergeMode::Line,
                &self.line_config(&ws.line),
                &Overrides::default(),
            )
            .await?
        {
            Ok(_) => return Ok(clean),
            Err(failed) => failed,
        };
        let Failed {
            draft,
            mut sides,
            blobs,
        } = failed;
        sides.sort_by(|a, b| a.path.cmp(&b.path));
        let mut page = sides
            .into_iter()
            .filter(|s| after.as_ref().is_none_or(|a| s.path > *a))
            .peekable();
        let mut paths = Vec::new();
        while paths.len() < limit {
            let Some(s) = page.next() else { break };
            paths.push(self.conflict_view(repo, s, &blobs).await?);
        }
        let next = page
            .peek()
            .is_some()
            .then(|| paths.last().map(|v: &ConflictView| v.path.to_string()))
            .flatten();
        Ok(Conflicts {
            onto: target,
            draft: Some(draft),
            paths,
            next,
        })
    }

    /// One conflicting path as the caller sees it: ids, and markers when
    /// all three sides are text within the limits.
    async fn conflict_view(
        &self,
        repo: RepoId,
        s: Sides,
        pending: &HashMap<ObjectId, Vec<u8>>,
    ) -> Result<ConflictView, ApiError> {
        let side = |e: Option<(EntryKind, ObjectId)>| e.map(|(kind, id)| ConflictSide { id, kind });
        let mut out = ConflictView {
            path: s.path,
            base: side(s.base),
            ours: side(s.ours),
            theirs: side(s.theirs),
            text_mergeable: false,
            markers: None,
            reason: None,
        };
        let (Some(b), Some(o), Some(t)) = (s.base, s.ours, s.theirs) else {
            out.reason = Some(ConflictReason::Clash);
            return Ok(out);
        };
        let file = |k: EntryKind| matches!(k, EntryKind::File | EntryKind::Exec);
        if !(file(b.0) && file(o.0) && file(t.0)) {
            out.reason = Some(ConflictReason::Kind);
            return Ok(out);
        }
        let mut texts = Vec::with_capacity(3);
        for id in [b.1, o.1, t.1] {
            match self.text_input(repo, id, pending).await? {
                Some(text) => texts.push(text),
                None => {
                    out.reason = Some(ConflictReason::TooLarge);
                    return Ok(out);
                }
            }
        }
        if !texts.iter().all(|t| is_text(t)) {
            out.reason = Some(ConflictReason::Binary);
            return Ok(out);
        }
        out.text_mergeable = true;
        let markers = render_conflict_markers(&texts[0], &texts[1], &texts[2]);
        if markers.len() > MAX_CONFLICT_MARKER_BYTES {
            out.reason = Some(ConflictReason::TooLarge);
        } else {
            out.markers = Some(markers);
        }
        Ok(out)
    }

    /// Blob `id`'s payload when under the D16 text limit: a merged blob of
    /// the plan, or the stored blob. `None` when larger, which the frame
    /// header tells without decoding it.
    async fn text_input(
        &self,
        repo: RepoId,
        id: ObjectId,
        pending: &HashMap<ObjectId, Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, ApiError> {
        if let Some(payload) = pending.get(&id) {
            return Ok(Some(payload.clone()));
        }
        let Some(bytes) = self.objects.get(repo, id).await.map_err(errors::store)? else {
            return Err(ApiError::MissingObjects { ids: vec![id] });
        };
        let frame = graft_proto::Frame::from_bytes(&bytes)
            .ok()
            .filter(|f| f.kind == ObjectKind::Blob)
            .ok_or(ApiError::CorruptObject { id })?;
        if frame.uncompressed_len >= MAX_TEXT_BYTES as u64 {
            return Ok(None);
        }
        frame
            .decode_verified(id)
            .map(Some)
            .map_err(|_| ApiError::CorruptObject { id })
    }

    /// Checks the resolutions at the boundary and turns them into the
    /// planner's overrides and inline payloads.
    fn overrides(
        &self,
        cx: &CallContext,
        repo_name: &str,
        req: Resolve,
    ) -> Result<Overrides, ApiError> {
        if req.resolutions.len() > self.config.max_changes as usize {
            return Err(errors::limit("changes", self.config.max_changes.into()));
        }
        let mut overrides = Overrides::default();
        let mut inline_bytes = 0u64;
        for r in req.resolutions {
            require(cx, repo_name, Need::Write(Some(&r.path)))?;
            let entry = match r.resolution {
                Resolution::Delete => None,
                Resolution::Put { kind, content } => {
                    if !resolvable(kind) {
                        return Err(errors::invalid(Some(&r.path), "resolution-kind"));
                    }
                    let id = match content {
                        ResolvedContent::Blob(id) => id.0,
                        ResolvedContent::Inline(bytes) => {
                            if bytes.len() as u64 > self.config.max_blob_bytes {
                                return Err(errors::limit(
                                    "blob-bytes",
                                    self.config.max_blob_bytes,
                                ));
                            }
                            inline_bytes += bytes.len() as u64;
                            if inline_bytes > self.config.max_inline_bytes {
                                return Err(errors::limit(
                                    "inline-bytes",
                                    self.config.max_inline_bytes,
                                ));
                            }
                            let id = ObjectId::hash(ObjectKind::Blob, &bytes);
                            overrides.blobs.insert(id, bytes);
                            id
                        }
                        _ => return Err(errors::invalid(Some(&r.path), "resolution-content")),
                    };
                    Some((kind, id))
                }
                _ => return Err(errors::invalid(Some(&r.path), "resolution")),
            };
            if overrides.entries.insert(r.path.clone(), entry).is_some() {
                return Err(errors::invalid(Some(&r.path), "resolution-duplicate"));
            }
        }
        Ok(overrides)
    }

    pub(crate) async fn do_resolve(
        &self,
        cx: &CallContext,
        req: Resolve,
    ) -> Result<Resolved, ApiError> {
        let wref = req.workspace.clone();
        require(cx, &wref.repo, Need::Workspace)?;
        check_name(&wref.name)?;
        let (onto, expected) = (req.onto, req.expected_head);
        let overrides = self.overrides(cx, &wref.repo, req)?;
        let repo = self.repo_id(&wref.repo).await?;
        for _ in 0..CAS_RETRIES {
            let ws = self
                .live_workspace(cx, &wref.repo, repo, &wref.name)
                .await?;
            let target = self.sync_target(repo, &ws.line, Some(onto)).await?;
            let onto_root = self.root_of(repo, target).await?;
            if ws.head != expected {
                // A retry after success: the same stack, target and
                // resolutions give the head the workspace already has.
                if let (true, Some(old)) = (ws.base == target, expected) {
                    let stack = self.chain(repo, old, target).await?;
                    if let Ok(plan) = self
                        .plan_with(
                            repo,
                            &stack,
                            target,
                            onto_root,
                            MergeMode::Line,
                            &self.line_config(&ws.line),
                            &overrides,
                        )
                        .await?
                        && ws.head.unwrap_or(target) == plan.head
                    {
                        return Ok(Resolved {
                            workspace: view(ws),
                            head: plan.head,
                            resolved_paths: plan.resolved,
                            merged_paths: plan.merged,
                        });
                    }
                }
                return Err(head_conflict(&ws));
            }
            let mut txn = guarded(&ws);
            let (drafts, records) = self
                .drafts(cx, &wref.repo, repo, &ws, &mut txn.writes)
                .await?;
            let plan = match self
                .plan_with(
                    repo,
                    &drafts,
                    target,
                    onto_root,
                    MergeMode::Line,
                    &self.line_config(&ws.line),
                    &overrides,
                )
                .await?
            {
                Ok(plan) => plan,
                Err(failed) => return Err(failed.into_error(target)),
            };
            // Objects first: the new drafts are stored abandoned, and the
            // workspace adopts them in the transaction below.
            self.store_plan(repo, &plan).await?;
            self.adopt(repo, &ws, &plan, records, target, &mut txn)
                .await?;

            let mut next = ws.clone();
            next.base = target;
            next.head = (plan.head != target).then_some(plan.head);
            txn.writes.push(Write::PutWorkspace(next.clone()));
            txn.writes.push(plan.draft_paths(&ws, &drafts));
            txn.writes.push(journal::workspace_synced(
                &ws.name, ws.base, next.base, ws.head, next.head,
            ));
            if let TxnOutcome::Committed(_) =
                self.meta.commit(repo, txn).await.map_err(errors::store)?
            {
                return Ok(Resolved {
                    workspace: view(next),
                    head: plan.head,
                    resolved_paths: plan.resolved,
                    merged_paths: plan.merged,
                });
            }
        }
        Err(contended())
    }
}
