//! Differential tests against the git CLI: a git history is imported as
//! changesets (first-parent changes, its first-parent chain landed on
//! `main`), then `Log --path` is compared with
//! `git log --first-parent -- <path>` and `Blame` with
//! `git blame --first-parent` line origins.
//!
//! The synthetic repositories run by default; rustTorrent is `#[ignore]`d
//! because it clones over the network:
//! `cargo test -p graft-index --test git -- --ignored`.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Fixture, block_on, first_parent_chain, path};
use graft_index::{BlameRun, LogQuery};
use graft_model::{
    Author, AuthorKind, Change, Changeset, ChangesetId, EntryKind, ObjectId, Timestamp, TreeId,
};
use graft_store::MetaStore;

/// A directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("graft-index-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A git command in `dir`, isolated from the user's configuration.
fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
    cmd
}

fn git_bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = git_cmd(dir, args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_bytes(dir, args)).unwrap()
}

/// A git history imported into a fixture.
struct Imported {
    fx: Fixture,
    dir: PathBuf,
    by_sha: HashMap<String, ChangesetId>,
    sha_of: HashMap<ChangesetId, String>,
    /// HEAD's first-parent chain, oldest first.
    line: Vec<String>,
}

/// One `git diff-tree --raw` entry.
struct RawChange {
    path: String,
    mode: String,
    sha: String,
    deleted: bool,
}

/// `commit`'s changes against its first parent (or everything, for a root).
fn raw_changes(dir: &Path, commit: &str, first: Option<&str>) -> Vec<RawChange> {
    let mut args = vec![
        "diff-tree",
        "-r",
        "-z",
        "--no-renames",
        "--raw",
        "--no-commit-id",
    ];
    match first {
        Some(p) => args.extend([p, commit]),
        None => args.extend(["--root", commit]),
    }
    let out = git_bytes(dir, &args);
    let mut fields = out.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let Some(meta) = fields.next() {
        let meta = std::str::from_utf8(meta).unwrap();
        let path = std::str::from_utf8(fields.next().unwrap()).unwrap();
        // ":old_mode new_mode old_sha new_sha status"
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        changes.push(RawChange {
            path: path.to_owned(),
            mode: parts[1].to_owned(),
            sha: parts[3].to_owned(),
            deleted: parts[4].starts_with('D'),
        });
    }
    changes
}

/// Reads blobs by sha with one `git cat-file --batch`.
fn cat_blobs(dir: &Path, shas: &BTreeSet<String>) -> HashMap<String, Vec<u8>> {
    let mut child = git_cmd(dir, &["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input: String = shas.iter().map(|s| format!("{s}\n")).collect();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()).unwrap());
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    writer.join().unwrap();
    assert!(child.wait().unwrap().success());

    let mut blobs = HashMap::new();
    let mut rest = &out[..];
    while !rest.is_empty() {
        let nl = rest.iter().position(|&b| b == b'\n').unwrap();
        let header = std::str::from_utf8(&rest[..nl]).unwrap();
        let parts: Vec<&str> = header.split(' ').collect();
        assert_eq!(parts[1], "blob", "{header}");
        let size: usize = parts[2].parse().unwrap();
        let body = &rest[nl + 1..nl + 1 + size];
        blobs.insert(parts[0].to_owned(), body.to_vec());
        rest = &rest[nl + 2 + size..];
    }
    blobs
}

/// Imports every commit reachable from HEAD and lands HEAD's first-parent
/// chain on `main`.
fn import(dir: &Path) -> Imported {
    let fx = Fixture::new();
    let commits: Vec<Vec<String>> = git(
        dir,
        &["rev-list", "--topo-order", "--reverse", "--parents", "HEAD"],
    )
    .lines()
    .map(|l| l.split(' ').map(str::to_owned).collect())
    .collect();
    let raws: Vec<Vec<RawChange>> = commits
        .iter()
        .map(|c| raw_changes(dir, &c[0], c.get(1).map(String::as_str)))
        .collect();
    let shas: BTreeSet<String> = raws
        .iter()
        .flatten()
        .filter(|r| !r.deleted && r.mode != "160000")
        .map(|r| r.sha.clone())
        .collect();
    let blobs = cat_blobs(dir, &shas);
    let mut blob_ids = HashMap::new();
    for (sha, content) in &blobs {
        blob_ids.insert(sha.clone(), fx.blob(content));
    }

    let mut by_sha = HashMap::new();
    let mut sha_of = HashMap::new();
    let mut css = HashMap::new();
    for (commit, raw) in commits.iter().zip(&raws) {
        let mut changes: Vec<Change> = raw
            .iter()
            .map(|r| {
                let p = path(&r.path);
                if r.deleted {
                    return Change::Delete { path: p };
                }
                let (kind, id) = match r.mode.as_str() {
                    "100755" => (EntryKind::Exec, blob_ids[&r.sha]),
                    "120000" => (EntryKind::Symlink, blob_ids[&r.sha]),
                    "160000" => {
                        let mut opaque = [0; 32];
                        opaque[..20].copy_from_slice(&hex(&r.sha));
                        (EntryKind::Submodule, ObjectId(opaque))
                    }
                    _ => (EntryKind::File, blob_ids[&r.sha]),
                };
                Change::Put { path: p, kind, id }
            })
            .collect();
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        let cs = Changeset {
            parents: commit[1..].iter().map(|p| by_sha[p]).collect(),
            root: TreeId(ObjectId([0; 32])),
            changes,
            author: Author {
                name: "t".into(),
                email: "t@example.com".into(),
                kind: AuthorKind::Human,
            },
            authored_at: Timestamp {
                ms: 0,
                offset_min: 0,
            },
            message: commit[0].clone(),
            metadata: Default::default(),
        };
        let id = fx.create(&cs);
        by_sha.insert(commit[0].clone(), id);
        sha_of.insert(id, commit[0].clone());
        css.insert(id, cs);
    }
    let head = by_sha[git(dir, &["rev-parse", "HEAD"]).trim()];
    let chain = first_parent_chain(&css, head);
    fx.land("main", &chain);
    fx.set_line("main", head);
    let line = chain.iter().map(|cs| sha_of[&cs.id()].clone()).collect();
    Imported {
        fx,
        dir: dir.to_owned(),
        by_sha,
        sha_of,
        line,
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

impl Imported {
    /// Our `Log` from `rev` for `p`, all pages of `limit`, as shas.
    fn log(&self, rev: &str, p: &str, limit: u32) -> Vec<String> {
        let fx = &self.fx;
        let mut q = LogQuery {
            from: self.by_sha[rev],
            until: None,
            paths: vec![path(p)],
            limit,
            after_pos: None,
        };
        let mut out = Vec::new();
        loop {
            let page = block_on(fx.index.log(fx.repo, q.clone())).unwrap();
            out.extend(page.iter().map(|id| self.sha_of[id].clone()));
            let Some(&last) = page.last() else { break };
            if page.len() < limit as usize {
                break;
            }
            match block_on(fx.index.meta().line_pos(fx.repo, "main", last)).unwrap() {
                Some(pos) => q.after_pos = Some(pos),
                None => {
                    let parent = self.git_first_parent(&self.sha_of[&last]);
                    match parent {
                        Some(p) => q.from = self.by_sha[&p],
                        None => break,
                    }
                }
            }
        }
        out
    }

    fn git_first_parent(&self, sha: &str) -> Option<String> {
        let parents = git(&self.dir, &["rev-list", "--parents", "-n", "1", sha]);
        parents.split_whitespace().nth(1).map(str::to_owned)
    }

    fn git_log(&self, rev: &str, p: &str) -> Vec<String> {
        git(
            &self.dir,
            &["log", "--first-parent", "--format=%H", rev, "--", p],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    /// Our `Blame` of `p` at `rev`, one sha per line.
    fn blame(&self, rev: &str, p: &str) -> Vec<String> {
        let fx = &self.fx;
        let runs: Vec<BlameRun> =
            block_on(fx.index.blame(fx.repo, self.by_sha[rev], &path(p))).unwrap();
        runs.iter()
            .flat_map(|r| {
                std::iter::repeat_n(self.sha_of[&r.origin].clone(), (r.end - r.start) as usize)
            })
            .collect()
    }

    /// `git blame --first-parent` origins of `p` at `rev`, one sha per line,
    /// and whether git followed the file from another name.
    fn git_blame(&self, rev: &str, p: &str) -> (Vec<String>, bool) {
        let out = git_bytes(
            &self.dir,
            &[
                "blame",
                "--first-parent",
                "--diff-algorithm=histogram",
                "--porcelain",
                rev,
                "--",
                p,
            ],
        );
        // Content lines (after a tab) may be any bytes.
        let out = String::from_utf8_lossy(&out);
        let mut lines: Vec<(usize, String)> = Vec::new();
        let mut renamed = false;
        for line in out.lines() {
            if line.starts_with('\t') {
                continue;
            }
            if let Some(name) = line.strip_prefix("filename ") {
                renamed |= name != p;
                continue;
            }
            let parts: Vec<&str> = line.split(' ').collect();
            let header = parts[0].len() == 40
                && parts[0].bytes().all(|b| b.is_ascii_hexdigit())
                && (3..=4).contains(&parts.len())
                && parts[1..].iter().all(|n| n.parse::<usize>().is_ok());
            if header {
                lines.push((parts[2].parse().unwrap(), parts[0].to_owned()));
            }
        }
        lines.sort();
        (lines.into_iter().map(|(_, sha)| sha).collect(), renamed)
    }

    /// Files in `rev`'s snapshot.
    fn files(&self, rev: &str) -> Vec<String> {
        git(&self.dir, &["ls-tree", "-r", "--name-only", rev])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Every path changed on the line, and every directory above one.
    fn touched(&self) -> BTreeSet<String> {
        let out = git(
            &self.dir,
            &[
                "log",
                "--first-parent",
                "--format=",
                "--name-only",
                "-m",
                "HEAD",
            ],
        );
        let mut paths = BTreeSet::new();
        for line in out.lines().filter(|l| !l.is_empty()) {
            let mut p = line;
            paths.insert(p.to_owned());
            while let Some((dir, _)) = p.rsplit_once('/') {
                paths.insert(dir.to_owned());
                p = dir;
            }
        }
        paths
    }
}

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Builds a synthetic repository: edits, deletes and re-creations of files
/// at several depths, a directory turned file and back, a file without a
/// final newline, and side branches merged with `--no-ff`. Every line is
/// unique, so any minimal diff agrees on what changed.
fn synthetic(dir: &Path, seed: u64, steps: usize) {
    git(dir, &["init", "-q", "-b", "main"]);
    let mut rng = Rng(seed);
    let mut unique = 0;
    let mut fresh = |tag: &str| {
        unique += 1;
        format!("{tag} line {unique}")
    };
    let pool = [
        "top.txt",
        "a/x.txt",
        "a/y.txt",
        "a/b/z.txt",
        "a/b/c/deep.txt",
        "e/f.txt",
        "nonl.txt",
    ];
    let mut files: HashMap<String, Vec<String>> = HashMap::new();
    let write = |files: &HashMap<String, Vec<String>>| {
        for (p, lines) in files {
            let full = dir.join(p);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let mut body = lines.join("\n");
            if !p.starts_with("nonl") && !lines.is_empty() {
                body.push('\n');
            }
            std::fs::write(full, body).unwrap();
        }
    };
    let mut n = 0;
    let mut commit = |msg: &str| {
        n += 1;
        let date = format!("{} +0000", 1_700_000_000 + n);
        git_cmd(dir, &["add", "-A"]).output().unwrap();
        let out = git_cmd(dir, &["commit", "-q", "--allow-empty", "-m", msg])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let edit = |lines: &mut Vec<String>, rng: &mut Rng, fresh: &mut dyn FnMut(&str) -> String| {
        for _ in 0..1 + rng.below(3) {
            let at = rng.below(lines.len() + 1);
            match rng.below(4) {
                0 | 1 => lines.insert(at, fresh("ins")),
                2 if at < lines.len() => {
                    lines.remove(at);
                }
                _ if at < lines.len() => lines[at] = fresh("mod"),
                _ => lines.push(fresh("app")),
            }
        }
    };

    for step in 0..steps {
        match (step, rng.below(10)) {
            // The directory `e` becomes a file, and later a directory again.
            (s, _) if s == steps / 3 => {
                std::fs::remove_dir_all(dir.join("e")).ok();
                files.retain(|p, _| !p.starts_with("e/"));
                files.insert("e".into(), (0..3).map(|_| fresh("efile")).collect());
            }
            (s, _) if s == 2 * steps / 3 => {
                std::fs::remove_file(dir.join("e")).ok();
                files.remove("e");
                files.insert("e/g.txt".into(), (0..2).map(|_| fresh("edir")).collect());
            }
            // Delete a file (it may come back later, from scratch).
            (_, 0) if files.len() > 2 => {
                let mut names: Vec<_> = files.keys().cloned().collect();
                names.sort();
                let victim = names[rng.below(names.len())].clone();
                std::fs::remove_file(dir.join(&victim)).unwrap();
                files.remove(&victim);
            }
            // A side branch with its own files, merged without fast-forward.
            (_, 1) => {
                let branch = format!("side{step}");
                git(dir, &["checkout", "-q", "-b", &branch]);
                for k in 0..1 + rng.below(3) {
                    let p = format!("s/{branch}-{}.txt", k % 2);
                    let lines = files.entry(p).or_default();
                    edit(lines, &mut rng, &mut fresh);
                    write(&files);
                    commit(&format!("{branch} {k}"));
                }
                git(dir, &["checkout", "-q", "main"]);
                // Side files are not on main until the merge.
                git(
                    dir,
                    &[
                        "merge",
                        "-q",
                        "--no-ff",
                        "-m",
                        &format!("merge {branch}"),
                        &branch,
                    ],
                );
                continue;
            }
            _ => {
                for _ in 0..1 + rng.below(2) {
                    let p = pool[rng.below(pool.len())];
                    if p.starts_with("e/") && files.contains_key("e") {
                        continue;
                    }
                    let lines = files.entry(p.to_owned()).or_default();
                    edit(lines, &mut rng, &mut fresh);
                }
            }
        }
        write(&files);
        commit(&format!("step {step}"));
    }
}

/// Every touched path and directory, from HEAD and from a commit halfway
/// down the line, against `git log --first-parent`; every file's blame at
/// both against `git blame --first-parent`.
fn check_all(imp: &Imported) {
    let mid = imp.line[imp.line.len() / 2].clone();
    let head = imp.line.last().unwrap().clone();
    assert!(
        imp.line.len() > 40,
        "{} commits on the line",
        imp.line.len()
    );
    let touched = imp.touched();
    assert!(touched.len() > 12, "{} paths", touched.len());
    for rev in [&head, &mid] {
        let files = imp.files(rev);
        assert!(files.len() > 3, "{} files", files.len());
        for p in &touched {
            assert_eq!(imp.log(rev, p, 3), imp.git_log(rev, p), "log {rev} -- {p}");
        }
        for f in files {
            let (want, renamed) = imp.git_blame(rev, &f);
            assert!(!renamed, "{f} was renamed");
            assert_eq!(imp.blame(rev, &f), want, "blame {rev} -- {f}");
        }
    }
}

#[test]
fn synthetic_histories_match_git() {
    for seed in [0x9e37_79b9_7f4a_7c15, 7, 1234567] {
        let tmp = TempDir::new("synthetic");
        synthetic(&tmp.0, seed, 60);
        let imp = import(&tmp.0);
        check_all(&imp);
    }
}

/// Side-branch commits are off the line: their `Log` walks to the line,
/// then uses the index.
#[test]
fn off_line_commits_match_git() {
    let tmp = TempDir::new("offline");
    synthetic(&tmp.0, 42, 40);
    let imp = import(&tmp.0);
    let on_line: BTreeSet<&String> = imp.line.iter().collect();
    let off: Vec<String> = git(&imp.dir, &["rev-list", "HEAD"])
        .lines()
        .filter(|s| !on_line.contains(&s.to_string()))
        .map(str::to_owned)
        .collect();
    assert!(!off.is_empty());
    for rev in &off {
        for p in ["a", "a/x.txt", "s", "top.txt"] {
            assert_eq!(imp.log(rev, p, 2), imp.git_log(rev, p), "log {rev} -- {p}");
        }
    }
}

/// rustTorrent (read-only clone): `Log --path` for 50 sampled paths and
/// `Blame` for 20 sampled files at HEAD equal git's.
#[test]
#[ignore = "clones rustTorrent over the network"]
fn rust_torrent_matches_git() {
    let tmp = TempDir::new("rust-torrent");
    let status = Command::new("git")
        .args(["clone", "--quiet", "--bare"])
        .arg("https://github.com/TheDancingDeveloper-org/rustTorrent.git")
        .arg(tmp.0.join("repo.git"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .unwrap();
    assert!(status.success(), "clone failed");
    let imp = import(&tmp.0.join("repo.git"));
    let head = imp.line.last().unwrap().clone();

    let touched: Vec<String> = imp.touched().into_iter().collect();
    assert!(touched.len() >= 50, "{} paths", touched.len());
    let sampled: Vec<&String> = (0..50).map(|k| &touched[k * touched.len() / 50]).collect();
    for p in sampled {
        assert_eq!(imp.log(&head, p, 25), imp.git_log(&head, p), "log -- {p}");
    }

    let files = imp.files(&head);
    let step = (files.len() / 40).max(1);
    let mut checked = 0;
    for f in files.iter().step_by(step) {
        if checked == 20 {
            break;
        }
        let (want, renamed) = imp.git_blame(&head, f);
        // git follows renames; graft's blame kind does not (yet).
        if renamed {
            continue;
        }
        assert_eq!(imp.blame(&head, f), want, "blame -- {f}");
        checked += 1;
    }
    assert_eq!(checked, 20);
}
