//! v0.2: the generalised `Attest`, `GetAttestations`, land-checks rows on
//! lines with post-land checks, and `GetLandChecks`, on both stores. None
//! of it gates a land (D25, D37).

mod common;

use common::*;
use graft_engine::policy::{LandPolicy, LineChecks};
use graft_engine::{EngineConfig, LineConfig};
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi};
use graft_store::MetaStore;
use graft_store::memory::MemoryStore;

/// `chk` has post-land checks `build` and `test`, one retry; `gate` is
/// `attested` on `ci/test` with post-land checks on `ci/test`; `main` and
/// every other line are off.
fn config() -> EngineConfig {
    let mut chk = LineConfig::direct("chk");
    chk.checks = LineChecks::PostLand {
        required: vec!["build".to_owned(), "test".to_owned()],
        retries: 1,
    };
    let mut gate = LineConfig::direct("gate");
    gate.policy = LandPolicy::Attested {
        checks: vec!["ci/test".to_owned()],
        attest_merged: false,
    };
    gate.checks = LineChecks::PostLand {
        required: vec!["ci/test".to_owned()],
        retries: 0,
    };
    EngineConfig {
        lines: vec![chk, gate],
        ..EngineConfig::default()
    }
}

struct T<'f, M> {
    r: Repo<'f, M>,
    root: ChangesetId,
}

impl<'f, M: MetaStore> T<'f, M> {
    fn new(f: &'f Fixture<M>) -> Self {
        let r = Repo::new(f, "r");
        let root = r.commit(vec![], &[("a", b"a0")], &[]).id;
        for line in ["main", "chk", "gate"] {
            r.update_ref(line, None, root, RefKind::Line).expect("line");
        }
        Self { r, root }
    }

    fn commit(&self, parent: ChangesetId, path: &str, data: &[u8]) -> ChangesetId {
        self.r.commit(vec![parent], &[(path, data)], &[]).id
    }

    fn try_land(&self, head: ChangesetId, line: &str) -> ApiResult<Landed> {
        block_on(self.r.e().land(
            &self.r.cx,
            Land {
                repo: "r".to_owned(),
                head,
                line: line.to_owned(),
                squash: false,
                merge: MergeMode::File,
                deadline_ms: None,
                merged_from: None,
            },
        ))
    }

    /// Lands `head` and returns what landed.
    fn land(&self, head: ChangesetId, line: &str) -> ChangesetId {
        match self.try_land(head, line).expect("land") {
            Landed::Landed { landed, .. } => landed,
            other => panic!("not landed: {other:?}"),
        }
    }

    fn attest(&self, id: ChangesetId, kind: AttestKind, check: &str, status: AttestStatus) {
        block_on(self.r.e().attest_v2(
            &self.r.cx,
            AttestV2 {
                repo: "r".to_owned(),
                changeset: id,
                kind,
                check: check.to_owned(),
                status,
                by: Some("Ann".to_owned()),
                url: format!("https://ci.example/{check}"),
            },
        ))
        .expect("attest");
    }

    fn check(&self, id: ChangesetId, check: &str, status: AttestStatus) {
        self.attest(id, AttestKind::Check, check, status);
    }

    fn land_checks(&self, line: &str, from_pos: u64, limit: u32) -> LandCheckPage {
        block_on(self.r.e().get_land_checks(
            &self.r.cx,
            GetLandChecks {
                repo: "r".to_owned(),
                line: line.to_owned(),
                from_pos,
                page: Page { limit, token: None },
            },
        ))
        .expect("land checks")
    }

    /// The state of the landing of `id` on `line`.
    fn state(&self, line: &str, id: ChangesetId) -> LandCheckState {
        let rows = self.land_checks(line, 0, 1000).rows;
        rows.iter()
            .find(|r| r.changeset == id)
            .unwrap_or_else(|| panic!("no row for {id} in {rows:?}"))
            .state
    }

    fn attestations(
        &self,
        selector: AttestationSelector,
        kinds: Vec<AttestKind>,
    ) -> Vec<Attestation> {
        block_on(self.r.e().get_attestations(
            &self.r.cx,
            GetAttestations {
                repo: "r".to_owned(),
                selector,
                kinds,
                page: Page {
                    limit: 0,
                    token: None,
                },
            },
        ))
        .expect("attestations")
        .attestations
    }
}

fn bodies<M: MetaStore>(r: &Repo<'_, M>) -> Vec<(u64, String, String)> {
    r.journal()
        .into_iter()
        .map(|e| (e.lsn, e.kind, e.body))
        .collect()
}

// Accept 1 / done-when 2: a line with checks off writes no row, and its
// land answers and journals exactly as without any checked line.
fn off_line_writes_no_row_and_lands_identically<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let head = t.commit(t.root, "b", b"b1");
    let got = t.try_land(head, "main").expect("land");
    assert!(t.land_checks("main", 0, 1000).rows.is_empty());

    // The same work on an engine with no checked line at all.
    let plain = memory_with(EngineConfig::default());
    let p = T::new(&plain);
    let want = p
        .try_land(p.commit(p.root, "b", b"b1"), "main")
        .expect("land");
    assert_eq!(got, want);
    assert_eq!(bodies(&t.r), bodies(&p.r));
}

// Accept 2: a bad landing never refuses, delays or parks a later land.
fn bad_never_blocks_a_land<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let first = t.land(t.commit(t.root, "b", b"b1"), "chk");
    assert_eq!(t.state("chk", first), LandCheckState::Pending);
    t.check(first, "build", AttestStatus::Fail);
    t.check(first, "build", AttestStatus::Fail);
    assert_eq!(t.state("chk", first), LandCheckState::Bad);
    let next = t.commit(first, "c", b"c1");
    assert!(matches!(t.try_land(next, "chk"), Ok(Landed::Landed { landed, .. }) if landed == next));
    assert_eq!(t.state("chk", next), LandCheckState::Pending);
}

// Accept 3: two required checks.
fn two_required_checks<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let a = t.land(t.commit(t.root, "b", b"b1"), "chk");
    t.check(a, "build", AttestStatus::Pass);
    t.check(a, "test", AttestStatus::Pending);
    assert_eq!(t.state("chk", a), LandCheckState::Pending);
    t.check(a, "test", AttestStatus::Pass);
    assert_eq!(t.state("chk", a), LandCheckState::Good);

    let b = t.land(t.commit(a, "c", b"c1"), "chk");
    t.check(b, "test", AttestStatus::Fail);
    assert_eq!(t.state("chk", b), LandCheckState::Pending, "one retry left");
    t.check(b, "test", AttestStatus::Fail);
    assert_eq!(t.state("chk", b), LandCheckState::Bad);
    // The rows are in position order and the first is untouched.
    let rows = t.land_checks("chk", 0, 1000).rows;
    assert_eq!(rows.iter().map(|r| r.changeset).collect::<Vec<_>>(), [a, b]);
    assert!(rows[0].pos < rows[1].pos);
}

// Accept 4: an old-shape `Attest` still works and journals exactly the
// v0.1 body (frozen).
fn old_attest_journals_old_keys_and_kind<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let a = t.land(t.commit(t.root, "b", b"b1"), "chk");
    block_on(t.r.e().attest(
        &t.r.cx,
        Attest {
            repo: "r".to_owned(),
            changeset: a,
            check: "build".to_owned(),
            passed: true,
            url: "https://ci/1".to_owned(),
        },
    ))
    .expect("attest");
    let last = t.r.journal().pop().expect("entry");
    assert_eq!(last.kind, "Attested");
    assert_eq!(
        last.body,
        format!(
            "{{\"changeset\":\"{a}\",\"check\":\"build\",\"passed\":true,\"url\":\"https://ci/1\",\"actor\":\"tester\"}}"
        )
    );
    let got = t.attestations(AttestationSelector::ByChangeset(a), Vec::new());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].kind, AttestKind::Check);
    assert_eq!(got[0].status, AttestStatus::Pass);
    // It still feeds the land state, and the attested policy.
    t.check(a, "test", AttestStatus::Pass);
    assert_eq!(t.state("chk", a), LandCheckState::Good);
}

// Accept 5: reviews and approvals are stored and returned, change no land
// state and gate nothing.
fn reviews_and_approvals_gate_nothing<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let a = t.land(t.commit(t.root, "b", b"b1"), "chk");
    t.attest(a, AttestKind::Review, "build", AttestStatus::Pass);
    t.attest(a, AttestKind::Approval, "test", AttestStatus::Pass);
    t.attest(a, AttestKind::Review, "test", AttestStatus::Fail);
    t.attest(a, AttestKind::Review, "test", AttestStatus::Fail);
    assert_eq!(t.state("chk", a), LandCheckState::Pending);
    let all = t.attestations(AttestationSelector::ByChangeset(a), Vec::new());
    assert_eq!(all.len(), 4);
    assert_eq!(all[0].by.as_deref(), Some("Ann"));
    let approvals = t.attestations(
        AttestationSelector::ByChangeset(a),
        vec![AttestKind::Approval],
    );
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].check, "test");
    // By line range: the landed changeset's attestations name its position.
    let pos = t.land_checks("chk", 0, 1).rows[0].pos;
    let ranged = t.attestations(
        AttestationSelector::ByLineRange {
            line: "chk".to_owned(),
            from_pos: pos,
            to_pos: pos,
        },
        Vec::new(),
    );
    assert_eq!(ranged.len(), 4);

    // A passing review of the gate's check does not satisfy the gate.
    let draft = t.commit(t.root, "c", b"c1");
    t.attest(draft, AttestKind::Review, "ci/test", AttestStatus::Pass);
    t.attest(draft, AttestKind::Approval, "ci/test", AttestStatus::Pass);
    assert!(matches!(
        t.try_land(draft, "gate"),
        Err(ApiError::Rejected { policy, .. }) if policy == "attested"
    ));
    // A check does, as before v0.2.
    t.check(draft, "ci/test", AttestStatus::Pass);
    let landed = t.land(draft, "gate");
    assert_eq!(t.state("gate", landed), LandCheckState::Pending);
    // The draft's pre-land pass is not a post-land result.
    t.check(landed, "ci/test", AttestStatus::Pass);
    assert_eq!(t.state("gate", landed), LandCheckState::Good);
}

// A revert land marks the landing it reverts.
fn a_revert_marks_its_landing<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let a = t.land(t.commit(t.root, "b", b"b1"), "chk");
    t.check(a, "test", AttestStatus::Fail);
    let revert = block_on(t.r.e().revert(
        &t.r.cx,
        Revert {
            repo: "r".to_owned(),
            changeset: a,
            onto: a,
            workspace: None,
        },
    ))
    .expect("revert");
    let landed = t.land(revert, "chk");
    let rows = t.land_checks("chk", 0, 1000).rows;
    let row = rows.iter().find(|r| r.changeset == a).expect("row");
    assert_eq!(row.state, LandCheckState::Reverted);
    assert_eq!(row.revert, Some(landed));
    assert_eq!(t.state("chk", landed), LandCheckState::Pending);
    let entry =
        t.r.journal()
            .into_iter()
            .find(|e| e.kind == "LandReverted")
            .expect("LandReverted");
    assert_eq!(
        entry.body,
        format!("{{\"line\":\"chk\",\"land\":\"{a}\",\"revert\":\"{landed}\",\"check\":\"test\"}}")
    );
    // A reverted landing keeps its state whatever is attested later.
    t.check(a, "build", AttestStatus::Pass);
    t.check(a, "test", AttestStatus::Pass);
    assert_eq!(t.state("chk", a), LandCheckState::Reverted);
}

// Paging through both reads.
fn reads_page<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let mut tip = t.root;
    let mut landed = Vec::new();
    for i in 0..5u8 {
        tip = t.land(t.commit(tip, "b", &[b'0' + i]), "chk");
        landed.push(tip);
    }
    let mut seen = Vec::new();
    let mut token = None;
    loop {
        let page = block_on(t.r.e().get_land_checks(
            &t.r.cx,
            GetLandChecks {
                repo: "r".to_owned(),
                line: "chk".to_owned(),
                from_pos: 0,
                page: Page { limit: 2, token },
            },
        ))
        .expect("page");
        seen.extend(page.rows.iter().map(|r| r.changeset));
        match page.next {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, landed);

    for status in [
        AttestStatus::Pending,
        AttestStatus::Error,
        AttestStatus::Pass,
    ] {
        t.check(tip, "build", status);
    }
    let mut got = Vec::new();
    let mut token = None;
    loop {
        let page = block_on(t.r.e().get_attestations(
            &t.r.cx,
            GetAttestations {
                repo: "r".to_owned(),
                selector: AttestationSelector::ByChangeset(tip),
                kinds: Vec::new(),
                page: Page { limit: 2, token },
            },
        ))
        .expect("page");
        got.extend(page.attestations.into_iter().map(|a| a.status));
        match page.next {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    assert_eq!(
        got,
        [
            AttestStatus::Pending,
            AttestStatus::Error,
            AttestStatus::Pass
        ]
    );
}

// The reads need `read`, and only `read`.
fn reads_need_read<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let reader = cx(&["repo:r:read"]);
    let none = cx(&["repo:s:read"]);
    let req = GetLandChecks {
        repo: "r".to_owned(),
        line: "chk".to_owned(),
        from_pos: 0,
        page: Page::default(),
    };
    assert!(block_on(t.r.e().get_land_checks(&reader, req.clone())).is_ok());
    assert!(matches!(
        block_on(t.r.e().get_land_checks(&none, req)),
        Err(ApiError::PermissionDenied { .. })
    ));
    let req = GetAttestations {
        repo: "r".to_owned(),
        selector: AttestationSelector::ByChangeset(t.root),
        kinds: Vec::new(),
        page: Page::default(),
    };
    assert!(block_on(t.r.e().get_attestations(&reader, req.clone())).is_ok());
    assert!(matches!(
        block_on(t.r.e().get_attestations(&none, req)),
        Err(ApiError::PermissionDenied { .. })
    ));
}

macro_rules! on_both {
    ($($name:ident),* $(,)?) => {
        mod memory_store {
            $( #[test] fn $name() { super::$name(&super::memory_with(super::config())) } )*
        }
        mod sqlite_store {
            $( #[test] fn $name() { super::$name(&super::sqlite_with(super::config())) } )*
        }
    };
}

on_both!(
    off_line_writes_no_row_and_lands_identically,
    bad_never_blocks_a_land,
    two_required_checks,
    old_attest_journals_old_keys_and_kind,
    reviews_and_approvals_gate_nothing,
    a_revert_marks_its_landing,
    reads_page,
    reads_need_read,
);

// Accept 6: the reads and the land-state recompute visit index rows of
// their own changeset or page only, never the history (a counting store).
#[test]
fn reads_cost_follows_the_page_not_the_history() {
    let meta = std::sync::Arc::new(MemoryStore::new());
    let f = build(meta.clone(), config(), None);
    let t = T::new(&f);
    let mut tip = t.root;
    for i in 0..20u8 {
        tip = t.land(t.commit(tip, "b", &[b'a' + i]), "chk");
        for _ in 0..5 {
            t.check(tip, "lint", AttestStatus::Pass);
        }
    }
    let quiet = t.land(t.commit(tip, "c", b"c1"), "chk");
    t.check(quiet, "build", AttestStatus::Pass);

    let before = meta.rows_visited();
    let got = t.attestations(AttestationSelector::ByChangeset(quiet), Vec::new());
    assert_eq!(got.len(), 1);
    assert_eq!(meta.rows_visited() - before, 1);

    let before = meta.rows_visited();
    assert_eq!(t.land_checks("chk", 0, 3).rows.len(), 3);
    assert_eq!(meta.rows_visited() - before, 3);

    // Recomputing the quiet landing's state reads its own row and its own
    // attestation: two rows, whatever the line's history holds.
    let before = meta.rows_visited();
    t.check(quiet, "test", AttestStatus::Pass);
    assert_eq!(meta.rows_visited() - before, 2);
    assert_eq!(t.state("chk", quiet), LandCheckState::Good);
}
