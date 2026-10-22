//! Synthetic git repositories built with the `git` CLI in temp dirs, and the
//! checks every import test runs.

#![allow(dead_code, unreachable_pub)]

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use graft_git::export::{GitSource, commit_sha, export_tree};
use graft_git::map::{MemoryMirrorMap, MirrorMap};
use graft_git::{GitError, GitSha};
use graft_model::{BlobId, EntryKind, ObjectId};
use graft_testkit::fake_api::FakeApi;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// The sha of git's empty tree.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A temp directory removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("graft-git-{tag}-{}-{n}", std::process::id()));
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

/// A git work tree with a fixed identity, clock and config.
pub struct Git {
    pub dir: TempDir,
    tick: u64,
}

impl Git {
    pub fn init(tag: &str) -> Git {
        let git = Git {
            dir: TempDir::new(tag),
            tick: 0,
        };
        git.run(&["init", "-q", "-b", "main"]);
        git
    }

    pub fn path(&self) -> &Path {
        &self.dir.0
    }

    pub fn git_dir(&self) -> PathBuf {
        self.dir.0.join(".git")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.dir.0)
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
            .args(args);
        cmd
    }

    /// Runs git and returns stdout; panics on failure.
    pub fn run(&self, args: &[&str]) -> Vec<u8> {
        self.run_with(args, None)
    }

    pub fn run_with(&self, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
        let mut cmd = self.command(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("git runs");
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

    pub fn out(&self, args: &[&str]) -> String {
        String::from_utf8(self.run(args)).unwrap().trim().to_owned()
    }

    pub fn write(&self, path: &str, bytes: &[u8]) {
        let p = self.dir.0.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    pub fn chmod_x(&self, path: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.dir.0.join(path);
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn symlink(&self, target: &str, path: &str) {
        std::os::unix::fs::symlink(target, self.dir.0.join(path)).unwrap();
    }

    /// Stages everything and commits; returns the new sha.
    pub fn commit(&mut self, message: &str) -> String {
        self.tick += 60;
        self.run(&["add", "-A"]);
        self.run(&["commit", "-q", "--allow-empty", "-m", message]);
        self.out(&["rev-parse", "HEAD"])
    }

    pub fn tick(&mut self) {
        self.tick += 60;
    }

    /// Writes an object verbatim (`hash-object --literally`).
    pub fn literal(&self, kind: &str, bytes: &[u8]) -> String {
        let out = self.run_with(
            &["hash-object", "-t", kind, "-w", "--literally", "--stdin"],
            Some(bytes),
        );
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    /// A raw tree from (mode, name, sha hex) entries, in the order given.
    pub fn raw_tree(&self, entries: &[(&str, &[u8], &str)]) -> String {
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

    pub fn blob(&self, bytes: &[u8]) -> String {
        let out = self.run_with(&["hash-object", "-w", "--stdin"], Some(bytes));
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    /// `commit-tree` with a fixed clock; returns the sha.
    pub fn commit_tree(&mut self, tree: &str, parents: &[&str], message: &str) -> String {
        self.tick += 60;
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.push("-p");
            args.push(p);
        }
        args.extend(["-m", message]);
        self.out(&args)
    }

    pub fn set_ref(&self, name: &str, sha: &str) {
        self.run(&["update-ref", name, sha]);
    }

    pub fn rev_list_all(&self) -> Vec<String> {
        self.out(&["rev-list", "--all"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// `git ls-tree -r` of a commit, as graft would hold it: path → (kind,
    /// graft id).
    pub fn ls_tree(&self, commit: &str) -> BTreeMap<String, (EntryKind, ObjectId)> {
        let out = self.run(&["ls-tree", "-r", "-z", "--full-tree", commit]);
        let mut snapshot = BTreeMap::new();
        for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            let tab = record.iter().position(|&b| b == b'\t').unwrap();
            let meta = std::str::from_utf8(&record[..tab]).unwrap();
            let path = String::from_utf8(record[tab + 1..].to_vec()).unwrap();
            let mut parts = meta.split(' ');
            let mode = parts.next().unwrap();
            let _kind = parts.next().unwrap();
            let sha = parts.next().unwrap();
            let kind = match mode {
                "100755" => EntryKind::Exec,
                "120000" => EntryKind::Symlink,
                "160000" => EntryKind::Submodule,
                m if m.starts_with("100") => EntryKind::File,
                m => panic!("unexpected mode {m}"),
            };
            let id = if kind == EntryKind::Submodule {
                let mut id = [0u8; 32];
                id[..20].copy_from_slice(&GitSha::from_hex(sha).unwrap().0);
                ObjectId(id)
            } else {
                BlobId::hash(&self.run(&["cat-file", "blob", sha])).0
            };
            snapshot.insert(path, (kind, id));
        }
        snapshot
    }
}

/// Merges, a side branch reached only through a merge, a second root,
/// symlinks, executables, a submodule, an empty merge, tags, and a file that
/// turns into a directory.
pub fn varied_repo() -> Git {
    let mut git = Git::init("varied");
    git.write("README.md", b"hello\n");
    git.write("src/lib.rs", b"pub fn a() {}\n");
    git.write("bin/run.sh", b"#!/bin/sh\necho run\n");
    git.chmod_x("bin/run.sh");
    git.symlink("src/lib.rs", "link");
    git.commit("initial");
    git.run(&["tag", "-a", "v0.1", "-m", "first release"]);

    git.run(&["checkout", "-q", "-b", "side"]);
    git.write("src/side.rs", b"side\n");
    git.commit("side work");
    git.write("src/lib.rs", b"pub fn a() {}\npub fn side() {}\n");
    git.commit("more side work");

    git.run(&["checkout", "-q", "main"]);
    git.write("docs/guide.md", b"guide\n");
    git.chmod_x("README.md");
    git.commit("docs, and README becomes executable");
    git.tick();
    git.run(&["merge", "-q", "--no-ff", "side", "-m", "merge side"]);

    // A second root, merged in.
    git.run(&["checkout", "-q", "--orphan", "other"]);
    git.run(&["rm", "-rq", "--cached", "."]);
    git.run(&["clean", "-fdq"]);
    git.write("other/notes.txt", b"unrelated history\n");
    git.commit("other root");
    git.run(&["checkout", "-q", "-f", "main"]);
    git.tick();
    git.run(&[
        "merge",
        "-q",
        "--no-ff",
        "--allow-unrelated-histories",
        "other",
        "-m",
        "merge other root",
    ]);

    // An empty merge: the tree is the first parent's.
    git.run(&["checkout", "-q", "-b", "discarded"]);
    git.write("discarded.txt", b"never merged\n");
    git.commit("discarded work");
    git.run(&["checkout", "-q", "main"]);
    git.tick();
    git.run(&[
        "merge",
        "-q",
        "-s",
        "ours",
        "discarded",
        "-m",
        "empty merge",
    ]);

    // A submodule (gitlink) entry, file → directory, symlink retarget.
    let sub = git.commit_tree(EMPTY_TREE, &[], "submodule target");
    git.write(
        ".gitmodules",
        b"[submodule \"vendor/dep\"]\n\tpath = vendor/dep\n\turl = ../dep\n",
    );
    git.run(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{sub},vendor/dep"),
    ]);
    git.run(&["rm", "-q", "--cached", "link"]);
    std::fs::remove_file(git.path().join("link")).unwrap();
    git.symlink("README.md", "link");
    std::fs::remove_file(git.path().join("docs/guide.md")).unwrap();
    git.write("docs/guide.md/index.md", b"now a directory\n");
    git.commit("submodule, retarget, file to directory");
    git.run(&["tag", "light"]);
    git
}

/// Runs git against the repository at `git_dir` with the fixed identity and
/// config, and returns stdout; panics on failure.
pub fn git_in(git_dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
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

/// `git ls-tree -r` of a commit in `git_dir`, as graft would hold it.
pub fn ls_tree_in(git_dir: &Path, commit: &str) -> BTreeMap<String, (EntryKind, ObjectId)> {
    let out = git_in(git_dir, &["ls-tree", "-r", "-z", "--full-tree", commit]);
    let mut snapshot = BTreeMap::new();
    for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let tab = record.iter().position(|&b| b == b'\t').unwrap();
        let meta = std::str::from_utf8(&record[..tab]).unwrap();
        let path = String::from_utf8(record[tab + 1..].to_vec()).unwrap();
        let mut parts = meta.split(' ');
        let mode = parts.next().unwrap();
        let _kind = parts.next().unwrap();
        let sha = parts.next().unwrap();
        let kind = match mode {
            "100755" => EntryKind::Exec,
            "120000" => EntryKind::Symlink,
            "160000" => EntryKind::Submodule,
            m if m.starts_with("100") => EntryKind::File,
            m => panic!("unexpected mode {m}"),
        };
        let id = if kind == EntryKind::Submodule {
            let mut id = [0u8; 32];
            id[..20].copy_from_slice(&GitSha::from_hex(sha).unwrap().0);
            ObjectId(id)
        } else {
            BlobId::hash(&git_in(git_dir, &["cat-file", "blob", sha])).0
        };
        snapshot.insert(path, (kind, id));
    }
    snapshot
}

/// The export's view of a repository, through the git CLI.
pub struct CliSource<'a> {
    pub git: &'a Git,
    pub map: &'a MemoryMirrorMap,
}

impl GitSource for CliSource<'_> {
    fn raw_tree(&mut self, sha: GitSha) -> Result<Vec<u8>, GitError> {
        Ok(self.git.run(&["cat-file", "tree", &sha.to_hex()]))
    }

    fn blob_sha(&mut self, id: BlobId) -> Result<GitSha, GitError> {
        Ok(self.map.blob_sha(id)?.expect("blob sha recorded"))
    }
}

/// For every commit in the repository: it was imported, its snapshot equals
/// `git ls-tree -r`, and the export from graft's changeset and the mirror
/// map alone reproduces its sha (`export(import(c)).sha == c.sha`).
pub fn check_every_commit(git: &Git, api: &FakeApi, map: &MemoryMirrorMap) {
    let commits = git.rev_list_all();
    assert_eq!(
        api.changeset_count(),
        commits.len(),
        "every commit imported"
    );
    let mut tree_of: HashMap<String, GitSha> = HashMap::new();
    for c in &commits {
        tree_of.insert(
            c.clone(),
            GitSha::from_hex(&git.out(&["rev-parse", &format!("{c}^{{tree}}")])).unwrap(),
        );
    }
    for c in &commits {
        let sha = GitSha::from_hex(c).unwrap();
        let id = map.changeset_of(sha).unwrap().expect("commit is mapped");
        let changeset = api.changeset(id).expect("changeset is on the server");
        assert_eq!(
            api.snapshot(changeset.root),
            git.ls_tree(c),
            "snapshot of {c} matches git ls-tree -r"
        );
        let record = map.imported(id).unwrap().expect("record");
        let parents: Vec<GitSha> = changeset
            .parents
            .iter()
            .map(|p| map.sha_of(*p).unwrap().expect("parent mapped"))
            .collect();
        let parent_tree = parents.first().map(|p| tree_of[&p.to_hex()]);
        let mut src = CliSource { git, map };
        let tree =
            export_tree(&mut src, parent_tree, &changeset.changes, &record.raw_trees).unwrap();
        assert_eq!(tree, tree_of[c], "tree of {c} re-exports");
        assert_eq!(
            commit_sha(tree, &parents, &record, &changeset.message),
            sha,
            "commit {c} re-exports to its sha"
        );
    }
}
