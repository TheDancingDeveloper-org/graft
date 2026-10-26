//! v0.2: `ReviewQueue`, the landings on a line since an LSN with no
//! `review` attestation, filtered by path prefix and size, on both stores.
//! A query, never a gate.

mod common;

use common::*;
use graft_engine::policy::LineChecks;
use graft_engine::{EngineConfig, LineConfig, MAX_REVIEW_SCAN};
use graft_model::{ChangesetId, RepoPath};
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi};
use graft_store::MetaStore;
use graft_store::memory::MemoryStore;

/// `chk` has post-land checks; `main` and every other line are off.
fn config() -> EngineConfig {
    let mut chk = LineConfig::direct("chk");
    chk.checks = LineChecks::PostLand {
        required: vec!["build".to_owned()],
        retries: 0,
    };
    EngineConfig {
        lines: vec![chk],
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
        let root = r.commit(vec![], &[("a", b"a0\n")], &[]).id;
        for line in ["main", "chk"] {
            r.update_ref(line, None, root, RefKind::Line).expect("line");
        }
        Self { r, root }
    }

    fn commit(
        &self,
        parent: ChangesetId,
        files: &[(&str, &[u8])],
        deletes: &[&str],
    ) -> ChangesetId {
        self.r.commit(vec![parent], files, deletes).id
    }

    fn try_land(&self, head: ChangesetId, line: &str) -> Landed {
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
        .expect("land")
    }

    fn land(&self, head: ChangesetId, line: &str) -> ChangesetId {
        match self.try_land(head, line) {
            Landed::Landed { landed, .. } => landed,
            other => panic!("not landed: {other:?}"),
        }
    }

    fn attest(&self, id: ChangesetId, kind: AttestKind, status: AttestStatus) {
        block_on(self.r.e().attest_v2(
            &self.r.cx,
            AttestV2 {
                repo: "r".to_owned(),
                changeset: id,
                kind,
                check: "build".to_owned(),
                status,
                by: Some("Ann".to_owned()),
                url: String::new(),
            },
        ))
        .expect("attest");
    }

    fn try_queue(
        &self,
        line: &str,
        since_lsn: u64,
        path: Option<&str>,
        max_lines: Option<u32>,
        limit: u32,
    ) -> Result<ReviewQueuePage, ApiError> {
        block_on(self.r.e().review_queue(
            &self.r.cx,
            ReviewQueue {
                repo: "r".to_owned(),
                line: line.to_owned(),
                since_lsn,
                path_prefix: path.map(|p| RepoPath::new(p).expect("path")),
                max_lines,
                page: Page { limit, token: None },
            },
        ))
    }

    fn queue(
        &self,
        line: &str,
        since_lsn: u64,
        path: Option<&str>,
        max_lines: Option<u32>,
        limit: u32,
    ) -> ReviewQueuePage {
        self.try_queue(line, since_lsn, path, max_lines, limit)
            .expect("review queue")
    }

    /// Every queued changeset, following `next_lsn` with pages of `limit`.
    fn drain(
        &self,
        line: &str,
        path: Option<&str>,
        max_lines: Option<u32>,
        limit: u32,
    ) -> Vec<ChangesetId> {
        let mut out = Vec::new();
        let mut since = 0;
        loop {
            let page = self.queue(line, since, path, max_lines, limit);
            assert!(page.items.len() <= limit as usize);
            out.extend(page.items.iter().map(|i| i.changeset));
            match page.next_lsn {
                Some(next) => {
                    assert!(next > since, "next_lsn advances");
                    since = next;
                }
                None => return out,
            }
        }
    }
}

fn ids(page: &ReviewQueuePage) -> Vec<ChangesetId> {
    page.items.iter().map(|i| i.changeset).collect()
}

// Accept 1: a `review` attestation of any status takes a landing out of
// the queue; `check` and `approval` attestations leave it there.
fn review_of_any_status_drops_out<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let mut tip = t.root;
    let mut landed = Vec::new();
    for i in 0..6u8 {
        tip = t.land(t.commit(tip, &[("b", &[b'0' + i, b'\n'])], &[]), "main");
        landed.push(tip);
    }
    assert_eq!(ids(&t.queue("main", 0, None, None, 0)), landed);

    let statuses = [
        AttestStatus::Pass,
        AttestStatus::Fail,
        AttestStatus::Pending,
        AttestStatus::Error,
    ];
    for (id, status) in landed.iter().zip(statuses) {
        t.attest(*id, AttestKind::Review, status);
    }
    t.attest(landed[4], AttestKind::Check, AttestStatus::Pass);
    t.attest(landed[4], AttestKind::Check, AttestStatus::Fail);
    t.attest(landed[5], AttestKind::Approval, AttestStatus::Pass);
    assert_eq!(
        ids(&t.queue("main", 0, None, None, 0)),
        vec![landed[4], landed[5]]
    );
}

// Accept 2: path prefix and size filters return exactly the matching
// landings, with paths and lines from each landing's summary.
fn path_and_size_filters_table<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    // l1: a new file of one line.
    let l1 = t.land(t.commit(t.root, &[("src/a.rs", b"x\n")], &[]), "main");
    // l2: three lines in docs.
    let l2 = t.land(t.commit(l1, &[("docs/r.md", b"1\n2\n3\n")], &[]), "main");
    // l3: ten lines in src and one in docs.
    let l3 = t.land(
        t.commit(
            l2,
            &[
                ("src/b.rs", b"0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n"),
                ("docs/x.md", b"x\n"),
            ],
            &[],
        ),
        "main",
    );
    // l4: deletes the one-line src/a.rs.
    let l4 = t.land(t.commit(l3, &[], &["src/a.rs"]), "main");
    // l5: a stack of two drafts landed as one landing: src/c.rs (two
    // lines) below docs/y.md (two lines). The path filter sees both.
    let low = t.commit(l4, &[("src/c.rs", b"c\nd\n")], &[]);
    let l5 = t.land(t.commit(low, &[("docs/y.md", b"y\nz\n")], &[]), "main");

    let all = t.queue("main", 0, None, None, 0);
    let summary: Vec<(ChangesetId, u32, u64)> = all
        .items
        .iter()
        .map(|i| (i.changeset, i.paths_touched, i.lines_changed))
        .collect();
    assert_eq!(
        summary,
        vec![(l1, 1, 1), (l2, 1, 3), (l3, 2, 11), (l4, 1, 1), (l5, 2, 4)]
    );
    assert!(all.items.windows(2).all(|w| w[0].lsn < w[1].lsn));
    assert_eq!(all.next_lsn, None);

    let cases: [(Option<&str>, Option<u32>, Vec<ChangesetId>); 10] = [
        (None, None, vec![l1, l2, l3, l4, l5]),
        (Some("src"), None, vec![l1, l3, l4, l5]),
        (Some("docs"), None, vec![l2, l3, l5]),
        (Some("src/a.rs"), None, vec![l1, l4]),
        (Some("docs/y.md"), None, vec![l5]),
        (Some("nope"), None, vec![]),
        (None, Some(0), vec![]),
        (None, Some(1), vec![l1, l4]),
        (None, Some(4), vec![l1, l2, l4, l5]),
        (Some("docs"), Some(3), vec![l2]),
    ];
    for (path, max_lines, want) in cases {
        let got = t.queue("main", 0, path, max_lines, 0);
        assert_eq!(ids(&got), want, "path {path:?}, max_lines {max_lines:?}");
        // The same set page by page.
        assert_eq!(
            t.drain("main", path, max_lines, 1),
            want,
            "paged {path:?} {max_lines:?}"
        );
    }
}

// Accept 4: the queue works on a line with checks off and on a post-land
// checked line alike, and a review never changes what a land does.
fn any_line_and_never_a_gate<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    for line in ["main", "chk"] {
        let a = t.land(t.commit(t.root, &[("b", line.as_bytes())], &[]), line);
        let page = t.queue(line, 0, None, None, 0);
        assert_eq!(ids(&page), vec![a], "{line}");
        t.attest(a, AttestKind::Review, AttestStatus::Fail);
        assert!(t.queue(line, 0, None, None, 0).items.is_empty(), "{line}");
        // A failed review blocks nothing: the next land goes straight on.
        let b = t.commit(a, &[("c", line.as_bytes())], &[]);
        assert!(matches!(t.try_land(b, line), Landed::Landed { landed, .. } if landed == b));
        assert_eq!(ids(&t.queue(line, 0, None, None, 0)), vec![b], "{line}");
    }
    // A land that moves nothing adds nothing to the queue.
    let tip = t.queue("main", 0, None, None, 0).items[0].changeset;
    let _ = t.try_land(tip, "main");
    assert_eq!(t.queue("main", 0, None, None, 0).items.len(), 1);
}

// Paging: `next_lsn` resumes after the last item; caught up answers none.
fn pages_resume_from_next_lsn<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let mut tip = t.root;
    let mut landed = Vec::new();
    for i in 0..5u8 {
        tip = t.land(t.commit(tip, &[("b", &[b'0' + i, b'\n'])], &[]), "main");
        landed.push(tip);
    }
    let first = t.queue("main", 0, None, None, 2);
    assert_eq!(ids(&first), landed[..2]);
    assert_eq!(first.next_lsn, Some(first.items[1].lsn));
    assert_eq!(t.drain("main", None, None, 2), landed);
    assert_eq!(t.drain("main", Some("b"), Some(2), 2), landed);
    // Past the last landing it is caught up.
    let last = t.queue("main", 0, None, None, 0).items[4].lsn;
    let done = t.queue("main", last, None, None, 0);
    assert!(done.items.is_empty());
    assert_eq!(done.next_lsn, None);
    // Another line's queue is its own.
    assert!(t.queue("chk", 0, None, None, 0).items.is_empty());
}

fn reads_need_read_and_reject_tokens<M: MetaStore>(f: &Fixture<M>) {
    let t = T::new(f);
    let req = ReviewQueue {
        repo: "r".to_owned(),
        line: "main".to_owned(),
        since_lsn: 0,
        path_prefix: None,
        max_lines: None,
        page: Page::default(),
    };
    assert!(block_on(t.r.e().review_queue(&cx(&["repo:r:read"]), req.clone())).is_ok());
    assert!(matches!(
        block_on(t.r.e().review_queue(&cx(&["repo:s:read"]), req.clone())),
        Err(ApiError::PermissionDenied { .. })
    ));
    let tokened = ReviewQueue {
        page: Page {
            limit: 0,
            token: Some("1".to_owned()),
        },
        ..req
    };
    assert!(matches!(
        block_on(t.r.e().review_queue(&t.r.cx, tokened)),
        Err(ApiError::InvalidChange { .. })
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
    review_of_any_status_drops_out,
    path_and_size_filters_table,
    any_line_and_never_a_gate,
    pages_resume_from_next_lsn,
    reads_need_read_and_reject_tokens,
);

// Accept 3: on a 10k-landing line a call visits the page plus the landings
// it filters out, capped at `MAX_REVIEW_SCAN`, never the line: a counting
// store records every review queue row read.
#[test]
fn cost_follows_the_page_not_the_line() {
    let meta = std::sync::Arc::new(MemoryStore::new());
    let f = build(meta.clone(), config(), None);
    let t = T::new(&f);
    const LANDS: usize = 10_000;
    const OPEN: usize = 10;
    let mut tip = t.root;
    let mut landed = Vec::with_capacity(LANDS);
    for i in 0..LANDS {
        let path = if i == LANDS - 1 { "c" } else { "b" };
        let body = format!("{i}\n");
        tip = t.land(t.commit(tip, &[(path, body.as_bytes())], &[]), "main");
        landed.push(tip);
    }

    // Unreviewed and filtered out everywhere: one call reads at most the
    // scan cap, then hands back `next_lsn` to go on.
    let before = meta.rows_visited();
    let page = t.queue("main", 0, Some("nope"), None, 0);
    let visited = meta.rows_visited() - before;
    assert!(page.items.is_empty());
    assert!(page.next_lsn.is_some());
    assert!(visited <= u64::from(MAX_REVIEW_SCAN), "visited {visited}");

    // An unfiltered page of 10 reads 10 rows.
    let before = meta.rows_visited();
    assert_eq!(t.queue("main", 0, None, None, 10).items.len(), 10);
    assert_eq!(meta.rows_visited() - before, 10);

    // Review all but the last `OPEN`: they leave the index, so the queue
    // reads only what is still open, wherever it starts.
    for id in &landed[..LANDS - OPEN] {
        t.attest(*id, AttestKind::Review, AttestStatus::Pass);
    }
    let before = meta.rows_visited();
    let page = t.queue("main", 0, None, None, 0);
    assert_eq!(ids(&page), landed[LANDS - OPEN..]);
    assert_eq!(page.next_lsn, None);
    assert!(meta.rows_visited() - before <= OPEN as u64);

    // A path filter reads the open window and keeps the one match.
    let before = meta.rows_visited();
    let page = t.queue("main", 0, Some("c"), None, 0);
    assert_eq!(ids(&page), vec![landed[LANDS - 1]]);
    assert!(meta.rows_visited() - before <= OPEN as u64);
}
