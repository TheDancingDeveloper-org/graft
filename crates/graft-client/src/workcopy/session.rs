//! The working copy's side of a workspace (`docs/design.md` §8.2, §8.4):
//! opening or reopening it, finishing an interrupted commit, `sync` and
//! `land`.
//!
//! Every write command starts with [`WorkingCopy::begin`]. It renews the
//! workspace lease (opening a workspace on first use, or reopening one whose
//! lease lapsed and adopting the drafts it held), finishes or rolls back an
//! interrupted commit, and checks that the server's head is the one this
//! working copy holds.
//!
//! `sync` fast-forwards a working copy with no stack, moving the workspace
//! by reopening it at the line tip. With a stack it asks the server to
//! `Sync`; when the server refuses with `Conflict`, the working copy moves
//! to the tip anyway and the conflicts are written into the files
//! (`super::conflict`), for the author to resolve and commit.
//!
//! **Moves that span the server and the disk.** Moving the workspace and
//! moving the files are two steps, and either can fail after the other has
//! happened. Before the first of them, `.graft/state` records where both
//! are going ([`Moving`]). Until the move finishes, [`WorkingCopy::begin`]
//! accepts the server's workspace on either side of it, and
//! [`WorkingCopy::finish`] completes it: the workspace if it is still on the
//! old side, then the files. So whatever fails, and in whatever order, the
//! working copy is never left on a base the line does not hold, and the next
//! command carries on from where the move stopped (`docs/design.md` §8.2).

use std::time::Duration;

use graft_model::{Change, ChangesetId};
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::types::{
    Land, Landed, MergeMode, MergedFrom, OpenWorkspace, ResolveRef, SetHead, WorkspaceRef,
    WorkspaceView,
};
use graft_proto::{ApiError, CallContext};
use graft_store::ObjectStore;

use super::commit::retryable;
use super::conflict::Conflicts;
use super::{Moving, UpdateStats, WorkingCopy, check_workspace_name};
use crate::fetch::FetchOptions;
use crate::workcopy::CommitLimits;
use crate::{Error, Result};

/// What [`WorkingCopy::begin`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Begun {
    /// The workspace, as the server holds it now.
    pub workspace: WorkspaceView,
    /// Whether the workspace was opened (first use, or after its lease
    /// lapsed).
    pub opened: bool,
    /// What happened to an interrupted commit, if there was one.
    pub recovered: Option<Recovered>,
    /// An earlier move did not finish ([`super::WcState::moving`]).
    /// `sync` and `land` finish it themselves; anything else must call
    /// [`WorkingCopy::finish`] first.
    pub unfinished: bool,
}

/// What finishing a move did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Moved {
    /// The files it rewrote.
    pub update: UpdateStats,
    /// The conflicts it wrote into the files, when it was a sync onto a
    /// tip the stack conflicts with.
    pub conflicts: Option<Conflicts>,
}

/// The fate of an interrupted commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovered {
    /// It was sent again and is now the head.
    Finished(ChangesetId),
    /// It could not succeed any more and was dropped; the working copy is
    /// as it was before it, its changes still uncommitted.
    RolledBack,
}

/// What `sync` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOutcome {
    /// The base before.
    pub from: ChangesetId,
    /// The base now.
    pub base: ChangesetId,
    /// The head now.
    pub head: Option<ChangesetId>,
    /// The files it rewrote.
    pub update: UpdateStats,
}

/// How `land` waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandOptions {
    /// Land the stack as one changeset.
    pub squash: bool,
    /// On `Conflict`, sync to the tip and write the conflicts into the
    /// files ([`Error::Conflicted`]) instead of leaving the working copy
    /// untouched.
    pub sync: bool,
    /// How long to wait for a queued land (a pull request) to finish.
    /// `None` returns as soon as it is queued.
    pub wait: Option<Duration>,
    /// How long to retry a contended land, when `wait` is not given.
    pub contended_wait: Duration,
    /// The first pause between attempts; it doubles up to `max_poll`.
    pub poll: Duration,
    /// The longest pause between attempts.
    pub max_poll: Duration,
    /// The lease to ask for when the workspace is reopened.
    pub lease_ms: u64,
    /// How the server's rebase treats a path both sides changed. The
    /// default is a three-way line merge for text files (decision D16);
    /// [`MergeMode::File`] refuses any path changed on both sides.
    pub merge: MergeMode,
    /// Lands a merge resolution (D29): the land records a merge point from
    /// this source line and changeset.
    pub merged_from: Option<MergedFrom>,
}

impl Default for LandOptions {
    fn default() -> Self {
        Self {
            squash: false,
            sync: false,
            wait: None,
            contended_wait: Duration::from_secs(60),
            poll: Duration::from_millis(500),
            max_poll: Duration::from_secs(15),
            lease_ms: crate::workspace::DEFAULT_LEASE_MS,
            merge: MergeMode::Line,
            merged_from: None,
        }
    }
}

/// What `land` did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LandOutcome {
    /// The line holds the stack; the working copy is on it.
    Landed {
        /// The top landed changeset (the new base).
        landed: ChangesetId,
        /// Whether the server rebased the stack.
        rebased: bool,
        /// The files the update rewrote.
        update: UpdateStats,
    },
    /// The land went to the PR bridge and is not finished.
    Queued {
        /// The pull request.
        pr_url: String,
    },
}

impl WorkingCopy {
    pub(crate) fn workspace_ref(&self, name: &str) -> WorkspaceRef {
        WorkspaceRef {
            repo: self.config.repo.clone(),
            name: name.to_owned(),
        }
    }

    /// Prepares a write: renews (or opens) the workspace, finishes an
    /// interrupted commit, and checks the server agrees with this working
    /// copy about the base and head.
    pub async fn begin<A: ObjectApi + RepoApi + WorkspaceApi>(
        &mut self,
        api: &A,
        cx: &CallContext,
        lease_ms: u64,
    ) -> Result<Begun> {
        let (mut view, opened) = self.ensure_workspace(api, cx, lease_ms).await?;
        if let Some(mut m) = self.state.moving {
            if m.by_server && view.base == m.base {
                // The server's rebase or rewrite happened: its head is
                // where the move goes.
                m = Moving::to(view.base, view.head);
                self.state.moving = Some(m);
                self.save_state()?;
            }
            // The server may be on either side of the unfinished move.
            if !on_target(&view, m) && view.head != self.state.head {
                return Err(Error::Diverged {
                    workspace: view.name,
                    server: view.head,
                    local: self.state.head,
                });
            }
            return Ok(Begun {
                workspace: view,
                opened,
                recovered: None,
                unfinished: true,
            });
        }
        let recovered = self.recover(api, cx).await?;
        if let Some(Recovered::Finished(id)) = recovered {
            view.head = Some(id);
        }
        if view.head != self.state.head {
            return Err(Error::Diverged {
                workspace: view.name,
                server: view.head,
                local: self.state.head,
            });
        }
        if view.head.is_none() && view.base != self.state.base {
            // No drafts to lose: the server follows this working copy (a
            // `sync` interrupted between its two halves).
            view = self
                .reset_workspace(api, cx, self.state.base, lease_ms)
                .await?;
        }
        Ok(Begun {
            workspace: view,
            opened,
            recovered,
            unfinished: false,
        })
    }

    /// Renews the workspace lease, or opens the workspace: on first use, or
    /// when its lease lapsed (or it was closed), under the same name, at this
    /// working copy's base, adopting the head draft it held. During an
    /// unfinished move it opens where the move is going instead.
    async fn ensure_workspace<A: WorkspaceApi>(
        &mut self,
        api: &A,
        cx: &CallContext,
        lease_ms: u64,
    ) -> Result<(WorkspaceView, bool)> {
        if let Some(name) = self.config.workspace.clone() {
            match api
                .renew_lease(cx, self.workspace_ref(&name), lease_ms)
                .await
            {
                Ok(view) => {
                    self.check_view(&view, &name)?;
                    return Ok((view, false));
                }
                Err(ApiError::LeaseExpired { .. } | ApiError::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let (at, head) = match self.state.moving {
            Some(m) if !m.by_server => (m.base, m.head),
            _ => (self.state.base, self.state.head),
        };
        let view = api
            .open_workspace(
                cx,
                OpenWorkspace {
                    repo: self.config.repo.clone(),
                    name: self.config.workspace.clone(),
                    line: self.state.line.clone(),
                    at: Some(at),
                    lease_ms,
                },
            )
            .await?;
        let name = self.config.workspace.clone().unwrap_or(view.name.clone());
        self.check_view(&view, &name)?;
        if view.base != at {
            return Err(protocol("the workspace opened at another base"));
        }
        if self.config.workspace.is_none() {
            self.set_workspace(Some(name.clone()))?;
        }
        let mut view = view;
        if let Some(head) = head.filter(|h| view.head != Some(*h)) {
            // Adopt the drafts the lapsed workspace held.
            view = api
                .set_head(
                    cx,
                    SetHead {
                        workspace: self.workspace_ref(&name),
                        expected: view.head,
                        new: Some(head),
                    },
                )
                .await?;
            self.check_view(&view, &name)?;
        }
        Ok((view, true))
    }

    /// Refuses a workspace answer for another workspace or line, or with a
    /// name a server could not have made.
    pub(crate) fn check_view(&self, view: &WorkspaceView, name: &str) -> Result<()> {
        check_workspace_name(&view.name)?;
        if view.name != name || view.line != self.state.line {
            return Err(protocol("the server answered for another workspace"));
        }
        Ok(())
    }

    /// Finishes an interrupted commit by sending it again, or rolls it back
    /// when it can no longer succeed. A failure that may pass (the server is
    /// unavailable) keeps it for the next command.
    async fn recover<A: ObjectApi + RepoApi + WorkspaceApi>(
        &mut self,
        api: &A,
        cx: &CallContext,
    ) -> Result<Option<Recovered>> {
        let pending = match self.load_pending() {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(None),
            // A file cut short by the crash was never sent.
            Err(Error::State(_)) => {
                self.drop_pending()?;
                return Ok(Some(Recovered::RolledBack));
            }
            Err(e) => return Err(e),
        };
        if self.config.workspace.as_deref() != Some(pending.workspace.as_str()) {
            self.drop_pending()?;
            return Ok(Some(Recovered::RolledBack));
        }
        match self.send(api, cx, &pending, &CommitLimits::default()).await {
            Ok(id) => {
                self.apply_pending(&pending, id)?;
                Ok(Some(Recovered::Finished(id)))
            }
            Err(e) if retryable(&e) => Err(e),
            Err(_) => {
                self.drop_pending()?;
                Ok(Some(Recovered::RolledBack))
            }
        }
    }

    /// Moves the server's workspace to `at` with no stack, by closing it
    /// and opening it again under the same name. Only for a workspace whose
    /// drafts are landed or that has none: closing abandons drafts.
    async fn reset_workspace<A: WorkspaceApi>(
        &self,
        api: &A,
        cx: &CallContext,
        at: ChangesetId,
        lease_ms: u64,
    ) -> Result<WorkspaceView> {
        let name = self.workspace_name()?;
        api.close_workspace(cx, self.workspace_ref(&name), false)
            .await?;
        let view = api
            .open_workspace(
                cx,
                OpenWorkspace {
                    repo: self.config.repo.clone(),
                    name: Some(name.clone()),
                    line: self.state.line.clone(),
                    at: Some(at),
                    lease_ms,
                },
            )
            .await?;
        self.check_view(&view, &name)?;
        if view.base != at || view.head.is_some() {
            return Err(protocol("the workspace reopened somewhere else"));
        }
        Ok(view)
    }

    /// Finishes an unfinished move ([`Begun::unfinished`]): moves the
    /// server's workspace if it is still on the old side, then the files,
    /// then writes the conflicts the move carries, if it carries any.
    /// Returns `None` when no move was unfinished.
    pub async fn finish<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        lease_ms: u64,
        opts: &FetchOptions,
    ) -> Result<Option<Moved>> {
        let Some(mut m) = self.state.moving else {
            return Ok(None);
        };
        let name = self.workspace_name()?;
        let view = api.get_workspace(cx, self.workspace_ref(&name)).await?;
        self.check_view(&view, &name)?;
        if m.by_server {
            if view.base == m.base {
                m = Moving::to(view.base, view.head);
            } else if view.head == self.state.head {
                // The server never moved: nothing to finish.
                self.state.moving = None;
                self.save_state()?;
                return Ok(Some(Moved::default()));
            }
        }
        let from = if on_target(&view, m) {
            None
        } else if view.head == self.state.head {
            Some((view.base, view.head))
        } else {
            return Err(Error::Diverged {
                workspace: view.name,
                server: view.head,
                local: self.state.head,
            });
        };
        self.complete_move(api, cache, cx, m, from, None, lease_ms, opts)
            .await
            .map(Some)
    }

    /// Moves the workspace and the working copy to `to`, recording the move
    /// first so that it can be finished after any failure. `from` is the
    /// server workspace's base and head when it has still to move (`None`
    /// when the server already moved it); `plan` is the file plan when the
    /// caller made it (and so checked the files) before moving the server.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_move<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        to: Moving,
        from: Option<(ChangesetId, Option<ChangesetId>)>,
        plan: Option<Vec<Change>>,
        lease_ms: u64,
        opts: &FetchOptions,
    ) -> Result<Moved> {
        if self.state.moving != Some(to) {
            self.state.moving = Some(to);
            self.save_state()?;
        }
        if let Some(from) = from {
            self.move_workspace(api, cx, from, to, lease_ms).await?;
        }
        let plan = match plan {
            Some(plan) => plan,
            None => {
                self.plan_move(api, cx, to.head.unwrap_or(to.base), opts)
                    .await?
            }
        };
        let update = self
            .apply_move(api, cache, cx, plan, to.base, to.head, opts)
            .await?;
        let conflicts = match self.load_conflicts()? {
            Some(record) if !record.applied => {
                Some(self.write_conflicts(api, cache, cx, record, opts).await?)
            }
            _ => None,
        };
        self.state.moving = None;
        self.save_state()?;
        Ok(Moved { update, conflicts })
    }

    /// Moves the server's workspace from `from` (its base and head) to
    /// `to`: the head by compare-and-swap when the base stays, else by
    /// reopening the workspace at the new base.
    async fn move_workspace<A: WorkspaceApi>(
        &self,
        api: &A,
        cx: &CallContext,
        from: (ChangesetId, Option<ChangesetId>),
        to: Moving,
        lease_ms: u64,
    ) -> Result<()> {
        let name = self.workspace_name()?;
        let expected = if from.0 == to.base {
            from.1
        } else {
            self.reset_workspace(api, cx, to.base, lease_ms).await?;
            None
        };
        if expected != to.head {
            let view = api
                .set_head(
                    cx,
                    SetHead {
                        workspace: self.workspace_ref(&name),
                        expected,
                        new: to.head,
                    },
                )
                .await?;
            self.check_view(&view, &name)?;
            if !on_target(&view, to) {
                return Err(protocol("the workspace moved somewhere else"));
            }
        }
        Ok(())
    }

    /// The line's tip.
    pub(crate) async fn tip<A: RepoApi>(&self, api: &A, cx: &CallContext) -> Result<ChangesetId> {
        Ok(api
            .resolve_ref(
                cx,
                ResolveRef {
                    repo: self.config.repo.clone(),
                    name: self.state.line.clone(),
                },
            )
            .await?
            .target)
    }

    /// Moves the working copy and its workspace to the line tip. With no
    /// stack this is a fast-forward of the files that changed. With a stack
    /// the server rebases it (`Sync`); when the stack conflicts with the
    /// tip, nothing changes and the error is the server's `Conflict`. An
    /// unfinished move is finished first. [`WorkingCopy::begin`] must have
    /// run first.
    pub async fn sync<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        lease_ms: u64,
        opts: &FetchOptions,
    ) -> Result<SyncOutcome> {
        self.sync_inner(api, cache, cx, lease_ms, opts, false).await
    }

    /// [`WorkingCopy::sync`], except that a conflicting stack does not stop
    /// it (`docs/design.md` §8.2): the working copy moves to the tip with
    /// the stack's changes on top as uncommitted edits, and the conflicting
    /// files carry markers or a sidecar ([`Error::Conflicted`]); the stack
    /// becomes one draft when they are committed.
    pub async fn sync_writing_conflicts<A, S>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        lease_ms: u64,
        opts: &FetchOptions,
    ) -> Result<SyncOutcome>
    where
        A: ObjectApi + RepoApi + WorkspaceApi,
        S: ObjectStore,
    {
        self.sync_inner(api, cache, cx, lease_ms, opts, true).await
    }

    async fn sync_inner<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        lease_ms: u64,
        opts: &FetchOptions,
        write_conflicts: bool,
    ) -> Result<SyncOutcome> {
        let from = self.state.base;
        let mut finished = UpdateStats::default();
        if let Some(moved) = self.finish(api, cache, cx, lease_ms, opts).await? {
            if let Some(conflicts) = moved.conflicts {
                return Err(Error::Conflicted(Box::new(conflicts)));
            }
            finished = moved.update;
        }
        self.check_settled()?;
        let name = self.workspace_name()?;
        let tip = self.tip(api, cx).await?;
        let mut update = if self.state.head.is_none() {
            if tip == self.state.base {
                return Ok(SyncOutcome {
                    from,
                    base: tip,
                    head: None,
                    update: finished,
                });
            }
            // Check the files first, then move the workspace, then write.
            // With no drafts to lose, `begin` puts the server back if the
            // write fails, so this move needs no record.
            let plan = self.plan_move(api, cx, tip, opts).await?;
            self.reset_workspace(api, cx, tip, lease_ms).await?;
            self.apply_move(api, cache, cx, plan, tip, None, opts)
                .await?
        } else {
            // The server picks the rebased head: if its answer is lost,
            // `begin` finds the workspace on the tip and takes that head.
            self.state.moving = Some(Moving {
                base: tip,
                head: None,
                by_server: true,
            });
            self.save_state()?;
            match api.sync(cx, self.workspace_ref(&name), Some(tip)).await {
                Ok(synced) => {
                    let view = synced.workspace;
                    self.check_view(&view, &name)?;
                    if view.base != tip {
                        return Err(protocol("the workspace synced to another base"));
                    }
                    // The server's drafts moved; the files follow, and a
                    // file in the way leaves the move to finish later.
                    let to = Moving::to(view.base, view.head);
                    self.complete_move(api, cache, cx, to, None, None, lease_ms, opts)
                        .await?
                        .update
                }
                Err(e @ ApiError::Conflict { .. }) if !write_conflicts => {
                    self.settle()?;
                    return Err(e.into());
                }
                Err(ApiError::Conflict { onto, .. }) => {
                    self.settle()?;
                    let conflicts = self
                        .sync_conflicting(api, cache, cx, onto, lease_ms, opts)
                        .await?;
                    return Err(Error::Conflicted(Box::new(conflicts)));
                }
                Err(e) => {
                    // A refusal moved nothing; an outage may have, and
                    // `begin` finds out.
                    if !retryable_api(&e) {
                        self.settle()?;
                    }
                    return Err(e.into());
                }
            }
        };
        update.fetch.add(&finished.fetch);
        update.changed += finished.changed;
        Ok(SyncOutcome {
            from,
            base: self.state.base,
            head: self.state.head,
            update,
        })
    }

    /// Lands the stack (`docs/design.md` §8.2). On a graft-primary line the
    /// answer is `Landed`; on a git-primary line it is `Queued` with the
    /// pull request, and with `wait` the land is asked again (it is
    /// idempotent) until it lands, is refused, or the time runs out
    /// ([`Error::StillQueued`]). A contended land is retried with backoff
    /// until the wait (or `contended_wait`) runs out. Once landed, the
    /// working copy and its workspace move to the landed changeset; when the
    /// server rebased the stack, that brings in the line's other changes
    /// within the slice. A `Conflict` leaves everything untouched, unless
    /// `sync` asks to write the conflicts into the files. An unfinished
    /// land is finished (asking `Land` again is idempotent).
    /// [`WorkingCopy::begin`] must have run first.
    pub async fn land<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        opts: &LandOptions,
        fetch: &FetchOptions,
    ) -> Result<LandOutcome> {
        if let Some(m) = self.state.moving {
            // A land that stopped after the line moved: the head is still
            // the landed draft, and the move goes to the line, with nothing
            // on top and no conflicts to write.
            let landing = m.head.is_none() && self.state.head.is_some() && !self.has_conflicts();
            let old_head = self.state.head;
            let moved = self
                .finish(api, cache, cx, opts.lease_ms, fetch)
                .await?
                .unwrap_or_default();
            if let Some(conflicts) = moved.conflicts {
                return Err(Error::Conflicted(Box::new(conflicts)));
            }
            if landing {
                return Ok(LandOutcome::Landed {
                    landed: m.base,
                    rebased: old_head != Some(m.base),
                    update: moved.update,
                });
            }
        }
        self.check_settled()?;
        let head = self.state.head.ok_or(Error::NoDraft)?;
        let start = tokio::time::Instant::now();
        let contended_until = start + opts.wait.unwrap_or(opts.contended_wait);
        let queued_until = opts.wait.map(|w| start + w);
        let mut pause = opts.poll;
        let mut synced = false;
        let mut req = Land {
            repo: self.config.repo.clone(),
            head,
            line: self.state.line.clone(),
            squash: opts.squash,
            merge: opts.merge,
            deadline_ms: None,
            merged_from: opts.merged_from.clone(),
        };
        let (landed, rebased) = loop {
            let until = match api.land(cx, req.clone()).await {
                Ok(Landed::Landed {
                    landed, rebased, ..
                }) => break (landed, rebased),
                Ok(Landed::Queued { pr_url }) => match queued_until {
                    None => return Ok(LandOutcome::Queued { pr_url }),
                    Some(until) if tokio::time::Instant::now() >= until => {
                        return Err(Error::StillQueued { pr_url });
                    }
                    Some(until) => until,
                },
                Ok(_) => return Err(protocol("an unknown land outcome")),
                Err(e @ ApiError::Contended { .. }) => {
                    if tokio::time::Instant::now() >= contended_until {
                        return Err(e.into());
                    }
                    contended_until
                }
                Err(ApiError::Conflict { .. }) if opts.sync && !synced => {
                    // Writes the conflicts (`Conflicted`), or, when the line
                    // moved on again and the stack now rebases cleanly,
                    // lands the rebased stack.
                    self.sync_writing_conflicts(api, cache, cx, opts.lease_ms, fetch)
                        .await?;
                    synced = true;
                    req.head = self.state.head.ok_or(Error::NoDraft)?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(pause.min(left)).await;
            pause = (pause * 2).min(opts.max_poll);
        };
        let plan = match self.plan_move(api, cx, landed, fetch).await {
            Ok(plan) => plan,
            Err(Error::Dirty(paths)) => return Err(Error::LandedDirty { landed, paths }),
            Err(e) => return Err(e),
        };
        let to = Moving::to(landed, None);
        let from = Some((self.state.base, self.state.head));
        let moved = self
            .complete_move(api, cache, cx, to, from, Some(plan), opts.lease_ms, fetch)
            .await?;
        Ok(LandOutcome::Landed {
            landed,
            rebased,
            update: moved.update,
        })
    }

    /// Refuses a write while a move is unfinished ([`WorkingCopy::finish`]
    /// first) or while the conflicts a sync wrote are uncommitted (other
    /// than by `commit`, which resolves them).
    /// Forgets a move that did not start.
    pub(crate) fn settle(&mut self) -> Result<()> {
        if self.state.moving.take().is_some() {
            self.save_state()?;
        }
        Ok(())
    }

    pub(crate) fn check_settled(&self) -> Result<()> {
        if self.state.moving.is_some() {
            return Err(Error::Unfinished);
        }
        if self.has_conflicts() {
            return Err(Error::Unresolved(
                self.load_conflicts()?
                    .map(|c| c.files.into_iter().map(|f| f.path).collect())
                    .unwrap_or_default(),
            ));
        }
        Ok(())
    }
}

/// Whether the workspace is where move `m` goes.
fn on_target(view: &WorkspaceView, m: Moving) -> bool {
    view.base == m.base && (m.by_server || view.head == m.head)
}

/// Whether a failed call may have run on the server.
pub(crate) fn retryable_api(e: &ApiError) -> bool {
    matches!(e, ApiError::Unavailable { .. })
}

fn protocol(detail: &str) -> Error {
    Error::Api(ApiError::Rejected {
        policy: "protocol".to_owned(),
        detail: detail.to_owned(),
    })
}
