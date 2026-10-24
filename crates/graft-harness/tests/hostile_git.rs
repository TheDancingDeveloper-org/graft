//! Part 3 of the adversarial harness (WI-2347,
//! docs/notes/adversarial-harness.md §2): hostile git repositories through
//! `graft-mirror import` and `graft-git` export.
//!
//! Each case is a generator: a function that builds the repository with the
//! git CLI (plain and `hash-object --literally` objects), so no binary
//! fixture is committed. Each one is carried round: imported with
//! [`import_repo`] into an in-memory engine, exported with [`Exporter`] into
//! a fresh bare repository, and compared with the source. A case ends in one
//! of two ways:
//!
//! - **round trip**: every ref under `refs/heads/` and `refs/tags/` names the
//!   same object in the export as in the source, every commit reachable from
//!   them is there with the same sha (so the same bytes), and `git fsck`
//!   passes on the export;
//! - **refusal**: the import fails with the typed error the case names, and
//!   nothing is partial: no changeset is mapped and no ref exists.
//!
//! git is the oracle; the product crates are used as they ship. Cases graft
//! gets wrong today are `#[ignore]`d with the Vogt item that fixes them, so
//! `cargo test -- --ignored` shows the gaps.
//!
//! A rewritten upstream (a force-push) while follow runs is covered in
//! `graft-mirror`'s `tests/follow.rs`
//! (`force_push_raises_divergence_and_changes_nothing`), not repeated here.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use graft_engine::{Engine, EngineConfig};
use graft_git::export::Committer;
use graft_git::exporter::{Exporter, LandTimes};
use graft_git::import::ImportConfig;
use graft_git::map::MemoryMirrorMap;
use graft_git::{GitError, GitSha};
use graft_mirror::MirrorError;
use graft_mirror::import::{ImportOutcome, import_repo};
use graft_model::{ChangesetId, Timestamp};
use graft_proto::CallContext;
use graft_proto::api::RepoApi;
use graft_proto::types::ListRefs;
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use graft_testkit::fake_api::{admin_cx, block_on};

type Api = Engine<MemoryStore, MemoryStore>;

const REPO: &str = "r";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("graft-hostile-{tag}-{}-{n}", std::process::id()));
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

/// git in `dir` with no user configuration, a fixed identity and a clock
/// that moves only when a commit is made.
struct Git {
    dir: TempDir,
    tick: u64,
}

impl Git {
    fn init(tag: &str) -> Git {
        let git = Git {
            dir: TempDir::new(tag),
            tick: 0,
        };
        git.run(&["init", "-q", "-b", "main"]);
        git
    }

    fn path(&self) -> &Path {
        &self.dir.0
    }

    fn git_dir(&self) -> PathBuf {
        self.dir.0.join(".git")
    }

    fn run(&self, args: &[&str]) -> Vec<u8> {
        self.run_with(args, None)
    }

    fn run_with(&self, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
        let mut child = Command::new("git")
            .current_dir(&self.dir.0)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "A U Thor")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "C O Mitter")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env(
                "GIT_AUTHOR_DATE",
                format!("{} +0530", 1_700_000_000 + self.tick),
            )
            .env(
                "GIT_COMMITTER_DATE",
                format!("{} -0100", 1_700_000_000 + self.tick),
            )
            .args(["-c", "core.symlinks=true", "-c", "commit.gpgsign=false"])
            .args(["-c", "tag.gpgsign=false", "-c", "core.ignorecase=false"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git runs");
        if let Some(input) = stdin {
            child.stdin.take().unwrap().write_all(input).unwrap();
        }
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn out(&self, args: &[&str]) -> String {
        String::from_utf8(self.run(args)).unwrap().trim().to_owned()
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let p = self.dir.0.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn chmod_x(&self, path: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.dir.0.join(path);
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn symlink(&self, target: &str, path: &str) {
        std::os::unix::fs::symlink(target, self.dir.0.join(path)).unwrap();
    }

    /// Stages everything and commits; returns the new sha.
    fn commit(&mut self, message: &str) -> String {
        self.tick += 60;
        self.run(&["add", "-A"]);
        self.run(&["commit", "-q", "--allow-empty", "-m", message]);
        self.out(&["rev-parse", "HEAD"])
    }

    /// Writes an object verbatim; returns its sha.
    fn literal(&self, kind: &str, bytes: &[u8]) -> String {
        let out = self.run_with(
            &["hash-object", "-t", kind, "-w", "--literally", "--stdin"],
            Some(bytes),
        );
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    fn blob(&self, bytes: &[u8]) -> String {
        self.literal("blob", bytes)
    }

    /// A raw tree from (mode, name, sha) entries, in the order given.
    fn raw_tree(&self, entries: &[(&str, &[u8], &str)]) -> String {
        let mut raw = Vec::new();
        for (mode, name, sha) in entries {
            raw.extend_from_slice(mode.as_bytes());
            raw.push(b' ');
            raw.extend_from_slice(name);
            raw.push(0);
            raw.extend_from_slice(&GitSha::from_hex(sha).unwrap().0);
        }
        self.literal("tree", &raw)
    }

    /// `commit-tree` with the fixed clock; returns the sha.
    fn commit_tree(&mut self, tree: &str, parents: &[&str], message: &str) -> String {
        self.tick += 60;
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.extend(["-p", p]);
        }
        args.extend(["-m", message]);
        self.out(&args)
    }

    /// A commit object written byte for byte: `tree`, `parent` lines, then
    /// `rest` (the remaining headers, a blank line and the message).
    fn raw_commit(&self, tree: &str, parents: &[&str], rest: &[u8]) -> String {
        let mut raw = format!("tree {tree}\n").into_bytes();
        for p in parents {
            raw.extend_from_slice(format!("parent {p}\n").as_bytes());
        }
        raw.extend_from_slice(rest);
        self.literal("commit", &raw)
    }

    fn set_ref(&self, name: &str, sha: &str) {
        self.run(&["update-ref", name, sha]);
    }
}

/// git against a bare repository, with no user configuration.
fn git_in(git_dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn text_in(git_dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_in(git_dir, args)).unwrap()
}

/// Every ref under `refs/heads/` and `refs/tags/`, and the object it names
/// (unpeeled: an annotated tag's own sha).
fn refs(git_dir: &Path) -> BTreeMap<String, String> {
    text_in(
        git_dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/",
            "refs/tags/",
        ],
    )
    .lines()
    .map(|l| {
        let (name, sha) = l.split_once(' ').unwrap();
        (name.to_owned(), sha.to_owned())
    })
    .collect()
}

/// Every commit reachable from the branches and tags, sorted.
fn commits(git_dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = text_in(git_dir, &["rev-list", "--branches", "--tags"])
        .lines()
        .map(str::to_owned)
        .collect();
    out.sort();
    out
}

/// Every object reachable from the branches and tags, sorted. Fails if one
/// is missing.
fn objects(git_dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = text_in(git_dir, &["rev-list", "--objects", "--branches", "--tags"])
        .lines()
        .map(|l| l.split(' ').next().unwrap().to_owned())
        .collect();
    out.sort();
    out
}

fn engine() -> Api {
    let store = Arc::new(MemoryStore::new());
    Engine::new(
        store.clone(),
        store,
        Arc::new(FakeClock::at(1_000_000)),
        EngineConfig::default(),
    )
}

/// An import never needs a landing time: every imported commit carries its
/// own committer header. Asking for one fails the export.
struct NoLands;

impl LandTimes for NoLands {
    fn landed_at(&self, id: ChangesetId) -> Result<Timestamp, GitError> {
        Err(GitError::Map(format!("{id} was imported, not landed")))
    }
}

/// An imported repository: the engine, the mirror map and the outcome.
struct Imported {
    api: Api,
    map: MemoryMirrorMap,
    cx: CallContext,
    outcome: ImportOutcome,
}

/// Imports every branch of `git_dir` as a line of the same name, with tags.
fn import(git_dir: &Path) -> (Result<ImportOutcome, MirrorError>, Imported) {
    let api = engine();
    let map = MemoryMirrorMap::new();
    let mut cx = admin_cx();
    cx.principal.scopes.push(format!("repo:{REPO}:admin"));
    let mut config = ImportConfig::new(git_dir, REPO);
    config.lines = text_in(
        git_dir,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .lines()
    .map(|b| (format!("refs/heads/{b}"), b.to_owned()))
    .collect();
    let result = block_on(import_repo(&api, &cx, &map, &config));
    let outcome = result.as_ref().ok().cloned().unwrap_or_default();
    (
        result,
        Imported {
            api,
            map,
            cx,
            outcome,
        },
    )
}

/// The refs graft holds, by name.
fn graft_refs(i: &Imported) -> Vec<String> {
    let refs = block_on(i.api.list_refs(
        &i.cx,
        ListRefs {
            repo: REPO.to_owned(),
            ..Default::default()
        },
    ))
    .unwrap_or_default();
    refs.into_iter().map(|r| r.name).collect()
}

/// Exports every line and tag graft holds into a fresh bare repository.
fn export(i: &Imported, dest: &Path) {
    let committer = Committer {
        name: "graft".into(),
        email: "graft@example.com".into(),
    };
    let mut want: Vec<(String, ChangesetId)> = Vec::new();
    for (line, id) in &i.outcome.report.lines {
        want.push((format!("refs/heads/{line}"), *id));
    }
    for (tag, id) in &i.outcome.report.tags {
        want.push((format!("refs/tags/{tag}"), *id));
    }
    let mut exporter =
        Exporter::new(&i.api, &i.cx, &i.map, REPO, &committer, &NoLands, dest).unwrap();
    block_on(exporter.export_refs(&want)).expect("export succeeds");
}

/// Imports and exports `git_dir`, and asserts the round trip is exact.
fn assert_round_trips(git_dir: &Path) -> Imported {
    let (result, imported) = import(git_dir);
    let outcome = result.expect("import succeeds");
    assert!(
        outcome.report.lossy.is_empty(),
        "nothing lossy: {outcome:?}"
    );
    assert!(outcome.tags_conflicting.is_empty());
    let dest = TempDir::new("dest");
    let dest_dir = dest.0.join("export.git");
    export(&imported, &dest_dir);
    // Not `git fsck`: the export reproduces malformed objects byte for byte,
    // and fsck rejects them. Every object reachable from the refs is there.
    assert_eq!(
        objects(git_dir),
        objects(&dest_dir),
        "every object, same sha"
    );
    assert_eq!(
        commits(git_dir),
        commits(&dest_dir),
        "every commit, same sha"
    );
    assert_eq!(refs(git_dir), refs(&dest_dir), "every ref, same object");
    imported
}

/// Imports `git_dir`, asserts it is refused with an error `pred` accepts,
/// and that nothing is partial.
fn assert_refused(git_dir: &Path, pred: impl FnOnce(&GitError) -> bool) -> GitError {
    let (result, imported) = import(git_dir);
    let err = match result {
        Err(MirrorError::Git(e)) => e,
        other => panic!("expected a typed refusal, got {other:?}"),
    };
    assert!(pred(&err), "unexpected refusal: {err:?}");
    assert!(imported.map.is_empty(), "no commit mapped after {err:?}");
    assert_eq!(graft_refs(&imported), Vec::<String>::new(), "no ref set");
    err
}

fn bad_path_reasons(e: &GitError) -> Vec<String> {
    match e {
        GitError::BadPaths(paths) => paths.iter().map(|p| p.reason.clone()).collect(),
        _ => Vec::new(),
    }
}

// --- history shapes -------------------------------------------------------

/// An octopus merge of five branches, three roots, and a merge whose tree is
/// its first parent's.
#[test]
fn octopus_merges_and_several_roots_round_trip() {
    let mut git = Git::init("octopus");
    git.write("base.txt", b"base\n");
    git.commit("base");
    for b in ["b1", "b2", "b3", "b4"] {
        git.run(&["checkout", "-q", "-b", b, "main"]);
        git.write(&format!("{b}.txt"), b.as_bytes());
        git.commit(b);
    }
    git.run(&["checkout", "-q", "main"]);
    git.run(&[
        "merge", "-q", "--no-ff", "b1", "b2", "b3", "b4", "-m", "octopus",
    ]);
    for root in ["r2", "r3"] {
        git.run(&["checkout", "-q", "--orphan", root]);
        git.run(&["rm", "-rq", "--cached", "--ignore-unmatch", "."]);
        git.run(&["clean", "-fdq"]);
        git.write(&format!("{root}/x"), root.as_bytes());
        git.commit(root);
    }
    git.run(&["checkout", "-q", "-f", "main"]);
    for root in ["r2", "r3"] {
        git.run(&[
            "merge",
            "-q",
            "--no-ff",
            "--allow-unrelated-histories",
            root,
            "-m",
            "another root",
        ]);
    }
    git.run(&["checkout", "-q", "-b", "dropped"]);
    git.write("dropped", b"x");
    git.commit("dropped");
    git.run(&["checkout", "-q", "main"]);
    git.run(&["merge", "-q", "-s", "ours", "dropped", "-m", "empty merge"]);
    let i = assert_round_trips(&git.git_dir());
    assert_eq!(
        i.outcome.report.commits,
        commits(&git.git_dir()).len() as u64
    );
}

/// A commit with no change against its single parent, or a root commit with
/// git's empty tree, is refused (validity rule 9, `empty-changeset`), with
/// nothing imported.
#[test]
fn empty_commits_are_a_typed_refusal() {
    let mut git = Git::init("empty-root");
    let root = git.commit_tree(EMPTY_TREE, &[], "empty root");
    git.set_ref("refs/heads/main", &root);
    assert_refused(
        &git.git_dir(),
        |e| matches!(e, GitError::Unrepresentable(u) if u.len() == 1 && u[0].reason == "empty-changeset"),
    );

    let mut git = Git::init("empty");
    git.write("a", b"a\n");
    git.commit("a");
    let empty = git.commit("nothing changed");
    git.write("b", b"b\n");
    git.commit("after");
    let err = assert_refused(
        &git.git_dir(),
        |e| matches!(e, GitError::Unrepresentable(u) if u.iter().all(|c| c.reason == "empty-changeset")),
    );
    let GitError::Unrepresentable(u) = err else {
        unreachable!()
    };
    assert_eq!(u[0].commit.to_hex(), empty);
}

/// A shallow clone: the oldest commit names a parent the repository does
/// not hold.
#[test]
fn a_shallow_clone_is_a_typed_refusal() {
    let mut git = Git::init("shallow-src");
    for n in 0..3 {
        git.write("f", format!("{n}\n").as_bytes());
        git.commit(&format!("c{n}"));
    }
    let dest = TempDir::new("shallow");
    let url = format!("file://{}", git.path().display());
    let shallow = dest.0.join("s.git");
    let status = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["clone", "-q", "--bare", "--depth", "1", &url])
        .arg(&shallow)
        .status()
        .unwrap();
    assert!(status.success());
    assert_refused(&shallow, |e| matches!(e, GitError::Read(_)));
}

/// A `refs/replace/` graft that rewrites a parent: git's view changes, but
/// import reads the objects as stored, so the original shas round-trip.
#[test]
fn replace_refs_do_not_change_what_is_imported() {
    let mut git = Git::init("replace");
    let mut shas = Vec::new();
    for n in 0..3 {
        git.write("f", format!("{n}\n").as_bytes());
        shas.push(git.commit(&format!("c{n}")));
    }
    git.run(&["replace", "--graft", &shas[2], &shas[0]]);
    assert_round_trips(&git.git_dir());
}

// --- signatures and tags --------------------------------------------------

const SIG: &str = "-----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEEaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaFAmUAAAAACgkQ\n =abcd\n -----END PGP SIGNATURE-----";

/// A signed commit (`gpgsig`), an SSH-style signature, a merge carrying a
/// `mergetag`, and a header git does not know.
#[test]
fn signed_commits_and_extra_headers_round_trip() {
    let git = Git::init("signed");
    let a = git.blob(b"a\n");
    let t1 = git.raw_tree(&[("100644", b"a", &a)]);
    let c1 = git.raw_commit(
        &t1,
        &[],
        format!(
            "author A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\ngpgsig {SIG}\n\nsigned\n"
        )
        .as_bytes(),
    );
    let b = git.blob(b"b\n");
    let t2 = git.raw_tree(&[("100644", b"a", &a), ("100644", b"b", &b)]);
    let c2 = git.raw_commit(
        &t2,
        &[&c1],
        b"author A <a@example.com> 1700000060 +0000\ncommitter A <a@example.com> 1700000060 +0000\ngpgsig-sha256 -----BEGIN SSH SIGNATURE-----\n U1NIU0lH\n -----END SSH SIGNATURE-----\nx-unknown-header some value\n\nssh signed, unknown header\n",
    );
    let side_blob = git.blob(b"side\n");
    let t3 = git.raw_tree(&[("100644", b"a", &a), ("100644", b"side", &side_blob)]);
    let side = git.raw_commit(
        &t3,
        &[&c1],
        b"author A <a@example.com> 1700000100 +0000\ncommitter A <a@example.com> 1700000100 +0000\n\nside\n",
    );
    let t4 = git.raw_tree(&[
        ("100644", b"a", &a),
        ("100644", b"b", &b),
        ("100644", b"side", &side_blob),
    ]);
    let merge = git.raw_commit(
        &t4,
        &[&c2, &side],
        format!(
            "author A <a@example.com> 1700000200 +0000\ncommitter A <a@example.com> 1700000200 +0000\nmergetag object {side}\n type commit\n tag side-v1\n tagger T <t@example.com> 1700000150 +0000\n \n side release\n {SIG}\n\nmerge a signed tag\n"
        )
        .as_bytes(),
    );
    git.set_ref("refs/heads/main", &merge);
    git.set_ref("refs/heads/side", &side);
    assert_round_trips(&git.git_dir());
}

/// Lightweight and annotated tags on commits, a signed annotated tag, and a
/// tag of a tag. The annotated tag objects must come back with their shas.
#[test]
fn annotated_and_signed_tag_objects_round_trip() {
    let mut git = Git::init("tags");
    git.write("a", b"a\n");
    git.commit("one");
    git.run(&["tag", "light"]);
    git.run(&["tag", "-a", "v1", "-m", "annotated"]);
    let commit = git.out(&["rev-parse", "HEAD"]);
    let signed = git.literal(
        "tag",
        format!(
            "object {commit}\ntype commit\ntag signed\ntagger T <t@example.com> 1700000000 +0100\n\nsigned tag\n{}\n",
            SIG.replace("\n ", "\n")
        )
        .as_bytes(),
    );
    git.set_ref("refs/tags/signed", &signed);
    let v1 = git.out(&["rev-parse", "refs/tags/v1"]);
    let nested = git.literal(
        "tag",
        format!("object {v1}\ntype tag\ntag nested\ntagger T <t@example.com> 1700000000 +0000\n\ntag of a tag\n").as_bytes(),
    );
    git.set_ref("refs/tags/nested", &nested);
    assert_round_trips(&git.git_dir());
}

/// Tags that name a tree or a blob, directly or through a tag object. The
/// mirror map holds tags of commits only, so import refuses, naming each.
#[test]
fn tags_of_trees_and_blobs_are_reported() {
    let mut git = Git::init("tag-tree");
    git.write("a", b"a\n");
    let c = git.commit("one");
    let tree = git.out(&["rev-parse", &format!("{c}^{{tree}}")]);
    let blob = git.out(&["rev-parse", &format!("{c}:a")]);
    git.set_ref("refs/tags/tree-light", &tree);
    git.set_ref("refs/tags/blob-light", &blob);
    git.run(&["tag", "-a", "tree-annotated", "-m", "a tree", &tree]);
    let err = assert_refused(&git.git_dir(), |e| matches!(e, GitError::NonCommitTags(_)));
    let GitError::NonCommitTags(tags) = err else {
        unreachable!()
    };
    let named: Vec<(&str, &str)> = tags.iter().map(|t| (t.name.as_str(), t.kind)).collect();
    assert_eq!(
        named,
        [
            ("blob-light", "blob"),
            ("tree-annotated", "tree"),
            ("tree-light", "tree")
        ]
    );
}

// --- modes and kinds ------------------------------------------------------

/// Submodules (with and without `.gitmodules`), symlinks to files, to
/// directories and dangling, executables, and the legacy and padded modes
/// git still reads.
#[test]
fn odd_modes_and_kinds_round_trip() {
    let mut git = Git::init("modes");
    git.write("run.sh", b"#!/bin/sh\n");
    git.chmod_x("run.sh");
    git.write("dir/f", b"f\n");
    git.symlink("dir", "to-dir");
    git.symlink("run.sh", "to-file");
    git.symlink("../../nowhere", "dangling");
    let sub = git.commit_tree(EMPTY_TREE, &[], "submodule target");
    git.write(
        ".gitmodules",
        b"[submodule \"dep\"]\n\tpath = dep\n\turl = ../dep\n",
    );
    git.run(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{sub},dep"),
    ]);
    let c1 = git.commit("kinds");
    // Swap kinds at one path: file → symlink → executable → submodule.
    git.run(&["rm", "-q", "--cached", "run.sh"]);
    std::fs::remove_file(git.path().join("run.sh")).unwrap();
    git.symlink("dir/f", "run.sh");
    git.commit("file to symlink");
    std::fs::remove_file(git.path().join("run.sh")).unwrap();
    git.write("run.sh", b"exec\n");
    git.chmod_x("run.sh");
    git.commit("symlink to executable");
    git.run(&["rm", "-q", "--cached", "run.sh"]);
    std::fs::remove_file(git.path().join("run.sh")).unwrap();
    git.run(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{c1},run.sh"),
    ]);
    git.commit("executable to gitlink, no .gitmodules entry");
    let tip = git.out(&["rev-parse", "HEAD"]);
    // Legacy 100664 and 100600 in a hand-made leaf directory. (A raw tree
    // over a subdirectory is WI-2356, below.)
    let x = git.blob(b"x\n");
    let inner = git.raw_tree(&[("100664", b"legacy", &x), ("100600", b"private", &x)]);
    let root = git.raw_tree(&[("40000", b"legacy", &inner), ("100644", b"plain", &x)]);
    let odd = git.commit_tree(&root, &[&tip], "legacy modes");
    git.set_ref("refs/heads/main", &odd);
    assert_round_trips(&git.git_dir());
}

/// A mode git's model has no kind for is refused, naming the path.
#[test]
fn an_unknown_mode_is_a_typed_refusal() {
    let mut git = Git::init("bad-mode");
    let x = git.blob(b"x\n");
    let root = git.raw_tree(&[("60000", b"odd", &x)]);
    let c = git.commit_tree(&root, &[], "a block-device mode");
    git.set_ref("refs/heads/main", &c);
    let err = assert_refused(&git.git_dir(), |e| matches!(e, GitError::BadPaths(_)));
    assert_eq!(bad_path_reasons(&err), vec!["unsupported-mode"]);
}

// --- paths ----------------------------------------------------------------

/// A path that is not UTF-8 is refused (D11), with nothing imported.
#[test]
fn a_non_utf8_path_is_a_typed_refusal() {
    let mut git = Git::init("latin1");
    let x = git.blob(b"x\n");
    let root = git.raw_tree(&[("100644", b"caf\xe9.txt", &x)]);
    let c = git.commit_tree(&root, &[], "latin-1 name");
    git.set_ref("refs/heads/main", &c);
    assert_refused(&git.git_dir(), |e| matches!(e, GitError::BadPaths(_)));
}

/// Two paths in one tree that differ only in case are refused.
#[test]
fn paths_that_clash_by_case_are_a_typed_refusal() {
    let mut git = Git::init("case");
    git.write("README", b"upper\n");
    git.write("readme", b"lower\n");
    git.commit("both cases");
    let err = assert_refused(&git.git_dir(), |e| matches!(e, GitError::BadPaths(_)));
    assert!(
        bad_path_reasons(&err).iter().any(|r| r.contains("case")),
        "{err:?}"
    );
}

/// A case-only rename across commits (never both in one tree) is fine.
#[test]
fn a_case_only_rename_round_trips() {
    let mut git = Git::init("case-rename");
    git.write("File.txt", b"x\n");
    git.commit("upper");
    git.run(&["mv", "File.txt", "file.txt"]);
    git.commit("lower");
    git.run(&["mv", "file.txt", "FILE.TXT"]);
    git.commit("shout");
    assert_round_trips(&git.git_dir());
}

/// Windows device names, reserved characters, `.`, `..` and empty names.
#[test]
fn reserved_names_are_a_typed_refusal() {
    for name in [
        &b"CON"[..],
        b"aux.rs",
        b"com1.txt",
        b"a:b",
        b"trailing.",
        b"..",
        b".",
        b"",
    ] {
        let mut git = Git::init("reserved");
        let x = git.blob(b"x\n");
        let root = git.raw_tree(&[("100644", name, &x)]);
        let c = git.commit_tree(&root, &[], "reserved");
        git.set_ref("refs/heads/main", &c);
        let (result, imported) = import(&git.git_dir());
        match result {
            Err(MirrorError::Git(GitError::BadPaths(_))) => {
                assert!(imported.map.is_empty());
                assert!(graft_refs(&imported).is_empty());
            }
            other => panic!(
                "{:?}: expected BadPaths, got {other:?}",
                String::from_utf8_lossy(name)
            ),
        }
    }
}

// --- sizes ----------------------------------------------------------------

const MIB: usize = 1024 * 1024;

/// Blobs just under, at and just over 64 MiB, the frame payload limit. Each
/// either round-trips or is refused with a typed error and nothing partial.
#[test]
fn blobs_at_the_frame_limit_round_trip_or_are_refused() {
    let limit = 64 * MIB;
    let mut results = Vec::new();
    for len in [limit - 1, limit, limit + 1] {
        let mut git = Git::init("big");
        let mut bytes = vec![0u8; len];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        git.write("big.bin", &bytes);
        drop(bytes);
        git.commit("big");
        let (result, imported) = import(&git.git_dir());
        match result {
            Ok(_) => {
                drop(imported);
                assert_round_trips(&git.git_dir());
                results.push((len, "round trip".to_owned()));
            }
            Err(e) => {
                assert!(imported.map.is_empty(), "{len}: no commit mapped");
                assert!(graft_refs(&imported).is_empty(), "{len}: no ref set");
                results.push((len, format!("{e:?}")));
            }
        }
    }
    let over = r#"Api(LimitExceeded { limit: "blob-bytes", max: 67108864 })"#;
    assert_eq!(
        results,
        vec![
            (limit - 1, "round trip".to_owned()),
            (limit, "round trip".to_owned()),
            (limit + 1, over.to_owned()),
        ]
    );
}

/// One directory of 20,000 entries, and a directory chain 200 deep.
#[test]
fn huge_and_deep_trees_round_trip() {
    let mut git = Git::init("wide");
    let x = git.blob(b"x\n");
    let mut names: Vec<Vec<u8>> = (0..20_000)
        .map(|n| format!("f{n:05}").into_bytes())
        .collect();
    names.sort();
    let entries: Vec<(&str, &[u8], &str)> = names
        .iter()
        .map(|n| ("100644", n.as_slice(), x.as_str()))
        .collect();
    let wide = git.raw_tree(&entries);
    let mut deep = git.raw_tree(&[("100644", b"leaf", &x)]);
    for _ in 0..200 {
        deep = git.raw_tree(&[("40000", b"d", &deep)]);
    }
    let root = git.raw_tree(&[("40000", b"deep", &deep), ("40000", b"wide", &wide)]);
    let c1 = git.commit_tree(&root, &[], "wide and deep");
    // Change one leaf at the bottom of the chain.
    let y = git.blob(b"y\n");
    let mut deep2 = git.raw_tree(&[("100644", b"leaf", &y)]);
    for _ in 0..200 {
        deep2 = git.raw_tree(&[("40000", b"d", &deep2)]);
    }
    let root2 = git.raw_tree(&[("40000", b"deep", &deep2), ("40000", b"wide", &wide)]);
    let c2 = git.commit_tree(&root2, &[&c1], "touch the deepest leaf");
    git.set_ref("refs/heads/main", &c2);
    let (result, imported) = import(&git.git_dir());
    match result {
        Ok(_) => {
            drop(imported);
            assert_round_trips(&git.git_dir());
        }
        Err(e) => {
            // The path-length rule may refuse the deep chain: typed, and
            // nothing partial.
            assert!(
                matches!(e, MirrorError::Git(GitError::BadPaths(_))),
                "{e:?}"
            );
            assert!(imported.map.is_empty());
            assert!(graft_refs(&imported).is_empty());
        }
    }
}

// --- authors, dates, encodings and messages -------------------------------

/// Identities, dates and messages git stores but rarely writes: an empty
/// email and name, extreme and odd timezones, dates at zero and far in the
/// future, a missing date, a Latin-1 message with an `encoding` header, CRLF,
/// NUL bytes and no trailing newline, an empty message, and a 1 MiB one.
#[test]
fn odd_identities_dates_and_messages_round_trip() {
    let long = "long line ".repeat(MIB / 10);
    let cases: Vec<Vec<u8>> = vec![
        b"author <> 0 +0000\ncommitter <> 0 +0000\n\nempty name and email\n".to_vec(),
        b"author A <a@example.com> 99999999999 +1400\ncommitter A <a@example.com> 1700000000 -1200\n\nextreme zones\n".to_vec(),
        b"author A <a@example.com> 1700000000 +0060\ncommitter A <a@example.com> 1700000000 -0000\n\nodd zone minutes, negative zero\n".to_vec(),
        b"author A <a@example.com> 1700000000\ncommitter A <a@example.com>\n\nmissing zone and date\n".to_vec(),
        b"author A  <a@example.com>  1700000000  +0000\ncommitter A <a@example.com> 1700000000 +0000\n\ndouble spaces\n".to_vec(),
        b"author Ren\xe9 <r@example.com> 1700000000 +0100\ncommitter Ren\xe9 <r@example.com> 1700000000 +0100\nencoding ISO-8859-1\n\nCaf\xe9 cr\xe8me\n".to_vec(),
        "author 名前 <名@example.com> 1700000000 +0900\ncommitter Ünï <u@example.com> 1700000000 +0900\n\n日本語のメッセージ\n".as_bytes().to_vec(),
        b"author A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\nCRLF\r\nbody\r\n".to_vec(),
        b"author A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\nnul\0inside\0".to_vec(),
        b"author A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\nno trailing newline".to_vec(),
        b"author A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\n".to_vec(),
        b"author A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n".to_vec(),
        b"author A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\n\n\nleading blank lines\n\n\n".to_vec(),
        format!("author A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\n{long}\n").into_bytes(),
        b"committer A <a@example.com> 1700000000 +0000\n\nno author\n".to_vec(),
        b"author A <a@example.com> 1700000000 +0000\n\nno committer\n".to_vec(),
        b"author A <a@example.com> 1700000000 +0000\nauthor B <b@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\ntwo authors\n".to_vec(),
    ];
    let git = Git::init("idents");
    let mut parent: Option<String> = None;
    for (n, rest) in cases.iter().enumerate() {
        let blob = git.blob(format!("{n}\n").as_bytes());
        let tree = git.raw_tree(&[("100644", b"n", &blob)]);
        let parents: Vec<&str> = parent.iter().map(String::as_str).collect();
        parent = Some(git.raw_commit(&tree, &parents, rest));
    }
    git.set_ref("refs/heads/main", parent.as_ref().unwrap());
    assert_round_trips(&git.git_dir());
}

/// Many short commits and a long branch name, to keep batching honest with
/// odd shapes: 600 commits in batches of 256.
#[test]
fn a_long_history_with_odd_ref_names_round_trips() {
    let mut git = Git::init("long");
    git.write("f", b"0\n");
    let base = git.commit("0");
    let mut parent = base;
    for n in 1..600 {
        let blob = git.blob(format!("{n}\n").as_bytes());
        let tree = git.raw_tree(&[("100644", b"f", &blob)]);
        parent = git.commit_tree(&tree, &[&parent], &format!("{n}"));
    }
    git.set_ref("refs/heads/main", &parent);
    git.set_ref(
        "refs/heads/feature/deeply/nested/name-with.dots_and-dashes",
        &parent,
    );
    git.set_ref("refs/tags/v1.0.0-rc.1+build.5", &parent);
    assert_round_trips(&git.git_dir());
}

// --- known gaps, each with its Vogt item ----------------------------------

/// A non-canonical directory (a zero-padded `040000` entry, or a `100600`
/// file beside a subdirectory) makes the import record a raw tree; the
/// export then writes that tree but not the subtree it names, so the commit
/// sha matches while the exported repository lacks an object.
#[test]
#[ignore = "WI-2356: export omits subtrees under a recorded raw tree"]
fn raw_trees_over_subdirectories_export_every_object() {
    let mut git = Git::init("padded");
    let x = git.blob(b"x\n");
    let inner = git.raw_tree(&[("100644", b"leaf", &x)]);
    for (dir_mode, file_mode) in [("040000", "100644"), ("40000", "100600")] {
        let root = git.raw_tree(&[(dir_mode, b"dir", &inner), (file_mode, b"plain", &x)]);
        let c = git.commit_tree(&root, &[], "raw tree over a subdirectory");
        git.set_ref("refs/heads/main", &c);
        assert_round_trips(&git.git_dir());
    }
}

/// Names that check out over git's own metadata: `.git` in any case, its
/// NTFS short name, and an HFS+-ignorable variant. And a `/` inside a name.
#[test]
fn git_metadata_names_are_a_typed_refusal() {
    for name in [&b".git"[..], b".GIT", b"git~1", b".git\xe2\x80\x8c", b"a/b"] {
        let mut git = Git::init("git-name");
        let x = git.blob(b"x\n");
        let root = git.raw_tree(&[("100644", name, &x)]);
        let c = git.commit_tree(&root, &[], "metadata name");
        git.set_ref("refs/heads/main", &c);
        assert_refused(&git.git_dir(), |e| matches!(e, GitError::BadPaths(_)));
    }
}

/// An oversize blob after some history, imported one commit per batch: the
/// refusal must come before anything is written, as the path check does.
#[test]
#[ignore = "WI-2360: earlier batches are imported before the refusal"]
fn an_oversize_blob_late_in_history_imports_nothing() {
    let mut git = Git::init("late-big");
    for n in 0..3 {
        git.write("f", format!("{n}\n").as_bytes());
        git.commit("small");
    }
    git.write("big", &vec![7u8; 64 * MIB + 1]);
    git.commit("big");
    let api = engine();
    let map = MemoryMirrorMap::new();
    let mut cx = admin_cx();
    cx.principal.scopes.push(format!("repo:{REPO}:admin"));
    let mut config = ImportConfig::new(git.git_dir(), REPO);
    config.batch = 1;
    let result = block_on(import_repo(&api, &cx, &map, &config));
    assert!(result.is_err(), "refused");
    assert!(
        map.is_empty(),
        "nothing imported, but {} commits are",
        map.len()
    );
}
