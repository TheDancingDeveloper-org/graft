//! Generalised `Attest`, `GetAttestations` and `GetLandChecks` (v0.2,
//! decisions D25 and D37).
//!
//! An attestation has a kind (`check`, `review` or `approval`), a status
//! (`pass`, `fail`, `pending` or `error`) and the attester's display name.
//! Every one is stored as an attestation row with its `Attested` journal
//! entry, in one transaction. A `check` that passes or fails also writes
//! the derived row the land policies read, as a pre-v0.2 `Attest` did;
//! `review` and `approval` gate nothing yet.
//!
//! On a line whose [`LineChecks`] is `PostLand`, each landing has a
//! land-checks row, `pending` from the land's transaction. A `check`
//! attestation on a landed changeset recomputes that row in the
//! attestation's transaction (see [`land_state`]); a revert land marks the
//! landing it reverts `reverted` and journals `LandReverted`. The rows
//! record; they never refuse, delay or queue a land.
//!
//! The reads use the store's indexes by changeset and by `(line, pos)`;
//! nothing scans the journal.
use graft_auth::oidc::pinned_changeset;
use graft_model::{Changeset, ChangesetId};
use graft_proto::types::{
    self as pt, AttestV2, AttestationPage, AttestationSelector, GetAttestations, GetLandChecks,
    LandCheckPage, MAX_REVIEW_PAGE,
};
use graft_proto::{ApiError, CallContext, PrincipalKind};
use graft_store::{
    AttestKind, AttestStatus, AttestationRow, LandCheckRow, LandCheckState, MetaStore, MetaTxn,
    ObjectStore, RecordState, RepoId, Write,
};

use crate::access::{Need, require};
use crate::journal::{self, AttestedEntry};
use crate::land::{
    ATTESTATION, KIND_VERSION, MAX_ATTEST_URL, check_check_name, encode_attestation, key,
};
use crate::policy::{LineChecks, LineConfig, RevertRequest};
use crate::rebase::Step;
use crate::refs::check_ref_name;
use crate::revert::REVERTS;
use crate::{Engine, errors};

/// Most attestation rows one land-state recompute reads: the page bound
/// of the review reads.
pub(crate) const MAX_REPORTS: u32 = MAX_REVIEW_PAGE;

/// The longest attester display name, in bytes.
pub const MAX_ATTEST_BY: usize = 256;

/// The state of a landing whose required checks reported `reports`
/// (`(check, status)`, oldest first): `bad` once a required check failed
/// more than `retries` times, `good` when every required check's latest
/// report passed, else `pending`. Reports of other checks are ignored.
pub(crate) fn land_state(
    required: &[String],
    retries: u32,
    reports: &[(&str, AttestStatus)],
) -> LandCheckState {
    let mut all_passed = true;
    for check in required {
        let mut failures: u64 = 0;
        let mut latest = None;
        for (_, status) in reports.iter().filter(|(n, _)| *n == check.as_str()) {
            if *status == AttestStatus::Fail {
                failures += 1;
            }
            latest = Some(*status);
        }
        if failures > u64::from(retries) {
            return LandCheckState::Bad;
        }
        all_passed &= latest == Some(AttestStatus::Pass);
    }
    if all_passed {
        LandCheckState::Good
    } else {
        LandCheckState::Pending
    }
}

/// The first required check whose latest report failed.
fn failed_check<'a>(required: &'a [String], reports: &[(&str, AttestStatus)]) -> Option<&'a str> {
    required.iter().map(String::as_str).find(|check| {
        reports
            .iter()
            .rev()
            .find(|(n, _)| n == check)
            .is_some_and(|(_, s)| *s == AttestStatus::Fail)
    })
}

fn to_store_kind(kind: pt::AttestKind) -> Result<AttestKind, ApiError> {
    match kind {
        pt::AttestKind::Check => Ok(AttestKind::Check),
        pt::AttestKind::Review => Ok(AttestKind::Review),
        pt::AttestKind::Approval => Ok(AttestKind::Approval),
        _ => Err(errors::invalid(None, "attest-kind")),
    }
}

fn to_store_status(status: pt::AttestStatus) -> Result<AttestStatus, ApiError> {
    match status {
        pt::AttestStatus::Pass => Ok(AttestStatus::Pass),
        pt::AttestStatus::Fail => Ok(AttestStatus::Fail),
        pt::AttestStatus::Pending => Ok(AttestStatus::Pending),
        pt::AttestStatus::Error => Ok(AttestStatus::Error),
        _ => Err(errors::invalid(None, "attest-status")),
    }
}

fn proto_kind(kind: AttestKind) -> pt::AttestKind {
    match kind {
        AttestKind::Check => pt::AttestKind::Check,
        AttestKind::Review => pt::AttestKind::Review,
        AttestKind::Approval => pt::AttestKind::Approval,
    }
}

fn proto_status(status: AttestStatus) -> pt::AttestStatus {
    match status {
        AttestStatus::Pass => pt::AttestStatus::Pass,
        AttestStatus::Fail => pt::AttestStatus::Fail,
        AttestStatus::Pending => pt::AttestStatus::Pending,
        AttestStatus::Error => pt::AttestStatus::Error,
    }
}

fn proto_state(state: LandCheckState) -> pt::LandCheckState {
    match state {
        LandCheckState::Pending => pt::LandCheckState::Pending,
        LandCheckState::Good => pt::LandCheckState::Good,
        LandCheckState::Bad => pt::LandCheckState::Bad,
        LandCheckState::Reverted => pt::LandCheckState::Reverted,
    }
}

fn kind_name(kind: AttestKind) -> &'static str {
    match kind {
        AttestKind::Check => "check",
        AttestKind::Review => "review",
        AttestKind::Approval => "approval",
    }
}

fn status_name(status: AttestStatus) -> &'static str {
    match status {
        AttestStatus::Pass => "pass",
        AttestStatus::Fail => "fail",
        AttestStatus::Pending => "pending",
        AttestStatus::Error => "error",
    }
}

fn proto_row(row: AttestationRow) -> pt::Attestation {
    pt::Attestation {
        changeset: row.changeset,
        kind: proto_kind(row.kind),
        check: row.name,
        status: proto_status(row.status),
        by: row.by,
        url: row.url,
    }
}

/// Parses a resume token of `N` dot-separated integers.
fn parse_token<const N: usize>(token: &str) -> Result<[u64; N], ApiError> {
    let bad = || errors::invalid(None, "page-token");
    let mut out = [0u64; N];
    let mut parts = token.split('.');
    for slot in &mut out {
        *slot = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    }
    if parts.next().is_some() {
        return Err(bad());
    }
    Ok(out)
}

/// The required checks and retries of a post-land checked line.
fn post_land(line: &LineConfig) -> Option<(&[String], u32)> {
    match &line.checks {
        LineChecks::PostLand { required, retries } => Some((required, *retries)),
        LineChecks::Off => None,
    }
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// The page size of a v0.2 review read.
    fn review_page(&self, limit: u32) -> u32 {
        self.page_size(limit).min(MAX_REVIEW_PAGE)
    }

    /// The land-checks row of the landing of `changeset` at `pos` on
    /// `line`, if it has one.
    async fn land_check_row(
        &self,
        repo: RepoId,
        line: &str,
        pos: u64,
        changeset: ChangesetId,
    ) -> Result<Option<LandCheckRow>, ApiError> {
        let rows = self
            .meta
            .land_checks(repo, line, pos, 1)
            .await
            .map_err(errors::store)?;
        Ok(rows
            .into_iter()
            .next()
            .filter(|r| r.pos == pos && r.changeset == changeset))
    }

    /// The `check` attestations of `changeset`, oldest first: at most
    /// [`MAX_REPORTS`] rows of one index range by changeset, so each attest
    /// reads a bounded page however often the changeset was attested. Past
    /// the cap the state counts the first page and the attestation being
    /// written.
    async fn check_reports(
        &self,
        repo: RepoId,
        changeset: ChangesetId,
    ) -> Result<Vec<(String, AttestStatus)>, ApiError> {
        let rows = self
            .meta
            .attestations_by_changeset(repo, changeset, None, MAX_REPORTS)
            .await
            .map_err(errors::store)?;
        Ok(rows
            .into_iter()
            .filter(|r| r.kind == AttestKind::Check)
            .map(|r| (r.name, r.status))
            .collect())
    }

    /// Records an attestation (v0.2). It needs `attest` or `admin`; agents
    /// never attest, and a CI token attests only its pinned changeset. A
    /// `check` that passes or fails also writes the derived row the land
    /// policies read, and a failing one asks for a revert on an
    /// `auto_revert` line (D6). On a post-land checked line the landing's
    /// land-checks row is recomputed in the same transaction.
    pub(crate) async fn do_attest_v2(
        &self,
        cx: &CallContext,
        req: AttestV2,
    ) -> Result<(), ApiError> {
        require(cx, &req.repo, Need::Attest)?;
        let pinned_here = || {
            pinned_changeset(&cx.principal.subject)
                .is_some_and(|pin| pin == req.changeset.to_string())
        };
        // Agents never attest. A CI token attests only the changeset its
        // exchange pinned it to (`ci:<provider>:<repo>#<changeset>`); an
        // unpinned one attests nothing.
        let refused = match cx.principal.kind {
            PrincipalKind::Agent => true,
            PrincipalKind::Ci => !pinned_here(),
            PrincipalKind::Human | PrincipalKind::Service => false,
        };
        if refused {
            return Err(ApiError::PermissionDenied {
                scope: format!("repo:{}:attest", req.repo),
                path: None,
            });
        }
        let kind = to_store_kind(req.kind)?;
        let status = to_store_status(req.status)?;
        check_check_name(&req.check)?;
        if req.url.len() > MAX_ATTEST_URL {
            return Err(errors::limit("attest-url", MAX_ATTEST_URL as u64));
        }
        if let Some(by) = &req.by {
            if by.len() > MAX_ATTEST_BY {
                return Err(errors::limit("attest-by", MAX_ATTEST_BY as u64));
            }
            if by.is_empty() || by.chars().any(char::is_control) {
                return Err(errors::invalid(None, "attest-by"));
            }
        }
        let repo = self.repo_id(&req.repo).await?;
        let (_, record) = self.changeset(repo, req.changeset).await?;
        let landed = record.state == RecordState::Landed;
        let is_check = kind == AttestKind::Check;
        let verdict = match status {
            AttestStatus::Pass => Some(true),
            AttestStatus::Fail => Some(false),
            AttestStatus::Pending | AttestStatus::Error => None,
        };

        let mut writes = Vec::new();
        if let (true, Some(passed)) = (is_check, verdict) {
            writes.push(Write::PutDerived {
                kind: ATTESTATION.to_owned(),
                version: KIND_VERSION,
                key: key(req.changeset, &req.check),
                value: encode_attestation(
                    passed,
                    cx.principal.kind,
                    self.clock.now().ms,
                    &req.url,
                    &cx.principal.subject,
                ),
            });
        }
        writes.push(journal::attested(&AttestedEntry {
            changeset: req.changeset,
            kind: kind_name(kind),
            check: &req.check,
            status: status_name(status),
            url: &req.url,
            actor: &cx.principal.subject,
            by: req.by.as_deref(),
        }));

        // The post-land checked lines the changeset landed on, with their
        // land-checks updates.
        let mut line_pos = None;
        let mut checks = Vec::new();
        if landed {
            for line in &self.config.lines {
                let Some((required, retries)) = post_land(line) else {
                    continue;
                };
                let Some(pos) = self
                    .meta
                    .line_pos(repo, &line.name, req.changeset)
                    .await
                    .map_err(errors::store)?
                else {
                    continue;
                };
                line_pos.get_or_insert_with(|| (line.name.clone(), pos));
                if !is_check || !required.contains(&req.check) {
                    continue;
                }
                let Some(row) = self
                    .land_check_row(repo, &line.name, pos, req.changeset)
                    .await?
                else {
                    continue;
                };
                if row.state == LandCheckState::Reverted {
                    continue;
                }
                let mut reports = self.check_reports(repo, req.changeset).await?;
                reports.push((req.check.clone(), status));
                let reports: Vec<(&str, AttestStatus)> =
                    reports.iter().map(|(n, s)| (n.as_str(), *s)).collect();
                let state = land_state(required, retries, &reports);
                if state != row.state {
                    checks.push(Write::SetLandCheck {
                        line: line.name.clone(),
                        pos,
                        state,
                        revert: None,
                    });
                }
            }
        }
        writes.push(Write::PutAttestation {
            changeset: req.changeset,
            line_pos,
            kind,
            name: req.check.clone(),
            status,
            by: req.by.clone(),
            url: req.url.clone(),
        });
        writes.extend(checks);

        let mut reverts = Vec::new();
        if is_check && verdict == Some(false) && landed {
            for line in &self.config.lines {
                let Some(post) = &line.post_land else {
                    continue;
                };
                if !post.auto_revert || !post.checks.contains(&req.check) {
                    continue;
                }
                let on_line = self
                    .meta
                    .line_pos(repo, &line.name, req.changeset)
                    .await
                    .map_err(errors::store)?;
                if on_line.is_some() {
                    writes.push(journal::revert_requested(
                        &line.name,
                        req.changeset,
                        &req.check,
                        &req.url,
                    ));
                    reverts.push(RevertRequest {
                        repo: req.repo.clone(),
                        line: line.name.clone(),
                        changeset: req.changeset,
                        check: req.check.clone(),
                        url: req.url.clone(),
                    });
                }
            }
        }
        let txn = MetaTxn {
            preconditions: Vec::new(),
            writes,
        };
        self.meta.commit(repo, txn).await.map_err(errors::store)?;
        for request in &reverts {
            self.hooks.revert_requested(request);
        }
        Ok(())
    }

    /// The land-checks writes of a land on a post-land checked line: for
    /// each changeset of the landed stack that reverts (`graft.reverts`) a
    /// landing of the line with a live row, that row marked `reverted` and
    /// a `LandReverted` entry. `steps` maps the stack to what landed.
    pub(crate) async fn revert_writes(
        &self,
        repo: RepoId,
        line: &LineConfig,
        stack: &[(ChangesetId, Changeset)],
        steps: &[Step],
    ) -> Result<Vec<Write>, ApiError> {
        let Some((required, _)) = post_land(line) else {
            return Ok(Vec::new());
        };
        let mut writes = Vec::new();
        for (id, cs) in stack {
            let Some(target) = cs
                .metadata
                .get(REVERTS)
                .and_then(|t| t.parse::<ChangesetId>().ok())
            else {
                continue;
            };
            let revert = match steps.iter().find(|s| s.from == *id) {
                Some(Step { to: Some(to), .. }) => *to,
                Some(Step { to: None, .. }) => continue,
                None => *id,
            };
            let Some(pos) = self
                .meta
                .line_pos(repo, &line.name, target)
                .await
                .map_err(errors::store)?
            else {
                continue;
            };
            let Some(row) = self.land_check_row(repo, &line.name, pos, target).await? else {
                continue;
            };
            if row.state == LandCheckState::Reverted {
                continue;
            }
            let reports = self.check_reports(repo, target).await?;
            let reports: Vec<(&str, AttestStatus)> =
                reports.iter().map(|(n, s)| (n.as_str(), *s)).collect();
            writes.push(Write::SetLandCheck {
                line: line.name.clone(),
                pos,
                state: LandCheckState::Reverted,
                revert: Some(revert),
            });
            writes.push(journal::land_reverted(
                &line.name,
                target,
                revert,
                failed_check(required, &reports),
            ));
        }
        Ok(writes)
    }

    /// A changeset's or a line range's attestations, paged. Needs `read`.
    pub(crate) async fn do_get_attestations(
        &self,
        cx: &CallContext,
        req: GetAttestations,
    ) -> Result<AttestationPage, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let kinds = req
            .kinds
            .iter()
            .map(|k| to_store_kind(*k))
            .collect::<Result<Vec<_>, _>>()?;
        let limit = self.review_page(req.page.limit);
        let repo = self.repo_id(&req.repo).await?;
        let (rows, next) = match &req.selector {
            AttestationSelector::ByChangeset(id) => {
                let after = match &req.page.token {
                    Some(t) => {
                        let [lsn, seq] = parse_token::<2>(t)?;
                        Some((
                            lsn,
                            u32::try_from(seq).map_err(|_| errors::invalid(None, "page-token"))?,
                        ))
                    }
                    None => None,
                };
                let rows = self
                    .meta
                    .attestations_by_changeset(repo, *id, after, limit)
                    .await
                    .map_err(errors::store)?;
                let next = (rows.len() == limit as usize)
                    .then(|| rows.last().map(|r| format!("{}.{}", r.lsn, r.seq)))
                    .flatten();
                (rows, next)
            }
            AttestationSelector::ByLineRange {
                line,
                from_pos,
                to_pos,
            } => {
                check_ref_name(line)?;
                if from_pos > to_pos {
                    return Err(errors::invalid(None, "line-range"));
                }
                let after = match &req.page.token {
                    Some(t) => {
                        let [pos, lsn, seq] = parse_token::<3>(t)?;
                        Some((
                            pos,
                            lsn,
                            u32::try_from(seq).map_err(|_| errors::invalid(None, "page-token"))?,
                        ))
                    }
                    None => None,
                };
                let rows = self
                    .meta
                    .attestations_by_line(repo, line, *from_pos, *to_pos, after, limit)
                    .await
                    .map_err(errors::store)?;
                let next = (rows.len() == limit as usize)
                    .then(|| {
                        rows.last().and_then(|r| {
                            let (_, pos) = r.line_pos.as_ref()?;
                            Some(format!("{pos}.{}.{}", r.lsn, r.seq))
                        })
                    })
                    .flatten();
                (rows, next)
            }
            _ => return Err(errors::invalid(None, "attestation-selector")),
        };
        let attestations = rows
            .into_iter()
            .filter(|r| kinds.is_empty() || kinds.contains(&r.kind))
            .map(proto_row)
            .collect();
        Ok(AttestationPage { attestations, next })
    }

    /// The check state of each landing on a line from a position, paged.
    /// Needs `read`.
    pub(crate) async fn do_get_land_checks(
        &self,
        cx: &CallContext,
        req: GetLandChecks,
    ) -> Result<LandCheckPage, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        check_ref_name(&req.line)?;
        let limit = self.review_page(req.page.limit);
        let from = match &req.page.token {
            Some(t) => parse_token::<1>(t)?[0].max(req.from_pos),
            None => req.from_pos,
        };
        let repo = self.repo_id(&req.repo).await?;
        let rows = self
            .meta
            .land_checks(repo, &req.line, from, limit)
            .await
            .map_err(errors::store)?;
        let next = (rows.len() == limit as usize)
            .then(|| rows.last().map(|r| r.pos.saturating_add(1).to_string()))
            .flatten();
        let rows = rows
            .into_iter()
            .map(|r| pt::LandCheck {
                pos: r.pos,
                changeset: r.changeset,
                state: proto_state(r.state),
                revert: r.revert,
            })
            .collect();
        Ok(LandCheckPage { rows, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn land_state_table() {
        use AttestStatus::{Error, Fail, Pass, Pending};
        use LandCheckState::{Bad, Good, Pending as P};
        let two = req(&["build", "test"]);
        type Row<'a> = (&'a [(&'a str, AttestStatus)], u32, LandCheckState);
        let table: &[Row] = &[
            (&[], 1, P),
            (&[("build", Pass)], 1, P),
            (&[("build", Pass), ("test", Pending)], 1, P),
            (&[("build", Pass), ("test", Pass)], 1, Good),
            (&[("build", Pass), ("test", Error)], 1, P),
            (&[("build", Fail)], 1, P),
            (&[("build", Fail), ("build", Fail)], 1, Bad),
            (&[("build", Fail)], 0, Bad),
            (&[("build", Fail), ("build", Pass), ("test", Pass)], 1, Good),
            (&[("build", Pass), ("test", Pass), ("other", Fail)], 0, Good),
            (&[("build", Pass), ("build", Pending), ("test", Pass)], 1, P),
        ];
        for (reports, retries, want) in table {
            assert_eq!(
                land_state(&two, *retries, reports),
                *want,
                "{reports:?} r={retries}"
            );
        }
        // No required checks: good at once.
        assert_eq!(land_state(&[], 0, &[]), Good);
    }

    #[test]
    fn failed_check_names_the_latest_failure() {
        use AttestStatus::{Fail, Pass};
        let two = req(&["build", "test"]);
        assert_eq!(failed_check(&two, &[("test", Fail)]), Some("test"));
        assert_eq!(failed_check(&two, &[("test", Fail), ("test", Pass)]), None);
        assert_eq!(failed_check(&two, &[]), None);
    }

    #[test]
    fn check_reports_read_at_most_one_page() {
        use std::sync::Arc;

        use graft_store::memory::MemoryStore;
        use graft_testkit::clock::FakeClock;

        use crate::EngineConfig;

        // The memory store answers at once: one poll completes a call.
        fn block_on<F: std::future::Future>(f: F) -> F::Output {
            let mut f = std::pin::pin!(f);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            match f.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(v) => v,
                std::task::Poll::Pending => panic!("memory store call pending"),
            }
        }

        let meta = Arc::new(MemoryStore::new());
        let repo = block_on(meta.create_repo("r")).expect("repo");
        let id = ChangesetId(graft_model::ObjectId([9; 32]));
        let total = MAX_REPORTS as usize + 5;
        for _ in 0..total {
            let txn = MetaTxn {
                preconditions: Vec::new(),
                writes: vec![
                    Write::Journal {
                        kind: "Attested".to_owned(),
                        body: Vec::new(),
                    },
                    Write::PutAttestation {
                        changeset: id,
                        line_pos: None,
                        kind: AttestKind::Check,
                        name: "build".to_owned(),
                        status: AttestStatus::Fail,
                        by: None,
                        url: String::new(),
                    },
                ],
            };
            block_on(meta.commit(repo, txn)).expect("commit");
        }
        let engine = Engine::new(
            meta.clone(),
            meta.clone(),
            Arc::new(FakeClock::at(0)),
            EngineConfig::default(),
        );
        let before = meta.rows_visited();
        let reports = block_on(engine.check_reports(repo, id)).expect("reports");
        assert_eq!(reports.len(), MAX_REPORTS as usize);
        assert_eq!(meta.rows_visited() - before, u64::from(MAX_REPORTS));
    }

    #[test]
    fn tokens_parse() {
        assert_eq!(parse_token::<2>("3.4").ok(), Some([3, 4]));
        assert!(parse_token::<2>("3").is_err());
        assert!(parse_token::<2>("3.4.5").is_err());
        assert!(parse_token::<1>("x").is_err());
    }
}
