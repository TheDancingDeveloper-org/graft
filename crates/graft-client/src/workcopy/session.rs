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
//! Until the server rebases (G2.7), `sync` only fast-forwards a working copy
//! with no stack, and moves the workspace by reopening it at the line tip.
//! With a stack it asks the server to `Sync`.

use std::time::Duration;

use graft_model::ChangesetId;
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::types::{
    Land, Landed, MergeMode, OpenWorkspace, ResolveRef, SetHead, WorkspaceRef, WorkspaceView,
};
use graft_proto::{ApiError, CallContext};
use graft_store::ObjectStore;

use super::commit::retryable;
use super::{UpdateStats, WorkingCopy, check_workspace_name};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LandOptions {
    /// Land the stack as one changeset.
    pub squash: bool,
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
}

impl Default for LandOptions {
    fn default() -> Self {
        Self {
            squash: false,
            wait: None,
            contended_wait: Duration::from_secs(60),
            poll: Duration::from_millis(500),
            max_poll: Duration::from_secs(15),
            lease_ms: crate::workspace::DEFAULT_LEASE_MS,
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
    fn workspace_ref(&self, name: &str) -> WorkspaceRef {
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
        })
    }

    /// Renews the workspace lease, or opens the workspace: on first use, or
    /// when its lease lapsed (or it was closed), under the same name, at this
    /// working copy's base, adopting the head draft it held.
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
        let view = api
            .open_workspace(
                cx,
                OpenWorkspace {
                    repo: self.config.repo.clone(),
                    name: self.config.workspace.clone(),
                    line: self.state.line.clone(),
                    at: Some(self.state.base),
                    lease_ms,
                },
            )
            .await?;
        let name = self.config.workspace.clone().unwrap_or(view.name.clone());
        self.check_view(&view, &name)?;
        if view.base != self.state.base {
            return Err(protocol("the workspace opened at another base"));
        }
        if self.config.workspace.is_none() {
            self.set_workspace(Some(name.clone()))?;
        }
        let mut view = view;
        if let Some(head) = self.state.head.filter(|h| view.head != Some(*h)) {
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
    fn check_view(&self, view: &WorkspaceView, name: &str) -> Result<()> {
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

    /// Moves the working copy and its workspace to the line tip. With no
    /// stack this is a fast-forward of the files that changed. With a stack
    /// the server rebases it (`Sync`); a conflict changes nothing.
    /// [`WorkingCopy::begin`] must have run first.
    pub async fn sync<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        lease_ms: u64,
        opts: &FetchOptions,
    ) -> Result<SyncOutcome> {
        let name = self.workspace_name()?;
        let from = self.state.base;
        let tip = api
            .resolve_ref(
                cx,
                ResolveRef {
                    repo: self.config.repo.clone(),
                    name: self.state.line.clone(),
                },
            )
            .await?
            .target;
        let (base, head, plan) = if self.state.head.is_none() {
            if tip == from {
                return Ok(SyncOutcome {
                    from,
                    base: from,
                    head: None,
                    update: UpdateStats::default(),
                });
            }
            // Check the files first, then move the workspace, then write.
            let plan = self.plan_move(api, cx, tip, opts).await?;
            self.reset_workspace(api, cx, tip, lease_ms).await?;
            (tip, None, plan)
        } else {
            let synced = api.sync(cx, self.workspace_ref(&name), Some(tip)).await?;
            let view = synced.workspace;
            self.check_view(&view, &name)?;
            if view.base != tip {
                return Err(protocol("the workspace synced to another base"));
            }
            // The server's drafts moved; a file in the way leaves the
            // working copy on the old ones, which `sync` can retry.
            let plan = self
                .plan_move(api, cx, view.head.unwrap_or(view.base), opts)
                .await?;
            (view.base, view.head, plan)
        };
        let update = self
            .apply_move(api, cache, cx, plan, base, head, opts)
            .await?;
        Ok(SyncOutcome {
            from,
            base,
            head,
            update,
        })
    }

    /// Lands the stack (`docs/design.md` §8.2). On a direct line the answer
    /// is `Landed`; on a git-primary line it is `Queued` with the pull
    /// request, and with `wait` the land is asked again (it is idempotent)
    /// until it lands, is refused, or the time runs out
    /// ([`Error::StillQueued`]). A contended land is retried until the wait
    /// (or `contended_wait`) runs out. Once landed, the working copy and its
    /// workspace move to the landed changeset. [`WorkingCopy::begin`] must
    /// have run first.
    pub async fn land<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        opts: &LandOptions,
        fetch: &FetchOptions,
    ) -> Result<LandOutcome> {
        let head = self.state.head.ok_or(Error::NoDraft)?;
        let start = tokio::time::Instant::now();
        let contended_until = start + opts.wait.unwrap_or(opts.contended_wait);
        let queued_until = opts.wait.map(|w| start + w);
        let mut pause = opts.poll;
        let req = Land {
            repo: self.config.repo.clone(),
            head,
            line: self.state.line.clone(),
            squash: opts.squash,
            merge: MergeMode::File,
            deadline_ms: None,
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
                Err(e) => return Err(e.into()),
            };
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(pause.min(left)).await;
            pause = (pause * 2).min(opts.max_poll);
        };
        let update = match self.plan_move(api, cx, landed, fetch).await {
            Ok(plan) => {
                self.reset_workspace(api, cx, landed, opts.lease_ms).await?;
                self.apply_move(api, cache, cx, plan, landed, None, fetch)
                    .await?
            }
            Err(Error::Dirty(paths)) => return Err(Error::LandedDirty { landed, paths }),
            Err(e) => return Err(e),
        };
        Ok(LandOutcome::Landed {
            landed,
            rebased,
            update,
        })
    }
}

fn protocol(detail: &str) -> Error {
    Error::Api(ApiError::Rejected {
        policy: "protocol".to_owned(),
        detail: detail.to_owned(),
    })
}
