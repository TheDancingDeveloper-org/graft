//! Local mode acceptance (WI-2366): one binary, no server process.
//!
//! Init, a second working copy via clone, two non-overlapping lands that
//! line-merge, a conflicting land that exits 3, then resolve and land.
//! A second test kills a child mid-land and checks the acknowledged
//! changeset and `main` ref are still there after reopen.

mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use graft_client::local::{LocalRepo, META_FILE, OBJECTS_DIR};
use graft_model::ChangesetId;
use graft_proto::api::RepoApi;
use graft_proto::types::ListRefs;
use support::graft;

fn author(cmd: &mut assert_cmd::Command, config_root: &Path) {
    cmd.env("GRAFT_AUTHOR_NAME", "Test Author")
        .env("GRAFT_AUTHOR_EMAIL", "author@example.com")
        .env("GRAFT_AUTHOR_KIND", "agent")
        .env("GRAFT_AUTHOR_DATE", "1700000000000")
        .env("GRAFT_CONFIG", config_root.join(".graft-test-config.toml"));
}

fn run(cwd: &Path, config_root: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = graft(cwd);
    author(&mut cmd, config_root);
    cmd.args(args).assert()
}

fn file_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn local_quick_start_two_copies_merge_and_conflict() {
    let dir = support::scratch();
    let root = dir.path().join("work");
    fs::create_dir(&root).unwrap();
    let demo = root.join("demo");
    run(&root, dir.path(), &["init", "demo", "--repo", "demo"]).success();
    fs::write(demo.join("file.txt"), "alpha\nshared\nomega\n").unwrap();
    run(&demo, dir.path(), &["commit", "-m", "start"]).success();
    run(&demo, dir.path(), &["land"]).success();

    let second = root.join("demo-2");
    run(
        &root,
        dir.path(),
        &["clone", demo.to_str().unwrap(), second.to_str().unwrap()],
    )
    .success();

    fs::write(demo.join("file.txt"), "ALPHA\nshared\nomega\n").unwrap();
    run(&demo, dir.path(), &["commit", "-m", "top"]).success();
    run(&demo, dir.path(), &["land"]).success();

    fs::write(second.join("file.txt"), "alpha\nshared\nOMEGA\n").unwrap();
    run(&second, dir.path(), &["commit", "-m", "bottom"]).success();
    run(&second, dir.path(), &["land"]).success();
    let merged = file_text(&second.join("file.txt"));
    assert!(
        merged.contains("ALPHA") && merged.contains("OMEGA"),
        "line merge kept both edits:\n{merged}"
    );

    fs::write(demo.join("file.txt"), "LEFT\nshared\nomega\n").unwrap();
    run(&demo, dir.path(), &["commit", "-m", "left"]).success();
    run(&demo, dir.path(), &["land"]).success();
    fs::write(second.join("file.txt"), "RIGHT\nshared\nOMEGA\n").unwrap();
    run(&second, dir.path(), &["commit", "-m", "right"]).success();
    run(&second, dir.path(), &["land", "--sync"]).code(3);

    let conflicted = file_text(&second.join("file.txt"));
    let resolved = conflicted
        .lines()
        .filter(|line| {
            !line.starts_with("<<<<<<<") && !line.starts_with(">>>>>>>") && *line != "======="
        })
        .map(|line| {
            if line == "LEFT" || line == "RIGHT" {
                "BOTH"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(second.join("file.txt"), resolved).unwrap();
    run(&second, dir.path(), &["commit", "-m", "resolved"]).success();
    run(&second, dir.path(), &["land"]).success();
    assert!(file_text(&second.join("file.txt")).contains("BOTH"));
}

#[test]
fn kill_during_local_land_keeps_the_acknowledged_write() {
    let dir = support::scratch();
    let root = dir.path().join("work");
    fs::create_dir(&root).unwrap();
    let demo = root.join("demo");
    run(&root, dir.path(), &["init", "demo", "--repo", "demo"]).success();
    fs::write(demo.join("file.txt"), "one\n").unwrap();
    run(&demo, dir.path(), &["commit", "-m", "start"]).success();
    run(&demo, dir.path(), &["land"]).success();

    let repo_dir = demo.join(".graft").join("repo");
    let before = open_main(&repo_dir);
    fs::write(demo.join("file.txt"), "one\ntwo\n").unwrap();
    run(&demo, dir.path(), &["commit", "-m", "two"]).success();

    let bin = assert_cmd::cargo::cargo_bin("graft");
    let mut child = Command::new(bin)
        .current_dir(&demo)
        .args(["land"])
        .env_remove("GRAFT_ENDPOINT")
        .env_remove("GRAFT_TOKEN")
        .env("GRAFT_AUTHOR_NAME", "Test Author")
        .env("GRAFT_AUTHOR_EMAIL", "author@example.com")
        .env("GRAFT_AUTHOR_KIND", "agent")
        .env("GRAFT_AUTHOR_DATE", "1700000001000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn land");
    thread::sleep(Duration::from_millis(20));
    let _ = child.kill();
    let _ = child.wait();

    let after = open_main(&repo_dir);
    if after != before {
        let after = after.expect("moved tip");
        let repo = LocalRepo::open(&repo_dir).expect("reopen");
        let cx = LocalRepo::context("demo", "kill-check");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let got = repo
                .engine()
                .get_changesets(
                    &cx,
                    graft_proto::types::GetChangesets {
                        repo: "demo".into(),
                        ids: vec![after],
                    },
                )
                .await
                .expect("acknowledged changeset is readable");
            assert_eq!(got.missing, Vec::<ChangesetId>::new());
            assert_eq!(got.found.len(), 1);
        });
    }
    // Either the land had not started (tip unchanged) or it committed
    // atomically: the tip still resolves and the stores reopen.
    let _ = (META_FILE, OBJECTS_DIR);
    assert!(repo_dir.join(META_FILE).is_file());
    assert!(repo_dir.join(OBJECTS_DIR).is_dir());
}

fn open_main(repo_dir: &Path) -> Option<ChangesetId> {
    let repo = LocalRepo::open(repo_dir).expect("open");
    let cx = LocalRepo::context("demo", "tip");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let refs = repo
            .engine()
            .list_refs(
                &cx,
                ListRefs {
                    repo: "demo".into(),
                    prefix: Some("main".into()),
                    page: Default::default(),
                },
            )
            .await
            .expect("list refs");
        refs.into_iter()
            .find(|r| r.name == "main" || r.name.ends_with("/main"))
            .map(|r| r.target)
    })
}
