//! Differential test: graft's line merge against `git merge-file`
//! (`docs/notes/adversarial-harness.md` §4, WI-2346).
//!
//! git is the test oracle here, run as `git merge-file -p --diff3
//! --diff-algorithm=histogram`. graft-harness is a test tool, so calling git
//! from its tests doesn't bring git into a product crate (rule 6).
//!
//! Every case gets a [`Verdict`]. Accepted differences are listed on
//! [`Verdict`] with their reason; any bug verdict fails the test with the
//! seed and case number. The run is skipped when git isn't installed.
//! `GRAFT_MERGE_CASES` and `GRAFT_MERGE_SEED` widen or move the run.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use graft_harness::merge::{Case, Git, Shape, Verdict, case, classify};

const SEED: u64 = 0x0057_492d_3233_3436;
const CASES: u64 = 1_300;

fn git_merge(dir: &Path, c: &Case) -> Git {
    std::fs::write(dir.join("base"), &c.base).unwrap();
    std::fs::write(dir.join("ours"), &c.ours).unwrap();
    std::fs::write(dir.join("theirs"), &c.theirs).unwrap();
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
    match out.status.code().expect("git merge-file was killed") {
        0 => Git::Clean(out.stdout),
        255 => Git::Refused,
        n => Git::Conflicts(out.stdout, n as usize),
    }
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn merge3_against_git_merge_file() {
    if !have_git() {
        eprintln!("git not installed; skipping");
        return;
    }
    let seed = env_u64("GRAFT_MERGE_SEED", SEED);
    let cases = env_u64("GRAFT_MERGE_CASES", CASES);
    let dir = std::env::temp_dir().join(format!("graft-merge-diff-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut tally: BTreeMap<(Shape, Verdict), u64> = BTreeMap::new();
    let mut bugs = Vec::new();
    for n in 0..cases {
        let c = case(seed, n);
        let v = classify(&c, &git_merge(&dir, &c));
        *tally.entry((c.shape, v)).or_default() += 1;
        if v.is_bug() {
            bugs.push(format!(
                "case {n} ({:?}): {v:?}  replay: GRAFT_MERGE_SEED={seed} GRAFT_MERGE_CASES={}",
                c.shape,
                n + 1
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    let mut by_verdict: BTreeMap<Verdict, u64> = BTreeMap::new();
    for ((shape, v), k) in &tally {
        eprintln!("{shape:?} {v:?}: {k}");
        *by_verdict.entry(*v).or_default() += k;
    }
    for (v, k) in &by_verdict {
        eprintln!("total {v:?}: {k} ({})", v.reason());
    }
    assert!(
        bugs.is_empty(),
        "{} disagreements are bugs:\n{}",
        bugs.len(),
        bugs.join("\n")
    );
}
