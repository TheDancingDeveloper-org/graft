//! Workspaces, drafts and land for [`SnapshotRepo`] (work item G2.5): a
//! small model of the engine's workspace API (`docs/design.md` §3, §7.2,
//! §8), enough to drive the client's write commands end to end.
//!
//! - `CreateChangeset` derives the root, stores the changeset, and with a
//!   workspace moves its head from `parents[0]` by compare-and-swap. Without
//!   one the changeset is abandoned, as in the engine.
//! - `SetHead` moves the head by CAS. Drafts that leave the stack are
//!   superseded by a new head, or abandoned when the head moves down.
//!   Abandoned changesets that join the stack are adopted.
//! - `Sync` moves the base to the line tip and re-applies each draft's
//!   changes on top. A draft path the line also changed is a conflict.
//! - `Land` fast-forwards a direct line. On a line marked git-primary
//!   ([`SnapshotRepo::set_git_primary`]) it opens a fake pull request and
//!   answers `Queued { pr_url }`; asking again answers `Queued` while the PR
//!   is open, `Landed` once [`SnapshotRepo::merge_pr`] merged it, and
//!   `Rejected` once [`SnapshotRepo::close_pr`] closed it.
//!
//! Any operation can be made to fail with [`SnapshotRepo::fail_next`].

use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use graft_model::{
    ApplyError, ApplyOptions, Change, Changeset, ChangesetId, RepoPath, apply_changes_with,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::ApiResult;
use graft_proto::error::ConflictPath;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext};

use super::{FakeSource, SnapshotRepo, empty_root, non_empty};
use crate::fake_api::admin_cx;

/// The lease when a request asks for none, ms (the engine's default).
const DEFAULT_LEASE_MS: u64 = 2 * 3600 * 1000;

/// The longest stack the fake walks.
const MAX_STACK: usize = 1000;

/// One workspace.
#[derive(Clone, Debug)]
pub(super) struct Ws {
    owner: String,
    line: String,
    base: ChangesetId,
    head: Option<ChangesetId>,
    lease_until_ms: i64,
}

/// A fake pull request.
#[derive(Clone, Debug)]
struct Pr {
    url: String,
    head: ChangesetId,
    line: String,
    state: PrState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrState {
    Open,
    Merged(ChangesetId),
    Closed,
}

/// The workspace state of a [`SnapshotRepo`].
#[derive(Default)]
pub(super) struct Workspaces {
    by_name: BTreeMap<String, Ws>,
    next_name: u64,
    /// Git-primary lines, with the base URL of their fake PRs.
    git_primary: BTreeMap<String, String>,
    prs: Vec<Pr>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn view(name: &str, ws: &Ws) -> WorkspaceView {
    WorkspaceView {
        name: name.to_owned(),
        owner: ws.owner.clone(),
        session: None,
        line: ws.line.clone(),
        base: ws.base,
        head: ws.head,
        lease_until_ms: ws.lease_until_ms,
    }
}

fn not_found(what: impl std::fmt::Display) -> ApiError {
    ApiError::NotFound {
        what: what.to_string(),
    }
}

fn invalid(path: Option<&RepoPath>, rule: &str) -> ApiError {
    ApiError::InvalidChange {
        path: path.map(|p| p.as_str().to_owned()).unwrap_or_default(),
        rule: rule.to_owned(),
    }
}

fn apply_error(e: ApplyError) -> ApiError {
    match e {
        ApplyError::Invalid(v) => invalid(v.path.as_ref(), v.rule.as_str()),
        ApplyError::Source(e) => invalid(None, &format!("source: {e}")),
    }
}

impl SnapshotRepo {
    /// Makes `line` git-primary: a land on it opens a fake pull request
    /// under `pr_base` (for example `https://github.example/o/r`).
    pub fn set_git_primary(&self, line: &str, pr_base: &str) {
        self.ws_state()
            .git_primary
            .insert(line.to_owned(), pr_base.to_owned());
    }

    /// The URLs of the fake pull requests, oldest first.
    pub fn pr_urls(&self) -> Vec<String> {
        self.ws_state().prs.iter().map(|p| p.url.clone()).collect()
    }

    /// Merges the fake pull request `url` the way the PR bridge's follow
    /// would see it: the stack's net change lands on the line as one
    /// changeset (a squash merge) unless the stack sits on the tip, when the
    /// line fast-forwards to it. The drafts become landed. Returns the new
    /// tip.
    pub async fn merge_pr(&self, url: &str) -> ChangesetId {
        let pr = self
            .ws_state()
            .prs
            .iter()
            .find(|p| p.url == url)
            .cloned()
            .expect("known PR");
        let drafts = self.draft_chain(pr.head);
        let bottom_parent = drafts
            .last()
            .and_then(|d| self.changeset_any(*d))
            .and_then(|c| c.parents.first().copied())
            .expect("a stack");
        let tip = self.tip(&pr.line).expect("line");
        let landed = if bottom_parent == tip {
            pr.head
        } else {
            let changes = self.net_changes(bottom_parent, pr.head).await;
            let message = format!("Merge {url}");
            self.store_changeset(tip, changes, &message)
                .await
                .expect("the PR applies cleanly")
                .0
        };
        for d in drafts {
            self.set_record(d, DraftState::Landed, None);
        }
        self.move_line(&pr.line, landed);
        for p in &mut self.ws_state().prs {
            if p.url == url {
                p.state = PrState::Merged(landed);
            }
        }
        landed
    }

    /// Closes the fake pull request `url` without merging it.
    pub fn close_pr(&self, url: &str) {
        for p in &mut self.ws_state().prs {
            if p.url == url {
                p.state = PrState::Closed;
            }
        }
    }

    /// Makes the next call of `op` (an RPC name such as `"Land"` or
    /// `"CreateChangeset"`) fail with `error`. Queued failures are used in
    /// order, one per call.
    pub fn fail_next(&self, op: &'static str, error: ApiError) {
        self.failures
            .lock()
            .expect("failures")
            .entry(op)
            .or_default()
            .push_back(error);
    }

    /// Ends the lease of workspace `name` now.
    pub fn expire_workspace(&self, name: &str) {
        if let Some(ws) = self.ws_state().by_name.get_mut(name) {
            ws.lease_until_ms = 0;
        }
    }

    /// Workspace `name`, if it is open.
    pub fn workspace(&self, name: &str) -> Option<WorkspaceView> {
        self.ws_state().by_name.get(name).map(|ws| view(name, ws))
    }

    /// The state of a changeset: drafts and their successors as recorded,
    /// everything else landed.
    pub fn draft_state(&self, id: ChangesetId) -> (DraftState, Option<ChangesetId>) {
        self.records
            .lock()
            .expect("records")
            .get(&id)
            .copied()
            .unwrap_or((DraftState::Landed, None))
    }

    pub(super) fn take_failure(&self, op: &'static str) -> ApiResult<()> {
        match self
            .failures
            .lock()
            .expect("failures")
            .get_mut(op)
            .and_then(|q| q.pop_front())
        {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn ws_state(&self) -> std::sync::MutexGuard<'_, Workspaces> {
        self.ws.lock().expect("workspaces")
    }

    pub(super) fn set_record(&self, id: ChangesetId, state: DraftState, by: Option<ChangesetId>) {
        self.records
            .lock()
            .expect("records")
            .insert(id, (state, by));
    }

    fn changeset_any(&self, id: ChangesetId) -> Option<Changeset> {
        self.raw_changesets
            .lock()
            .expect("raw")
            .get(&id)
            .cloned()
            .or_else(|| self.api.changeset(id))
    }

    /// The drafts from `head` down its first parents, newest first, while
    /// they are drafts.
    fn draft_chain(&self, head: ChangesetId) -> Vec<ChangesetId> {
        let mut out = Vec::new();
        let mut cur = Some(head);
        while let Some(id) = cur {
            if self.draft_state(id).0 == DraftState::Landed || out.len() >= MAX_STACK {
                break;
            }
            out.push(id);
            cur = self
                .changeset_any(id)
                .and_then(|c| c.parents.first().copied());
        }
        out
    }

    /// The stack of a workspace: `head` down to, not including, `base`.
    fn stack_of(
        &self,
        base: ChangesetId,
        head: Option<ChangesetId>,
    ) -> ApiResult<Vec<ChangesetId>> {
        let mut out = Vec::new();
        let mut cur = head;
        while let Some(id) = cur {
            if id == base {
                return Ok(out);
            }
            if out.len() >= MAX_STACK {
                return Err(invalid(None, "stack-too-deep"));
            }
            out.push(id);
            cur = self
                .changeset_any(id)
                .ok_or_else(|| not_found(format!("changeset {id}")))?
                .parents
                .first()
                .copied();
        }
        if head.is_none() {
            return Ok(out);
        }
        Err(invalid(None, "head-not-on-base"))
    }

    /// The changes that take snapshot `from` to snapshot `to`.
    async fn net_changes(&self, from: ChangesetId, to: ChangesetId) -> Vec<Change> {
        let mut out = Vec::new();
        let mut token = None;
        loop {
            let page = self
                .diff(
                    &admin_cx(),
                    Diff {
                        repo: String::new(),
                        from,
                        to,
                        paths: Vec::new(),
                        patch: None,
                        page: Page { limit: 1000, token },
                    },
                )
                .await
                .expect("diff");
            out.extend(page.files.into_iter().map(|f| f.change));
            match page.next {
                Some(n) => token = Some(n),
                None => return out,
            }
        }
    }

    /// Derives the root of `changes` on `parent` and stores the changeset
    /// (validated by the fake's import). Returns its id and root.
    async fn store_changeset(
        &self,
        parent: ChangesetId,
        changes: Vec<Change>,
        message: &str,
    ) -> ApiResult<(ChangesetId, graft_model::TreeId)> {
        let template = self
            .changeset_any(parent)
            .ok_or(ApiError::UnknownParent { id: parent })?;
        let changeset = Changeset {
            parents: vec![parent],
            root: empty_root(),
            changes,
            author: template.author,
            authored_at: template.authored_at,
            message: message.to_owned(),
            metadata: Default::default(),
        };
        self.store(changeset).await
    }

    /// Fills in `changeset.root` from its first parent and stores it.
    pub(super) async fn store(
        &self,
        mut changeset: Changeset,
    ) -> ApiResult<(ChangesetId, graft_model::TreeId)> {
        let parent = *changeset
            .parents
            .first()
            .ok_or_else(|| invalid(None, "root-changeset"))?;
        let base = self
            .root(parent)
            .ok_or(ApiError::UnknownParent { id: parent })?;
        let applied = apply_changes_with(
            &FakeSource(&self.api),
            non_empty(base),
            &changeset.changes,
            &ApplyOptions { case_clash: true },
        )
        .await
        .map_err(apply_error)?;
        changeset.root = applied.root.unwrap_or_else(empty_root);
        let id = changeset.id();
        let root = changeset.root;
        self.api
            .import_changesets(
                &admin_cx(),
                ImportChangesets {
                    repo: String::new(),
                    changesets: vec![changeset],
                },
            )
            .await?;
        Ok((id, root))
    }

    /// Moves `line` to `tip` and refreshes its file map.
    fn move_line(&self, line: &str, tip: ChangesetId) {
        let root = self.root(tip).unwrap_or_else(empty_root);
        let files = self
            .api
            .snapshot(root)
            .into_iter()
            .map(|(path, (kind, id))| {
                let bytes = self
                    .api
                    .blob(graft_model::BlobId(id))
                    .expect("snapshot blob");
                (path, (kind, bytes))
            })
            .collect();
        let mut lines = self.lines.lock().expect("lines");
        let version = lines.get(line).map_or(0, |l| l.version);
        lines.insert(
            line.to_owned(),
            super::Line {
                tip,
                version: version + 1,
                files,
            },
        );
    }

    /// A live workspace, or `LeaseExpired` (ending it) when its lease lapsed.
    fn live(&self, name: &str) -> ApiResult<Ws> {
        let ws = self
            .ws_state()
            .by_name
            .get(name)
            .cloned()
            .ok_or_else(|| not_found(format!("workspace {name}")))?;
        if ws.lease_until_ms < now_ms() {
            self.end(name);
            return Err(ApiError::LeaseExpired {
                workspace: name.to_owned(),
            });
        }
        Ok(ws)
    }

    /// Removes a workspace; its drafts become abandoned.
    fn end(&self, name: &str) {
        let Some(ws) = self.ws_state().by_name.remove(name) else {
            return;
        };
        for id in self.stack_of(ws.base, ws.head).unwrap_or_default() {
            if self.draft_state(id).0 == DraftState::Draft {
                self.set_record(id, DraftState::Abandoned, None);
            }
        }
    }

    /// `CreateChangeset` (see the module documentation).
    pub(super) async fn create(&self, req: CreateChangeset) -> ApiResult<Created> {
        self.take_failure("CreateChangeset")?;
        if !req.inline_blobs.is_empty() {
            self.api
                .put_objects(
                    &admin_cx(),
                    PutObjects {
                        repo: String::new(),
                        blobs: req.inline_blobs.clone(),
                    },
                )
                .await?;
        }
        let parent = *req
            .parents
            .first()
            .ok_or_else(|| invalid(None, "root-changeset"))?;
        let ws = match &req.workspace {
            Some(name) => Some(self.live(name)?),
            None => None,
        };
        let changeset = Changeset {
            parents: req.parents,
            root: empty_root(),
            changes: req.changes,
            author: req.author,
            authored_at: req.authored_at,
            message: req.message,
            metadata: req.metadata,
        };
        // The id is known only once the root is: check the workspace after.
        let (id, root) = self.store(changeset).await?;
        let state = match (&req.workspace, ws) {
            (Some(name), Some(ws)) => {
                if ws.head != Some(id) {
                    if ws.head.unwrap_or(ws.base) != parent {
                        return Err(ApiError::RefConflict {
                            name: name.clone(),
                            actual: ws.head,
                        });
                    }
                    let mut all = self.ws_state();
                    let current = all.by_name.get_mut(name).expect("live workspace");
                    if current.head != ws.head {
                        return Err(ApiError::RefConflict {
                            name: name.clone(),
                            actual: current.head,
                        });
                    }
                    current.head = Some(id);
                    drop(all);
                    self.set_record(id, DraftState::Draft, None);
                }
                DraftState::Draft
            }
            _ => {
                if !self.records.lock().expect("records").contains_key(&id) {
                    self.set_record(id, DraftState::Abandoned, None);
                }
                self.draft_state(id).0
            }
        };
        let lsn = self.api.changeset_count() as u64;
        Ok(Created {
            id,
            root,
            lsn,
            state,
        })
    }

    /// `GetChangesets` states for drafts.
    pub(super) fn restate(&self, out: &mut Changesets) {
        for v in &mut out.found {
            let (state, by) = self.draft_state(v.changeset.id());
            v.state = state;
            v.superseded_by = by;
        }
    }

    /// `Land` (see the module documentation).
    pub(super) async fn do_land(&self, req: Land) -> ApiResult<Landed> {
        self.take_failure("Land")?;
        let pr_base = self.ws_state().git_primary.get(&req.line).cloned();
        if let Some(pr_base) = pr_base {
            let mut all = self.ws_state();
            if let Some(pr) =
                all.prs.iter().rev().find(|p| {
                    p.head == req.head && p.line == req.line && p.state != PrState::Closed
                })
            {
                return Ok(match pr.state {
                    PrState::Merged(landed) => Landed::Landed {
                        landed,
                        line_version: 0,
                        pos: 0,
                        rebased: landed != req.head,
                        merged_paths: Vec::new(),
                    },
                    _ => Landed::Queued {
                        pr_url: pr.url.clone(),
                    },
                });
            }
            if let Some(pr) = all
                .prs
                .iter()
                .rev()
                .find(|p| p.head == req.head && p.line == req.line)
            {
                return Err(ApiError::Rejected {
                    policy: "pr-bridge".to_owned(),
                    detail: format!("pull request closed: {}", pr.url),
                });
            }
            let url = format!("{pr_base}/pull/{}", all.prs.len() + 1);
            all.prs.push(Pr {
                url: url.clone(),
                head: req.head,
                line: req.line,
                state: PrState::Open,
            });
            return Ok(Landed::Queued { pr_url: url });
        }
        let tip = self
            .tip(&req.line)
            .ok_or_else(|| not_found(format!("ref {}", req.line)))?;
        let drafts = self.draft_chain(req.head);
        let bottom_parent = match drafts.last() {
            Some(d) => self
                .changeset_any(*d)
                .and_then(|c| c.parents.first().copied()),
            // Already landed: a retry.
            None => Some(req.head),
        };
        if bottom_parent != Some(tip) && !drafts.is_empty() {
            return Err(ApiError::Conflict {
                paths: Vec::new(),
                onto: tip,
            });
        }
        for d in &drafts {
            self.set_record(*d, DraftState::Landed, None);
        }
        if !drafts.is_empty() {
            self.move_line(&req.line, req.head);
        }
        Ok(Landed::Landed {
            landed: req.head,
            line_version: 0,
            pos: 0,
            rebased: false,
            merged_paths: Vec::new(),
        })
    }
}

impl WorkspaceApi for SnapshotRepo {
    async fn open_workspace(
        &self,
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        self.take_failure("OpenWorkspace")?;
        let tip = self
            .tip(&req.line)
            .ok_or_else(|| not_found(format!("ref {}", req.line)))?;
        let base = req.at.unwrap_or(tip);
        let lease = if req.lease_ms == 0 {
            DEFAULT_LEASE_MS
        } else {
            req.lease_ms
        };
        let mut all = self.ws_state();
        let name = match req.name {
            Some(n) => n,
            None => {
                all.next_name += 1;
                format!("w-{}", all.next_name)
            }
        };
        if let Some(ws) = all.by_name.get(&name) {
            if ws.lease_until_ms >= now_ms() {
                if ws.line == req.line && req.at.is_none_or(|at| at == ws.base) {
                    return Ok(view(&name, ws));
                }
                return Err(ApiError::RefConflict { name, actual: None });
            }
            drop(all);
            self.end(&name);
            all = self.ws_state();
        }
        let ws = Ws {
            owner: cx.principal.subject.clone(),
            line: req.line,
            base,
            head: None,
            lease_until_ms: now_ms() + lease as i64,
        };
        let out = view(&name, &ws);
        all.by_name.insert(name, ws);
        Ok(out)
    }

    async fn get_workspace(
        &self,
        _cx: &CallContext,
        req: WorkspaceRef,
    ) -> ApiResult<WorkspaceView> {
        self.take_failure("GetWorkspace")?;
        self.ws_state()
            .by_name
            .get(&req.name)
            .map(|ws| view(&req.name, ws))
            .ok_or_else(|| not_found(format!("workspace {}", req.name)))
    }

    async fn list_workspaces(
        &self,
        _cx: &CallContext,
        _repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        self.take_failure("ListWorkspaces")?;
        let after = page.token.unwrap_or_default();
        let limit = page.limit.clamp(1, 1000) as usize;
        Ok(self
            .ws_state()
            .by_name
            .iter()
            .filter(|(name, _)| after.is_empty() || name.as_str() > after.as_str())
            .take(limit)
            .map(|(name, ws)| view(name, ws))
            .collect())
    }

    async fn renew_lease(
        &self,
        _cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        self.take_failure("RenewLease")?;
        self.live(&req.name)?;
        let lease = if lease_ms == 0 {
            DEFAULT_LEASE_MS
        } else {
            lease_ms
        };
        let mut all = self.ws_state();
        let ws = all
            .by_name
            .get_mut(&req.name)
            .ok_or_else(|| not_found(format!("workspace {}", req.name)))?;
        ws.lease_until_ms = now_ms() + lease as i64;
        Ok(view(&req.name, ws))
    }

    async fn set_head(&self, _cx: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        self.take_failure("SetHead")?;
        let name = req.workspace.name;
        let ws = self.live(&name)?;
        if ws.head != req.expected {
            return Err(ApiError::RefConflict {
                name,
                actual: ws.head,
            });
        }
        if req.new == ws.head {
            return Ok(view(&name, &ws));
        }
        let old = self.stack_of(ws.base, ws.head)?;
        let new = self.stack_of(ws.base, req.new)?;
        let old_ids: HashSet<ChangesetId> = old.iter().copied().collect();
        let new_ids: HashSet<ChangesetId> = new.iter().copied().collect();
        for id in &new {
            if old_ids.contains(id) {
                continue;
            }
            match self.draft_state(*id).0 {
                DraftState::Abandoned => {}
                DraftState::Draft => return Err(invalid(None, "draft-held")),
                DraftState::Landed => return Err(invalid(None, "draft-landed")),
                DraftState::Superseded => return Err(invalid(None, "draft-superseded")),
            }
        }
        let successor = req.new.filter(|n| !old_ids.contains(n));
        for id in &new {
            if !old_ids.contains(id) {
                self.set_record(*id, DraftState::Draft, None);
            }
        }
        for id in old {
            if new_ids.contains(&id) {
                continue;
            }
            match successor {
                Some(s) => self.set_record(id, DraftState::Superseded, Some(s)),
                None => self.set_record(id, DraftState::Abandoned, None),
            }
        }
        let mut all = self.ws_state();
        let current = all.by_name.get_mut(&name).expect("live workspace");
        current.head = req.new;
        Ok(view(&name, current))
    }

    async fn sync(
        &self,
        _cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        self.take_failure("Sync")?;
        let name = req.name;
        let ws = self.live(&name)?;
        let onto = match onto {
            Some(id) => id,
            None => self
                .tip(&ws.line)
                .ok_or_else(|| not_found(format!("ref {}", ws.line)))?,
        };
        let old_top = ws.head.unwrap_or(ws.base);
        let mut stack = self.stack_of(ws.base, ws.head)?;
        stack.reverse();
        let theirs: HashSet<RepoPath> = self
            .net_changes(ws.base, onto)
            .await
            .into_iter()
            .map(|c| c.path().clone())
            .collect();
        let mut conflicts = Vec::new();
        for id in &stack {
            let c = self.changeset_any(*id).expect("draft");
            for ch in &c.changes {
                if theirs.contains(ch.path()) {
                    conflicts.push(ConflictPath {
                        path: ch.path().to_string(),
                        base: None,
                        ours: None,
                        theirs: None,
                    });
                }
            }
        }
        if !conflicts.is_empty() {
            return Err(ApiError::Conflict {
                paths: conflicts,
                onto,
            });
        }
        let mut parent = onto;
        let mut rebased = Vec::new();
        for id in &stack {
            let c = self.changeset_any(*id).expect("draft");
            let next = Changeset {
                parents: vec![parent],
                root: empty_root(),
                ..c
            };
            let (new_id, _) = self.store(next).await?;
            rebased.push((*id, new_id));
            parent = new_id;
        }
        for (old, new) in &rebased {
            self.set_record(*old, DraftState::Superseded, Some(*new));
            self.set_record(*new, DraftState::Draft, None);
        }
        let head = (!rebased.is_empty()).then_some(parent);
        let changed_paths = self
            .net_changes(old_top, head.unwrap_or(onto))
            .await
            .into_iter()
            .map(|c| c.path().clone())
            .collect();
        let mut all = self.ws_state();
        let current = all.by_name.get_mut(&name).expect("live workspace");
        current.base = onto;
        current.head = head;
        Ok(Synced {
            workspace: view(&name, current),
            changed_paths,
        })
    }

    async fn stack(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        Err(ApiError::Rejected {
            policy: "fake".to_owned(),
            detail: "stack operations are not implemented by SnapshotRepo".to_owned(),
        })
    }

    async fn close_workspace(
        &self,
        _cx: &CallContext,
        req: WorkspaceRef,
        _keep_drafts: bool,
    ) -> ApiResult<()> {
        self.take_failure("CloseWorkspace")?;
        self.end(&req.name);
        Ok(())
    }
}
