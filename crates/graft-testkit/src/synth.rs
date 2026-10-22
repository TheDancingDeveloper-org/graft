//! Seeded synthetic repositories (work item G1.2).
//!
//! The generator is deterministic: the same spec gives the same snapshots on
//! every machine, so a failing property can be replayed from its seed.

use std::collections::BTreeMap;

/// Shape of a synthetic repository.
#[derive(Clone, Debug)]
pub struct SynthSpec {
    /// Files in the final snapshot.
    pub files: usize,
    /// Maximum directory depth.
    pub depth: usize,
    /// Changesets of history.
    pub history: usize,
    /// Average file size, bytes.
    pub file_bytes: usize,
    /// Seed.
    pub seed: u64,
}

/// A generated repository: snapshots as path → bytes, oldest first.
#[derive(Clone, Debug, Default)]
pub struct SynthRepo {
    /// Snapshots.
    pub snapshots: Vec<BTreeMap<String, Vec<u8>>>,
}

/// SplitMix64: a small seeded generator, so the crate needs no `rand`.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`. `n` must be non-zero.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }

    /// A size around `average`: from half to one and a half times it.
    fn size(&mut self, average: usize) -> usize {
        if average == 0 {
            return 0;
        }
        average / 2 + self.below(average + 1)
    }
}

/// A path with up to `depth` directories. Directory names come from a small
/// pool, so files share directories as they do in real trees. File names
/// carry a counter, so every path is unique.
fn new_path(rng: &mut Rng, depth: usize, counter: usize) -> String {
    let dirs = rng.below(depth + 1);
    let mut parts: Vec<String> = (0..dirs).map(|_| format!("d{}", rng.below(8))).collect();
    parts.push(format!("f{counter}.dat"));
    parts.join("/")
}

/// Generates a repository deterministically from `spec`.
///
/// There is one snapshot per changeset, so `history` snapshots (at least one)
/// with the last one holding exactly `spec.files` files. Each step adds files
/// to reach its share of the final count, and rewrites about one file in
/// eight. Paths never exceed `spec.depth` directories.
pub fn generate(spec: &SynthSpec) -> SynthRepo {
    let mut rng = Rng(spec.seed);
    let steps = spec.history.max(1);
    let mut current: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut snapshots = Vec::with_capacity(steps);
    let mut counter = 0;
    for step in 1..=steps {
        let target = spec.files * step / steps;
        for path in current.keys().cloned().collect::<Vec<_>>() {
            if rng.below(8) == 0 {
                let len = rng.size(spec.file_bytes);
                current.insert(path, rng.bytes(len));
            }
        }
        while current.len() < target {
            let path = new_path(&mut rng, spec.depth, counter);
            counter += 1;
            let len = rng.size(spec.file_bytes);
            current.insert(path, rng.bytes(len));
        }
        snapshots.push(current.clone());
    }
    SynthRepo { snapshots }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(seed: u64) -> SynthSpec {
        SynthSpec {
            files: 40,
            depth: 3,
            history: 5,
            file_bytes: 64,
            seed,
        }
    }

    #[test]
    fn same_seed_gives_the_same_repository() {
        assert_eq!(generate(&spec(7)).snapshots, generate(&spec(7)).snapshots);
    }

    #[test]
    fn another_seed_gives_another_repository() {
        assert_ne!(generate(&spec(7)).snapshots, generate(&spec(8)).snapshots);
    }

    #[test]
    fn shape_matches_the_spec() {
        let repo = generate(&spec(1));
        assert_eq!(repo.snapshots.len(), 5, "one snapshot per changeset");
        let counts: Vec<usize> = repo.snapshots.iter().map(BTreeMap::len).collect();
        assert!(
            counts.windows(2).all(|w| w[0] <= w[1]),
            "history only grows"
        );
        assert_eq!(
            counts.last(),
            Some(&40),
            "the final snapshot has every file"
        );
    }

    #[test]
    fn paths_are_valid_and_bounded_by_depth() {
        for path in generate(&spec(3))
            .snapshots
            .last()
            .expect("snapshot")
            .keys()
        {
            let parts: Vec<&str> = path.split('/').collect();
            assert!(parts.len() <= 3 + 1, "{path} is deeper than the spec");
            assert!(
                parts
                    .iter()
                    .all(|p| !p.is_empty() && *p != "." && *p != ".."),
                "{path} has an invalid component"
            );
        }
    }

    #[test]
    fn zero_history_still_gives_one_snapshot() {
        let repo = generate(&SynthSpec {
            history: 0,
            ..spec(2)
        });
        assert_eq!(repo.snapshots.len(), 1);
        assert_eq!(repo.snapshots[0].len(), 40);
    }
}
