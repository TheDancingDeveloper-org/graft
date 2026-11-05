//! `ResolveRef`, `ListRefs`, `CreateLine` and admin `UpdateRef` (`docs/design.md` §4.6,
//! §7.9).
//!
//! A ref moves only by compare-and-swap, with its `RefMoved` journal entry
//! in the same transaction. A line moves only by first-parent fast-forward;
//! the move indexes every changeset it adds to the line (positions and path
//! rows) and marks it landed, in that one transaction.

use std::collections::HashSet;

use graft_index::kinds::Skips;
use graft_model::{Changeset, ChangesetId};
use graft_proto::types::{
    CreateLine, FOLLOWED_REF_PREFIX, ListRefs, RefKind, RefView, ResolveRef, UpdateRef,
    followed_ref,
};
use graft_proto::{ApiError, CallContext};
use graft_store::{
    ChangesetRecord, MAX_PAGE, MetaStore, MetaTxn, ObjectStore, Precondition, RecordState,
    RefKind as StoredKind, RefRecord, RepoId, TxnOutcome, Write,
};

use crate::access::{Need, is_mirror, require};
use crate::protect::distinct_parents;
use crate::workspace::{CAS_RETRIES, contended};
use crate::{Engine, errors, journal};

/// The longest ref name, in bytes.
pub const MAX_REF_NAME: usize = 255;

fn to_api(kind: StoredKind) -> RefKind {
    match kind {
        StoredKind::Line => RefKind::Line,
        StoredKind::Tag => RefKind::Tag,
        StoredKind::Tool => RefKind::Tool,
    }
}

fn to_stored(kind: RefKind) -> StoredKind {
    match kind {
        RefKind::Line => StoredKind::Line,
        RefKind::Tag => StoredKind::Tag,
        RefKind::Tool => StoredKind::Tool,
    }
}

fn view(r: RefRecord) -> RefView {
    RefView {
        name: r.name,
        target: r.target,
        version: r.version,
        kind: to_api(r.kind),
    }
}

/// A ref name: 1 to 255 bytes, no whitespace or control characters.
pub(crate) fn check_ref_name(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_REF_NAME
        && !name.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok {
        Ok(())
    } else {
        Err(ApiError::InvalidChange {
            path: name.to_owned(),
            rule: "ref-name".to_owned(),
        })
    }
}

fn rejected(detail: String) -> ApiError {
    ApiError::Rejected {
        policy: "line-fast-forward".to_owned(),
        detail,
    }
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    pub(crate) async fn do_resolve_ref(
        &self,
        cx: &CallContext,
        req: ResolveRef,
    ) -> Result<RefView, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        self.meta
            .get_ref(repo, &req.name)
            .await
            .map_err(errors::store)?
            .map(view)
            .ok_or_else(|| errors::not_found(format!("ref {}", req.name)))
    }

    /// Refs by prefix, in name order. The page token is the last name of
    /// the previous page. The port lists from the start of the prefix, so a
    /// resumed listing reaches at most `MAX_PAGE` names past it; refs are a
    /// handful per repository.
    pub(crate) async fn do_list_refs(
        &self,
        cx: &CallContext,
        req: ListRefs,
    ) -> Result<Vec<RefView>, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        let limit = self.page_size(req.page.limit) as usize;
        let prefix = req.prefix.unwrap_or_default();
        let fetch = if req.page.token.is_some() {
            MAX_PAGE
        } else {
            limit as u32
        };
        let refs = self
            .meta
            .list_refs(repo, &prefix, fetch)
            .await
            .map_err(errors::store)?;
        Ok(refs
            .into_iter()
            .filter(|r| req.page.token.as_ref().is_none_or(|t| r.name > *t))
            .take(limit)
            .map(view)
            .collect())
    }

    pub(crate) async fn do_update_ref(
        &self,
        cx: &CallContext,
        req: UpdateRef,
    ) -> Result<RefView, ApiError> {
        check_ref_name(&req.name)?;
        // The mirror service moves a followed line, by CAS, and nothing
        // else; its set has no read (D41, WI-3140).
        let mirror = is_mirror(cx, &req.repo);
        let repo = if mirror {
            self.repo_id(&req.repo).await?
        } else {
            self.repo_id_for_ref(cx, &req).await?
        };
        let current = self
            .meta
            .get_ref(repo, &req.name)
            .await
            .map_err(errors::store)?;
        let kind = current.as_ref().map_or(to_stored(req.kind), |r| r.kind);
        // A followed-line marker is admin-only: it decides who may land.
        let marker = req.name.starts_with(FOLLOWED_REF_PREFIX);
        let followed =
            kind == StoredKind::Line && mirror && self.is_followed(repo, &req.name).await?;
        let need = match kind {
            StoredKind::Line if followed => None,
            StoredKind::Line => Some(Need::Admin),
            StoredKind::Tool if marker => Some(Need::Admin),
            StoredKind::Tag | StoredKind::Tool => Some(Need::Write(None)),
        };
        if let Some(need) = need {
            require(cx, &req.repo, need)?;
        }
        if (current.is_some() && to_stored(req.kind) != kind)
            || (marker && kind != StoredKind::Tool)
        {
            return Err(errors::invalid(None, "ref-kind"));
        }

        let actual = current.as_ref().map(|r| r.target);
        if actual != req.expected {
            // A retry after success finds the ref already at `new`.
            return match current {
                Some(r) if r.target == req.new => Ok(view(r)),
                _ => Err(ApiError::RefConflict {
                    name: req.name,
                    actual,
                }),
            };
        }
        if kind == StoredKind::Tag && current.is_some() {
            return Err(errors::invalid(None, "tag-immutable"));
        }
        // The target exists here (and is not collected).
        self.changeset(repo, req.new).await?;
        // The mirror moves a followed line only under its authority lease,
        // checked in the move's own transaction (D41 §3.6).
        let lease = if followed {
            let Some(epoch) = req.lease_epoch else {
                return Err(ApiError::LeaseLost { repo: req.repo });
            };
            Some((epoch, req.repo))
        } else {
            None
        };
        self.move_ref(cx, repo, req.name, current, kind, req.new, lease)
            .await
    }

    /// Forks line `req.name` at `req.from`: a CAS from absent that never
    /// overwrites a ref (`docs/design.md` §4.6). `land:<name>` or `admin`
    /// may create the line `<name>`. Its land policy is the server's
    /// config for that name, `direct` by default (D22); nothing is stored
    /// for it here.
    pub(crate) async fn do_create_line(
        &self,
        cx: &CallContext,
        req: CreateLine,
    ) -> Result<RefView, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        require(cx, &req.repo, Need::Land(&req.name))?;
        check_ref_name(&req.name)?;
        if req.name.starts_with(FOLLOWED_REF_PREFIX) {
            return Err(errors::invalid(None, "ref-name"));
        }
        if let Some(r) = self
            .meta
            .get_ref(repo, &req.name)
            .await
            .map_err(errors::store)?
        {
            return Err(ApiError::RefConflict {
                name: req.name,
                actual: Some(r.target),
            });
        }
        self.changeset(repo, req.from).await?;
        self.move_ref(cx, repo, req.name, None, StoredKind::Line, req.from, None)
            .await
    }

    /// Moves ref `name` from `current` to `new` by CAS, with its journal
    /// entry (and, for a line, its index rows) in the same transaction.
    /// `lease` is the mirror's `(epoch, repository name)`: the move commits
    /// only while that lease is held.
    #[allow(clippy::too_many_arguments)]
    async fn move_ref(
        &self,
        cx: &CallContext,
        repo: RepoId,
        name: String,
        current: Option<RefRecord>,
        kind: StoredKind,
        new: ChangesetId,
        lease: Option<(u64, String)>,
    ) -> Result<RefView, ApiError> {
        let actual = current.as_ref().map(|r| r.target);
        let version = current.as_ref().map_or(1, |r| r.version + 1);
        // Records the move lands are guarded by their state, so a concurrent
        // workspace transition cannot be overwritten; losing that race
        // (and only that) is retried.
        for _ in 0..CAS_RETRIES {
            let mut preconditions = vec![Precondition::RefIs {
                name: name.clone(),
                expected: current.as_ref().map(|r| (r.target, r.version)),
            }];
            if let Some((epoch, _)) = &lease {
                preconditions.push(Precondition::LeaseHeld {
                    epoch: *epoch,
                    now_ms: self.clock.now().ms,
                });
            }
            let mut writes = Vec::new();
            if kind == StoredKind::Line && actual != Some(new) {
                writes = self
                    .line_rows(repo, &name, actual, new, &mut preconditions)
                    .await?;
            }
            writes.push(Write::SetRef(RefRecord {
                name: name.clone(),
                target: new,
                version,
                kind,
            }));
            writes.push(journal::ref_moved(
                &name,
                actual,
                new,
                version,
                &cx.principal.subject,
            ));
            let txn = MetaTxn {
                preconditions,
                writes,
            };
            match self.meta.commit(repo, txn).await.map_err(errors::store)? {
                TxnOutcome::Committed(_) => {
                    return Ok(RefView {
                        name,
                        target: new,
                        version,
                        kind: to_api(kind),
                    });
                }
                TxnOutcome::PreconditionFailed(0) => {
                    let now = self
                        .meta
                        .get_ref(repo, &name)
                        .await
                        .map_err(errors::store)?;
                    return Err(ApiError::RefConflict {
                        name,
                        actual: now.map(|r| r.target),
                    });
                }
                TxnOutcome::PreconditionFailed(1) if lease.is_some() => {
                    let (_, repo) = lease.expect("checked");
                    return Err(ApiError::LeaseLost { repo });
                }
                TxnOutcome::PreconditionFailed(_) => {}
            }
        }
        Err(contended())
    }

    /// The repository id for an `UpdateRef`. Without any scope on the
    /// repository the caller learns nothing, not even whether it exists.
    /// Whether `line` carries the followed-line marker.
    async fn is_followed(&self, repo: RepoId, line: &str) -> Result<bool, ApiError> {
        let marker = self
            .meta
            .get_ref(repo, &followed_ref(line))
            .await
            .map_err(errors::store)?;
        Ok(marker.is_some())
    }

    async fn repo_id_for_ref(&self, cx: &CallContext, req: &UpdateRef) -> Result<RepoId, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        self.repo_id(&req.repo).await
    }

    /// The rows that move line `line` from `old` (absent for a new line) to
    /// `new`: line positions and path rows for every changeset the move
    /// adds, and their records marked landed. Refuses a move that is not a
    /// first-parent fast-forward, after O(log distance) skip-list reads.
    ///
    /// A new line forks from the line holding the nearest landed ancestor
    /// of `new` and indexes only the unlanded stretch above it
    /// (`docs/notes/line-fork-point.md`).
    pub(crate) async fn line_rows(
        &self,
        repo: RepoId,
        line: &str,
        old: Option<ChangesetId>,
        new: ChangesetId,
        guards: &mut Vec<Precondition>,
    ) -> Result<Vec<Write>, ApiError> {
        let mut writes = Vec::new();
        let chain = match old {
            None => self.fork_rows(repo, line, new, &mut writes).await?,
            Some(old) => {
                let new_skips = self.skips(repo, new, &mut writes).await?;
                let old_skips = self.skips(repo, old, &mut writes).await?;
                let Some(distance) = new_skips.depth.checked_sub(old_skips.depth) else {
                    return Err(rejected(format!("{new} does not descend from {old}")));
                };
                if self.ancestor_at(repo, new, distance, &mut writes).await? != Some(old) {
                    return Err(rejected(format!(
                        "{old} is not on the first-parent chain of {new}"
                    )));
                }
                // The added stretch, oldest first.
                let mut chain: Vec<(Changeset, ChangesetRecord)> = Vec::new();
                let mut cur = Some(new);
                for _ in 0..distance {
                    let id = cur.expect("the depth bounds the walk");
                    let (cs, record) = self.changeset(repo, id).await?;
                    cur = cs.first_parent();
                    chain.push((cs, record));
                }
                chain.reverse();
                let landed: Vec<Changeset> = chain.iter().map(|(cs, _)| cs.clone()).collect();
                writes.extend(
                    self.index
                        .on_land(repo, line, &landed)
                        .await
                        .map_err(errors::store)?,
                );
                chain
            }
        };

        // Landing zeroes a record's own pins (the store does), and releases
        // the pins it holds on parents off the line: every pin comes from a
        // non-landed child (design §8.6). First parents are on the line, so
        // a linear chain releases nothing.
        let ids: HashSet<ChangesetId> = chain.iter().map(|(cs, _)| cs.id()).collect();
        let mut released = Vec::new();
        for (cs, mut record) in chain {
            if record.state != RecordState::Landed {
                guards.push(Precondition::RecordStateIs {
                    id: cs.id(),
                    state: record.state,
                });
                record.state = RecordState::Landed;
                record.holder = None;
                record.superseded_by = None;
                writes.push(Write::SetRecord {
                    id: cs.id(),
                    record,
                });
                let first = cs.first_parent();
                released.extend(
                    distinct_parents(&cs)
                        .into_iter()
                        .filter(|p| Some(*p) != first && !ids.contains(p)),
                );
            }
        }
        writes.extend(released.into_iter().map(Write::Unpin));
        Ok(writes)
    }

    /// The stretch a new line `line` at `new` adds, oldest first, with its
    /// index rows pushed to `writes`. The walk stops at the first landed
    /// first-parent ancestor: the line forks from a line holding it (one
    /// `line_pos` read per line) and records that fork, so the cost is the
    /// unlanded stretch, not the history.
    async fn fork_rows(
        &self,
        repo: RepoId,
        line: &str,
        new: ChangesetId,
        writes: &mut Vec<Write>,
    ) -> Result<Vec<(Changeset, ChangesetRecord)>, ApiError> {
        let mut chain: Vec<(Changeset, ChangesetRecord)> = Vec::new();
        let mut cur = Some(new);
        let mut fork_at = None;
        while let Some(id) = cur {
            let (cs, record) = self.changeset(repo, id).await?;
            if record.state == RecordState::Landed {
                fork_at = Some(id);
                break;
            }
            cur = cs.first_parent();
            chain.push((cs, record));
        }
        chain.reverse();
        let start = match fork_at {
            None => 0,
            Some(at) => {
                let (parent, pos) = self.line_holding(repo, at).await?;
                writes.push(Write::PutLineFork {
                    line: line.to_owned(),
                    parent,
                    pos,
                });
                pos + 1
            }
        };
        for (pos, (cs, _)) in (start..).zip(&chain) {
            let id = cs.id();
            writes.push(Write::PutLinePos {
                line: line.to_owned(),
                changeset: id,
                pos,
            });
            writes.extend(graft_index::line_path_rows(line, pos, id, cs));
        }
        Ok(chain)
    }

    /// A line holding the landed changeset `id` in its own rows, and its
    /// position there: one indexed read. Forking from a direct holder keeps
    /// fork chains from growing with repeated creates. Every landed
    /// changeset has a direct holder (it got its own row on the line it
    /// landed on, and lines are never deleted).
    async fn line_holding(&self, repo: RepoId, id: ChangesetId) -> Result<(String, u64), ApiError> {
        self.meta
            .line_holders(repo, id, 1)
            .await
            .map_err(errors::store)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                errors::store(graft_store::StoreError::Corrupt(format!(
                    "landed changeset {id} has no line position"
                )))
            })
    }

    /// A changeset's skip list, adding rows for any values the walk had to
    /// compute (after a kind version bump).
    pub(crate) async fn skips(
        &self,
        repo: RepoId,
        id: ChangesetId,
        writes: &mut Vec<Write>,
    ) -> Result<Skips, ApiError> {
        let derived = self
            .index
            .skip_list(repo, id)
            .await
            .map_err(errors::store)?;
        writes.extend(derived.rows);
        Ok(derived.value)
    }

    /// The first-parent ancestor of `id` at `distance`, by skip-list jumps.
    pub(crate) async fn ancestor_at(
        &self,
        repo: RepoId,
        id: ChangesetId,
        mut distance: u64,
        writes: &mut Vec<Write>,
    ) -> Result<Option<ChangesetId>, ApiError> {
        let mut cur = id;
        while distance > 0 {
            let k = distance.ilog2();
            let skips = self.skips(repo, cur, writes).await?;
            let Some(&next) = skips.ancestors.get(k as usize) else {
                return Ok(None);
            };
            cur = next;
            distance -= 1 << k;
        }
        Ok(Some(cur))
    }
}
