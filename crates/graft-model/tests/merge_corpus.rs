//! Differential test of `merge3` against `git merge-file` (work item G3.4).
//!
//! Each case is a generated base with two random edits of it. git runs as
//! `git merge-file -p --diff3 --diff-algorithm=histogram -L ours -L base -L
//! theirs`; a clean exit must match `Merge3::Clean` byte for byte, and a
//! conflicted exit must match `Conflicts` (with the same number of regions)
//! and `render_conflict_markers` byte for byte.
//!
//! The corpus also passed with 20 000 cases while it was written.
//!
//! Known, intended differences (the corpus does not generate them):
//!
//! - CRLF files: git writes conflict markers with `\r\n` when the conflicting
//!   lines use it; graft always writes `\n`.
//! - Regions whose every common line repeats more than 64 times: git falls
//!   back to its tuned Myers diff (`xdl_do_diff` with heuristics), graft to a
//!   plain Myers diff, which can pick a different alignment.
//! - git's `xhistogram` also falls back when more than 64 distinct lines
//!   collide in one bucket of its hash table; graft has no such limit.
//!
//! The test is skipped (with a note on stderr) when `git` is not installed.

use std::path::Path;
use std::process::Command;

use graft_model::merge::{Merge3, merge3, render_conflict_markers};

const CASES: usize = 600;

/// A small deterministic generator (splitmix64), so the corpus is stable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A line from a small alphabet (so lines repeat and diffs are ambiguous),
/// or occasionally a unique line.
fn line(rng: &mut Rng, fresh: &mut usize) -> String {
    if rng.below(4) == 0 {
        *fresh += 1;
        format!("unique {fresh}\n")
    } else {
        const WORDS: [&str; 6] = ["{", "}", "a", "b", "", "return x;"];
        format!("{}\n", WORDS[rng.below(WORDS.len())])
    }
}

fn edit(rng: &mut Rng, base: &[String], fresh: &mut usize) -> Vec<String> {
    let mut out = base.to_vec();
    for _ in 0..1 + rng.below(8) {
        let at = rng.below(out.len() + 1);
        match rng.below(3) {
            0 => {
                for _ in 0..1 + rng.below(8) {
                    out.insert(at, line(rng, fresh));
                }
            }
            1 if at < out.len() => {
                let n = (1 + rng.below(3)).min(out.len() - at);
                out.drain(at..at + n);
            }
            _ if at < out.len() => out[at] = line(rng, fresh),
            _ => out.push(line(rng, fresh)),
        }
    }
    out
}

fn join(lines: &[String], final_newline: bool) -> Vec<u8> {
    let mut s = lines.concat();
    if !final_newline && s.ends_with('\n') {
        s.pop();
    }
    s.into_bytes()
}

fn git_merge(dir: &Path, base: &[u8], ours: &[u8], theirs: &[u8]) -> (bool, Vec<u8>) {
    std::fs::write(dir.join("base"), base).unwrap();
    std::fs::write(dir.join("ours"), ours).unwrap();
    std::fs::write(dir.join("theirs"), theirs).unwrap();
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "merge-file",
            "-p",
            "--diff3",
            "--diff-algorithm=histogram",
            "-L",
            "ours",
            "-L",
            "base",
            "-L",
            "theirs",
            "ours",
            "base",
            "theirs",
        ])
        .output()
        .unwrap();
    let code = out.status.code().expect("git merge-file was killed");
    assert!(code >= 0, "git merge-file failed: {out:?}");
    (code == 0, out.stdout)
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn agrees_with_git_merge_file() {
    if !have_git() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("merge_corpus");
    std::fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng(0x6772_6166_7433_2e34);
    let (mut clean, mut conflicted) = (0, 0);
    for case in 0..CASES {
        let mut fresh = 0;
        let len = rng.below(40);
        let base: Vec<String> = (0..len).map(|_| line(&mut rng, &mut fresh)).collect();
        let ours = edit(&mut rng, &base, &mut fresh);
        let theirs = edit(&mut rng, &base, &mut fresh);
        let nl = [rng.below(5) != 0, rng.below(5) != 0, rng.below(5) != 0];
        let (b, o, t) = (join(&base, nl[0]), join(&ours, nl[1]), join(&theirs, nl[2]));

        let (git_clean, git_out) = git_merge(&dir, &b, &o, &t);
        let ctx = || {
            format!(
                "case {case}\n--- base\n{}--- ours\n{}--- theirs\n{}--- git\n{}",
                String::from_utf8_lossy(&b),
                String::from_utf8_lossy(&o),
                String::from_utf8_lossy(&t),
                String::from_utf8_lossy(&git_out),
            )
        };
        match merge3(&b, &o, &t) {
            Merge3::Clean(bytes) => {
                assert!(git_clean, "graft clean, git conflicted\n{}", ctx());
                assert_eq!(
                    String::from_utf8_lossy(&bytes),
                    String::from_utf8_lossy(&git_out),
                    "{}",
                    ctx()
                );
                clean += 1;
            }
            Merge3::Conflicts(regions) => {
                assert!(!git_clean, "graft conflicted, git clean\n{}", ctx());
                assert!(!regions.is_empty());
                let markers = git_out
                    .split_inclusive(|&c| c == b'\n')
                    .filter(|l| l.starts_with(b"<<<<<<< "))
                    .count();
                assert_eq!(regions.len(), markers, "{}", ctx());
                assert_eq!(
                    String::from_utf8_lossy(&render_conflict_markers(&b, &o, &t)),
                    String::from_utf8_lossy(&git_out),
                    "{}",
                    ctx()
                );
                conflicted += 1;
            }
            Merge3::NotText => panic!("generated text is text"),
        }
    }
    // The corpus must exercise both outcomes.
    assert!(
        clean > CASES / 10 && conflicted > CASES / 10,
        "{clean} clean, {conflicted} conflicted"
    );
}
