//! A synthetic git repository for the CI-sized pilot run, written with
//! `git fast-import`, whose stream format is stable across git versions.
//! Same spec, same commits, same shas, on every machine.
//!
//! The history is text files edited line by line (so `log --path`, `diff`
//! and `blame` have something to say), plus the adversarial entries the
//! fidelity check must get right: names with unicode, spaces and a leading
//! dash; symlinks to a file and to a directory; an executable that later
//! loses its exec bit; an empty file; a large binary; a file that becomes a
//! symlink; a deletion; a merge of a side branch; and a tag.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::PilotError;
use super::proc::git;

/// Shape of the synthetic repository.
#[derive(Clone, Debug)]
pub struct FixtureSpec {
    /// Commits on `main` (at least 4; the side branch adds two more).
    pub commits: usize,
    /// Text files in the tree (besides the adversarial entries).
    pub files: usize,
    /// Lines in each new text file.
    pub lines: usize,
    /// Size of the large binary, bytes.
    pub large_bytes: usize,
    /// Seed.
    pub seed: u64,
}

impl Default for FixtureSpec {
    fn default() -> Self {
        FixtureSpec {
            commits: 40,
            files: 60,
            lines: 40,
            large_bytes: 3 << 20,
            seed: 16,
        }
    }
}

/// The file every text commit edits, so it has a long history.
pub const HOT_FILE: &str = "src/hot.rs";

/// The directory a sparse clone takes.
pub const SPARSE_DIR: &str = "src";

/// SplitMix64, as in `synth`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const WORDS: &[&str] = &[
    "graft", "line", "tree", "blob", "land", "fetch", "slice", "journal", "frame", "index",
];

fn text_line(rng: &mut Rng, n: usize) -> String {
    let a = WORDS[rng.below(WORDS.len())];
    let b = WORDS[rng.below(WORDS.len())];
    format!("let v{n} = {a}_{b}({});\n", rng.below(1000))
}

/// One tree entry in the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    File { exec: bool, bytes: Vec<u8> },
    Link(String),
}

/// Writes the fast-import stream for `spec`.
fn stream(spec: &FixtureSpec) -> Vec<u8> {
    let mut rng = Rng(spec.seed);
    let mut out = Vec::new();
    let mut tree: BTreeMap<String, Entry> = BTreeMap::new();
    let mut mark = 0usize;
    let mut ts = 1_700_000_000u64;

    let text = |rng: &mut Rng, lines: usize| -> Vec<u8> {
        (0..lines)
            .flat_map(|n| text_line(rng, n).into_bytes())
            .collect()
    };

    let mut commit = |out: &mut Vec<u8>,
                      branch: &str,
                      msg: &str,
                      from: Option<usize>,
                      merge: Option<usize>,
                      changes: &[(String, Option<Entry>)]|
     -> usize {
        mark += 1;
        ts += 60;
        let _ = write!(
            out,
            "commit refs/heads/{branch}\nmark :{mark}\nauthor Pilot <pilot@example.invalid> {ts} +0000\ncommitter Pilot <pilot@example.invalid> {ts} +0000\ndata {}\n{msg}\n",
            msg.len()
        );
        if let Some(f) = from {
            let _ = writeln!(out, "from :{f}");
        }
        if let Some(m) = merge {
            let _ = writeln!(out, "merge :{m}");
        }
        for (path, entry) in changes {
            match entry {
                None => {
                    let _ = writeln!(out, "D {path}");
                }
                Some(Entry::File { exec, bytes }) => {
                    let mode = if *exec { "100755" } else { "100644" };
                    let _ = writeln!(out, "M {mode} inline {path}\ndata {}", bytes.len());
                    out.extend_from_slice(bytes);
                    out.push(b'\n');
                }
                Some(Entry::Link(target)) => {
                    let _ = writeln!(
                        out,
                        "M 120000 inline {path}\ndata {}\n{target}",
                        target.len()
                    );
                }
            }
        }
        out.push(b'\n');
        mark
    };

    let apply = |tree: &mut BTreeMap<String, Entry>, changes: &[(String, Option<Entry>)]| {
        for (p, e) in changes {
            match e {
                Some(e) => tree.insert(p.clone(), e.clone()),
                None => tree.remove(p),
            };
        }
    };

    // Commit 1: the text files and every adversarial entry.
    let mut first: Vec<(String, Option<Entry>)> = Vec::new();
    for i in 0..spec.files {
        let dir = ["src", "src/net", "docs", "tools"][i % 4];
        let bytes = text(&mut rng, spec.lines);
        first.push((
            format!("{dir}/f{i}.rs"),
            Some(Entry::File { exec: false, bytes }),
        ));
    }
    let hot = text(&mut rng, spec.lines);
    first.push((
        HOT_FILE.into(),
        Some(Entry::File {
            exec: false,
            bytes: hot,
        }),
    ));
    let file = |s: &str| {
        Some(Entry::File {
            exec: false,
            bytes: s.as_bytes().to_vec(),
        })
    };
    first.push(("README.md".into(), file("# pilot fixture\n")));
    first.push(("-leading-dash.txt".into(), file("dash\n")));
    first.push(("with space/file name.txt".into(), file("spaces\n")));
    first.push(("ünïcødé/naïve-ß.txt".into(), file("unicode\n")));
    first.push((
        "empty.txt".into(),
        Some(Entry::File {
            exec: false,
            bytes: Vec::new(),
        }),
    ));
    first.push((
        "bin/run.sh".into(),
        Some(Entry::File {
            exec: true,
            bytes: b"#!/bin/sh\necho run\n".to_vec(),
        }),
    ));
    first.push((
        "link-to-readme".into(),
        Some(Entry::Link("README.md".into())),
    ));
    first.push(("link-to-src".into(), Some(Entry::Link("src".into()))));
    first.push((
        "dangling".into(),
        Some(Entry::Link("does/not/exist".into())),
    ));
    first.push(("becomes-link.txt".into(), file("a file first\n")));
    first.push(("doomed.txt".into(), file("deleted later\n")));
    let mut large = Vec::with_capacity(spec.large_bytes);
    while large.len() < spec.large_bytes {
        large.extend_from_slice(&rng.next().to_le_bytes());
    }
    large.truncate(spec.large_bytes);
    first.push((
        "assets/large.bin".into(),
        Some(Entry::File {
            exec: false,
            bytes: large,
        }),
    ));
    let mut tip = commit(&mut out, "main", "initial import", None, None, &first);
    apply(&mut tree, &first);
    let _ = write!(out, "reset refs/tags/v0.1\nfrom :{tip}\n\n");

    let commits = spec.commits.max(4);
    let side_at = commits / 2;
    for n in 2..=commits {
        let mut changes: Vec<(String, Option<Entry>)> = Vec::new();
        // Edit the hot file: rewrite one line and append one.
        if let Some(Entry::File { bytes, .. }) = tree.get(HOT_FILE) {
            let mut lines: Vec<String> = String::from_utf8_lossy(bytes)
                .lines()
                .map(|l| format!("{l}\n"))
                .collect();
            let at = rng.below(lines.len());
            lines[at] = text_line(&mut rng, at + 10_000 * n);
            lines.push(text_line(&mut rng, lines.len()));
            changes.push((
                HOT_FILE.into(),
                Some(Entry::File {
                    exec: false,
                    bytes: lines.concat().into_bytes(),
                }),
            ));
        }
        // And a couple of other text files.
        for _ in 0..2 {
            let i = rng.below(spec.files.max(1));
            let path = format!("{}/f{i}.rs", ["src", "src/net", "docs", "tools"][i % 4]);
            if let Some(Entry::File { bytes, .. }) = tree.get(&path) {
                let mut b = bytes.clone();
                b.extend_from_slice(text_line(&mut rng, n).as_bytes());
                changes.push((
                    path,
                    Some(Entry::File {
                        exec: false,
                        bytes: b,
                    }),
                ));
            }
        }
        match n {
            3 => {
                changes.push((
                    "bin/run.sh".into(),
                    Some(Entry::File {
                        exec: false,
                        bytes: b"#!/bin/sh\necho run\n".to_vec(),
                    }),
                ));
                changes.push((
                    "becomes-link.txt".into(),
                    Some(Entry::Link("README.md".into())),
                ));
            }
            4 => {
                changes.push(("doomed.txt".into(), None));
                changes.push((
                    "link-to-readme".into(),
                    Some(Entry::Link("with space/file name.txt".into())),
                ));
            }
            _ => {}
        }
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        changes.dedup_by(|a, b| a.0 == b.0);
        tip = commit(
            &mut out,
            "main",
            &format!("change {n}"),
            None,
            None,
            &changes,
        );
        apply(&mut tree, &changes);

        if n == side_at {
            // A side branch off this commit, merged back two commits later.
            let side1 = vec![("side/notes.txt".to_owned(), file("side 1\n"))];
            let s1 = commit(&mut out, "side", "side work 1", Some(tip), None, &side1);
            let side2 = vec![("side/notes.txt".to_owned(), file("side 1\nside 2\n"))];
            let s2 = commit(&mut out, "side", "side work 2", Some(s1), None, &side2);
            let mut merged = tree.clone();
            apply(&mut merged, &side1);
            apply(&mut merged, &side2);
            let changes = vec![(
                "side/notes.txt".to_owned(),
                merged.get("side/notes.txt").cloned(),
            )];
            tip = commit(
                &mut out,
                "main",
                "merge side",
                Some(tip),
                Some(s2),
                &changes,
            );
            apply(&mut tree, &changes);
        }
    }
    let _ = tip;
    out.extend_from_slice(b"done\n");
    out
}

/// Builds the fixture as a bare repository at `dir` (created) and returns it.
pub fn build(spec: &FixtureSpec, dir: &Path) -> Result<PathBuf, PilotError> {
    let bare = dir.to_path_buf();
    super::proc::run(git(None).args(["init", "-q", "--bare"]).arg(&bare))?;
    super::proc::run(git(Some(&bare)).args(["symbolic-ref", "HEAD", "refs/heads/main"]))?;
    let data = stream(spec);
    let mut child = git(Some(&bare))
        .args(["fast-import", "--quiet", "--done"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| PilotError::Step(format!("git fast-import: {e}")))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(&data)
        .map_err(|e| PilotError::Step(format!("git fast-import: write: {e}")))?;
    let out = child
        .wait_with_output()
        .map_err(|e| PilotError::Step(format!("git fast-import: {e}")))?;
    if !out.status.success() {
        return Err(PilotError::Step(format!(
            "git fast-import: {}",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("")
        )));
    }
    Ok(bare)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_is_deterministic() {
        let spec = FixtureSpec {
            large_bytes: 100,
            ..FixtureSpec::default()
        };
        assert_eq!(stream(&spec), stream(&spec));
        let other = FixtureSpec {
            seed: 17,
            ..spec.clone()
        };
        assert_ne!(stream(&spec), stream(&other));
    }
}
