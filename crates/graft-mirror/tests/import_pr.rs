//! The scratch repository and change checks behind `import-pr` (WI-2716).

use std::path::Path;
use std::process::Command;

use graft_git::GitSha;
use graft_mirror::git::Mirror;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn sha(s: &str) -> GitSha {
    GitSha::from_hex(s).unwrap()
}

#[tokio::test]
async fn a_pre_existing_scratch_directory_is_never_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let planted = tmp.path().join("repo");
    git(tmp.path(), &["init", "-q", "--bare", "repo"]);
    git(&planted, &["config", "core.hooksPath", "/tmp/evil"]);
    let mirror = Mirror::new(&planted, "https://github.com/o/r.git");
    let err = mirror.init_fresh().await.unwrap_err().to_string();
    assert!(err.contains("scratch repository"), "{err}");
    assert_eq!(
        git(&planted, &["config", "core.hooksPath"]),
        "/tmp/evil",
        "the planted repository is left untouched, not adopted"
    );
}

#[tokio::test]
async fn a_fresh_scratch_repository_is_private_and_runs_no_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    Mirror::new(&dir, "https://github.com/o/r.git")
        .init_fresh()
        .await
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    assert_eq!(git(&dir, &["config", "core.hooksPath"]), "/dev/null");
    assert_eq!(git(&dir, &["config", "core.fsmonitor"]), "false");
}

/// A repository with a base commit and a head that adds `n` files, a
/// symlink and a submodule entry. Returns (dir, base, head).
fn history(tmp: &Path, n: usize) -> (std::path::PathBuf, GitSha, GitSha) {
    let dir = tmp.join("src");
    std::fs::create_dir(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    let base = git(&dir, &["rev-parse", "HEAD"]);
    for i in 0..n {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
    git(&dir, &["add", "."]);
    git(
        &dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{base},sub"),
        ],
    );
    git(&dir, &["commit", "-q", "-m", "head"]);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    (dir.join(".git"), sha(&base), sha(&head))
}

#[tokio::test]
async fn changed_paths_report_links_and_refuse_past_the_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, base, head) = history(tmp.path(), 3);
    let mirror = Mirror::new(&dir, "unused");
    let paths = mirror.changed_paths(base, head, 100).await.unwrap();
    let links: Vec<&str> = paths
        .iter()
        .filter(|p| p.is_link())
        .map(|p| p.path.as_str())
        .collect();
    #[cfg(unix)]
    assert_eq!(links, ["link", "sub"]);
    assert_eq!(paths.len(), 3 + links.len());
    assert!(
        paths
            .iter()
            .filter(|p| !p.is_link())
            .all(|p| p.new_mode == "100644")
    );

    let err = mirror
        .changed_paths(base, head, 2)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("more than 2 paths"), "{err}");
}

#[tokio::test]
async fn a_patch_over_the_byte_cap_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, base, head) = history(tmp.path(), 1);
    let mirror = Mirror::new(&dir, "unused");
    let patch = mirror.binary_diff(base, head, 1 << 20).await.unwrap();
    assert!(!patch.is_empty());
    let err = mirror
        .binary_diff(base, head, patch.len() - 1)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("cap"), "{err}");
}
