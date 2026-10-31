//! The import-pr scratch repository ignores the user's git config (WI-2716).
//!
//! The test runs its checks in a child process whose environment carries a
//! global git config that rewrites URLs. Setting `GIT_CONFIG_GLOBAL` on the
//! child rather than on this process needs no `unsafe`, which the workspace
//! forbids.

use std::path::{Path, PathBuf};
use std::process::Command;

use graft_mirror::git::Mirror;

/// Set in the child's environment: the directory holding the fixtures.
const CHILD_DIR_ENV: &str = "GRAFT_IMPORT_PR_CONFIG_DIR";
const TEST_NAME: &str = "a_global_config_does_not_redirect_the_scratch_fetch";

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

/// A repository with one commit on `main`, whose file says `what`.
fn repo_with_commit(dir: &Path, what: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), what).unwrap();
    git(dir, &["add", "."]);
    git(
        dir,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", what],
    );
}

#[tokio::test]
async fn a_global_config_does_not_redirect_the_scratch_fetch() {
    if let Some(dir) = std::env::var_os(CHILD_DIR_ENV) {
        check_in_child(&PathBuf::from(dir)).await;
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    repo_with_commit(&tmp.path().join("real"), "real");
    repo_with_commit(&tmp.path().join("decoy"), "decoy");
    let real = tmp.path().join("real");
    let decoy = tmp.path().join("decoy");
    // The user's global config sends every fetch of `real` to `decoy`.
    let global = tmp.path().join("gitconfig");
    std::fs::write(
        &global,
        format!(
            "[url \"{}\"]\n\tinsteadOf = {}\n",
            decoy.display(),
            real.display()
        ),
    )
    .unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
        .env("GIT_CONFIG_GLOBAL", &global)
        .env(CHILD_DIR_ENV, tmp.path())
        .status()
        .unwrap();
    assert!(status.success(), "the child check failed: {status}");
}

/// The checks, run with the global config in the environment.
async fn check_in_child(dir: &Path) {
    let (real, decoy) = (dir.join("real"), dir.join("decoy"));
    let real_head = git(&real, &["rev-parse", "HEAD"]);
    let decoy_head = git(&decoy, &["rev-parse", "HEAD"]);
    assert_ne!(real_head, decoy_head);
    let real_url = real.to_str().unwrap();

    // Control: plain git in this environment does follow the rewrite, so
    // the assertion below can only pass if the scratch fetch ignores it.
    let control = git(dir, &["ls-remote", real_url, "refs/heads/main"]);
    assert!(control.starts_with(&decoy_head), "control: {control}");

    let mirror = Mirror::scratch(dir.join("repo"), real_url);
    mirror.init_fresh().await.unwrap();
    mirror.fetch(&["main"]).await.unwrap();
    let fetched = mirror
        .local_ref(&Mirror::upstream_ref("main"))
        .await
        .unwrap()
        .map(|sha| sha.to_hex());
    assert_eq!(fetched.as_deref(), Some(real_head.as_str()));
}

const PARAMS_TEST_NAME: &str = "a_config_parameter_does_not_redirect_the_scratch_fetch";

/// `real` and `decoy` in `dir`, each with one commit on `main`.
fn real_and_decoy(dir: &Path) -> (PathBuf, PathBuf) {
    let (real, decoy) = (dir.join("real"), dir.join("decoy"));
    repo_with_commit(&real, "real");
    repo_with_commit(&decoy, "decoy");
    (real, decoy)
}

#[tokio::test]
async fn a_config_parameter_does_not_redirect_the_scratch_fetch() {
    if let Some(dir) = std::env::var_os(CHILD_DIR_ENV) {
        check_parameter_in_child(&PathBuf::from(dir)).await;
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (real, decoy) = real_and_decoy(tmp.path());
    // What `git -c url.<decoy>.insteadOf=<real>` leaves in the environment
    // of every child git: the same rewrite as the global config test.
    let params = format!("'url.{}.insteadOf'='{}'", decoy.display(), real.display());
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            PARAMS_TEST_NAME,
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GIT_CONFIG_PARAMETERS", params)
        .env(CHILD_DIR_ENV, tmp.path())
        .status()
        .unwrap();
    assert!(status.success(), "the child check failed: {status}");
}

/// The checks, run with the `-c` rewrite in the environment.
async fn check_parameter_in_child(dir: &Path) {
    let (real, decoy) = (dir.join("real"), dir.join("decoy"));
    let real_head = git(&real, &["rev-parse", "HEAD"]);
    let decoy_head = git(&decoy, &["rev-parse", "HEAD"]);
    assert_ne!(real_head, decoy_head);
    let real_url = real.to_str().unwrap();

    // Control: a mirror without isolation follows the rewrite, so the
    // isolated fetch below can only pass by ignoring the inherited variable.
    let plain = Mirror::new(dir.join("plain"), real_url);
    plain.ensure().await.unwrap();
    plain.fetch(&["main"]).await.unwrap();
    let followed = plain
        .local_ref(&Mirror::upstream_ref("main"))
        .await
        .unwrap()
        .map(|sha| sha.to_hex());
    assert_eq!(followed.as_deref(), Some(decoy_head.as_str()));

    let mirror = Mirror::scratch(dir.join("repo"), real_url);
    mirror.init_fresh().await.unwrap();
    mirror.fetch(&["main"]).await.unwrap();
    let fetched = mirror
        .local_ref(&Mirror::upstream_ref("main"))
        .await
        .unwrap()
        .map(|sha| sha.to_hex());
    assert_eq!(fetched.as_deref(), Some(real_head.as_str()));
}
