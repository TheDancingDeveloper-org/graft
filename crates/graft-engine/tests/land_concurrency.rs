//! G3.1 acceptance under real concurrency: threads racing `Land` against
//! the SQLite store. Every assertion holds for any interleaving, so the
//! tests are deterministic in what they check, whatever the scheduler does.

mod common;

use std::collections::BTreeSet;
use std::sync::Barrier;
use std::thread;

use common::*;
use graft_engine::EngineConfig;
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi};
use graft_store_sqlite::SqliteMetaStore;

const RACERS: usize = 8;

fn land_req(head: ChangesetId) -> Land {
    Land {
        repo: "r".to_owned(),
        head,
        line: "trunk".to_owned(),
        squash: false,
        merge: MergeMode::File,
        deadline_ms: None,
    }
}

/// Lands every head at once, one thread each, released together.
fn race(
    f: &Fixture<SqliteMetaStore>,
    r: &Repo<'_, SqliteMetaStore>,
    heads: &[ChangesetId],
) -> Vec<ApiResult<Landed>> {
    let barrier = Barrier::new(heads.len());
    thread::scope(|s| {
        let handles: Vec<_> = heads
            .iter()
            .map(|&head| {
                let (barrier, cx) = (&barrier, r.cx.clone());
                s.spawn(move || {
                    barrier.wait();
                    block_on(f.engine.land(&cx, land_req(head)))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("racer"))
            .collect()
    })
}

/// A repository with line `trunk` at a root holding `a`.
fn setup(f: &Fixture<SqliteMetaStore>) -> (Repo<'_, SqliteMetaStore>, ChangesetId) {
    let r = Repo::new(f, "r");
    let root = r.commit(vec![], &[("a", b"line one\nline two\n")], &[]).id;
    r.update_ref("trunk", None, root, RefKind::Line)
        .expect("line");
    (r, root)
}

fn tip(r: &Repo<'_, SqliteMetaStore>) -> RefView {
    block_on(r.e().resolve_ref(
        &r.cx,
        ResolveRef {
            repo: "r".to_owned(),
            name: "trunk".to_owned(),
        },
    ))
    .expect("resolve")
}

/// 8 concurrent lands on disjoint files all land, and the line ends with
/// all of them, each at its own position.
#[test]
fn disjoint_racers_all_land() {
    let f = sqlite_with(EngineConfig::default());
    let (r, root) = setup(&f);
    let heads: Vec<ChangesetId> = (0..RACERS)
        .map(|i| {
            let path = format!("f{i}");
            let data = format!("racer {i}\n");
            r.commit(vec![root], &[(&path, data.as_bytes())], &[]).id
        })
        .collect();
    let results = race(&f, &r, &heads);

    let mut positions = BTreeSet::new();
    for result in &results {
        match result {
            Ok(Landed::Landed { pos, .. }) => assert!(positions.insert(*pos)),
            other => panic!("every racer lands, got {other:?}"),
        }
    }
    assert_eq!(positions, (1..=RACERS as u64).collect());
    let line = tip(&r);
    assert_eq!(line.version, 1 + RACERS as u64);
    let files = r.fetch(r.changeset(line.target).changeset.root);
    assert_eq!(files.len(), 1 + RACERS);
    for i in 0..RACERS {
        assert_eq!(files[&format!("f{i}")], format!("racer {i}\n").into_bytes());
    }
    // The last one to land holds the tip; every result is on the line.
    for result in &results {
        let Ok(Landed::Landed { landed, .. }) = result else {
            unreachable!()
        };
        assert!(
            block_on(r.e().is_ancestor(
                &r.cx,
                Pair {
                    repo: "r".to_owned(),
                    a: *landed,
                    b: line.target,
                },
            ))
            .expect("ancestry")
        );
    }
}

/// Concurrent lands on the same lines of one file: exactly one lands, and
/// every other gets a file-level `Conflict` (none is left `Contended`).
#[test]
fn same_lines_racers_conflict_but_one() {
    let f = sqlite_with(EngineConfig::default());
    let (r, root) = setup(&f);
    let heads: Vec<ChangesetId> = (0..RACERS)
        .map(|i| {
            let data = format!("line one, by racer {i}\nline two\n");
            r.commit(vec![root], &[("a", data.as_bytes())], &[]).id
        })
        .collect();
    let results = race(&f, &r, &heads);

    let winners: Vec<ChangesetId> = results
        .iter()
        .filter_map(|r| match r {
            Ok(Landed::Landed { landed, .. }) => Some(*landed),
            _ => None,
        })
        .collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    let winner = winners[0];
    assert!(heads.contains(&winner), "a fast-forward needs no rebase");
    for result in &results {
        match result {
            Ok(_) => {}
            Err(ApiError::Conflict { paths, onto }) => {
                assert_eq!(*onto, winner);
                let paths: Vec<&str> = paths.iter().map(|p| p.path.as_str()).collect();
                assert_eq!(paths, ["a"]);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
    let line = tip(&r);
    assert_eq!((line.target, line.version), (winner, 2));
}

/// Racing retries of one land agree: every caller gets the same `Landed`,
/// and the line moved once.
#[test]
fn racing_retries_of_one_land_agree() {
    let f = sqlite_with(EngineConfig::default());
    let (r, root) = setup(&f);
    // The line moves first, so every retry rebases onto the same tip.
    let first = r.commit(vec![root], &[("b", b"b\n")], &[]).id;
    let head = r.commit(vec![root], &[("c", b"c\n")], &[]).id;
    block_on(r.e().land(&r.cx, land_req(first))).expect("first");
    let results = race(&f, &r, &[head; 4]);
    for result in &results {
        assert_eq!(result, &results[0]);
    }
    let Ok(Landed::Landed {
        landed, rebased, ..
    }) = &results[0]
    else {
        panic!("landed: {results:?}");
    };
    assert!(*rebased);
    let line = tip(&r);
    assert_eq!((line.target, line.version), (*landed, 3));
}
