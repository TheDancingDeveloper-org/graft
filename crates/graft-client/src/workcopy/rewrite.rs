//! `revert`, `pick` and the stack operations (`docs/design.md` §3, §7.2, §8.5):
//! the server makes the new drafts, the workspace head moves to them, and
//! the working copy follows.
//!
//! Both go through the recorded move of `super::session`, so a failure
//! between the server's change and the files' is finished by the next
//! command. `revert` asks for the inverse without a workspace and checks the
//! files before it moves the head, so local edits in the way change nothing.
//! `pick` is the same over `Rebase` of a landed changeset, which moves only
//! that changeset (WI-2364); a conflicting pick writes the changeset's
//! changes into the files instead, as a conflicting `sync` does.
//! A stack operation moves the head on the server in the same call, so it
//! needs a clean working copy first.

use graft_model::ChangesetId;
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::types::{DraftState, GetChangesets, MergeMode, Rebase, Revert, StackOp};
use graft_proto::{ApiError, CallContext};
use graft_store::ObjectStore;

use super::session::retryable_api;
use super::{Moving, UpdateStats, WorkingCopy};
use crate::fetch::FetchOptions;
use crate::{Error, Result};

/// What `revert` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reverted {
    /// The changeset reverted.
    pub reverted: ChangesetId,
    /// The new draft, now the head.
    pub id: ChangesetId,
    /// The files the update rewrote.
    pub update: UpdateStats,
}

/// What `pick` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked {
    /// The changeset picked.
    pub picked: ChangesetId,
    /// The new draft, now the head.
    pub id: ChangesetId,
    /// The files the update rewrote.
    pub update: UpdateStats,
}

/// What a stack operation did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rewritten {
    /// The head before.
    pub from: Option<ChangesetId>,
    /// The head now.
    pub head: Option<ChangesetId>,
    /// The files the update rewrote.
    pub update: UpdateStats,
}

impl WorkingCopy {
    /// Adds a draft on top of the head that inverts `changeset`
    /// (`graft.reverts`), and moves the working copy to it. A file the
    /// inverse changes must not have local changes. [`WorkingCopy::begin`]
    /// must have run first.
    pub async fn revert<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        changeset: ChangesetId,
        lease_ms: u64,
        opts: &FetchOptions,
    ) -> Result<Reverted> {
        self.check_settled()?;
        self.workspace_name()?;
        let onto = self.snapshot();
        let id = api
            .revert(
                cx,
                Revert {
                    repo: self.config.repo.clone(),
                    changeset,
                    onto,
                    workspace: None,
                },
            )
            .await?;
        if id == onto {
            return Err(Error::NothingToCommit);
        }
        let plan = self.plan_move(api, cx, id, opts).await?;
        let to = Moving::to(self.state.base, Some(id));
        let from = Some((self.state.base, self.state.head));
        let moved = self
            .complete_move(api, cache, cx, to, from, Some(plan), lease_ms, opts)
            .await?;
        Ok(Reverted {
            reverted: changeset,
            id,
            update: moved.update,
        })
    }

    /// Adds a draft on top of the head that re-applies the landed
    /// `changeset`'s own changes (a cherry-pick): the server rebases it onto
    /// the snapshot, which records `graft.rebased-from`, and the working copy
    /// moves to it. A file the pick changes must not have local changes.
    /// When the server reports a conflict, the working copy must be clean;
    /// the pick's changes are then written into the files, with markers or
    /// a sidecar where both sides changed ([`Error::Conflicted`]), and the
    /// next `commit` makes the draft. [`WorkingCopy::begin`] must have run
    /// first.
    #[allow(clippy::too_many_arguments)]
    pub async fn pick<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        changeset: ChangesetId,
        merge: MergeMode,
        lease_ms: u64,
        opts: &FetchOptions,
    ) -> Result<Picked> {
        self.check_settled()?;
        self.workspace_name()?;
        let onto = self.snapshot();
        let parent = self.landed_parent(api, cx, changeset).await?;
        let rebased = api
            .rebase(
                cx,
                Rebase {
                    repo: self.config.repo.clone(),
                    changeset,
                    onto,
                    merge,
                    dry_run: false,
                },
            )
            .await;
        let id = match rebased {
            Ok(id) => id,
            Err(ApiError::Conflict { .. }) => {
                let conflicts = self
                    .pick_conflicting(api, cache, cx, onto, parent, changeset, opts)
                    .await?;
                return Err(Error::Conflicted(Box::new(conflicts)));
            }
            Err(e) => return Err(e.into()),
        };
        // Nothing to add: the snapshot already has every change, or the
        // changeset already sits on it.
        if id == onto || id == changeset {
            return Err(Error::NothingToCommit);
        }
        let plan = self.plan_move(api, cx, id, opts).await?;
        let to = Moving::to(self.state.base, Some(id));
        let from = Some((self.state.base, self.state.head));
        let moved = self
            .complete_move(api, cache, cx, to, from, Some(plan), lease_ms, opts)
            .await?;
        Ok(Picked {
            picked: changeset,
            id,
            update: moved.update,
        })
    }

    /// The first parent of `changeset`, which must be landed: `Rebase` of a
    /// draft would move the drafts below it too.
    async fn landed_parent<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        changeset: ChangesetId,
    ) -> Result<ChangesetId> {
        let got = api
            .get_changesets(
                cx,
                GetChangesets {
                    repo: self.config.repo.clone(),
                    ids: vec![changeset],
                },
            )
            .await?;
        if got.collected.contains(&changeset) {
            return Err(ApiError::Collected { id: changeset }.into());
        }
        let view = got
            .found
            .into_iter()
            .find(|v| v.changeset.id() == changeset)
            .ok_or_else(|| ApiError::NotFound {
                what: format!("changeset {changeset}"),
            })?;
        if view.state != DraftState::Landed {
            return Err(Error::State(format!(
                "{changeset} is not landed: only a landed changeset can be picked"
            )));
        }
        view.changeset
            .first_parent()
            .ok_or_else(|| Error::State(format!("{changeset} is a root: it has nothing to pick")))
    }

    /// Reorders, squashes, splits or drops drafts of the stack on the
    /// server, and moves the working copy to the new head. The working copy
    /// must be clean. [`WorkingCopy::begin`] must have run first.
    pub async fn rewrite_stack<A: ObjectApi + RepoApi + WorkspaceApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        op: StackOp,
        lease_ms: u64,
        opts: &FetchOptions,
    ) -> Result<Rewritten> {
        self.check_settled()?;
        let from = self.state.head.ok_or(Error::NoDraft)?;
        let dirty: Vec<_> = self.status()?.iter().map(|s| s.path().clone()).collect();
        if !dirty.is_empty() {
            return Err(Error::Dirty(dirty));
        }
        let name = self.workspace_name()?;
        // The server moves the head itself: if its answer is lost, `begin`
        // takes whatever head the workspace has.
        self.state.moving = Some(Moving {
            base: self.state.base,
            head: None,
            by_server: true,
        });
        self.save_state()?;
        let view = match api.stack(cx, self.workspace_ref(&name), op).await {
            Ok(view) => view,
            Err(e) => {
                if !retryable_api(&e) {
                    self.settle()?;
                }
                return Err(e.into());
            }
        };
        self.check_view(&view, &name)?;
        if view.base != self.state.base {
            return Err(Error::Api(ApiError::Rejected {
                policy: "protocol".to_owned(),
                detail: "the stack moved to another base".to_owned(),
            }));
        }
        let to = Moving::to(view.base, view.head);
        let moved = self
            .complete_move(api, cache, cx, to, None, None, lease_ms, opts)
            .await?;
        Ok(Rewritten {
            from: Some(from),
            head: view.head,
            update: moved.update,
        })
    }
}
