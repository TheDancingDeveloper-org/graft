//! The pilot harness (work item G1.16, `docs/design.md` §15, the phase 1
//! exit criteria in `docs/plan.md`).
//!
//! [`run_pilot`] takes a git repository (a path or URL, or the synthetic
//! [`fixture`]) and, against the real `graft-server`, `graft-mirror` and
//! `graft` binaries on this host:
//!
//! 1. mirrors it into a scratch bare repository and imports it with
//!    `graft-mirror import` into a fresh server site, once per run;
//! 2. on the first run, clones every imported changeset with
//!    `graft clone --at` and compares it with `git checkout` of its commit
//!    (paths, file or symlink, exec bit, bytes, symlink targets), and checks
//!    that `log --path`, `diff` and `blame` agree with git;
//! 3. times the trial's scenarios against graft and git: full and sparse
//!    clone, `log --path`, `diff`, `blame` cold and cached, the share of
//!    full-clone time spent decompressing frames (for D5), and restart to
//!    serving;
//! 4. writes `results.tsv` (all runs pooled), `results-<run>.tsv`, and
//!    `report.md` with the budgets of the exit criteria and the run-to-run
//!    spread.
//!
//! [`crash::crash_suite`] is the long `kill -9` suite. Every child process
//! the harness runs has a deadline, so a dead or wedged server fails the
//! step instead of hanging the harness. Tokens travel in the environment of
//! the child (`GRAFT_TOKEN`), never in its arguments or in the report.

#![cfg(unix)]

pub mod crash;
pub mod fixture;
pub mod proc;
mod rpc;
pub mod verify;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use graft_model::ChangesetId;
use graft_proto::frame::Frame;
use imara_diff::{Algorithm, Diff, InternedInput};
use serde_json::Value;

use crate::bench::{Results, Samples, Spread, spread, spread_markdown};
use fixture::FixtureSpec;
use proc::{Bins, Ran, Site, Token, git, run};
use verify::{Fidelity, Mapped};

/// Why the harness stopped.
#[derive(Debug)]
pub enum PilotError {
    /// A step failed.
    Step(String),
    /// A step ran past its deadline and was killed.
    Timeout(String),
    /// A file operation failed.
    Io(String),
    /// graft disagreed with git, or lost an acknowledged write.
    Mismatch(String),
}

impl PilotError {
    pub(crate) fn io(path: &Path, e: std::io::Error) -> PilotError {
        PilotError::Io(format!("{}: {e}", path.display()))
    }
}

impl fmt::Display for PilotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PilotError::Step(s) => write!(f, "step failed: {s}"),
            PilotError::Timeout(s) => write!(f, "timed out: {s}"),
            PilotError::Io(s) => write!(f, "io: {s}"),
            PilotError::Mismatch(s) => write!(f, "mismatch: {s}"),
        }
    }
}

impl std::error::Error for PilotError {}

/// Where the repository under test comes from.
#[derive(Clone, Debug)]
pub enum Source {
    /// The synthetic fixture.
    Fixture(FixtureSpec),
    /// A git path or URL, mirrored with `git clone --mirror`.
    Git(String),
}

/// What to run.
#[derive(Clone, Debug)]
pub struct PilotConfig {
    /// The repository under test.
    pub source: Source,
    /// The git ref imported as line `main`.
    pub git_ref: String,
    /// The graft repository name.
    pub repo: String,
    /// Scratch directory (created; holds sites, mirrors and clones).
    pub work: PathBuf,
    /// Where `results.tsv` and `report.md` go (created).
    pub out: PathBuf,
    /// Independent runs (fresh site and import each).
    pub runs: usize,
    /// Samples per scenario per run.
    pub iterations: usize,
    /// The directory of the sparse clone (default: the largest top-level
    /// directory).
    pub sparse_path: Option<String>,
    /// The file for `log --path` and cached `blame` (default: the file
    /// changed most often on the first-parent history).
    pub hot_file: Option<String>,
    /// Verify at most this many changesets (default: every one).
    pub verify_limit: Option<usize>,
    /// Run the crash suite with this many restarts after the scenarios.
    pub crash_restarts: usize,
}

impl PilotConfig {
    /// The CI-sized defaults for `source`.
    pub fn new(source: Source, work: &Path, out: &Path) -> PilotConfig {
        PilotConfig {
            source,
            git_ref: "refs/heads/main".into(),
            repo: "pilot".into(),
            work: work.to_path_buf(),
            out: out.to_path_buf(),
            runs: 1,
            iterations: 5,
            sparse_path: None,
            hot_file: None,
            verify_limit: None,
            crash_restarts: 0,
        }
    }
}

/// How `log`, `diff` and `blame` compared with git.
#[derive(Clone, Debug, Default)]
pub struct Agreement {
    /// `log --path <hot file>` gave the same commits in the same order.
    pub log: bool,
    /// `diff <ref>~k <ref>` named the same paths.
    pub diff: bool,
    /// Blame lines whose origin matched git's (equivalent tie-breaks
    /// included), and lines compared.
    pub blame_lines: (usize, usize),
    /// Of the agreeing blame lines, those whose origins differ but are
    /// equivalent tie-breaks (see [`TieCheck`]).
    pub blame_ties: usize,
    /// What differed, briefly.
    pub notes: Vec<String>,
}

/// What [`run_pilot`] found.
#[derive(Debug)]
pub struct PilotOutcome {
    /// Commits reachable from the imported ref and the tags (git's count; the import takes both).
    pub commits: usize,
    /// The fidelity check.
    pub fidelity: Fidelity,
    /// Agreement with git.
    pub agreement: Agreement,
    /// Each run's samples.
    pub runs: Vec<Samples>,
    /// The pooled table (what `results.tsv` holds).
    pub results: Results,
    /// Run-to-run spread of each p50.
    pub spreads: Vec<Spread>,
    /// The crash suite, when it ran.
    pub crash: Option<crash::CrashOutcome>,
    /// `results.tsv`.
    pub tsv: PathBuf,
    /// `report.md`.
    pub report: PathBuf,
}

/// The `graft` CLI against one server, run outside any working copy.
struct Cli<'a> {
    bins: &'a Bins,
    url: String,
    repo: &'a str,
    token: &'a Token,
    cwd: &'a Path,
}

impl Cli<'_> {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.bins.cli);
        c.current_dir(self.cwd)
            .env("GRAFT_TOKEN", self.token.expose())
            .env_remove("GRAFT_ENDPOINT")
            .args(["--endpoint", &self.url, "--repo", self.repo, "--json"])
            .args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Result<Ran, PilotError> {
        run(&mut self.cmd(args))
    }

    fn json(&self, args: &[&str]) -> Result<(Value, Duration), PilotError> {
        let ran = self.run(args)?;
        let v = serde_json::from_slice(&ran.stdout)
            .map_err(|e| PilotError::Step(format!("graft {}: bad JSON: {e}", args[0])))?;
        Ok((v, ran.elapsed))
    }
}

fn git_text(bare: &Path, args: &[&str]) -> Result<String, PilotError> {
    let mut c = git(None);
    c.arg("--git-dir").arg(bare).args(args);
    Ok(run(&mut c)?.text())
}

fn git_timed(bare: &Path, args: &[&str]) -> Result<Duration, PilotError> {
    let mut c = git(None);
    c.arg("--git-dir").arg(bare).args(args);
    Ok(run(&mut c)?.elapsed)
}

fn rm(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

/// Facts about the source repository, from git.
struct Shape {
    branch: String,
    tip: String,
    /// First-parent history, newest first.
    first_parent: Vec<String>,
    hot_file: String,
    sparse_path: String,
    /// Text files at the tip, most changed first (for cold blame).
    blame_files: Vec<String>,
    files_at_tip: usize,
}

fn shape(cfg: &PilotConfig, bare: &Path) -> Result<Shape, PilotError> {
    let r = cfg.git_ref.as_str();
    let tip = git_text(bare, &["rev-parse", &format!("{r}^{{commit}}")])?
        .trim()
        .to_owned();
    let first_parent: Vec<String> = git_text(bare, &["rev-list", "--first-parent", r])?
        .lines()
        .map(str::to_owned)
        .collect();
    // Change counts per path on the first-parent history. This walks the
    // whole history, which is fine for a harness and never for graft.
    let mut changes: BTreeMap<String, usize> = BTreeMap::new();
    for line in git_text(
        bare,
        &[
            "log",
            "--first-parent",
            "--no-renames",
            "--name-only",
            "--format=",
            r,
        ],
    )?
    .lines()
    .filter(|l| !l.is_empty())
    {
        *changes.entry(line.to_owned()).or_default() += 1;
    }
    // Regular files at the tip under 1 MiB, with their sizes.
    let mut text_files = Vec::new();
    let mut top_dirs: BTreeMap<String, usize> = BTreeMap::new();
    let mut files_at_tip = 0;
    for line in git_text(bare, &["ls-tree", "-r", "-l", "-z", r])?
        .split('\0')
        .filter(|l| !l.is_empty())
    {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let f: Vec<&str> = meta.split_whitespace().collect();
        files_at_tip += 1;
        if let Some((dir, _)) = path.split_once('/') {
            *top_dirs.entry(dir.to_owned()).or_default() += 1;
        }
        let size: u64 = f.get(3).and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
        if f.first().is_some_and(|m| *m == "100644" || *m == "100755") && size < 1 << 20 && size > 0
        {
            text_files.push(path.to_owned());
        }
    }
    text_files.sort_by_key(|p| {
        (
            std::cmp::Reverse(changes.get(p).copied().unwrap_or(0)),
            p.clone(),
        )
    });
    let hot_file = match &cfg.hot_file {
        Some(h) => h.clone(),
        None => text_files
            .first()
            .cloned()
            .ok_or_else(|| PilotError::Step("no text file at the tip to blame".into()))?,
    };
    let sparse_path = match &cfg.sparse_path {
        Some(s) => s.clone(),
        None => top_dirs
            .iter()
            .max_by_key(|(d, n)| (**n, std::cmp::Reverse((*d).clone())))
            .map(|(d, _)| d.clone())
            .ok_or_else(|| PilotError::Step("no directory for a sparse clone".into()))?,
    };
    let blame_files = text_files.into_iter().filter(|p| *p != hot_file).collect();
    Ok(Shape {
        branch: r.strip_prefix("refs/heads/").unwrap_or(r).to_owned(),
        tip,
        first_parent,
        hot_file,
        sparse_path,
        blame_files,
        files_at_tip,
    })
}

/// Runs the harness. See the module documentation.
pub fn run_pilot(cfg: &PilotConfig) -> Result<PilotOutcome, PilotError> {
    let bins = Bins::locate()?;
    fs::create_dir_all(&cfg.work).map_err(|e| PilotError::io(&cfg.work, e))?;
    fs::create_dir_all(&cfg.out).map_err(|e| PilotError::io(&cfg.out, e))?;
    let bare = cfg.work.join("source.git");
    rm(&bare);
    match &cfg.source {
        Source::Fixture(spec) => {
            fixture::build(spec, &bare)?;
        }
        Source::Git(src) => {
            proc::run_with(
                git(None)
                    .args(["clone", "--mirror", "--quiet", src])
                    .arg(&bare),
                Duration::from_secs(1800),
                false,
            )?;
        }
    }
    // Let the sparse git clone ask for a blob filter over file://.
    git_text(&bare, &["config", "uploadpack.allowFilter", "true"])?;
    let shape = shape(cfg, &bare)?;
    let commits: usize = git_text(&bare, &["rev-list", "--count", &cfg.git_ref, "--tags"])?
        .trim()
        .parse()
        .map_err(|_| PilotError::Step("rev-list --count".into()))?;

    let mut fidelity = Fidelity::default();
    let mut agreement = Agreement::default();
    let mut runs = Vec::new();
    for n in 0..cfg.runs.max(1) {
        let dir = cfg.work.join(format!("run-{n}"));
        rm(&dir);
        let site = Site::new(&bins, &dir.join("site"))?;
        let mut samples = Samples::default();
        let mut server = site.start()?;
        let admin = site.mint("pilot-mirror", &[&format!("repo:{}:admin", cfg.repo)])?;
        let db = dir.join("mirror.sqlite");
        let mut import = Command::new(&bins.mirror);
        import
            .env("GRAFT_TOKEN", admin.expose())
            .arg("import")
            .arg(&bare)
            .args(["--repo", &cfg.repo, "--server", &server.url()])
            .arg("--db")
            .arg(&db)
            .args(["--line", &format!("{}=main", cfg.git_ref)]);
        let ran = proc::run_with(&mut import, Duration::from_secs(3600), false)?;
        samples.add_time(
            "import",
            "graft",
            &format!("{commits} commits"),
            ran.elapsed,
        );
        let map = verify::commit_map(&db)?;
        if map.len() != commits {
            return Err(PilotError::Mismatch(format!(
                "the mirror DB maps {} commits; git has {commits}",
                map.len()
            )));
        }
        let by_sha: BTreeMap<String, ChangesetId> =
            map.iter().map(|m| (m.sha.clone(), m.changeset)).collect();
        let read = site.mint("pilot-reader", &[&format!("repo:{}:read", cfg.repo)])?;
        let clones = dir.join("clones");
        fs::create_dir_all(&clones).map_err(|e| PilotError::io(&clones, e))?;
        let cli = Cli {
            bins: &bins,
            url: server.url(),
            repo: &cfg.repo,
            token: &read,
            cwd: &clones,
        };
        if n == 0 {
            fidelity = check_fidelity(&cli, &bare, &map, cfg.verify_limit, &clones)?;
            agreement = check_agreement(&cli, &bare, &shape, &by_sha)?;
        }
        scenarios(cfg, &cli, &bare, &shape, &by_sha, &clones, &mut samples)?;
        drop(cli);
        // Restart to serving, after kill -9 (design §15: under 2 s).
        for _ in 0..3 {
            server.kill9();
            server = site.start()?;
            samples.add_time(
                "restart to serving",
                "graft",
                "kill -9, then start",
                server.startup,
            );
        }
        server.kill9();
        fs::write(
            cfg.out.join(format!("results-{n}.tsv")),
            samples.results().to_tsv(),
        )
        .map_err(|e| PilotError::io(&cfg.out, e))?;
        runs.push(samples);
        rm(&clones);
    }

    let crash = if cfg.crash_restarts > 0 {
        Some(crash::crash_suite(
            &bins,
            &cfg.work.join("crash"),
            cfg.crash_restarts,
            0x6716,
        )?)
    } else {
        None
    };

    let results = Samples::pool(&runs).results();
    let spreads = spread(&runs.iter().map(Samples::results).collect::<Vec<_>>());
    let tsv = cfg.out.join("results.tsv");
    fs::write(&tsv, results.to_tsv()).map_err(|e| PilotError::io(&tsv, e))?;
    let outcome_report = cfg.out.join("report.md");
    let outcome = PilotOutcome {
        commits,
        fidelity,
        agreement,
        runs,
        results,
        spreads,
        crash,
        tsv,
        report: outcome_report,
    };
    let text = report(cfg, &shape, &outcome);
    fs::write(&outcome.report, text).map_err(|e| PilotError::io(&outcome.report, e))?;
    Ok(outcome)
}

/// Clones each changeset and compares it with git's checkout.
fn check_fidelity(
    cli: &Cli<'_>,
    bare: &Path,
    map: &[Mapped],
    limit: Option<usize>,
    scratch: &Path,
) -> Result<Fidelity, PilotError> {
    let mut f = Fidelity::default();
    let take = limit.unwrap_or(map.len()).min(map.len());
    // Spread a limited sample across the history.
    let step = (map.len() / take.max(1)).max(1);
    for m in map.iter().step_by(step).take(take) {
        let git_dir = scratch.join("verify-git");
        let graft_dir = scratch.join("verify-graft");
        rm(&git_dir);
        rm(&graft_dir);
        verify::git_checkout(bare, &m.sha, &git_dir)?;
        let at = m.changeset.to_string();
        cli.run(&["clone", cli.repo, path_str(&graft_dir)?, "--at", &at])?;
        let want = verify::walk(&git_dir, &[])?;
        let got = verify::walk(&graft_dir, &[".graft"])?;
        let problems = verify::compare(&want, &got);
        f.checked += 1;
        f.entries += want.len();
        if problems.is_empty() {
            f.matched += 1;
        } else {
            f.failed += 1;
            for p in problems {
                if f.problems.len() < 50 {
                    f.problems.push(format!("{}: {p}", &m.sha[..12]));
                }
            }
        }
        rm(&git_dir);
        rm(&graft_dir);
    }
    Ok(f)
}

fn path_str(p: &Path) -> Result<&str, PilotError> {
    p.to_str()
        .ok_or_else(|| PilotError::Step(format!("{} is not UTF-8", p.display())))
}

/// Every page of a paged CLI read, following `next`.
fn all_pages(cli: &Cli<'_>, args: &[&str], key: &str) -> Result<Vec<Value>, PilotError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10_000 {
        let mut a: Vec<&str> = args.to_vec();
        if let Some(c) = &cursor {
            a.extend(["--cursor", c]);
        }
        let (v, _) = cli.json(&a)?;
        out.extend(v[key].as_array().cloned().unwrap_or_default());
        match v["next"].as_str() {
            Some(n) => cursor = Some(n.to_owned()),
            None => return Ok(out),
        }
    }
    Err(PilotError::Step(format!(
        "graft {}: too many pages",
        args[0]
    )))
}

/// Per-line origin shas from `git blame`.
fn git_blame(bare: &Path, tip: &str, path: &str) -> Result<Vec<String>, PilotError> {
    git_blame_with(bare, tip, path, "--diff-algorithm=myers")
}

/// Per-line origin shas from `git blame`, diffing with `algorithm`.
fn git_blame_with(
    bare: &Path,
    tip: &str,
    path: &str,
    algorithm: &str,
) -> Result<Vec<String>, PilotError> {
    Ok(git_text(
        bare,
        &[
            "blame",
            "-s",
            "-l",
            "--root",
            "--first-parent",
            algorithm,
            tip,
            "--",
            path,
        ],
    )?
    .lines()
    .filter_map(|l| l.split_whitespace().next())
    .map(str::to_owned)
    .collect())
}

/// Decides whether two blame origins for one line are equivalent tie-breaks.
///
/// Identical lines (blank lines, `}`, the same dependency line in a
/// lock file) can be matched by a diff to any of their copies, so two
/// correct blames can name different commits for one line. The check does
/// not reason about contents. It runs the blames a correct tool gives,
/// one per diff, and accepts an origin only if one of them names it:
///
/// - `git blame --first-parent` with Myers diff (git's default),
/// - the same with histogram diff,
/// - graft's own rule replayed here: histogram diff with line
///   postprocessing (`imara-diff`, as `graft-index` runs it), carried
///   along the file's first-parent versions.
///
/// Two differing origins agree only if both are valid. Each blame is a
/// whole-file blame that uses one diff throughout, so every accepted origin
/// is one a real blame gives. A line moved, reordered, or deleted and
/// re-added is added by the later commit's diff under all three, so an
/// earlier commit that also wrote that content is never accepted. The path
/// is the first-parent one because both blames under test are first-parent
/// blames: a line that arrives from a merge's second parent is added by
/// the merge's first-parent diff, so the merge is its origin and the
/// side-branch commit never is.
///
/// Graft's coarse fallback (a diff over the work budget blames the middle
/// on the newer version) is not replayed: a coarse attribution is an
/// approximation, not a tie, and still counts as a disagreement.
///
/// The cost is two git blames and the file's first-parent versions, the
/// same ones blame walks, each read once and diffed once, and only when
/// some line disagrees.
struct TieCheck<'a> {
    bare: &'a Path,
    tip: &'a str,
    path: &'a str,
    /// The reference blames, per tip line; filled on first use.
    blames: Option<Vec<Vec<String>>>,
}

/// One first-parent version of a file: the commit that wrote it and the
/// blob it holds (`None` where the commit left no file there).
struct FileVersion {
    commit: String,
    blob: Option<String>,
}

impl<'a> TieCheck<'a> {
    fn new(bare: &'a Path, tip: &'a str, path: &'a str) -> Self {
        Self {
            bare,
            tip,
            path,
            blames: None,
        }
    }

    /// Whether `git` and `graft` (commit shas) are equivalent origins for
    /// line `line` (0-based) of the file at the tip.
    fn equivalent(&mut self, line: usize, git: &str, graft: &str) -> Result<bool, PilotError> {
        Ok(self.valid(line, git)? && self.valid(line, graft)?)
    }

    /// Whether one of the reference blames attributes line `line` to `sha`.
    fn valid(&mut self, line: usize, sha: &str) -> Result<bool, PilotError> {
        if self.blames.is_none() {
            self.blames = Some(vec![
                git_blame_with(self.bare, self.tip, self.path, "--diff-algorithm=myers")?,
                git_blame_with(self.bare, self.tip, self.path, "--histogram")?,
                self.graft_blame()?,
            ]);
        }
        Ok(self
            .blames
            .iter()
            .flatten()
            .any(|b| b.get(line).is_some_and(|o| o == sha)))
    }

    /// The file's first-parent versions, oldest first.
    fn versions(&self) -> Result<Vec<FileVersion>, PilotError> {
        let log = git_text(
            self.bare,
            &[
                "log",
                "--first-parent",
                "--no-renames",
                "--raw",
                "--no-abbrev",
                "--reverse",
                "--format=commit %H",
                self.tip,
                "--",
                self.path,
            ],
        )?;
        let mut out: Vec<FileVersion> = Vec::new();
        for l in log.lines() {
            if let Some(sha) = l.strip_prefix("commit ") {
                out.push(FileVersion {
                    commit: sha.to_owned(),
                    blob: None,
                });
            } else if let Some(raw) = l.strip_prefix(':') {
                // `:<old mode> <new mode> <old> <new> <status>\t<path>`.
                let (meta, p) = raw.split_once('\t').unwrap_or((raw, ""));
                let fields: Vec<&str> = meta.split(' ').collect();
                if p != self.path || fields.len() < 5 {
                    continue;
                }
                let (mode, blob) = (fields[1], fields[3]);
                if let Some(v) = out.last_mut() {
                    // A regular file or symlink has lines; a deletion
                    // (mode 000000) or a submodule has none.
                    let file = mode.starts_with("100") || mode == "120000";
                    v.blob = file.then(|| blob.to_owned());
                }
            }
        }
        Ok(out)
    }

    /// Graft's blame rule, replayed: per tip line, its origin when every
    /// version is diffed against the one before it as `graft-index` does.
    fn graft_blame(&self) -> Result<Vec<String>, PilotError> {
        let mut origins: Vec<String> = Vec::new();
        let mut old: Option<Vec<u8>> = None;
        for v in self.versions()? {
            let Some(blob) = &v.blob else {
                (origins, old) = (Vec::new(), None);
                continue;
            };
            let mut c = git(None);
            c.arg("--git-dir")
                .arg(self.bare)
                .args(["cat-file", "blob", blob]);
            let new = run(&mut c)?.stdout;
            origins = match &old {
                Some(old) => graft_alignment(old, &new)
                    .into_iter()
                    .map(|kept| kept.map_or_else(|| v.commit.clone(), |o| origins[o].clone()))
                    .collect(),
                None => vec![v.commit.clone(); split_lines(&new).len()],
            };
            old = Some(new);
        }
        Ok(origins)
    }
}

/// For each line of `new`, the line of `old` graft's diff keeps it as:
/// histogram diff with line postprocessing, as `graft-index` blame runs it.
fn graft_alignment(old: &[u8], new: &[u8]) -> Vec<Option<usize>> {
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let mut out = Vec::with_capacity(input.after.len());
    let mut kept = 0;
    for hunk in diff.hunks() {
        let unchanged = hunk.after.start as usize - out.len();
        out.extend((kept..kept + unchanged).map(Some));
        out.extend(hunk.after.map(|_| None));
        kept = hunk.before.end as usize;
    }
    let rest = input.after.len() - out.len();
    out.extend((kept..kept + rest).map(Some));
    out
}

/// A file's lines, without the empty piece after a final newline.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    body.split(|&b| b == b'\n').collect()
}

/// Per-line origin shas from `graft blame`, mapped through the mirror DB.
fn graft_blame(
    cli: &Cli<'_>,
    path: &str,
    sha_of: &BTreeMap<ChangesetId, String>,
) -> Result<Vec<String>, PilotError> {
    let (v, _) = cli.json(&["blame", path, "--at", "main"])?;
    let mut lines = Vec::new();
    for r in v["ranges"].as_array().cloned().unwrap_or_default() {
        let (start, end) = (
            r["start"].as_u64().unwrap_or(0),
            r["end"].as_u64().unwrap_or(0),
        );
        let id = r["origin"]["id"].as_str().unwrap_or("");
        let sha = ChangesetId::parse(id)
            .ok()
            .and_then(|id| sha_of.get(&id).cloned())
            .unwrap_or_else(|| "?".into());
        for _ in start..end {
            lines.push(sha.clone());
        }
    }
    Ok(lines)
}

fn check_agreement(
    cli: &Cli<'_>,
    bare: &Path,
    shape: &Shape,
    by_sha: &BTreeMap<String, ChangesetId>,
) -> Result<Agreement, PilotError> {
    let sha_of: BTreeMap<ChangesetId, String> =
        by_sha.iter().map(|(s, c)| (*c, s.clone())).collect();
    let mut a = Agreement::default();

    // log --path: same commits, same order.
    for path in [shape.hot_file.as_str(), shape.sparse_path.as_str()] {
        let graft: Vec<String> = all_pages(
            cli,
            &["log", "--path", path, "--at", "main", "--limit", "500"],
            "changesets",
        )?
        .iter()
        .map(|c| {
            ChangesetId::parse(c["id"].as_str().unwrap_or(""))
                .ok()
                .and_then(|id| sha_of.get(&id).cloned())
                .unwrap_or_else(|| "?".into())
        })
        .collect();
        let git: Vec<String> = git_text(
            bare,
            &[
                "log",
                "--first-parent",
                "--format=%H",
                &shape.tip,
                "--",
                path,
            ],
        )?
        .lines()
        .map(str::to_owned)
        .collect();
        if graft != git {
            a.notes.push(format!(
                "log --path {path}: graft lists {} commits, git {}",
                graft.len(),
                git.len()
            ));
        }
    }
    a.log = a.notes.is_empty();

    // diff <tip>~k..<tip>: the same paths.
    let k = 20.min(shape.first_parent.len().saturating_sub(1));
    let from_sha = &shape.first_parent[k];
    let from = by_sha
        .get(from_sha)
        .ok_or_else(|| PilotError::Mismatch(format!("{from_sha} is not imported")))?;
    let graft: BTreeSet<String> = all_pages(
        cli,
        &["diff", &from.to_string(), "main", "--limit", "500"],
        "files",
    )?
    .iter()
    .filter(|f| f["kind"].as_str() != Some("tree"))
    .filter_map(|f| f["path"].as_str().map(str::to_owned))
    .collect();
    let git: BTreeSet<String> = git_text(
        bare,
        &[
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            from_sha,
            &shape.tip,
        ],
    )?
    .split('\0')
    .filter(|s| !s.is_empty())
    .map(str::to_owned)
    .collect();
    a.diff = graft == git;
    if !a.diff {
        a.notes.push(format!(
            "diff ~{k}: graft names {} paths, git {} ({} only in graft, {} only in git)",
            graft.len(),
            git.len(),
            graft.difference(&git).count(),
            git.difference(&graft).count()
        ));
    }

    // blame: per-line origins on the hot file and a few others.
    let mut files = vec![shape.hot_file.clone()];
    files.extend(shape.blame_files.iter().take(5).cloned());
    for f in &files {
        let g = graft_blame(cli, f, &sha_of)?;
        let want = git_blame(bare, &shape.tip, f)?;
        let mut ties = TieCheck::new(bare, &shape.tip, f);
        let (mut same, mut tie) = (0, 0);
        for (i, (x, y)) in g.iter().zip(&want).enumerate() {
            if x == y {
                same += 1;
            } else if ties.equivalent(i, y, x)? {
                same += 1;
                tie += 1;
            }
        }
        a.blame_lines.0 += same;
        a.blame_lines.1 += want.len().max(g.len());
        a.blame_ties += tie;
        if same != want.len() || g.len() != want.len() {
            a.notes
                .push(format!("blame {f}: {same} of {} lines agree", want.len()));
        }
    }
    Ok(a)
}

/// Decodes every frame in a working copy's object cache and returns how
/// long the decoding took (reading the files is not counted).
fn decode_cache(wc: &Path) -> Result<(Duration, usize), PilotError> {
    let mut frames = Vec::new();
    let mut stack = vec![wc.join(".graft").join("cache")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let ft = e.file_type().map_err(|e| PilotError::io(&p, e))?;
            if ft.is_dir() {
                stack.push(p);
            } else if p
                .parent()
                .and_then(|d| d.parent())
                .and_then(Path::file_name)
                == Some("objects".as_ref())
            {
                frames.push(fs::read(&p).map_err(|e| PilotError::io(&p, e))?);
            }
        }
    }
    let start = Instant::now();
    for bytes in &frames {
        let frame = Frame::from_bytes(bytes)
            .map_err(|e| PilotError::Step(format!("cached frame: {e:?}")))?;
        let payload = frame
            .decode()
            .map_err(|e| PilotError::Step(format!("cached frame: {e:?}")))?;
        std::hint::black_box(payload);
    }
    Ok((start.elapsed(), frames.len()))
}

fn scenarios(
    cfg: &PilotConfig,
    cli: &Cli<'_>,
    bare: &Path,
    shape: &Shape,
    by_sha: &BTreeMap<String, ChangesetId>,
    clones: &Path,
    s: &mut Samples,
) -> Result<(), PilotError> {
    let n = cfg.iterations.max(1);
    let url = format!("file://{}", path_str(bare)?);
    let repo = cli.repo;

    // Full clone, and the decompression share of it (D5).
    let full_note = format!("{} files", shape.files_at_tip);
    for i in 0..n {
        let dir = clones.join(format!("full-{i}"));
        let ran = cli.run(&["clone", repo, path_str(&dir)?])?;
        s.add_time("full clone", "graft", &full_note, ran.elapsed);
        let (decode, frames) = decode_cache(&dir)?;
        s.add_time(
            "decompress frames",
            "graft",
            &format!("{frames} frames, gzip"),
            decode,
        );
        let share = 100.0 * decode.as_secs_f64() / ran.elapsed.as_secs_f64().max(1e-9);
        s.add("decompress share of full clone", "graft", "%", "D5", share);
        rm(&dir);
        let dir = clones.join(format!("git-full-{i}"));
        let mut c = git(None);
        c.args([
            "clone",
            "-q",
            "--depth",
            "1",
            "--no-tags",
            "--branch",
            &shape.branch,
            &url,
        ])
        .arg(&dir);
        s.add_time(
            "full clone",
            "git",
            "depth 1, file://",
            run(&mut c)?.elapsed,
        );
        rm(&dir);
    }

    // Sparse clone of one directory plus the root files.
    let sp = shape.sparse_path.as_str();
    let sparse_note = format!("{sp}/ + root files");
    for i in 0..n {
        let dir = clones.join(format!("sparse-{i}"));
        let ran = cli.run(&["clone", repo, path_str(&dir)?, "--path", sp, "--root-files"])?;
        s.add_time("sparse clone", "graft", &sparse_note, ran.elapsed);
        rm(&dir);
        let dir = clones.join(format!("git-sparse-{i}"));
        let mut c = git(None);
        c.args([
            "clone",
            "-q",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            "--no-tags",
        ])
        .args(["--branch", &shape.branch, &url])
        .arg(&dir);
        let first = run(&mut c)?.elapsed;
        let second = run(git(Some(&dir)).args(["sparse-checkout", "set", "--cone", sp]))?.elapsed;
        s.add_time(
            "sparse clone",
            "git",
            &format!("{sparse_note}, blob:none"),
            first + second,
        );
        rm(&dir);
    }

    // log --path on the hot file: one warm-up, then n samples.
    let hot = shape.hot_file.as_str();
    cli.run(&["log", "--path", hot, "--at", "main"])?;
    for _ in 0..n {
        s.add_time(
            "log --path",
            "graft",
            hot,
            cli.run(&["log", "--path", hot, "--at", "main"])?.elapsed,
        );
        s.add_time(
            "log --path",
            "git",
            hot,
            git_timed(
                bare,
                &[
                    "log",
                    "-n",
                    "50",
                    "--first-parent",
                    "--format=%H",
                    &shape.tip,
                    "--",
                    hot,
                ],
            )?,
        );
    }

    // diff <ref>~k..<ref>.
    let k = 20.min(shape.first_parent.len().saturating_sub(1));
    let from_sha = &shape.first_parent[k];
    if let Some(from) = by_sha.get(from_sha) {
        let from = from.to_string();
        let note = format!("main~{k}..main");
        for _ in 0..n {
            s.add_time(
                "diff",
                "graft",
                &format!("{note}, with patches"),
                cli.run(&["diff", &from, "main"])?.elapsed,
            );
            s.add_time(
                "diff",
                "git",
                &format!("{note}, with patches"),
                git_timed(bare, &["diff", "--no-renames", from_sha, &shape.tip])?,
            );
        }
    }

    // blame cold: the first blame of each of n files. Then cached: the hot
    // file again and again after one warm-up.
    for f in shape.blame_files.iter().take(n) {
        s.add_time(
            "blame cold",
            "graft",
            "first blame of a file",
            cli.run(&["blame", f, "--at", "main"])?.elapsed,
        );
        s.add_time(
            "blame cold",
            "git",
            "first blame of a file",
            git_timed(
                bare,
                &["blame", "-s", "--first-parent", &shape.tip, "--", f],
            )?,
        );
    }
    cli.run(&["blame", hot, "--at", "main"])?;
    for _ in 0..n {
        s.add_time(
            "blame cached",
            "graft",
            hot,
            cli.run(&["blame", hot, "--at", "main"])?.elapsed,
        );
        s.add_time(
            "blame cached",
            "git",
            hot,
            git_timed(
                bare,
                &["blame", "-s", "--first-parent", &shape.tip, "--", hot],
            )?,
        );
    }
    Ok(())
}

/// The phase 1 exit criterion 5 budgets (`docs/plan.md`), p50 in ms.
pub const EXIT_BUDGETS: &[(&str, f64)] = &[
    ("sparse clone", 50.0),
    ("log --path", 10.0),
    ("blame cached", 10.0),
    ("full clone", 150.0),
];

fn host_line() -> String {
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let cpus = std::thread::available_parallelism().map_or(0, usize::from);
    let git = Command::new("git")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!(
        "host `{}`, {cpus} CPUs, {git}, harness built `{profile}`",
        host.trim()
    )
}

fn report(cfg: &PilotConfig, shape: &Shape, o: &PilotOutcome) -> String {
    let mut r = String::new();
    let source = match &cfg.source {
        Source::Fixture(spec) => format!(
            "synthetic fixture (seed {}, {} commits)",
            spec.seed, spec.commits
        ),
        Source::Git(s) => format!("`{s}`"),
    };
    let _ = writeln!(r, "# graft pilot harness report\n");
    let _ = writeln!(
        r,
        "- Source: {source}, ref `{}` at `{}`",
        cfg.git_ref,
        &shape.tip[..12.min(shape.tip.len())]
    );
    let _ = writeln!(r, "- {}", host_line());
    let _ = writeln!(
        r,
        "- {} commits, {} files at the tip; {} run(s) × {} samples per scenario",
        o.commits,
        shape.files_at_tip,
        o.runs.len(),
        cfg.iterations
    );
    let _ = writeln!(
        r,
        "- Hot file `{}`; sparse path `{}/`\n",
        shape.hot_file, shape.sparse_path
    );

    let f = &o.fidelity;
    let _ = writeln!(r, "## Fidelity (exit criterion 1)\n");
    let _ = writeln!(
        r,
        "{} of {} changesets checked match `git checkout` exactly ({} entries compared: paths, file or symlink, exec bit, bytes, symlink targets).{}\n",
        f.matched,
        f.checked,
        f.entries,
        if f.checked < o.commits {
            format!(" Sampled {} of {} commits.", f.checked, o.commits)
        } else {
            String::new()
        }
    );
    for p in &f.problems {
        let _ = writeln!(r, "- {p}");
    }

    let a = &o.agreement;
    let _ = writeln!(r, "## Agreement with git (exit criterion 4)\n");
    let yes = |b: bool| if b { "agrees" } else { "**differs**" };
    let _ = writeln!(r, "- `log --path`: {}", yes(a.log));
    let _ = writeln!(r, "- `diff`: {}", yes(a.diff));
    let _ = writeln!(
        r,
        "- `blame`: {} of {} lines agree ({} as equivalent tie-breaks)",
        a.blame_lines.0, a.blame_lines.1, a.blame_ties
    );
    for n in &a.notes {
        let _ = writeln!(r, "  - {n}");
    }

    let _ = writeln!(r, "\n## Timings (all runs pooled)\n");
    r.push_str(&o.results.to_markdown());
    let _ = writeln!(
        r,
        "\nEvery time is end to end: process start to exit of `graft` or `git`. Blame cold is the first blame of each of several files; cached repeats the hot file. The decompression share is the time to decode the frames a full clone fetched, over the clone's time.\n"
    );

    let _ = writeln!(
        r,
        "## Budgets (exit criterion 5) and the later-phase 2× target\n"
    );
    let _ = writeln!(
        r,
        "| Scenario | graft p50 | Budget | Within | git p50 | git ÷ graft | 2× faster |"
    );
    let _ = writeln!(r, "| --- | ---: | ---: | --- | ---: | ---: | --- |");
    for (scenario, budget) in EXIT_BUDGETS {
        let Some(g) = o.results.get(scenario, "graft") else {
            continue;
        };
        let git = o.results.get(scenario, "git").map(|x| x.summary.p50);
        let ratio = git.map(|x| x / g.summary.p50.max(1e-9));
        let _ = writeln!(
            r,
            "| {scenario} | {:.2} ms | {budget} ms | {} | {} | {} | {} |",
            g.summary.p50,
            if g.summary.p50 <= *budget {
                "yes"
            } else {
                "**no**"
            },
            git.map_or(String::new(), |x| format!("{x:.2} ms")),
            ratio.map_or(String::new(), |x| format!("{x:.2}")),
            ratio.map_or(String::new(), |x| if x >= 2.0 {
                "yes".into()
            } else {
                "**no**".into()
            }),
        );
    }

    if !o.spreads.is_empty() && o.runs.len() > 1 {
        let _ = writeln!(
            r,
            "\n## Reproducibility across runs (acceptance: within 10%)\n"
        );
        r.push_str(&spread_markdown(&o.spreads, 0.10));
    }

    if let Some(c) = &o.crash {
        let _ = writeln!(r, "\n## Crash suite (exit criterion 2)\n");
        r.push_str(&c.markdown());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_say_what_failed() {
        let e = PilotError::Timeout("graft log after 1s".into());
        assert_eq!(e.to_string(), "timed out: graft log after 1s");
    }

    #[test]
    fn tokens_are_redacted_in_debug_output() {
        let t = format!("{:?}", proc::Token::new("secret-value"));
        assert!(!t.contains("secret-value"));
    }

    /// A bare repo built by `git fast-import`. `commits` are, in order,
    /// `(branch, from, merge, content of f.txt)`; `from` and `merge` are
    /// indices of earlier commits, and the file gets a final newline. Returns the repo's temp dir.
    fn tie_repo(
        name: &str,
        commits: &[(&str, Option<usize>, Option<usize>, &str)],
    ) -> crate::crash::TempDir {
        let dir = crate::crash::TempDir::new(name);
        let bare = dir.path().join("repo.git");
        run(git(None).arg("init").arg("-q").arg("--bare").arg(&bare)).expect("init");
        let mut stream = String::new();
        for (i, (branch, from, merge, content)) in commits.iter().enumerate() {
            let msg = format!("c{i}");
            let _ = write!(
                stream,
                "blob\nmark :{}\ndata {}\n{content}\n\n\
commit refs/heads/{branch}\nmark :{}\ncommitter t <t@example.com> {} +0000\ndata {}\n{msg}\n",
                1000 + i,
                content.len() + 1,
                i + 1,
                i + 1,
                msg.len(),
            );
            if let Some(f) = from {
                let _ = writeln!(stream, "from :{}", f + 1);
            }
            if let Some(m) = merge {
                let _ = writeln!(stream, "merge :{}", m + 1);
            }
            let _ = writeln!(stream, "M 100644 :{} f.txt", 1000 + i);
        }
        stream.push_str("done\n");
        let mut child = git(Some(&bare))
            .args(["fast-import", "--quiet", "--done"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("fast-import");
        std::io::Write::write_all(&mut child.stdin.take().expect("stdin"), stream.as_bytes())
            .expect("write stream");
        assert!(child.wait().expect("wait").success());
        dir
    }

    fn sha(bare: &Path, rev: &str) -> String {
        git_text(bare, &["rev-parse", rev])
            .expect("rev-parse")
            .trim()
            .to_owned()
    }

    /// git's own first-parent blame is always a valid attribution.
    fn git_blame_is_valid(ties: &mut TieCheck<'_>, bare: &Path) {
        for (i, origin) in git_blame(bare, "main", "f.txt")
            .expect("blame")
            .iter()
            .enumerate()
        {
            assert!(ties.valid(i, origin).expect("check"), "line {i}: {origin}");
        }
    }

    /// Graft's alignment pairs each kept line with an equal old line, in
    /// order, over many small files built from three repeated lines.
    #[test]
    fn graft_alignment_keeps_equal_lines_in_order() {
        let mut seed = 0x2222_u64;
        let mut file = |n: u64| {
            let mut out = Vec::new();
            for _ in 0..n {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                out.extend_from_slice([b"x\n", b"y\n", b"z\n"][(seed >> 33) as usize % 3]);
            }
            out
        };
        for i in 0..500 {
            let (old, new) = (file(i % 9), file(i % 7 + 1));
            let (a, b) = (split_lines(&old), split_lines(&new));
            let kept = graft_alignment(&old, &new);
            assert_eq!(kept.len(), b.len());
            let mut last = None;
            for (j, o) in kept.iter().enumerate() {
                if let Some(o) = *o {
                    assert_eq!(a[o], b[j]);
                    assert!(last < Some(o));
                    last = Some(o);
                }
            }
        }
    }

    /// `z x y z`, then `x x z x`. Both diffs keep a `z` and one `x`, but
    /// not the same `x`: Myers keeps the first and adds the last,
    /// histogram adds the first and keeps the last. So lines 0 and 3 are
    /// genuine ties, each validly c1's or c2's. Line 1 only c2 wrote, and
    /// line 2 (`z`) every diff keeps from c1.
    #[test]
    fn equivalent_tie_breaks_agree_and_wrong_origins_do_not() {
        let dir = tie_repo(
            "tie-check",
            &[
                ("main", None, None, "z\nx\ny\nz"),
                ("main", Some(0), None, "x\nx\nz\nx"),
            ],
        );
        let bare = dir.path().join("repo.git");
        let (c2, c1) = (sha(&bare, "main"), sha(&bare, "main~1"));

        let mut ties = TieCheck::new(&bare, "main", "f.txt");
        git_blame_is_valid(&mut ties, &bare);
        assert!(ties.equivalent(0, &c2, &c1).expect("check"));
        assert!(ties.equivalent(3, &c2, &c1).expect("check"));
        assert!(!ties.equivalent(1, &c2, &c1).expect("check"));
        assert!(!ties.equivalent(2, &c1, &c2).expect("check"));
        // A commit outside the file's history is never equivalent.
        let other = "0".repeat(40);
        assert!(!ties.equivalent(3, &c2, &other).expect("check"));
    }

    /// Fixtures where a later commit rewrites a line whose content an
    /// earlier commit also wrote, and the content never leaves the file.
    /// `(commits, line, its only origin, a stale one)`; commits are
    /// first-parent, `c<i>` is commit `i`.
    #[test]
    fn a_moved_line_belongs_to_the_commit_that_moved_it() {
        let cases: [(&str, &[&str], usize, usize, usize); 4] = [
            // The reviewer's case: `a` moved to the end.
            ("move", &["a\nb\nc", "b\nc\na"], 2, 1, 0),
            // A reorder: `p q r s` reversed. Each diff keeps one line, and
            // none keeps `p` (now last).
            ("reorder", &["p\nq\nr\ns", "s\nr\nq\np"], 3, 1, 0),
            // A move across hunks: `a` leaves the top, `b` changes in the
            // middle, `a` lands at the end.
            (
                "across-hunks",
                &["a\nm1\nm2\nb\nn1\nn2", "m1\nm2\nB\nn1\nn2\na"],
                5,
                1,
                0,
            ),
            // `a` in two hunks; the later commit drops the first and moves
            // the second past `n`, so the count drops but `a` stays.
            (
                "net-move",
                &["a\nm1\nm2\nm3\na\nn1\nn2\nn3", "m1\nm2\nm3\nn1\nn2\nn3\na"],
                6,
                1,
                0,
            ),
        ];
        for (name, contents, line, origin, stale) in cases {
            let commits: Vec<(&str, Option<usize>, Option<usize>, &str)> = contents
                .iter()
                .enumerate()
                .map(|(i, c)| ("main", i.checked_sub(1), None, *c))
                .collect();
            let dir = tie_repo(&format!("tie-{name}"), &commits);
            let bare = dir.path().join("repo.git");
            let at = |i: usize| sha(&bare, &format!("main~{}", contents.len() - 1 - i));
            let (origin, stale) = (at(origin), at(stale));

            let mut ties = TieCheck::new(&bare, "main", "f.txt");
            git_blame_is_valid(&mut ties, &bare);
            assert!(ties.valid(line, &origin).expect("check"), "{name}");
            assert!(!ties.valid(line, &stale).expect("check"), "{name}");
            assert!(
                !ties.equivalent(line, &origin, &stale).expect("check"),
                "{name}"
            );
        }
    }

    /// `a x`, then `a` (x deleted), then `a x` again. c1 wrote `x`, but the
    /// `x` at the tip is c3's: naming c1 is a stale origin.
    #[test]
    fn a_line_deleted_and_re_added_is_not_a_tie() {
        let dir = tie_repo(
            "tie-readd",
            &[
                ("main", None, None, "a\nx"),
                ("main", Some(0), None, "a"),
                ("main", Some(1), None, "a\nx"),
            ],
        );
        let bare = dir.path().join("repo.git");
        let (c3, c1) = (sha(&bare, "main"), sha(&bare, "main~2"));

        let mut ties = TieCheck::new(&bare, "main", "f.txt");
        git_blame_is_valid(&mut ties, &bare);
        assert!(ties.valid(1, &c3).expect("check"));
        assert!(!ties.equivalent(1, &c3, &c1).expect("check"));
        // `a` never left, so c1 stays its origin.
        assert!(ties.valid(0, &c1).expect("check"));
    }

    /// main has `x`; a side branch makes it `x x y`; main merges the side.
    /// Under first-parent blame the merge's diff keeps the first `x` (c1's)
    /// and adds the second `x` and `y`, so those are the merge's. The side
    /// commit that wrote them is never an origin, and c1, which wrote the
    /// same content, is not one for the second `x`.
    #[test]
    fn a_line_from_a_merges_second_parent_belongs_to_the_merge() {
        let dir = tie_repo(
            "tie-merge",
            &[
                ("main", None, None, "x"),
                ("side", Some(0), None, "x\nx\ny"),
                ("main", Some(0), Some(1), "x\nx\ny"),
            ],
        );
        let bare = dir.path().join("repo.git");
        let (m, c1, s1) = (sha(&bare, "main"), sha(&bare, "main^1"), sha(&bare, "side"));

        let mut ties = TieCheck::new(&bare, "main", "f.txt");
        git_blame_is_valid(&mut ties, &bare);
        assert!(ties.valid(0, &c1).expect("check"));
        assert!(!ties.valid(0, &s1).expect("check"));
        assert!(ties.valid(1, &m).expect("check"));
        assert!(!ties.equivalent(1, &m, &c1).expect("check"));
        assert!(!ties.equivalent(1, &m, &s1).expect("check"));
        // `y` only the merge brought in on the first-parent path.
        assert!(ties.valid(2, &m).expect("check"));
        assert!(!ties.valid(2, &c1).expect("check"));
        assert!(!ties.valid(2, &s1).expect("check"));
    }
}
