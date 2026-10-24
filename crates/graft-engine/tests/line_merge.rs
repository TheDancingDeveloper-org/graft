//! WI-2368 (D29): merges between lines record a merge point, and
//! `MergeBase` reads merge points before the ancestry walk. A repeated
//! merge starts from the last one, git flow's release → main finds the
//! earlier release through develop, a third line merged into the source is
//! followed, a capped search falls back to ancestry and says so, and the
//! merge point row is written with its journal entry or not at all. On both
//! stores.

mod common;

use common::*;
use graft_engine::EngineConfig;
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi};
use graft_store::MetaStore;

on_both_stores!(
    a_repeated_merge_starts_from_the_last_one,
    release_into_main_finds_the_earlier_release_through_develop,
    a_third_line_merged_into_the_source_is_followed,
    the_merge_point_is_written_with_its_journal_entry,
    an_older_merge_below_a_full_page_of_rows_is_found,
    line_merges_pages_newest_first,
    a_merge_result_says_how_its_base_was_found,
);

const START: &str = "1\n2\n3\n4\n5\n";

/// `START` with line `n` (from 1) replaced by `text`.
fn with(base: &str, n: usize, text: &str) -> String {
    base.lines()
        .enumerate()
        .map(|(i, l)| if i + 1 == n { text } else { l })
        .map(|l| format!("{l}\n"))
        .collect()
}

fn create<M: MetaStore>(r: &Repo<'_, M>, name: &str, from: ChangesetId) {
    block_on(r.e().create_line(
        &r.cx,
        CreateLine {
            repo: r.name.clone(),
            name: name.to_owned(),
            from,
        },
    ))
    .expect("create line");
}

fn tip<M: MetaStore>(r: &Repo<'_, M>, line: &str) -> ChangesetId {
    block_on(r.e().resolve_ref(
        &r.cx,
        ResolveRef {
            repo: r.name.clone(),
            name: line.to_owned(),
        },
    ))
    .expect("ref")
    .target
}

/// Commits `f = text` on `line`'s tip and lands it.
fn land_text<M: MetaStore>(r: &Repo<'_, M>, line: &str, text: &str) -> ChangesetId {
    let id = r
        .commit(vec![tip(r, line)], &[("f", text.as_bytes())], &[])
        .id;
    let landed = block_on(r.e().land(
        &r.cx,
        Land {
            repo: r.name.clone(),
            head: id,
            line: line.to_owned(),
            squash: false,
            merge: MergeMode::Line,
            deadline_ms: None,
            merged_from: None,
        },
    ));
    assert!(matches!(landed, Ok(Landed::Landed { .. })), "{landed:?}");
    id
}

fn merge<M: MetaStore>(r: &Repo<'_, M>, src: &str, dst: &str) -> ApiResult<LineMergeOutcome> {
    block_on(r.e().line_merge(
        &r.cx,
        LineMerge {
            repo: r.name.clone(),
            src: src.to_owned(),
            dst: dst.to_owned(),
            src_at: None,
            merge: MergeMode::Line,
            deadline_ms: None,
            message: None,
        },
    ))
}

fn merged<M: MetaStore>(r: &Repo<'_, M>, src: &str, dst: &str) -> ChangesetId {
    match merge(r, src, dst) {
        Ok(LineMergeOutcome::Merged { landed, .. }) => {
            assert_eq!(tip(r, dst), landed);
            landed
        }
        other => panic!("merge {src} into {dst}: {other:?}"),
    }
}

fn base<M: MetaStore>(r: &Repo<'_, M>, a: ChangesetId, b: ChangesetId) -> MergeBaseResult {
    block_on(r.e().merge_base_via(
        &r.cx,
        Pair {
            repo: r.name.clone(),
            a,
            b,
        },
    ))
    .expect("merge base")
}

fn text<M: MetaStore>(r: &Repo<'_, M>, at: ChangesetId) -> String {
    let file = block_on(r.e().read_file(
        &r.cx,
        PathAt {
            repo: r.name.clone(),
            at,
            path: p("f"),
        },
    ))
    .expect("read");
    String::from_utf8(file.data).expect("utf-8")
}

fn points<M: MetaStore>(r: &Repo<'_, M>, dst: &str) -> Vec<LineMergePoint> {
    block_on(r.e().line_merges(
        &r.cx,
        LineMerges {
            repo: r.name.clone(),
            dst: dst.to_owned(),
            src: None,
            page: Page::default(),
        },
    ))
    .expect("line merges")
    .merges
}

/// `main` = m0; `develop` forks there.
fn start<'f, M: MetaStore>(f: &'f Fixture<M>) -> (Repo<'f, M>, ChangesetId) {
    let r = Repo::new(f, "r");
    let m0 = r.commit(vec![], &[("f", START.as_bytes())], &[]).id;
    r.update_ref("main", None, m0, RefKind::Line).expect("main");
    create(&r, "develop", m0);
    (r, m0)
}

/// develop edits line 1 and merges into main, main edits line 5, then
/// develop edits line 1 again. Returns (d1, d2).
fn two_merges_apart<M: MetaStore>(r: &Repo<'_, M>) -> (ChangesetId, ChangesetId) {
    let d1 = land_text(r, "develop", &with(START, 1, "1d1"));
    land_text(r, "main", &with(START, 5, "5m"));
    let first = merged(r, "develop", "main");
    assert_eq!(text(r, first), with(&with(START, 1, "1d1"), 5, "5m"));
    let d2 = land_text(r, "develop", &with(START, 1, "1d2"));
    (d1, d2)
}

fn a_repeated_merge_starts_from_the_last_one<M: MetaStore>(f: &Fixture<M>) {
    let (r, _) = start(f);
    let (d1, d2) = two_merges_apart(&r);

    // From the fork, both sides changed line 1; from the last merge only
    // develop did.
    let found = base(&r, d2, tip(&r, "main"));
    assert_eq!(found.base, Some(d1));
    assert_eq!(found.via, MergeBaseVia::MergePoint);
    let second = merged(&r, "develop", "main");
    assert_eq!(text(&r, second), with(&with(START, 1, "1d2"), 5, "5m"));
    assert_eq!(
        merge(&r, "develop", "main"),
        Ok(LineMergeOutcome::UpToDate { tip: second }),
        "nothing new on develop"
    );

    // The back-merge starts from what main last took from develop.
    let found = base(&r, second, d2);
    assert_eq!(found.base, Some(d2));
    assert_eq!(found.via, MergeBaseVia::MergePoint);
    let back = merged(&r, "main", "develop");
    assert_eq!(text(&r, back), with(&with(START, 1, "1d2"), 5, "5m"));
    let marks: Vec<(String, ChangesetId)> = points(&r, "main")
        .into_iter()
        .map(|p| (p.src, p.src_changeset))
        .collect();
    assert_eq!(
        marks,
        vec![("develop".to_owned(), d2), ("develop".to_owned(), d1)]
    );
}

/// git flow: release/1.0 forks from develop, merges into main and back
/// into develop; release/1.1 forks from develop later. Its merge into main
/// starts from main's merge of release/1.0, found through develop's fork
/// and merge rows. Returns main's merge landing and release/1.1's tip.
fn gitflow<M: MetaStore>(r: &Repo<'_, M>) -> (ChangesetId, ChangesetId) {
    let d1 = land_text(r, "develop", &with(START, 2, "2d"));
    create(r, "release/1.0", d1);
    land_text(r, "release/1.0", &with(&with(START, 2, "2d"), 3, "3r10"));
    let main_merge = merged(r, "release/1.0", "main");
    merged(r, "release/1.0", "develop");
    let develop = text(r, tip(r, "develop"));
    land_text(r, "develop", &with(&develop, 4, "4d"));
    create(r, "release/1.1", tip(r, "develop"));
    let release = text(r, tip(r, "release/1.1"));
    let r11 = land_text(r, "release/1.1", &with(&release, 5, "5r11"));
    (main_merge, r11)
}

fn release_into_main_finds_the_earlier_release_through_develop<M: MetaStore>(f: &Fixture<M>) {
    let (r, _) = start(f);
    let (main_merge, r11) = gitflow(&r);
    let found = base(&r, r11, tip(&r, "main"));
    assert_eq!(found.base, Some(main_merge));
    assert_eq!(found.via, MergeBaseVia::MergePoint);
    let landed = merged(&r, "release/1.1", "main");
    assert_eq!(text(&r, landed), "1\n2d\n3r10\n4d\n5r11\n");
}

fn a_third_line_merged_into_the_source_is_followed<M: MetaStore>(f: &Fixture<M>) {
    let (r, m0) = start(f);
    create(&r, "hotfix", m0);
    land_text(&r, "hotfix", &with(START, 1, "1h"));
    let main_merge = merged(&r, "hotfix", "main");
    merged(&r, "hotfix", "develop");
    let develop = text(&r, tip(&r, "develop"));
    let d1 = land_text(&r, "develop", &with(&develop, 3, "3d"));

    // develop holds main's hotfix merge: both of its inputs.
    let found = base(&r, d1, tip(&r, "main"));
    assert_eq!(found.base, Some(main_merge));
    assert_eq!(found.via, MergeBaseVia::MergePoint);
    let landed = merged(&r, "develop", "main");
    assert_eq!(text(&r, landed), with(&with(START, 1, "1h"), 3, "3d"));
}

fn the_merge_point_is_written_with_its_journal_entry<M: MetaStore>(f: &Fixture<M>) {
    let (r, _) = start(f);
    let d1 = land_text(&r, "develop", &with(START, 1, "1d1"));
    land_text(&r, "main", &with(START, 5, "5m"));
    let before = r.journal().len();
    let landing = merged(&r, "develop", "main");

    let point = points(&r, "main");
    assert_eq!(point.len(), 1);
    let pos = point[0].dst_pos;
    assert_eq!(point[0].dst_changeset, landing);
    assert_eq!(point[0].src_changeset, d1);
    assert_eq!(point[0].src_pos, 1);

    // The row's journal entry is in the move's transaction: right after
    // its RefMoved.
    let journal = r.journal();
    let at = journal
        .iter()
        .position(|e| e.kind == LINE_MERGED)
        .expect("a LineMerged entry");
    assert!(at >= before);
    assert_eq!(journal[at - 1].kind, "RefMoved");
    assert_eq!(journal[at - 1].lsn + 1, journal[at].lsn);
    assert_eq!(
        journal[at].body,
        format!(
            "{{\"src\":\"develop\",\"dst\":\"main\",\"src_changeset\":\"{d1}\",\"src_pos\":1,\"dst_pos\":{pos}}}"
        )
    );

    // A land naming a source changeset its line does not hold writes
    // nothing.
    let other = r.commit(vec![tip(&r, "main")], &[("f", b"x\n")], &[]).id;
    let entries = r.journal().len();
    let bad = block_on(r.e().land(
        &r.cx,
        Land {
            repo: r.name.clone(),
            head: other,
            line: "main".to_owned(),
            squash: false,
            merge: MergeMode::Line,
            deadline_ms: None,
            merged_from: Some(MergedFrom {
                line: "develop".to_owned(),
                changeset: landing,
            }),
        },
    ));
    assert!(
        matches!(bad, Err(ApiError::InvalidChange { .. })),
        "{bad:?}"
    );
    assert_eq!(r.journal().len(), entries);
    assert_eq!(points(&r, "main").len(), 1);

    // A resolution landed with `merged_from` records the same row.
    let develop = tip(&r, "develop");
    let resolution = r
        .commit(vec![tip(&r, "main")], &[("f", b"resolved\n")], &[])
        .id;
    let landed = block_on(r.e().land(
        &r.cx,
        Land {
            repo: r.name.clone(),
            head: resolution,
            line: "main".to_owned(),
            squash: false,
            merge: MergeMode::Line,
            deadline_ms: None,
            merged_from: Some(MergedFrom {
                line: "develop".to_owned(),
                changeset: develop,
            }),
        },
    ));
    assert!(matches!(landed, Ok(Landed::Landed { .. })), "{landed:?}");
    let point = points(&r, "main");
    assert_eq!(point.len(), 2);
    assert_eq!(point[0].dst_changeset, resolution);
    assert_eq!(point[0].src_changeset, develop);
}

/// develop takes the hotfix, then nine merges from feature: more rows than
/// one read returns. Rule 3 pages down to the hotfix row, so develop's
/// merge into main still starts from main's hotfix merge.
fn an_older_merge_below_a_full_page_of_rows_is_found<M: MetaStore>(f: &Fixture<M>) {
    let (r, m0) = start(f);
    create(&r, "hotfix", m0);
    land_text(&r, "hotfix", &with(START, 1, "1h"));
    let main_merge = merged(&r, "hotfix", "main");
    merged(&r, "hotfix", "develop");
    create(&r, "feature", tip(&r, "develop"));
    for i in 0..9 {
        let now = text(&r, tip(&r, "feature"));
        land_text(&r, "feature", &with(&now, 4, &format!("4f{i}")));
        merged(&r, "feature", "develop");
    }
    assert_eq!(points(&r, "develop").len(), 10);
    let develop = text(&r, tip(&r, "develop"));
    let d1 = land_text(&r, "develop", &with(&develop, 3, "3d"));

    let found = base(&r, d1, tip(&r, "main"));
    assert_eq!(found.via, MergeBaseVia::MergePoint);
    assert_eq!(found.base, Some(main_merge));
}

fn line_merges_pages_newest_first<M: MetaStore>(f: &Fixture<M>) {
    let (r, _) = start(f);
    let mut sources = Vec::new();
    for i in 0..3 {
        sources.push(land_text(&r, "develop", &with(START, 1, &format!("1d{i}"))));
        merged(&r, "develop", "main");
    }
    sources.reverse();
    let mut seen = Vec::new();
    let mut token = None;
    loop {
        let page = block_on(r.e().line_merges(
            &r.cx,
            LineMerges {
                repo: r.name.clone(),
                dst: "main".to_owned(),
                src: Some("develop".to_owned()),
                page: Page { limit: 2, token },
            },
        ))
        .expect("line merges");
        assert!(page.merges.len() <= 2);
        seen.extend(page.merges.into_iter().map(|p| p.src_changeset));
        token = page.next;
        if token.is_none() {
            break;
        }
    }
    assert_eq!(seen, sources);

    // A source filter that matches nothing is an empty page.
    let none = block_on(r.e().line_merges(
        &r.cx,
        LineMerges {
            repo: r.name.clone(),
            dst: "main".to_owned(),
            src: Some("release".to_owned()),
            page: Page::default(),
        },
    ))
    .expect("line merges");
    assert_eq!(none, LineMergePage::default());
}

fn a_merge_result_says_how_its_base_was_found<M: MetaStore>(f: &Fixture<M>) {
    let (r, m0) = start(f);
    land_text(&r, "develop", &with(START, 1, "1d1"));
    match merge(&r, "develop", "main") {
        Ok(LineMergeOutcome::Merged { base, via, .. }) => {
            assert_eq!((base, via), (m0, MergeBaseVia::Fork));
        }
        other => panic!("{other:?}"),
    }
    land_text(&r, "develop", &with(START, 1, "1d2"));
    match merge(&r, "develop", "main") {
        Ok(LineMergeOutcome::Merged { via, .. }) => assert_eq!(via, MergeBaseVia::MergePoint),
        other => panic!("{other:?}"),
    }
}

/// A search over its budget answers through the ancestry walk, says so,
/// and a merge from there meets the conflict the merge point avoids. No
/// row or journal entry is written for it.
fn capped<M: MetaStore>(f: &Fixture<M>) {
    let (r, m0) = start(f);
    let (_, d2) = two_merges_apart(&r);
    let found = base(&r, d2, tip(&r, "main"));
    assert_eq!(found.via, MergeBaseVia::Ancestry);
    assert_eq!(found.base, Some(m0));
    let entries = r.journal().len();
    let main = tip(&r, "main");
    let got = merge(&r, "develop", "main");
    match got {
        Err(ApiError::MergeConflict(m)) => {
            assert_eq!(m.paths.len(), 1);
            assert_eq!((m.onto, m.base, m.source), (main, m0, d2));
        }
        other => panic!("expected a merge conflict: {other:?}"),
    }
    assert_eq!(points(&r, "main").len(), 1);
    assert!(r.journal()[entries..].iter().all(|e| e.kind != LINE_MERGED));
}

/// Too few hops to reach release/1.0 through develop: ancestry.
fn shallow<M: MetaStore>(f: &Fixture<M>) {
    let (r, m0) = start(f);
    let (_, r11) = gitflow(&r);
    let found = base(&r, r11, tip(&r, "main"));
    assert_eq!(found.via, MergeBaseVia::Ancestry);
    assert_eq!(found.base, Some(m0));
}

fn reads(n: u32) -> EngineConfig {
    EngineConfig {
        merge_reads: n,
        ..EngineConfig::default()
    }
}

fn hops(n: u32) -> EngineConfig {
    EngineConfig {
        merge_hops: n,
        ..EngineConfig::default()
    }
}

#[test]
fn a_capped_read_budget_falls_back_to_ancestry_on_memory() {
    capped(&memory_with(reads(1)));
}

#[test]
fn a_capped_read_budget_falls_back_to_ancestry_on_sqlite() {
    capped(&sqlite_with(reads(1)));
}

#[test]
fn a_capped_hop_budget_falls_back_to_ancestry_on_memory() {
    shallow(&memory_with(hops(1)));
}

#[test]
fn a_capped_hop_budget_falls_back_to_ancestry_on_sqlite() {
    shallow(&sqlite_with(hops(1)));
}
