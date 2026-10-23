//! `ResolveRef`, `ListRefs` and admin `UpdateRef` (`docs/design.md` §4.6,
//! §7.9).
//!
//! A ref moves only by compare-and-swap, with its `RefMoved` journal entry
//! in the same transaction. A line moves only by first-parent fast-forward;
//! the move indexes every changeset it adds to the line (positions and path
//! rows) and marks it landed, in that one transaction.

use graft_index::kinds::Skips;
use graft_model::{Changeset, ChangesetId};
use graft_proto::types::{ListRefs, RefKind, RefView, ResolveRef, UpdateRef};
use graft_proto::{ApiError, CallContext};
use graft_store::{
    ChangesetRecord, MAX_PAGE, MetaStore, MetaTxn, ObjectStore, Precondition, RecordState,
    RefKind as StoredKind, RefRecord, RepoId, TxnOutcome, Write,
};

use crate::access::{Need, require};
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
        let repo = self.repo_id_for_ref(cx, &req).await?;
        let current = self
            .meta
            .get_ref(repo, &req.name)
            .await
            .map_err(errors::store)?;
        let kind = current.as_ref().map_or(to_stored(req.kind), |r| r.kind);
        let need = match kind {
            StoredKind::Line => Need::Admin,
            StoredKind::Tag | StoredKind::Tool => Need::Write(None),
        };
        require(cx, &req.repo, need)?;
        if current.is_some() && to_stored(req.kind) != kind {
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

        let version = current.as_ref().map_or(1, |r| r.version + 1);
        // Records the move lands are guarded by their state, so a concurrent
        // workspace transition cannot be overwritten; losing that race
        // (and only that) is retried.
        for _ in 0..CAS_RETRIES {
            let mut preconditions = vec![Precondition::RefIs {
                name: req.name.clone(),
                expected: current.as_ref().map(|r| (r.target, r.version)),
            }];
            let mut writes = Vec::new();
            if kind == StoredKind::Line && actual != Some(req.new) {
                writes = self
                    .line_rows(repo, &req.name, actual, req.new, &mut preconditions)
                    .await?;
            }
            writes.push(Write::SetRef(RefRecord {
                name: req.name.clone(),
                target: req.new,
                version,
                kind,
            }));
            writes.push(journal::ref_moved(
                &req.name,
                actual,
                req.new,
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
                        name: req.name,
                        target: req.new,
                        version,
                        kind: to_api(kind),
                    });
                }
                TxnOutcome::PreconditionFailed(0) => {
                    let now = self
                        .meta
                        .get_ref(repo, &req.name)
                        .await
                        .map_err(errors::store)?;
                    return Err(ApiError::RefConflict {
                        name: req.name,
                        actual: now.map(|r| r.target),
                    });
                }
                TxnOutcome::PreconditionFailed(_) => {}
            }
        }
        Err(contended())
    }

    /// The repository id for an `UpdateRef`. Without any scope on the
    /// repository the caller learns nothing, not even whether it exists.
    async fn repo_id_for_ref(&self, cx: &CallContext, req: &UpdateRef) -> Result<RepoId, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        self.repo_id(&req.repo).await
    }

    /// The rows that move line `line` from `old` (absent for a new line) to
    /// `new`: line positions and path rows for every changeset the move
    /// adds, and their records marked landed. Refuses a move that is not a
    /// first-parent fast-forward, after O(log distance) skip-list reads.
    pub(crate) async fn line_rows(
        &self,
        repo: RepoId,
        line: &str,
        old: Option<ChangesetId>,
        new: ChangesetId,
        guards: &mut Vec<Precondition>,
    ) -> Result<Vec<Write>, ApiError> {
        let mut writes = Vec::new();
        let new_skips = self.skips(repo, new, &mut writes).await?;
        // How many changesets the move adds: back to `old`, or to the root.
        let added = match old {
            None => new_skips.depth + 1,
            Some(old) => {
                let old_skips = self.skips(repo, old, &mut writes).await?;
                let Some(distance) = new_skips.depth.checked_sub(old_skips.depth) else {
                    return Err(rejected(format!("{new} does not descend from {old}")));
                };
                if self.ancestor_at(repo, new, distance, &mut writes).await? != Some(old) {
                    return Err(rejected(format!(
                        "{old} is not on the first-parent chain of {new}"
                    )));
                }
                distance
            }
        };

        // The added stretch, oldest first.
        let mut chain: Vec<(Changeset, ChangesetRecord)> = Vec::new();
        let mut cur = Some(new);
        for _ in 0..added {
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
            }
        }
        Ok(writes)
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
