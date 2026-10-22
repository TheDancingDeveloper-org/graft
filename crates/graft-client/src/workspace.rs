//! Workspace helpers (`docs/design.md` §3, §8.5, §8.6): lease heartbeats
//! for long-running commands, and reading a stack.

use std::future::Future;
use std::time::Duration;

use graft_model::ChangesetId;
use graft_proto::api::{RepoApi, WorkspaceApi};
use graft_proto::types::{ChangesetView, GetChangesets, WorkspaceRef};
use graft_proto::{ApiError, CallContext};

use crate::Result;
use crate::fetch::over_budget;

/// The lease a client asks for: the design's default, 2 hours.
pub const DEFAULT_LEASE_MS: u64 = 2 * 3600 * 1000;

/// How often a long-running command renews its lease: a sixth of the
/// lease, so several renewals can fail before it lapses.
pub const HEARTBEAT_EVERY: Duration = Duration::from_millis(DEFAULT_LEASE_MS / 6);

/// The deepest stack [`stack`] walks (the engine's default `max_stack`).
pub const MAX_STACK: usize = 1000;

/// Runs `work`, renewing the lease of `workspace` every `every` until it
/// finishes. A renewal that fails is ignored: `work` itself fails with
/// `LeaseExpired` if the lease really lapsed, and a later renewal may still
/// succeed. Returns what `work` returns.
pub async fn with_heartbeat<A: WorkspaceApi, F: Future>(
    api: &A,
    cx: &CallContext,
    workspace: WorkspaceRef,
    lease_ms: u64,
    every: Duration,
    work: F,
) -> F::Output {
    let mut work = std::pin::pin!(work);
    let start = tokio::time::Instant::now() + every;
    let mut tick = tokio::time::interval_at(start, every.max(Duration::from_millis(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            out = &mut work => return out,
            _ = tick.tick() => {
                let _ = api.renew_lease(cx, workspace.clone(), lease_ms).await;
            }
        }
    }
}

/// The drafts between `base` and `head`, oldest first, read one changeset
/// at a time along first parents. At most [`MAX_STACK`] are read; a server
/// answer that is not the changeset asked for is refused.
pub async fn stack<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    repo: &str,
    base: ChangesetId,
    head: Option<ChangesetId>,
) -> Result<Vec<ChangesetView>> {
    let mut out = Vec::new();
    let mut cur = head;
    while let Some(id) = cur.filter(|id| *id != base) {
        if out.len() >= MAX_STACK {
            return Err(over_budget("max_stack", MAX_STACK as u64));
        }
        let got = api
            .get_changesets(
                cx,
                GetChangesets {
                    repo: repo.to_owned(),
                    ids: vec![id],
                },
            )
            .await?;
        if got.collected.contains(&id) {
            return Err(ApiError::Collected { id }.into());
        }
        let view = got
            .found
            .into_iter()
            .find(|v| v.changeset.id() == id)
            .ok_or_else(|| ApiError::NotFound {
                what: format!("changeset {id}"),
            })?;
        cur = view.changeset.parents.first().copied();
        if cur.is_none() {
            // A root below the head: the base is not under it.
            return Err(ApiError::NotFound {
                what: format!("base {base} under head"),
            }
            .into());
        }
        out.push(view);
    }
    out.reverse();
    Ok(out)
}
