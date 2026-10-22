//! Generation and skip lists against a brute-force model on random DAGs,
//! and the read cost of eager and lazy derivation.

mod common;

use common::{Fixture, block_on, changeset};
use graft_index::kinds::{Generation, SkipList, Skips};
use graft_index::{
    DerivedKind, Mode, ParentDerived, ParentScope, ValueKind, changeset_key, needed_parents,
};
use graft_model::{Changeset, ChangesetId};
use proptest::prelude::*;

/// One node of a random DAG: a root, or parents chosen among earlier nodes.
#[derive(Clone, Debug)]
struct Node {
    parents: Vec<usize>,
    /// Whether the node is created with its eager rows, or only stored.
    eager: bool,
}

fn dag() -> impl Strategy<Value = Vec<Node>> {
    prop::collection::vec(
        (
            prop::bool::weighted(0.1),
            prop::collection::vec(any::<usize>(), 1..4),
            any::<bool>(),
        ),
        1..60,
    )
    .prop_map(|raw| {
        raw.into_iter()
            .enumerate()
            .map(|(i, (root, picks, eager))| {
                let mut parents = Vec::new();
                if i > 0 && !root {
                    for p in picks {
                        let p = p % i;
                        if !parents.contains(&p) {
                            parents.push(p);
                        }
                    }
                }
                Node { parents, eager }
            })
            .collect()
    })
}

/// The changesets of a DAG, in order.
fn build(nodes: &[Node]) -> Vec<Changeset> {
    let mut out: Vec<Changeset> = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        let parents = n.parents.iter().map(|&p| out[p].id()).collect();
        out.push(changeset(parents, i as u64, Vec::new()));
    }
    out
}

fn brute_generation(nodes: &[Node]) -> Vec<u64> {
    let mut g: Vec<u64> = Vec::new();
    for n in nodes {
        g.push(1 + n.parents.iter().map(|&p| g[p]).max().unwrap_or(0));
    }
    g
}

fn brute_skips(nodes: &[Node], ids: &[ChangesetId], i: usize) -> Skips {
    let mut chain = Vec::new();
    let mut cur = i;
    while let Some(&p) = nodes[cur].parents.first() {
        chain.push(p);
        cur = p;
    }
    let ancestors = (0..)
        .map(|k| 1usize << k)
        .take_while(|&d| d <= chain.len())
        .map(|d| ids[chain[d - 1]])
        .collect();
    Skips {
        depth: chain.len() as u64,
        ancestors,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Every node, whether created eagerly or derived lazily later, gets
    /// the brute-force generation and skip list.
    #[test]
    fn generation_and_skips_match_brute_force(nodes in dag()) {
        let fx = Fixture::new();
        let css = build(&nodes);
        let ids: Vec<_> = css.iter().map(Changeset::id).collect();
        for (n, cs) in nodes.iter().zip(&css) {
            if n.eager {
                fx.create(cs);
            } else {
                fx.store_only(cs);
            }
        }
        let gens = brute_generation(&nodes);
        for (i, &id) in ids.iter().enumerate() {
            let g = block_on(fx.index.derive::<Generation>(fx.repo, id)).unwrap();
            prop_assert_eq!(g.value, gens[i]);
            let s = block_on(fx.index.skip_list(fx.repo, id)).unwrap();
            prop_assert_eq!(&s.value, &brute_skips(&nodes, &ids, i));
            // Caching what a walk computed changes no value.
            fx.commit(g.rows.into_iter().chain(s.rows).collect());
            let again = block_on(fx.index.get::<Generation>(fx.repo, &changeset_key(id))).unwrap();
            prop_assert_eq!(again, Some(gens[i]));
        }
    }

    /// Eager creation over a fully eager DAG reads no changeset and writes
    /// exactly the new changeset's two rows.
    #[test]
    fn eager_creation_reads_only_parent_values(nodes in dag()) {
        let fx = Fixture::new();
        for cs in build(&nodes) {
            fx.reads.reset();
            let rows = block_on(fx.index.on_create(fx.repo, &cs)).unwrap();
            prop_assert_eq!(fx.reads.changeset(), 0);
            prop_assert_eq!(rows.len(), 2);
            fx.create(&cs);
        }
    }
}

/// A chain of `n` changesets on top of `base`, oldest first.
fn chain(base: Option<ChangesetId>, from_tag: u64, n: usize) -> Vec<Changeset> {
    let mut out: Vec<Changeset> = Vec::new();
    for i in 0..n {
        let parents = out.last().map(Changeset::id).or(base).into_iter().collect();
        out.push(changeset(parents, from_tag + i as u64, Vec::new()));
    }
    out
}

#[test]
fn lazy_generation_walks_only_to_the_nearest_computed_value() {
    let fx = Fixture::new();
    let old = chain(None, 0, 50);
    for cs in &old {
        fx.create(cs);
    }
    let new = chain(old.last().map(Changeset::id), 1000, 10);
    for cs in &new {
        fx.store_only(cs);
    }
    let tip = new.last().unwrap().id();

    fx.reads.reset();
    let g = block_on(fx.index.derive::<Generation>(fx.repo, tip)).unwrap();
    assert_eq!(g.value, 60);
    // Ten misses and the hit at the nearest computed value; one content
    // read per computed changeset.
    assert_eq!(fx.reads.derived(), 11);
    assert_eq!(fx.reads.changeset(), 10);
    assert_eq!(g.rows.len(), 10);

    // Once cached, a read is one lookup.
    fx.commit(g.rows);
    fx.reads.reset();
    let g = block_on(fx.index.derive::<Generation>(fx.repo, tip)).unwrap();
    assert_eq!((g.value, g.rows.len()), (60, 0));
    assert_eq!((fx.reads.derived(), fx.reads.changeset()), (1, 0));
}

#[test]
fn lazy_skip_list_walks_only_to_the_nearest_computed_value() {
    let fx = Fixture::new();
    let old = chain(None, 0, 500);
    for cs in &old {
        fx.create(cs);
    }
    let new = chain(old.last().map(Changeset::id), 1000, 10);
    for cs in &new {
        fx.store_only(cs);
    }
    let tip = new.last().unwrap().id();

    fx.reads.reset();
    let s = block_on(fx.index.skip_list(fx.repo, tip)).unwrap();
    assert_eq!(s.value.depth, 509);
    assert_eq!(s.value.ancestors.len(), 9); // distances 1..=256
    assert_eq!(fx.reads.changeset(), 10);
    assert_eq!(s.rows.len(), 10);
    // Ten misses, the hit, and at most one list per skip level per
    // computed changeset (older ancestors' lists, each read once).
    assert!(fx.reads.derived() <= 11 + 10 * 9, "{}", fx.reads.derived());
}

/// A lazy test kind over all parents, `1 + sum(parents)`, to exercise the
/// walk through merges.
struct Weight;

impl DerivedKind for Weight {
    const NAME: &'static str = "test-weight";
    const VERSION: u32 = 1;
    const PARENTS: ParentScope = ParentScope::All;
    const MODE: Mode = Mode::Lazy;
}

impl ValueKind for Weight {
    type Value = u64;
    fn encode(v: &u64) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }
    fn decode(b: &[u8]) -> Option<u64> {
        Some(u64::from_be_bytes(b.try_into().ok()?))
    }
}

impl ParentDerived for Weight {
    fn derive(_cs: &Changeset, parents: &[u64]) -> u64 {
        parents.iter().sum::<u64>() + 1
    }
}

#[test]
fn lazy_walk_through_a_merge_stops_at_computed_values_on_each_side() {
    // root - a1 - a2 - a3 ----- m
    //          \               /
    //           b1 - b2 - b3 -
    let fx = Fixture::new();
    let root = changeset(Vec::new(), 0, Vec::new());
    let a1 = changeset(vec![root.id()], 1, Vec::new());
    let a2 = changeset(vec![a1.id()], 2, Vec::new());
    let a3 = changeset(vec![a2.id()], 3, Vec::new());
    let b1 = changeset(vec![a1.id()], 11, Vec::new());
    let b2 = changeset(vec![b1.id()], 12, Vec::new());
    let b3 = changeset(vec![b2.id()], 13, Vec::new());
    let m = changeset(vec![a3.id(), b3.id()], 20, Vec::new());
    for cs in [&root, &a1, &a2, &a3, &b1, &b2, &b3, &m] {
        fx.store_only(cs);
    }
    // Cache a2 and b2: the walk from m computes only m, a3 and b3.
    let a2_value = block_on(fx.index.derive::<Weight>(fx.repo, a2.id())).unwrap();
    let b2_value = block_on(fx.index.derive::<Weight>(fx.repo, b2.id())).unwrap();
    fx.commit(a2_value.rows.into_iter().chain(b2_value.rows).collect());

    fx.reads.reset();
    let w = block_on(fx.index.derive::<Weight>(fx.repo, m.id())).unwrap();
    // root 1, a1 2, a2 3, a3 4, b1 3, b2 4, b3 5, m 4 + 5 + 1.
    assert_eq!(w.value, 10);
    assert_eq!(fx.reads.changeset(), 3);
    assert_eq!(fx.reads.derived(), 5); // m, a3, b3 miss; a2, b2 hit
    assert_eq!(w.rows.len(), 3);
    assert_eq!(needed_parents(Weight::PARENTS, &m).len(), 2);
}

#[test]
fn eager_rows_use_the_kinds_name_version_and_key() {
    let fx = Fixture::new();
    let root = changeset(Vec::new(), 0, Vec::new());
    let rows = block_on(fx.index.on_create(fx.repo, &root)).unwrap();
    let want_gen = graft_index::row::<Generation>(changeset_key(root.id()), &1);
    let want_skips = graft_index::row::<SkipList>(
        changeset_key(root.id()),
        &Skips {
            depth: 0,
            ancestors: Vec::new(),
        },
    );
    assert_eq!(rows, vec![want_gen, want_skips]);
}

#[test]
fn creation_on_a_long_chain_reads_log_depth_values() {
    let fx = Fixture::new();
    let old = chain(None, 0, 1024);
    for cs in &old {
        fx.create(cs);
    }
    let next = changeset(vec![old.last().unwrap().id()], 5000, Vec::new());
    fx.reads.reset();
    block_on(fx.index.on_create(fx.repo, &next)).unwrap();
    assert_eq!(fx.reads.changeset(), 0);
    // One generation, the parent's skip list, and one list per level.
    assert!(fx.reads.derived() <= 2 + 11, "{}", fx.reads.derived());
}
