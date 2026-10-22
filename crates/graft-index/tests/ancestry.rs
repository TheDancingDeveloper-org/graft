//! `IsAncestor` and `MergeBase` against brute force on random DAGs.

mod common;

use std::collections::{HashMap, HashSet};

use common::{Fixture, block_on, changeset, first_parent_chain};
use graft_model::{Changeset, ChangesetId};
use proptest::prelude::*;
use proptest::sample::Index as Pick;

/// A random DAG: node `i` has no parents (a root) or up to three distinct
/// earlier nodes. Node 0 is always a root.
fn dag() -> impl Strategy<Value = Vec<Vec<usize>>> {
    prop::collection::vec((0u8..8, prop::collection::vec(any::<Pick>(), 1..4)), 1..40).prop_map(
        |nodes| {
            nodes
                .into_iter()
                .enumerate()
                .map(|(i, (root, picks))| {
                    if i == 0 || root == 0 {
                        return Vec::new();
                    }
                    let mut parents = Vec::new();
                    for pick in picks {
                        let p = pick.index(i);
                        if !parents.contains(&p) {
                            parents.push(p);
                        }
                    }
                    parents
                })
                .collect()
        },
    )
}

struct Built {
    fx: Fixture,
    ids: Vec<ChangesetId>,
    /// Every node's ancestors, itself included.
    ancestors: Vec<HashSet<usize>>,
}

/// Creates the DAG, and lands first-parent chains on up to two lines.
fn build(shape: &[Vec<usize>], lines: u8) -> Built {
    let fx = Fixture::new();
    let mut ids: Vec<ChangesetId> = Vec::new();
    let mut css = HashMap::new();
    let mut ancestors: Vec<HashSet<usize>> = Vec::new();
    for (i, parents) in shape.iter().enumerate() {
        let cs = changeset(parents.iter().map(|&p| ids[p]).collect(), i as u64, vec![]);
        let id = fx.create(&cs);
        css.insert(id, cs);
        ids.push(id);
        let mut anc = HashSet::from([i]);
        for &p in parents {
            anc.extend(&ancestors[p]);
        }
        ancestors.push(anc);
    }
    let n = ids.len();
    for (line, tip) in [("main", n - 1), ("a-release", n / 2)]
        .into_iter()
        .take(lines.into())
    {
        let chain: Vec<Changeset> = first_parent_chain(&css, ids[tip]);
        fx.land(line, &chain);
        fx.set_line(line, ids[tip]);
    }
    Built { fx, ids, ancestors }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn is_ancestor_and_merge_base_match_brute_force(shape in dag(), lines in 0u8..3) {
        let Built { fx, ids, ancestors } = build(&shape, lines);
        let n = ids.len();
        let index_of: HashMap<ChangesetId, usize> =
            ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        for a in 0..n {
            for b in 0..n {
                let got = block_on(fx.index.is_ancestor(fx.repo, ids[a], ids[b])).unwrap();
                prop_assert_eq!(got, ancestors[b].contains(&a), "is_ancestor({}, {})", a, b);

                let common: HashSet<usize> =
                    ancestors[a].intersection(&ancestors[b]).copied().collect();
                // Best: common ancestors that are not a strict ancestor of
                // another common ancestor.
                let best: HashSet<usize> = common
                    .iter()
                    .copied()
                    .filter(|&c| !common.iter().any(|&d| d != c && ancestors[d].contains(&c)))
                    .collect();
                let got = block_on(fx.index.merge_base(fx.repo, ids[a], ids[b])).unwrap();
                match got {
                    None => prop_assert!(best.is_empty(), "merge_base({}, {}) missed {:?}", a, b, best),
                    Some(id) => prop_assert!(
                        best.contains(&index_of[&id]),
                        "merge_base({}, {}) = {} not in {:?}", a, b, index_of[&id], best
                    ),
                }
            }
        }
    }
}

/// On a line, ancestry is a position comparison: no changeset is read.
#[test]
fn line_positions_answer_without_walking() {
    let shape: Vec<Vec<usize>> = (0..200)
        .map(|i| if i == 0 { vec![] } else { vec![i - 1] })
        .collect();
    let Built { fx, ids, .. } = build(&shape, 1);
    fx.reads.reset();
    assert!(block_on(fx.index.is_ancestor(fx.repo, ids[3], ids[190])).unwrap());
    assert!(!block_on(fx.index.is_ancestor(fx.repo, ids[190], ids[3])).unwrap());
    assert_eq!(
        block_on(fx.index.merge_base(fx.repo, ids[190], ids[3])).unwrap(),
        Some(ids[3])
    );
    assert_eq!(fx.reads.changeset(), 0);
}

/// Off the line, the walk stops at generation and at the line: a draft
/// stack of three on a long line reads about three changesets.
#[test]
fn drafts_walk_only_to_the_line() {
    let mut shape: Vec<Vec<usize>> = (0..500)
        .map(|i| if i == 0 { vec![] } else { vec![i - 1] })
        .collect();
    // A draft stack on 400 (500..503), and a side stack on 450 (503..505).
    shape.extend([vec![400], vec![500], vec![501], vec![450], vec![503]]);
    let fx = Fixture::new();
    let mut ids: Vec<ChangesetId> = Vec::new();
    let mut css = HashMap::new();
    for (i, parents) in shape.iter().enumerate() {
        let cs = changeset(parents.iter().map(|&p| ids[p]).collect(), i as u64, vec![]);
        ids.push(fx.create(&cs));
        css.insert(cs.id(), cs);
    }
    fx.land("main", &first_parent_chain(&css, ids[499]));
    fx.set_line("main", ids[499]);

    fx.reads.reset();
    assert!(block_on(fx.index.is_ancestor(fx.repo, ids[10], ids[502])).unwrap());
    assert!(fx.reads.changeset() <= 3, "{} reads", fx.reads.changeset());
    fx.reads.reset();
    assert!(!block_on(fx.index.is_ancestor(fx.repo, ids[420], ids[502])).unwrap());
    assert!(fx.reads.changeset() <= 3, "{} reads", fx.reads.changeset());
    fx.reads.reset();
    assert_eq!(
        block_on(fx.index.merge_base(fx.repo, ids[502], ids[504])).unwrap(),
        Some(ids[400])
    );
    // Both stacks, then down the line from 450 to 400.
    assert!(fx.reads.changeset() <= 60, "{} reads", fx.reads.changeset());
}
