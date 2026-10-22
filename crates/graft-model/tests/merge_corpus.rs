//! Differential test of `merge3` against `git merge-file` (work item G3.4).
//!
//! Each case is a generated base with two random edits of it. git runs as
//! `git merge-file -p --diff3 --diff-algorithm=histogram -L ours -L base -L
//! theirs`; a clean exit must match `Merge3::Clean` byte for byte, and a
//! conflicted exit must match `Conflicts` (with the same number of regions)
//! and `render_conflict_markers` byte for byte.
//!
//! The required test, `agrees_with_recorded_git`, is hermetic: it compares
//! against git's output recorded in `fixtures/merge3/corpus.txt` (git
//! 2.53.0), because git's placement of conflict regions differs between
//! releases. `agrees_with_git_merge_file` (ignored) runs the installed git
//! live; with `GRAFT_BLESS=1` it rewrites the fixture instead of comparing.
//!
//! The corpus also passed with 20 000 cases while it was written.
//!
//! Known, intended differences (the corpus does not generate them):
//!
//! - CRLF files: git writes conflict markers with `\r\n` when the conflicting
//!   lines use it; graft always writes `\n`.
//! - Regions whose every common line repeats more than 64 times: git falls
//!   back to its tuned Myers diff (`xdl_do_diff` with heuristics), graft to a
//!   minimal linear-space Myers diff, which can pick a different alignment.
//! - Work budget: when git's histogram diff of one side runs over its share
//!   of the budget (many scattered hunks), graft first anchors on lines unique
//!   to both files, which can align differently; and a diff that runs over
//!   the whole budget is `Merge3::TooComplex` where git would still merge.
//!   Neither happens below a million units of work, so not in this corpus.
//! - Older git releases (e.g. the 2.43 on GitHub's Ubuntu runners) can place
//!   a conflict differently: in corpus case 88 they end the first conflict
//!   at `{` and leave the shared `a … return x;` run outside it, where git
//!   2.53 and graft fold that run into the conflict. Both are valid diff3
//!   output; graft follows current git, so the fixture records 2.53.
//! - git's `xhistogram` also falls back when more than 64 distinct lines
//!   collide in one bucket of its hash table; graft has no such limit.
//!

use std::path::{Path, PathBuf};
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

/// One generated case: base, ours, theirs.
fn corpus() -> Vec<[Vec<u8>; 3]> {
    let mut rng = Rng(0x6772_6166_7433_2e34);
    (0..CASES)
        .map(|_| {
            let mut fresh = 0;
            let len = rng.below(40);
            let base: Vec<String> = (0..len).map(|_| line(&mut rng, &mut fresh)).collect();
            let ours = edit(&mut rng, &base, &mut fresh);
            let theirs = edit(&mut rng, &base, &mut fresh);
            let nl = [rng.below(5) != 0, rng.below(5) != 0, rng.below(5) != 0];
            [join(&base, nl[0]), join(&ours, nl[1]), join(&theirs, nl[2])]
        })
        .collect()
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge3/corpus.txt")
}

/// The fixture: per case a header `<case> <clean|conflict> <len>\n`, then
/// `len` bytes of git's output, then `\n`.
fn write_fixture(results: &[(bool, Vec<u8>)]) {
    let mut out = Vec::new();
    for (case, (clean, bytes)) in results.iter().enumerate() {
        let kind = if *clean { "clean" } else { "conflict" };
        out.extend_from_slice(format!("{case} {kind} {}\n", bytes.len()).as_bytes());
        out.extend_from_slice(bytes);
        out.push(b'\n');
    }
    std::fs::create_dir_all(fixture_path().parent().unwrap()).unwrap();
    std::fs::write(fixture_path(), out).unwrap();
}

fn read_fixture() -> Vec<(bool, Vec<u8>)> {
    let data = std::fs::read(fixture_path()).expect("read the merge3 fixture");
    let mut rest = &data[..];
    let mut results = Vec::new();
    while !rest.is_empty() {
        let nl = rest.iter().position(|&c| c == b'\n').unwrap();
        let header = std::str::from_utf8(&rest[..nl]).unwrap();
        let mut parts = header.split(' ');
        let case: usize = parts.next().unwrap().parse().unwrap();
        assert_eq!(case, results.len(), "fixture cases are in order");
        let clean = match parts.next().unwrap() {
            "clean" => true,
            "conflict" => false,
            other => panic!("bad fixture kind {other}"),
        };
        let len: usize = parts.next().unwrap().parse().unwrap();
        let body = &rest[nl + 1..];
        results.push((clean, body[..len].to_vec()));
        assert_eq!(body[len], b'\n');
        rest = &body[len + 1..];
    }
    results
}

/// Checks graft against one recorded git result.
fn check(case: usize, [b, o, t]: &[Vec<u8>; 3], git_clean: bool, git_out: &[u8]) -> bool {
    let ctx = || {
        format!(
            "case {case}\n--- base\n{}--- ours\n{}--- theirs\n{}--- git\n{}",
            String::from_utf8_lossy(b),
            String::from_utf8_lossy(o),
            String::from_utf8_lossy(t),
            String::from_utf8_lossy(git_out),
        )
    };
    match merge3(b, o, t) {
        Merge3::Clean(bytes) => {
            assert!(git_clean, "graft clean, git conflicted\n{}", ctx());
            assert_eq!(
                String::from_utf8_lossy(&bytes),
                String::from_utf8_lossy(git_out),
                "{}",
                ctx()
            );
            true
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
                String::from_utf8_lossy(&render_conflict_markers(b, o, t)),
                String::from_utf8_lossy(git_out),
                "{}",
                ctx()
            );
            false
        }
        Merge3::NotText => panic!("generated text is text"),
        Merge3::TooComplex => panic!("a small case is never too complex\n{}", ctx()),
    }
}

fn check_all(results: &[(bool, Vec<u8>)]) {
    let cases = corpus();
    assert_eq!(
        results.len(),
        cases.len(),
        "fixture is out of date; re-bless"
    );
    let mut clean = 0;
    for (case, (inputs, (git_clean, git_out))) in cases.iter().zip(results).enumerate() {
        clean += usize::from(check(case, inputs, *git_clean, git_out));
    }
    // The corpus must exercise both outcomes.
    let conflicted = CASES - clean;
    assert!(
        clean > CASES / 10 && conflicted > CASES / 10,
        "{clean} clean, {conflicted} conflicted"
    );
}

#[test]
fn agrees_with_recorded_git() {
    check_all(&read_fixture());
}

/// Runs the installed git live. `GRAFT_BLESS=1` rewrites the fixture.
#[test]
#[ignore = "depends on the installed git version; the fixture test is the required one"]
fn agrees_with_git_merge_file() {
    assert!(have_git(), "git is not installed");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("merge_corpus");
    std::fs::create_dir_all(&dir).unwrap();
    let results: Vec<_> = corpus()
        .iter()
        .map(|[b, o, t]| git_merge(&dir, b, o, t))
        .collect();
    if std::env::var_os("GRAFT_BLESS").is_some() {
        write_fixture(&results);
    }
    check_all(&results);
}
