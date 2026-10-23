//! `Log` against a brute-force first-parent walk, and its read costs.

mod common;

use std::collections::HashMap;

use common::{Fixture, block_on, changeset, first_parent_chain, path, put};
use graft_index::LogQuery;
use graft_model::{Changeset, ChangesetId, RepoPath};
use graft_store::MetaStore;
use proptest::prelude::*;
use proptest::sample::Index as Pick;

fn file_path() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!["a", "b", "c"]), 1..4)
        .prop_map(|parts| parts.join("/") + ".f")
}

fn filter_path() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!["a", "b", "c"]), 1..3).prop_map(|parts| {
        let p = parts.join("/");
        // Half the filters are directories, half files.
        if parts.len() % 2 == 0 { p + ".f" } else { p }
    })
}

/// The brute-force answer: walk first parents from `from`, stop at `until`
/// (and, once on the line, at or below its position there), keep the
/// changesets touching a filter.
fn brute(
    css: &HashMap<ChangesetId, Changeset>,
    pos: &HashMap<ChangesetId, u64>,
    from: ChangesetId,
    until: Option<ChangesetId>,
    paths: &[RepoPath],
) -> Vec<ChangesetId> {
    let floor = until.and_then(|u| pos.get(&u).copied());
    let mut out = Vec::new();
    let mut cur = Some(from);
    while let Some(id) = cur {
        if Some(id) == until {
            break;
        }
        if let (Some(p), Some(f)) = (pos.get(&id), floor)
            && *p <= f
        {
            break;
        }
        let cs = &css[&id];
        if paths.is_empty()
            || cs
                .changes
                .iter()
                .any(|c| paths.iter().any(|p| c.path().starts_with(p)))
        {
            out.push(id);
        }
        cur = cs.first_parent();
    }
    out
}

/// Every page of a query, paging by line position when the last result is
/// on the line and by first parent otherwise (or always by first parent).
fn all_pages(fx: &Fixture, q: &LogQuery, by_parent_only: bool) -> Vec<ChangesetId> {
    let mut q = q.clone();
    let mut out = Vec::new();
    loop {
        let page = block_on(fx.index.log(fx.repo, q.clone())).unwrap();
        assert!(page.len() <= q.limit as usize);
        let Some(&last) = page.last() else { break };
        out.extend(&page);
        if page.len() < q.limit as usize {
            break;
        }
        let pos = block_on(fx.index.meta().line_pos(fx.repo, "main", last)).unwrap();
        match pos {
            Some(pos) if !by_parent_only => q.after_pos = Some(pos),
            _ => {
                let (content, _) = block_on(fx.index.meta().changeset(fx.repo, last))
                    .unwrap()
                    .unwrap();
                match Changeset::decode(&content).unwrap().first_parent() {
                    Some(p) => {
                        q.from = p;
                        q.after_pos = None;
                    }
                    None => break,
                }
            }
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn log_matches_a_first_parent_walk(
        nodes in prop::collection::vec(
            (0u8..6, any::<Pick>(), any::<Pick>(), prop::collection::vec(file_path(), 0..4)),
            1..40,
        ),
        queries in prop::collection::vec(
            (any::<Pick>(), any::<Option<Pick>>(), prop::collection::vec(filter_path(), 0..3), 1u32..6),
            1..8,
        ),
    ) {
        let fx = Fixture::new();
        let mut ids: Vec<ChangesetId> = Vec::new();
        let mut css = HashMap::new();
        for (i, (kind, first, second, files)) in nodes.iter().enumerate() {
            // A root, a child of the previous node, or a merge.
            let parents = match (i, kind) {
                (0, _) | (_, 0) => vec![],
                (_, 1) => vec![ids[first.index(i)], ids[second.index(i)]],
                _ => vec![ids[first.index(i)]],
            };
            let mut parents_dedup = Vec::new();
            for p in parents {
                if !parents_dedup.contains(&p) {
                    parents_dedup.push(p);
                }
            }
            let cs = changeset(parents_dedup, i as u64, files.iter().map(|f| put(f)).collect());
            ids.push(fx.create(&cs));
            css.insert(cs.id(), cs);
        }
        // The line: the first-parent chain of a node in the middle, so
        // later nodes on it are drafts.
        let tip = ids[ids.len() / 2];
        let chain = first_parent_chain(&css, tip);
        fx.land("main", &chain);
        fx.set_line("main", tip);
        let pos: HashMap<ChangesetId, u64> =
            chain.iter().enumerate().map(|(i, cs)| (cs.id(), i as u64)).collect();

        for (from, until, filters, limit) in &queries {
            let paths: Vec<RepoPath> = filters.iter().map(|p| path(p)).collect();
            let q = LogQuery {
                from: ids[from.index(ids.len())],
                until: until.map(|u| ids[u.index(ids.len())]),
                paths: paths.clone(),
                limit: *limit,
                after_pos: None,
            };
            let want = brute(&css, &pos, q.from, q.until, &paths);
            prop_assert_eq!(&all_pages(&fx, &q, false), &want);
            prop_assert_eq!(&all_pages(&fx, &q, true), &want);
        }
    }
}

/// A line of `n` changesets: every `every`th touches `x/y.f` (and the
/// first), the rest touch `z.f`.
fn long_line(n: u64, every: u64) -> (Fixture, Vec<ChangesetId>) {
    let fx = Fixture::new();
    let mut chain: Vec<Changeset> = Vec::new();
    for i in 0..n {
        let parents = chain.last().map(Changeset::id).into_iter().collect();
        let file = if i % every == 0 { "x/y.f" } else { "z.f" };
        let cs = changeset(parents, i, vec![put(file)]);
        fx.create(&cs);
        chain.push(cs);
    }
    fx.land("main", &chain);
    let ids: Vec<ChangesetId> = chain.iter().map(Changeset::id).collect();
    fx.set_line("main", *ids.last().unwrap());
    (fx, ids)
}

fn query(from: ChangesetId, paths: &[&str], limit: u32) -> LogQuery {
    LogQuery {
        from,
        until: None,
        paths: paths.iter().map(|p| path(p)).collect(),
        limit,
        after_pos: None,
    }
}

/// A path `Log` reads O(results) index rows and no changesets, however
/// long the line.
#[test]
fn path_log_reads_rows_in_proportion_to_results() {
    let (fx, ids) = long_line(3000, 150);
    let tip = *ids.last().unwrap();
    for (filter, results) in [("x/y.f", 20), ("x", 20)] {
        fx.reads.reset();
        let got = block_on(fx.index.log(fx.repo, query(tip, &[filter], 1000))).unwrap();
        assert_eq!(got.len(), results);
        assert_eq!(got[0], ids[2850]);
        assert_eq!(*got.last().unwrap(), ids[0]);
        assert!(
            fx.reads.scan_rows() <= results,
            "{} rows",
            fx.reads.scan_rows()
        );
        assert_eq!(fx.reads.changeset(), 0);
        assert!(fx.reads.line_pos() <= 2);
    }

    // A page of 5 reads at most 5 rows, wherever it starts.
    fx.reads.reset();
    let q = LogQuery {
        after_pos: Some(1500),
        ..query(tip, &["x"], 5)
    };
    let got = block_on(fx.index.log(fx.repo, q)).unwrap();
    assert_eq!(
        got,
        vec![ids[1350], ids[1200], ids[1050], ids[900], ids[750]]
    );
    assert!(fx.reads.scan_rows() <= 5);
    assert_eq!(fx.reads.changeset(), 0);
}

/// Without paths, a page starting deep in the line jumps by skip list and
/// then reads one changeset per result.
#[test]
fn unfiltered_log_jumps_then_walks() {
    let (fx, ids) = long_line(4000, 1000);
    let tip = *ids.last().unwrap();
    fx.reads.reset();
    let q = LogQuery {
        after_pos: Some(1000),
        ..query(tip, &[], 10)
    };
    let got = block_on(fx.index.log(fx.repo, q)).unwrap();
    assert_eq!(
        got,
        ids[990..1000].iter().rev().copied().collect::<Vec<_>>()
    );
    assert!(
        fx.reads.changeset() <= 10,
        "{} changesets",
        fx.reads.changeset()
    );
    // log2(3000) skip lists.
    assert!(fx.reads.derived() <= 13, "{} derived", fx.reads.derived());
}

/// A plain page returns its changesets decoded, each read once: one
/// changeset read per result, however long the line (WI-2303).
#[test]
fn log_changesets_reads_each_result_once() {
    let (fx, ids) = long_line(4000, 1000);
    let tip = *ids.last().unwrap();
    fx.reads.reset();
    let got = block_on(fx.index.log_changesets(fx.repo, query(tip, &[], 50))).unwrap();
    let want: Vec<_> = ids.iter().rev().take(50).copied().collect();
    assert_eq!(got.iter().map(|(id, _)| *id).collect::<Vec<_>>(), want);
    assert!(got.iter().all(|(id, cs)| cs.id() == *id));
    assert_eq!(fx.reads.changeset(), 50);
    assert_eq!(fx.reads.derived(), 0);

    // With paths, the rows come from the index and each result is read once.
    fx.reads.reset();
    let got = block_on(fx.index.log_changesets(fx.repo, query(tip, &["x"], 20))).unwrap();
    assert!(!got.is_empty());
    assert_eq!(fx.reads.changeset(), got.len());
}

/// A draft stack is filtered by walking it, then the walk switches to the
/// index; `until` cuts both.
#[test]
fn drafts_then_the_line() {
    let (fx, ids) = long_line(100, 10);
    let base = ids[95];
    let d1 = changeset(vec![base], 1000, vec![put("x/new.f")]);
    let d2 = changeset(vec![d1.id()], 1001, vec![put("other.f")]);
    let d3 = changeset(vec![d2.id()], 1002, vec![put("x/y.f")]);
    for d in [&d1, &d2, &d3] {
        fx.create(d);
    }
    fx.reads.reset();
    let got = block_on(fx.index.log(fx.repo, query(d3.id(), &["x"], 4))).unwrap();
    assert_eq!(got, vec![d3.id(), d1.id(), ids[90], ids[80]]);
    // Three drafts read; the line costs rows, not changesets.
    assert_eq!(fx.reads.changeset(), 3);

    let q = LogQuery {
        until: Some(ids[80]),
        ..query(d3.id(), &["x"], 100)
    };
    let got = block_on(fx.index.log(fx.repo, q)).unwrap();
    assert_eq!(got, vec![d3.id(), d1.id(), ids[90]]);

    // `until` above the draft's base on the line: only the drafts remain.
    fx.set_line("main", ids[99]);
    let q = LogQuery {
        until: Some(ids[99]),
        ..query(d3.id(), &[], 100)
    };
    let got = block_on(fx.index.log(fx.repo, q)).unwrap();
    assert_eq!(got, vec![d3.id(), d2.id(), d1.id()]);

    // `until` inside the stack.
    let q = LogQuery {
        until: Some(d1.id()),
        ..query(d3.id(), &[], 100)
    };
    let got = block_on(fx.index.log(fx.repo, q)).unwrap();
    assert_eq!(got, vec![d3.id(), d2.id()]);
}

/// Two filters, one beneath the other, give each changeset once.
#[test]
fn overlapping_filters_do_not_repeat() {
    let (fx, ids) = long_line(30, 10);
    let got = block_on(
        fx.index
            .log(fx.repo, query(ids[29], &["x", "x/y.f", "z.f"], 1000)),
    )
    .unwrap();
    assert_eq!(got, ids.iter().rev().copied().collect::<Vec<_>>());
}
