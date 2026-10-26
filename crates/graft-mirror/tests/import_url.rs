//! A `file://` URL is cloned into a temp dir and imported with the fidelity
//! check. A path imports without a clone. A path the importer already
//! rejects is reported as `<sha> <reason>`.

use std::fs;
use std::path::Path;
use std::process::Command;

use graft_git::GitError;
use graft_git::import::{ImportConfig, import};
use graft_git::map::MemoryMirrorMap;
use graft_mirror::import_source;
use graft_testkit::fake_api::{FakeApi, admin_cx, block_on};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.test")
        .env("GIT_AUTHOR_DATE", "1000000000 +0000")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.test")
        .env("GIT_COMMITTER_DATE", "1000000000 +0000")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn init(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("a.txt"), b"one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "one"]);
}

fn import_dir(dir: &Path) -> Result<graft_git::import::ImportReport, GitError> {
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let mut cfg = ImportConfig::new(dir, "r");
    cfg.tags = false;
    block_on(import(&api, &admin_cx(), &map, &cfg))
}

#[tokio::test]
async fn file_url_matches_a_path_import_and_the_clone_is_removed() {
    let src = tempfile::tempdir().unwrap();
    init(src.path());
    let bare = tempfile::tempdir().unwrap();
    let bare_path = bare.path().join("repo.git");
    assert!(
        Command::new("git")
            .args(["clone", "-q", "--bare"])
            .arg(src.path())
            .arg(&bare_path)
            .status()
            .unwrap()
            .success()
    );
    let from_path = import_dir(&bare_path).expect("path import");
    assert_eq!(from_path.verified, from_path.commits);
    assert!(from_path.lossy.is_empty());
    assert!(from_path.commits >= 1);

    let url = format!("file://{}", bare_path.display());
    let held = import_source(Path::new(&url))
        .await
        .expect("clone file url")
        .expect("a URL is cloned");
    let dest = held.path().to_path_buf();
    assert!(dest.join("HEAD").is_file(), "bare clone");
    let from_url = import_dir(&dest).expect("url import");
    assert_eq!(from_url.commits, from_path.commits);
    assert_eq!(from_url.verified, from_url.commits);
    assert_eq!(from_url.lines, from_path.lines);
    drop(held);
    assert!(!dest.exists(), "temp clone is gone");
}

#[test]
fn a_path_import_does_not_clone() {
    let src = tempfile::tempdir().unwrap();
    init(src.path());
    let report = import_dir(src.path()).expect("path");
    assert_eq!(report.commits, 1);
    assert_eq!(report.verified, 1);
    // The source is the import root, not a temp clone beside it.
    assert!(src.path().join(".git").is_dir());
}

#[tokio::test]
async fn a_rejected_commit_is_named_and_the_clone_is_removed() {
    let src = tempfile::tempdir().unwrap();
    init(src.path());
    // `CON.txt` is a path the importer already refuses (reserved-name).
    fs::write(src.path().join("CON.txt"), b"no\n").unwrap();
    git(src.path(), &["add", "CON.txt"]);
    git(src.path(), &["commit", "-q", "-m", "reserved name"]);

    let bare = tempfile::tempdir().unwrap();
    let bare_path = bare.path().join("repo.git");
    assert!(
        Command::new("git")
            .args(["clone", "-q", "--bare"])
            .arg(src.path())
            .arg(&bare_path)
            .status()
            .unwrap()
            .success()
    );
    let url = format!("file://{}", bare_path.display());
    let held = import_source(Path::new(&url))
        .await
        .expect("clone")
        .expect("a URL is cloned");
    let dest = held.path().to_path_buf();
    let err = import_dir(&dest).expect_err("reserved name");
    let GitError::BadPaths(list) = err else {
        panic!("expected BadPaths, got {err:?}");
    };
    let hit = list
        .iter()
        .find(|b| b.reason.contains("reserved-name"))
        .expect("reserved-name");
    // The command's own report, not a line the test assembled.
    let printed = graft_mirror::report_import_failure_lines(&graft_mirror::MirrorError::Git(
        GitError::BadPaths(list.clone()),
    ));
    let line = printed
        .iter()
        .find(|line| line.contains("reserved-name"))
        .expect("the report names the reserved path");
    let (sha, reason) = line.split_once(' ').expect("sha then reason");
    assert_eq!(sha.len(), 40);
    assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(*sha, hit.commit.to_string());
    assert!(reason.contains("reserved-name"));
    drop(held);
    assert!(!dest.exists(), "temp clone is gone after the rejection");
}

#[tokio::test]
async fn a_failed_clone_leaves_no_temp_dir() {
    let missing = tempfile::tempdir().unwrap();
    let url = format!("file://{}/no-such-repo.git", missing.path().display());
    let before: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("graft-mirror-import-"))
        })
        .collect();
    let err = import_source(Path::new(&url))
        .await
        .expect_err("clone fails");
    assert!(err.contains("git"), "{err}");
    let after: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("graft-mirror-import-"))
                && !before.contains(p)
        })
        .collect();
    assert!(after.is_empty(), "failed clone left temp dirs: {after:?}");
}
