//! Workspaces, draft states and holders (`docs/design.md` §3, §4.7, §7.2,
//! §8.6).
//!
//! A workspace is a leased record `{ base, head }`. Its *stack* is the
//! first-parent chain from `head` down to, not including, `base`. A draft is
//! a changeset in exactly one stack, and its record names that workspace as
//! its holder (`ws:<name>`). A changeset that nothing holds is `abandoned`.
//!
//! Every transition is one metadata transaction: a whole-record CAS on the
//! workspace, a state CAS on each record it moves, the moves themselves and
//! the journal entry. A transition that loses its CAS to a change that
//! leaves it valid (a lease renewal racing a `SetHead`) re-reads and retries,
//! at most [`CAS_RETRIES`] times. Stack walks are bounded by
//! `EngineConfig::max_stack`, checked by skip-list depth before any walk.

use std::collections::HashSet;

use graft_model::{Changeset, ChangesetId, ObjectId, ObjectKind};
use graft_proto::types::{OpenWorkspace, Page, SetHead, WorkspaceRef, WorkspaceView};
use graft_proto::{ApiError, CallContext};
use graft_store::{
    ChangesetRecord, MetaStore, MetaTxn, ObjectStore, Precondition, RecordState, RefKind, RepoId,
    TxnOutcome, WorkspaceRecord, Write,
};

use crate::access::{Need, is_admin, require, require_changes};
use crate::{Engine, errors, journal};

/// The longest workspace name, in bytes.
pub const MAX_WORKSPACE_NAME: usize = 128;

/// How many times a transition re-reads and retries after losing a CAS to a
/// concurrent change that leaves it valid.
pub(crate) const CAS_RETRIES: u32 = 8;

/// The retry hint when a transition keeps losing its CAS, ms.
const CONTENDED_RETRY_MS: u64 = 100;

/// The holder a workspace's drafts record.
pub(crate) fn holder(name: &str) -> String {
    format!("ws:{name}")
}

/// Whether `record` is a draft held by workspace `name`.
pub(crate) fn held_by(record: &ChangesetRecord, name: &str) -> bool {
    record.state == RecordState::Draft
        && record.holder.as_deref().and_then(|h| h.strip_prefix("ws:")) == Some(name)
}

/// Checks a workspace name: at most 128 bytes of `/`-separated segments,
/// each of lowercase ASCII letters, digits, `.`, `_` and `-`, starting with
/// a letter or digit. Lowercase ASCII only, so two names never differ only
/// by case or by Unicode normalisation.
pub(crate) fn check_name(name: &str) -> Result<(), ApiError> {
    let segment = |seg: &str| {
        seg.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && seg.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
            })
    };
    if name.len() <= MAX_WORKSPACE_NAME && name.split('/').all(segment) {
        Ok(())
    } else {
        Err(ApiError::InvalidPath {
            path: name.chars().take(MAX_WORKSPACE_NAME).collect(),
            reason: "workspace-name".to_owned(),
        })
    }
}

/// The name for an `OpenWorkspace` that names none: derived from the caller,
/// the request id and the line, so a retried open finds the same workspace.
fn generated_name(cx: &CallContext, line: &str) -> String {
    let seed = format!("{}\0{}\0{line}", cx.principal.subject, cx.request_id);
    let hex = ObjectId::hash(ObjectKind::Blob, seed.as_bytes()).to_hex();
    format!("w-{}", &hex[..16])
}

pub(crate) fn view(ws: WorkspaceRecord) -> WorkspaceView {
    WorkspaceView {
        name: ws.name,
        owner: ws.owner,
        session: ws.session,
        line: ws.line,
        base: ws.base,
        head: ws.head,
        lease_until_ms: ws.lease_until_ms,
    }
}

/// `Unavailable` for a transition that kept losing its CAS.
pub(crate) fn contended() -> ApiError {
    ApiError::Unavailable {
        retry_after_ms: CONTENDED_RETRY_MS,
    }
}

/// `RefConflict` for a stale head expectation, naming the workspace.
pub(crate) fn head_conflict(ws: &WorkspaceRecord) -> ApiError {
    ApiError::RefConflict {
        name: ws.name.clone(),
        actual: ws.head,
    }
}

/// Appends a record transition guarded by a CAS on its current state, with
/// the record's new expiry (design §8.6): `None` for a draft a workspace
/// holds, the end time plus the retention window for a draft that stops
/// being one.
pub(crate) fn transition(
    id: ChangesetId,
    mut record: ChangesetRecord,
    state: RecordState,
    holder: Option<String>,
    superseded_by: Option<ChangesetId>,
    expires_at_ms: Option<i64>,
    txn: &mut MetaTxn,
) {
    txn.preconditions.push(Precondition::RecordStateIs {
        id,
        state: record.state,
    });
    record.state = state;
    record.holder = holder;
    record.superseded_by = superseded_by;
    txn.writes.push(Write::SetRecord { id, record });
    txn.writes.push(Write::SetExpiry { id, expires_at_ms });
}

/// A transaction whose first precondition is that `ws` is unchanged.
pub(crate) fn guarded(ws: &WorkspaceRecord) -> MetaTxn {
    MetaTxn {
        preconditions: vec![Precondition::WorkspaceIs {
            name: ws.name.clone(),
            expected: Some(ws.clone()),
        }],
        writes: Vec::new(),
    }
}

/// The two gauge families G2.1 reports (`docs/design.md` §13).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gauges {
    /// `graft_orphans{state="held"}`: drafts, each held by a workspace.
    pub held: u64,
    /// `graft_orphans{state="abandoned"}`: changesets nothing holds.
    pub abandoned: u64,
    /// `graft_orphans{state="superseded"}`: drafts replaced by a successor.
    pub superseded: u64,
    /// `graft_workspaces{state="active"}`: workspaces with a live lease.
    pub active: u64,
    /// `graft_workspaces{state="lapsed"}`: workspaces whose lease ended and
    /// that [`Engine::expire_leases`] has not closed yet.
    pub lapsed: u64,
}

impl Gauges {
    /// `(metric, state, value)` samples, for the telemetry registry.
    pub fn samples(&self) -> [(&'static str, &'static str, u64); 5] {
        [
            ("graft_orphans", "held", self.held),
            ("graft_orphans", "abandoned", self.abandoned),
            ("graft_orphans", "superseded", self.superseded),
            ("graft_workspaces", "active", self.active),
            ("graft_workspaces", "lapsed", self.lapsed),
        ]
    }
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// The lease end for a requested length: the default for 0, otherwise
    /// clamped to the longest lease.
    fn lease_until(&self, lease_ms: u64) -> i64 {
        let ms = match lease_ms {
            0 => self.config.default_lease_ms,
            n => n,
        }
        .clamp(1, self.config.max_lease_ms.max(1));
        self.clock
            .now()
            .ms
            .saturating_add(i64::try_from(ms).unwrap_or(i64::MAX))
    }

    /// Whether a workspace's lease has ended.
    pub(crate) fn lapsed(&self, ws: &WorkspaceRecord) -> bool {
        ws.lease_until_ms < self.clock.now().ms
    }

    pub(crate) async fn load_workspace(
        &self,
        repo: RepoId,
        name: &str,
    ) -> Result<WorkspaceRecord, ApiError> {
        self.meta
            .workspace(repo, name)
            .await
            .map_err(errors::store)?
            .ok_or_else(|| errors::not_found(format!("workspace {name}")))
    }

    /// A workspace the caller may change: present, theirs (or the caller
    /// administers the repository), and with a live lease. A lapsed lease
    /// ends the workspace (its drafts become abandoned) and fails with
    /// `LeaseExpired`.
    pub(crate) async fn live_workspace(
        &self,
        cx: &CallContext,
        repo_name: &str,
        repo: RepoId,
        name: &str,
    ) -> Result<WorkspaceRecord, ApiError> {
        let ws = self.load_workspace(repo, name).await?;
        if ws.owner != cx.principal.subject && !is_admin(cx, repo_name) {
            return Err(ApiError::PermissionDenied {
                scope: format!("repo:{repo_name}:admin"),
                path: None,
            });
        }
        if self.lapsed(&ws) {
            // A concurrent change to the record means someone else ended
            // or renewed it; either way this caller's lease is gone.
            self.end_workspace(repo, &ws, "lease-expired").await?;
            return Err(ApiError::LeaseExpired {
                workspace: name.to_owned(),
            });
        }
        Ok(ws)
    }

    /// How many drafts `head` stacks on `base`: at least one, at most
    /// `max_stack`. Two skip-list reads and O(log n) jumps, before any walk.
    pub(crate) async fn stack_depth(
        &self,
        repo: RepoId,
        base: ChangesetId,
        head: ChangesetId,
        writes: &mut Vec<Write>,
    ) -> Result<u64, ApiError> {
        let not_on_base = || errors::invalid(None, "not-on-base");
        let head_depth = self.skips(repo, head, writes).await?.depth;
        let base_depth = self.skips(repo, base, writes).await?.depth;
        let distance = head_depth
            .checked_sub(base_depth)
            .filter(|d| *d > 0)
            .ok_or_else(not_on_base)?;
        if distance > u64::from(self.config.max_stack) {
            return Err(errors::limit("stack", self.config.max_stack.into()));
        }
        if self.ancestor_at(repo, head, distance, writes).await? != Some(base) {
            return Err(not_on_base());
        }
        Ok(distance)
    }

    /// The stack of `head` over `base`, top first, with each record.
    async fn stack(
        &self,
        repo: RepoId,
        base: ChangesetId,
        head: Option<ChangesetId>,
        writes: &mut Vec<Write>,
    ) -> Result<Vec<(ChangesetId, ChangesetRecord)>, ApiError> {
        let stack = self.stack_content(repo, base, head, writes).await?;
        Ok(stack
            .into_iter()
            .map(|(id, _, record)| (id, record))
            .collect())
    }

    /// The stack of `head` over `base`, top first, with each changeset and
    /// its record.
    pub(crate) async fn stack_content(
        &self,
        repo: RepoId,
        base: ChangesetId,
        head: Option<ChangesetId>,
        writes: &mut Vec<Write>,
    ) -> Result<Vec<(ChangesetId, Changeset, ChangesetRecord)>, ApiError> {
        let Some(head) = head else {
            return Ok(Vec::new());
        };
        let depth = self.stack_depth(repo, base, head, writes).await?;
        let mut out = Vec::with_capacity(depth as usize);
        let mut cur = Some(head);
        for _ in 0..depth {
            let id = cur.ok_or_else(|| errors::invalid(None, "not-on-base"))?;
            let (cs, record) = self.changeset(repo, id).await?;
            cur = cs.first_parent();
            out.push((id, cs, record));
        }
        Ok(out)
    }

    /// Ends a workspace: its drafts become abandoned and the record goes, in
    /// one transaction. `false` when the workspace changed meanwhile.
    async fn end_workspace(
        &self,
        repo: RepoId,
        ws: &WorkspaceRecord,
        reason: &str,
    ) -> Result<bool, ApiError> {
        let mut txn = guarded(ws);
        let stack = self.stack(repo, ws.base, ws.head, &mut txn.writes).await?;
        // A lapsed lease ended at `lease_until`; a close ends it now.
        let expires = self.retained_until(ws.lease_until_ms.min(self.clock.now().ms));
        let mut abandoned = 0;
        for (id, record) in stack {
            if held_by(&record, &ws.name) {
                transition(
                    id,
                    record,
                    RecordState::Abandoned,
                    None,
                    None,
                    expires,
                    &mut txn,
                );
                abandoned += 1;
            }
        }
        txn.writes.push(Write::DeleteWorkspace(ws.name.clone()));
        txn.writes
            .push(journal::workspace_closed(&ws.name, reason, abandoned));
        match self.meta.commit(repo, txn).await.map_err(errors::store)? {
            TxnOutcome::Committed(_) => Ok(true),
            TxnOutcome::PreconditionFailed(_) => Ok(false),
        }
    }

    pub(crate) async fn do_open_workspace(
        &self,
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> Result<WorkspaceView, ApiError> {
        require(cx, &req.repo, Need::Workspace)?;
        let name = match req.name {
            Some(name) => name,
            None => generated_name(cx, &req.line),
        };
        check_name(&name)?;
        let repo = self.repo_id(&req.repo).await?;
        let line = self
            .meta
            .get_ref(repo, &req.line)
            .await
            .map_err(errors::store)?
            .ok_or_else(|| errors::not_found(format!("ref {}", req.line)))?;
        if line.kind != RefKind::Line {
            return Err(errors::invalid(None, "not-a-line"));
        }
        let base = match req.at {
            None => line.target,
            Some(at) if at == line.target => at,
            Some(at) => {
                let on_line = self
                    .meta
                    .line_pos(repo, &req.line, at)
                    .await
                    .map_err(errors::store)?;
                if on_line.is_none() {
                    return Err(errors::invalid(None, "base-not-on-line"));
                }
                at
            }
        };
        let record = WorkspaceRecord {
            name: name.clone(),
            owner: cx.principal.subject.clone(),
            session: cx.principal.session.clone(),
            line: req.line,
            base,
            head: None,
            lease_until_ms: self.lease_until(req.lease_ms),
            // Set from `cx.principal.work_item` once the SQLite store keeps
            // the column (WI-2310 step b); a store that drops it would fail
            // every later whole-record CAS.
            work_item: None,
        };

        for _ in 0..CAS_RETRIES {
            match self
                .meta
                .workspace(repo, &name)
                .await
                .map_err(errors::store)?
            {
                // A lapsed workspace frees its name.
                Some(ws) if self.lapsed(&ws) => {
                    self.end_workspace(repo, &ws, "lease-expired").await?;
                    continue;
                }
                // A retried open finds its own workspace.
                Some(ws)
                    if ws.owner == record.owner
                        && ws.line == record.line
                        && req.at.is_none_or(|at| at == ws.base) =>
                {
                    return Ok(view(ws));
                }
                Some(_) => {
                    return Err(ApiError::RefConflict { name, actual: None });
                }
                None => {}
            }
            let txn = MetaTxn {
                preconditions: vec![Precondition::WorkspaceIs {
                    name: name.clone(),
                    expected: None,
                }],
                writes: vec![
                    Write::PutWorkspace(record.clone()),
                    journal::workspace_opened(&name, &record.owner, &record.line, base),
                ],
            };
            if let TxnOutcome::Committed(_) =
                self.meta.commit(repo, txn).await.map_err(errors::store)?
            {
                return Ok(view(record));
            }
        }
        Err(contended())
    }

    pub(crate) async fn do_get_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
    ) -> Result<WorkspaceView, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        check_name(&req.name)?;
        let repo = self.repo_id(&req.repo).await?;
        self.load_workspace(repo, &req.name).await.map(view)
    }

    /// The caller's workspaces in name order. The page token is the last
    /// name of the previous page.
    pub(crate) async fn do_list_workspaces(
        &self,
        cx: &CallContext,
        repo_name: String,
        page: Page,
    ) -> Result<Vec<WorkspaceView>, ApiError> {
        require(cx, &repo_name, Need::Read)?;
        let repo = self.repo_id(&repo_name).await?;
        let after = page.token.unwrap_or_default();
        let rows = self
            .meta
            .list_workspaces(
                repo,
                &cx.principal.subject,
                &after,
                self.page_size(page.limit),
            )
            .await
            .map_err(errors::store)?;
        Ok(rows.into_iter().map(view).collect())
    }

    pub(crate) async fn do_renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> Result<WorkspaceView, ApiError> {
        require(cx, &req.repo, Need::Workspace)?;
        check_name(&req.name)?;
        let repo = self.repo_id(&req.repo).await?;
        for _ in 0..CAS_RETRIES {
            let ws = self.live_workspace(cx, &req.repo, repo, &req.name).await?;
            let mut next = ws.clone();
            next.lease_until_ms = self.lease_until(lease_ms);
            let mut txn = guarded(&ws);
            txn.writes.push(Write::PutWorkspace(next.clone()));
            txn.writes
                .push(journal::workspace_renewed(&next.name, next.lease_until_ms));
            if let TxnOutcome::Committed(_) =
                self.meta.commit(repo, txn).await.map_err(errors::store)?
            {
                return Ok(view(next));
            }
        }
        Err(contended())
    }

    /// Moves the head by CAS. Drafts that leave the stack become superseded
    /// by the new head when it is new to the stack, and abandoned when the
    /// head moves down the stack or to none. Changesets that join the stack
    /// must be abandoned (nothing holds them) and become this workspace's
    /// drafts.
    pub(crate) async fn do_set_head(
        &self,
        cx: &CallContext,
        req: SetHead,
    ) -> Result<WorkspaceView, ApiError> {
        let wref = &req.workspace;
        require(cx, &wref.repo, Need::Workspace)?;
        check_name(&wref.name)?;
        let repo = self.repo_id(&wref.repo).await?;
        if let Some(new) = req.new {
            // Present and not collected.
            self.changeset(repo, new).await?;
        }
        for _ in 0..CAS_RETRIES {
            let ws = self
                .live_workspace(cx, &wref.repo, repo, &wref.name)
                .await?;
            if ws.head != req.expected {
                return Err(head_conflict(&ws));
            }
            if req.new == ws.head {
                return Ok(view(ws));
            }
            let mut txn = guarded(&ws);
            let old = self.stack(repo, ws.base, ws.head, &mut txn.writes).await?;
            let new = self.stack(repo, ws.base, req.new, &mut txn.writes).await?;
            let old_ids: HashSet<ChangesetId> = old.iter().map(|(id, _)| *id).collect();
            let new_ids: HashSet<ChangesetId> = new.iter().map(|(id, _)| *id).collect();

            for (id, record) in new {
                if old_ids.contains(&id) || held_by(&record, &ws.name) {
                    continue;
                }
                let rule = match record.state {
                    RecordState::Abandoned => {
                        // Adopting a changeset writes its changes into this
                        // workspace: the caller needs write on every path it
                        // changes, as `CreateChangeset` does (design §12.2).
                        let (cs, _) = self.changeset(repo, id).await?;
                        require_changes(cx, &wref.repo, &cs.changes)?;
                        transition(
                            id,
                            record,
                            RecordState::Draft,
                            Some(holder(&ws.name)),
                            None,
                            None,
                            &mut txn,
                        );
                        continue;
                    }
                    RecordState::Draft => "draft-held",
                    RecordState::Landed => "draft-landed",
                    RecordState::Superseded => "draft-superseded",
                    RecordState::Collected => return Err(ApiError::Collected { id }),
                };
                return Err(errors::invalid(None, rule));
            }
            let successor = req.new.filter(|n| !old_ids.contains(n));
            let expires = self.retained_until(self.clock.now().ms);
            for (id, record) in old {
                if new_ids.contains(&id) || !held_by(&record, &ws.name) {
                    continue;
                }
                let state = match successor {
                    Some(_) => RecordState::Superseded,
                    None => RecordState::Abandoned,
                };
                transition(id, record, state, None, successor, expires, &mut txn);
            }

            let mut next = ws.clone();
            next.head = req.new;
            txn.writes.push(Write::PutWorkspace(next.clone()));
            txn.writes
                .push(journal::workspace_head_moved(&ws.name, ws.head, req.new));
            if let TxnOutcome::Committed(_) =
                self.meta.commit(repo, txn).await.map_err(errors::store)?
            {
                return Ok(view(next));
            }
        }
        Err(contended())
    }

    /// Closes a workspace. Its drafts become abandoned whether or not
    /// `keep_drafts` is set: a draft may not outlive its holder, and an
    /// abandoned draft is kept for the retention window, readable and
    /// adoptable by `SetHead` in another workspace. Closing a workspace
    /// that does not exist (a retry) succeeds.
    pub(crate) async fn do_close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        _keep_drafts: bool,
    ) -> Result<(), ApiError> {
        require(cx, &req.repo, Need::Workspace)?;
        check_name(&req.name)?;
        let repo = self.repo_id(&req.repo).await?;
        for _ in 0..CAS_RETRIES {
            let Some(ws) = self
                .meta
                .workspace(repo, &req.name)
                .await
                .map_err(errors::store)?
            else {
                return Ok(());
            };
            if ws.owner != cx.principal.subject && !is_admin(cx, &req.repo) {
                return Err(ApiError::PermissionDenied {
                    scope: format!("repo:{}:admin", req.repo),
                    path: None,
                });
            }
            let reason = if self.lapsed(&ws) {
                "lease-expired"
            } else {
                "closed"
            };
            if self.end_workspace(repo, &ws, reason).await? {
                return Ok(());
            }
        }
        Err(contended())
    }

    /// Ends up to `limit` workspaces whose lease has lapsed, abandoning
    /// their drafts, and returns how many it ended. The server calls it
    /// periodically. Its cost follows the lapsed workspaces (one index
    /// range), never all of them.
    pub async fn expire_leases(&self, repo: &str, limit: u32) -> Result<u32, ApiError> {
        let repo = self.repo_id(repo).await?;
        let lapsed = self
            .meta
            .expired_workspaces(repo, self.clock.now().ms, self.page_size(limit))
            .await
            .map_err(errors::store)?;
        let mut ended = 0;
        for ws in lapsed {
            if self.end_workspace(repo, &ws, "lease-expired").await? {
                ended += 1;
            }
        }
        Ok(ended)
    }

    /// The `graft_orphans{state}` and `graft_workspaces{state}` gauges for
    /// one repository. Each count costs the population it counts.
    pub async fn gauges(&self, repo: &str) -> Result<Gauges, ApiError> {
        let repo = self.repo_id(repo).await?;
        let records = |state| self.meta.count_records(repo, state);
        let held = records(RecordState::Draft).await.map_err(errors::store)?;
        let abandoned = records(RecordState::Abandoned)
            .await
            .map_err(errors::store)?;
        let superseded = records(RecordState::Superseded)
            .await
            .map_err(errors::store)?;
        let total = self
            .meta
            .count_workspaces(repo, None)
            .await
            .map_err(errors::store)?;
        let lapsed = self
            .meta
            .count_workspaces(repo, Some(self.clock.now().ms))
            .await
            .map_err(errors::store)?;
        Ok(Gauges {
            held,
            abandoned,
            superseded,
            active: total.saturating_sub(lapsed),
            lapsed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_names() {
        for ok in [
            "w",
            "w-1",
            "github-pr/12",
            "a.b_c",
            "0",
            &"x".repeat(MAX_WORKSPACE_NAME),
        ] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "W",
            "ws-A",
            "-w",
            ".w",
            "a//b",
            "a/",
            "/a",
            "a/../b",
            "sp ace",
            "w:1",
            "caf\u{e9}",
            "cafe\u{301}",
            "\u{212a}elvin",
            &"x".repeat(MAX_WORKSPACE_NAME + 1),
        ] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn holders_name_their_workspace_exactly() {
        let record = |holder: &str| ChangesetRecord {
            lsn: 1,
            received_at_ms: 0,
            state: RecordState::Draft,
            holder: Some(holder.to_owned()),
            superseded_by: None,
        };
        assert!(held_by(&record(&holder("w")), "w"));
        assert!(!held_by(&record(&holder("w2")), "w"));
        assert!(!held_by(&record("land:w"), "w"));
        let mut abandoned = record(&holder("w"));
        abandoned.state = RecordState::Abandoned;
        assert!(!held_by(&abandoned, "w"));
    }
}
