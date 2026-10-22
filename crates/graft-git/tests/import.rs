//! Import of synthetic git repositories: fidelity, the differential check
//! against `git ls-tree -r`, path rules, batching and resume.

mod common;

use std::process::Command;

use common::{Git, TempDir, check_every_commit};
use graft_git::import::{ImportConfig, ImportReport, SHA_KEY, import};
use graft_git::map::{MemoryMirrorMap, MirrorMap};
use graft_git::{GitError, GitSha};
use graft_testkit::fake_api::{Call, FakeApi, admin_cx, block_on};

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn config(git: &Git, lines: &[&str]) -> ImportConfig {
    let mut c = ImportConfig::new(git.git_dir(), "r");
    c.lines = lines
        .iter()
        .map(|l| (format!("refs/heads/{l}"), (*l).to_owned()))
        .collect();
    c
}

fn run(
    api: &FakeApi,
    map: &MemoryMirrorMap,
    config: &ImportConfig,
) -> Result<ImportReport, GitError> {
    block_on(import(api, &admin_cx(), map, config))
}

fn import_all(git: &Git, lines: &[&str]) -> (FakeApi, MemoryMirrorMap, ImportReport) {
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let report = run(&api, &map, &config(git, lines)).expect("import succeeds");
    check_every_commit(git, &api, &map);
    (api, map, report)
}

/// Merges, a side branch reached only through a merge, a second root,
/// symlinks, executables, a submodule, an empty merge, tags, and a file that
/// turns into a directory.
fn varied_repo() -> Git {
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

#[test]
fn varied_history_imports_losslessly() {
    let git = varied_repo();
    let (api, map, report) = import_all(&git, &["main", "other", "side", "discarded"]);
    let n = git.rev_list_all().len() as u64;
    assert_eq!(report.commits, n);
    assert_eq!(report.verified, n);
    assert!(report.lossy.is_empty());
    assert_eq!(report.lines.len(), 4);
    let tags: Vec<&str> = report.tags.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(tags, vec!["light", "v0.1"]);
    let main = GitSha::from_hex(&git.out(&["rev-parse", "main"])).unwrap();
    assert_eq!(
        report.lines[0],
        ("main".into(), map.changeset_of(main).unwrap().unwrap())
    );
    // Several roots.
    let roots = api
        .changesets_in_order()
        .iter()
        .filter(|c| c.parents.is_empty())
        .count();
    assert_eq!(
        roots, 2,
        "main's and other's (a gitlink target is not reachable)"
    );
    // The empty merge has no changes and two parents.
    let empty_merge = GitSha::from_hex(&git.out(&["rev-parse", "main~1"])).unwrap();
    let c = api
        .changeset(map.changeset_of(empty_merge).unwrap().unwrap())
        .unwrap();
    assert_eq!(c.parents.len(), 2);
    assert!(c.changes.is_empty());
}

#[test]
fn a_bare_clone_imports_the_same_changesets() {
    let git = varied_repo();
    let (api, _, _) = import_all(&git, &["main"]);
    let bare = TempDir::new("bare");
    let status = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(git.path())
        .arg(bare.0.join("r.git"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success());
    let mut c = ImportConfig::new(bare.0.join("r.git"), "r");
    c.tags = false;
    let api2 = FakeApi::new();
    let map2 = MemoryMirrorMap::new();
    run(&api2, &map2, &c).unwrap();
    let main_ids = |a: &FakeApi| {
        let mut ids: Vec<_> = a.changesets_in_order().iter().map(|c| c.id()).collect();
        ids.sort();
        ids
    };
    let mut want = main_ids(&api);
    want.retain(|id| api2.changeset(*id).is_some());
    assert_eq!(main_ids(&api2), want, "same commits give the same ids");
    assert!(!want.is_empty());
}

#[test]
fn non_canonical_trees_are_kept_raw() {
    let mut git = Git::init("raw");
    let x = git.blob(b"x\n");
    let y = git.blob(b"y\n");
    // Legacy 100664 and an empty subtree.
    let legacy = git.raw_tree(&[("100664", b"old.txt", &x)]);
    let root1 = git.raw_tree(&[
        ("100644", b"a.txt", &x),
        ("40000", b"empty", EMPTY_TREE),
        ("40000", b"legacy", &legacy),
    ]);
    let c1 = git.commit_tree(&root1, &[], "legacy modes");
    // Touch another file in the legacy directory: git keeps 100664.
    let legacy2 = git.raw_tree(&[("100664", b"old.txt", &x), ("100644", b"new.txt", &y)]);
    let legacy2_sorted = git.raw_tree(&[("100644", b"new.txt", &y), ("100664", b"old.txt", &x)]);
    let root2 = git.raw_tree(&[
        ("100644", b"a.txt", &x),
        ("40000", b"empty", EMPTY_TREE),
        ("040000", b"legacy", &legacy2),
    ]);
    let c2 = git.commit_tree(&root2, &[&c1], "unsorted entries and a 040000 mode");
    // Only a mode change graft cannot see (100644 → 100664) plus a real one.
    let root3 = git.raw_tree(&[
        ("100664", b"a.txt", &x),
        ("100644", b"b.txt", &y),
        ("40000", b"empty", EMPTY_TREE),
        ("40000", b"legacy", &legacy2_sorted),
    ]);
    let c3 = git.commit_tree(&root3, &[&c2], "invisible mode change");
    // Delete everything in legacy/: git keeps nothing, graft drops the dir.
    let root4 = git.raw_tree(&[("100664", b"a.txt", &x), ("100644", b"b.txt", &y)]);
    let c4 = git.commit_tree(&root4, &[&c3], "drop the empty and legacy dirs");
    git.set_ref("refs/heads/main", &c4);
    let (_, map, report) = import_all(&git, &["main"]);
    assert!(report.raw_trees > 0, "raw trees were recorded: {report:?}");
    let c1_id = map
        .changeset_of(GitSha::from_hex(&c1).unwrap())
        .unwrap()
        .unwrap();
    let record = map.imported(c1_id).unwrap().unwrap();
    let dirs: Vec<String> = record
        .raw_trees
        .iter()
        .map(|r| {
            r.dir
                .as_ref()
                .map_or_else(String::new, |d| d.as_str().to_owned())
        })
        .collect();
    assert!(
        dirs.contains(&"legacy".to_owned()) || dirs.contains(&String::new()),
        "the legacy directory is kept raw: {dirs:?}"
    );
}

#[test]
fn signed_commits_and_odd_headers_round_trip() {
    let mut git = Git::init("signed");
    git.write("f", b"1\n");
    let base = git.commit("base");
    let blob = git.blob(b"2\n");
    let tree = git.raw_tree(&[("100644", b"f", &blob)]);
    // A fixed signature block, a mergetag and an extra header, verbatim.
    let raw = format!(
        "tree {tree}\nparent {base}\n\
author A U Thor <author@example.com> 1700000600 +0530\n\
committer C O Mitter <committer@example.com> 1700000600 -0100\n\
x-extra-header some value\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEEstaticTESTsignatureNOTaREALkey000000000=\n =abcd\n -----END PGP SIGNATURE-----\n\
\nsigned (statically) commit\n"
    );
    let signed = git.literal("commit", raw.as_bytes());
    git.set_ref("refs/heads/main", &signed);
    let (_, map, report) = import_all(&git, &["main"]);
    assert_eq!(report.verified, 2);
    let id = map
        .changeset_of(GitSha::from_hex(&signed).unwrap())
        .unwrap()
        .unwrap();
    let record = map.imported(id).unwrap().unwrap();
    assert!(record.headers.windows(6).any(|w| w == b"gpgsig"));
}

#[test]
fn ssh_signed_commits_round_trip() {
    if Command::new("ssh-keygen").arg("-?").output().is_err() {
        eprintln!("ssh-keygen is not installed; skipping");
        return;
    }
    let mut git = Git::init("sshsign");
    // A throwaway key made for this test run; never a real identity.
    let key = git
        .path()
        .join("..")
        .join(format!("graft-git-test-key-{}", std::process::id()));
    let _ = std::fs::remove_file(&key);
    let status = Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "graft-git test key",
            "-f",
        ])
        .arg(&key)
        .status()
        .unwrap();
    assert!(status.success());
    git.write("f", b"1\n");
    git.commit("base");
    git.write("f", b"2\n");
    git.run(&["add", "-A"]);
    git.tick();
    git.run(&[
        "-c",
        "gpg.format=ssh",
        "-c",
        &format!("user.signingkey={}", key.display()),
        "commit",
        "-q",
        "-S",
        "-m",
        "ssh-signed",
    ]);
    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(key.with_extension("pub"));
    let head = git.run(&["cat-file", "commit", "HEAD"]);
    assert!(
        head.windows(6).any(|w| w == b"gpgsig"),
        "the commit is signed"
    );
    let (_, _, report) = import_all(&git, &["main"]);
    assert_eq!(report.verified, 2);
}

#[test]
fn non_utf8_messages_and_encodings_round_trip() {
    let mut git = Git::init("latin1");
    git.write("f", b"1\n");
    git.commit("base");
    git.write("f", b"2\n");
    git.run(&["add", "-A"]);
    git.tick();
    let msg_file = git
        .path()
        .join("..")
        .join(format!("graft-msg-{}", std::process::id()));
    std::fs::write(&msg_file, b"caf\xe9 au lait \xff\n").unwrap();
    git.run(&[
        "-c",
        "i18n.commitEncoding=ISO-8859-1",
        "commit",
        "-q",
        "-F",
        msg_file.to_str().unwrap(),
    ]);
    std::fs::remove_file(&msg_file).unwrap();
    let (api, map, _) = import_all(&git, &["main"]);
    let head = GitSha::from_hex(&git.out(&["rev-parse", "HEAD"])).unwrap();
    let id = map.changeset_of(head).unwrap().unwrap();
    let record = map.imported(id).unwrap().unwrap();
    assert_eq!(
        record.message.as_deref(),
        Some(&b"caf\xe9 au lait \xff\n"[..])
    );
    assert!(record.headers.windows(8).any(|w| w == b"encoding"));
    assert!(api.changeset(id).unwrap().message.contains('\u{fffd}'));
}

#[test]
fn bad_paths_stop_the_import_with_the_full_list() {
    let mut git = Git::init("bad");
    let x = git.blob(b"x\n");
    let sub = git.raw_tree(&[("100644", b"CON.txt", &x), ("100644", b"ok.txt", &x)]);
    let root1 = git.raw_tree(&[
        ("100644", b"Readme", &x),
        ("100644", b"latin\xe9", &x),
        ("100644", b"readme", &x),
        ("40000", b"sub", &sub),
    ]);
    let c1 = git.commit_tree(&root1, &[], "bad names");
    let root2 = git.raw_tree(&[
        ("100644", b"Readme", &x),
        ("100644", b"latin\xe9", &x),
        ("100644", b"readme", &x),
        ("40000", b"sub", &sub),
        ("100644", b"trailing.", &x),
        ("100644", b"what?", &x),
    ]);
    let c2 = git.commit_tree(&root2, &[&c1], "more bad names");
    git.set_ref("refs/heads/main", &c2);
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let err = run(&api, &map, &config(&git, &["main"])).unwrap_err();
    let GitError::BadPaths(list) = err else {
        panic!("expected BadPaths, got {err:?}");
    };
    let got: Vec<(String, String)> = list
        .iter()
        .map(|b| (b.path.clone(), b.reason.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("Readme".into(), "portability (case-clash)".into()),
            ("latin\u{fffd}".into(), "not-utf8".into()),
            ("readme".into(), "portability (case-clash)".into()),
            ("sub/CON.txt".into(), "portability (reserved-name)".into()),
            (
                "trailing.".into(),
                "portability (trailing-dot-or-space)".into()
            ),
            ("what?".into(), "portability (reserved-char)".into()),
        ]
    );
    let c1 = GitSha::from_hex(&c1).unwrap();
    assert!(
        list.iter()
            .filter(|b| !b.path.starts_with('t') && !b.path.starts_with('w'))
            .all(|b| b.commit == c1)
    );
    assert!(
        api.calls().is_empty(),
        "nothing was written: {:?}",
        api.calls()
    );
    assert!(map.is_empty());
}

#[test]
fn empty_non_merge_commits_are_reported() {
    let mut git = Git::init("empty");
    git.write("f", b"1\n");
    let base = git.commit("base");
    let empty = git.commit("nothing");
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let err = run(&api, &map, &config(&git, &["main"])).unwrap_err();
    let GitError::Unrepresentable(list) = err else {
        panic!("expected Unrepresentable, got {err:?}");
    };
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].commit, GitSha::from_hex(&empty).unwrap());
    assert_eq!(list[0].reason, "empty-changeset");
    assert!(api.calls().is_empty());
    assert_ne!(base, empty);
}

#[test]
fn commits_that_differ_only_in_committer_stay_distinct() {
    // A rebase onto the same parent with nothing to change: the copy differs
    // from the original only in its committer. The `mirror.git-commit`
    // metadata key keeps them apart, so both import and both re-export.
    let mut git = Git::init("indistinct");
    git.write("f", b"1\n");
    let base = git.commit("base");
    let x = git.blob(b"2\n");
    let tree = git.raw_tree(&[("100644", b"f", &x)]);
    let raw = |committer: &str| {
        format!(
            "tree {tree}\nparent {base}\n\
author A U Thor <author@example.com> 1700000600 +0530\n\
committer {committer} 1700000900 +0000\n\nsame change\n"
        )
    };
    let original = git.literal(
        "commit",
        raw("C O Mitter <committer@example.com>").as_bytes(),
    );
    let copy = git.literal("commit", raw("Someone Else <else@example.com>").as_bytes());
    let merge_tree = git.out(&["rev-parse", &format!("{original}^{{tree}}")]);
    let merge = git.commit_tree(&merge_tree, &[&original, &copy], "join both");
    git.set_ref("refs/heads/main", &merge);
    let (api, map, report) = import_all(&git, &["main"]);
    assert_eq!(report.verified, 4);
    let id = map
        .changeset_of(GitSha::from_hex(&copy).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        api.changeset(id).unwrap().metadata.get(SHA_KEY),
        Some(&copy)
    );
}

#[test]
fn batches_upload_blobs_before_the_changesets_that_need_them() {
    let git = varied_repo();
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let mut c = config(&git, &["main"]);
    c.batch = 2;
    run(&api, &map, &c).unwrap();
    check_every_commit_on(&git, &api, &map, "main");
    let imports: Vec<usize> = api
        .calls()
        .iter()
        .filter_map(|c| match c {
            Call::ImportChangesets(ids) => Some(ids.len()),
            _ => None,
        })
        .collect();
    assert!(imports.len() > 1);
    assert!(imports.iter().all(|n| *n <= 2), "{imports:?}");
}

/// [`check_every_commit`] restricted to what `line` reaches.
fn check_every_commit_on(git: &Git, api: &FakeApi, map: &MemoryMirrorMap, line: &str) {
    let reachable: Vec<String> = git
        .out(&["rev-list", line])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(api.changeset_count(), reachable.len());
    for c in reachable {
        let id = map
            .changeset_of(GitSha::from_hex(&c).unwrap())
            .unwrap()
            .unwrap();
        let cs = api.changeset(id).unwrap();
        assert_eq!(api.snapshot(cs.root), git.ls_tree(&c));
    }
}

#[test]
fn a_second_run_imports_only_new_commits() {
    let mut git = varied_repo();
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let c = config(&git, &["main", "other", "side", "discarded"]);
    let first = run(&api, &map, &c).unwrap();
    git.run(&["checkout", "-q", "main"]);
    git.write("src/lib.rs", b"changed after the first import\n");
    git.write("vendor/new.txt", b"new\n");
    git.commit("after");
    let calls_before = api.calls().len();
    let second = run(&api, &map, &c).unwrap();
    assert_eq!(second.commits, 1);
    assert_eq!(second.blobs, 2);
    assert!(first.commits > 1);
    let new_calls = &api.calls()[calls_before..];
    assert!(
        new_calls
            .iter()
            .any(|c| matches!(c, Call::GetChangesets(_))),
        "the parent's root comes from the server: {new_calls:?}"
    );
    assert!(
        new_calls.iter().any(|c| matches!(c, Call::GetObjects(_))),
        "the parent's trees come from the server"
    );
    check_every_commit(&git, &api, &map);
    // Nothing new: nothing written.
    let third = run(&api, &map, &c).unwrap();
    assert_eq!(third.commits, 0);
}

#[test]
fn the_import_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let cx = admin_cx();
    let c = ImportConfig::new("/nonexistent", "r");
    let fut = import(&api, &cx, &map, &c);
    assert_send(&fut);
}

#[test]
fn a_missing_ref_is_an_error() {
    let mut git = Git::init("noref");
    git.write("f", b"1\n");
    git.commit("base");
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let err = run(&api, &map, &config(&git, &["nope"])).unwrap_err();
    assert!(matches!(err, GitError::Read(_)), "{err:?}");
}

/// The pilot repository (decision D14). Needs the network, so it is opt-in:
/// `cargo test -p graft-git --test import -- --ignored --nocapture`, or set
/// `GRAFT_RUSTTORRENT` to a local bare clone.
#[test]
#[ignore = "clones rustTorrent from GitHub"]
fn rust_torrent_imports_with_zero_exceptions() {
    let source = std::env::var("GRAFT_RUSTTORRENT").ok();
    let dir = TempDir::new("rusttorrent");
    let path = if let Some(local) = source {
        local.into()
    } else {
        let path = dir.0.join("rustTorrent.git");
        let status = Command::new("git")
            .args([
                "clone",
                "-q",
                "--bare",
                "https://github.com/TheDancingDeveloper-org/rustTorrent.git",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path
    };
    let git = Command::new("git")
        .arg("--git-dir")
        .arg(&path)
        .args(["for-each-ref", "--format=%(refname)", "refs/heads/"])
        .output()
        .unwrap();
    let mut c = ImportConfig::new(&path, "rustTorrent");
    c.lines = String::from_utf8(git.stdout)
        .unwrap()
        .lines()
        .map(|r| (r.to_owned(), r.trim_start_matches("refs/heads/").to_owned()))
        .collect();
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let report = run(&api, &map, &c).expect("rustTorrent imports");
    eprintln!(
        "rustTorrent: {} commits, {} verified, {} blobs, {} raw trees, {} lossy, {} lines, {} tags",
        report.commits,
        report.verified,
        report.blobs,
        report.raw_trees,
        report.lossy.len(),
        report.lines.len(),
        report.tags.len()
    );
    assert_eq!(report.verified, report.commits);
    assert!(report.lossy.is_empty());
}
