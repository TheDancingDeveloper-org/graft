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
use std::time::{Duration, Instant};

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
    fs::write(demo.join("file.txt"), "base\n").unwrap();
    run(&demo, dir.path(), &["commit", "-m", "start"]).success();
    let setup = run(&demo, dir.path(), &["land", "--json"]).success();
    let setup_out = String::from_utf8_lossy(&setup.get_output().stdout).into_owned();
    let setup_id = serde_json::from_str::<serde_json::Value>(setup_out.trim())
        .ok()
        .filter(|value| value.get("outcome").and_then(|v| v.as_str()) == Some("landed"))
        .and_then(|value| {
            value
                .get("landed")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .expect("setup land is acknowledged with a changeset id");

    let repo_dir = demo.join(".graft").join("repo");
    let mut acked: Vec<String> = vec![setup_id];
    let mut killed = 0u32;
    // Keep going until several lands die mid-operation. A land that
    // finishes before the kill does not count. Hitting the bound is the
    // failure; there is no minimum that a short fixed loop can miss.
    const KILLS: u32 = 3;
    const BOUND: u32 = 40;
    let mut seed: u64 = 0x2366;
    let mut n = 0u32;
    while killed < KILLS {
        assert!(
            n < BOUND,
            "stopped after {BOUND} lands with only {killed} mid-land kills"
        );
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let delay = Duration::from_millis(15 + (seed % 30));
        fs::write(demo.join("file.txt"), format!("base\nedit {n}\n")).unwrap();
        run(&demo, dir.path(), &["commit", "-m", &format!("edit {n}")]).success();
        match land_then_kill(&demo, dir.path(), delay) {
            LandEnd::Ack(id) => acked.push(id),
            LandEnd::Killed { id } => {
                killed += 1;
                if let Some(id) = id {
                    acked.push(id);
                }
            }
            LandEnd::Failed(code) => panic!("land exited {code} before a kill"),
        }
        n += 1;
    }
    assert!(!acked.is_empty(), "at least one land was acknowledged");

    let repo = LocalRepo::open(&repo_dir).expect("lock is recoverable");
    let cx = LocalRepo::context("demo", "kill-check");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ids: Vec<ChangesetId> = acked
            .iter()
            .map(|id| ChangesetId::parse(id).expect("changeset id"))
            .collect();
        let got = repo
            .engine()
            .get_changesets(
                &cx,
                graft_proto::types::GetChangesets {
                    repo: "demo".into(),
                    ids: ids.clone(),
                },
            )
            .await
            .expect("reopen");
        assert!(
            got.missing.is_empty(),
            "acknowledged changesets missing after kill: {:?}",
            got.missing
        );
        assert_eq!(got.found.len(), ids.len());
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
            .expect("main ref");
        assert!(
            refs.iter()
                .any(|r| r.name == "main" || r.name.ends_with("/main")),
            "main ref survived"
        );
    });
    drop(repo);
    assert_eq!(integrity_check(&repo_dir.join(META_FILE)), "ok");
    assert!(repo_dir.join(OBJECTS_DIR).is_dir());
    // A later open is the lock recovery: the killed process must not leave
    // the repository unopenable.
    let _again = LocalRepo::open(&repo_dir).expect("second open after kill");
}

enum LandEnd {
    Ack(String),
    Killed { id: Option<String> },
    Failed(i32),
}

/// Runs `graft land --json`, kills it after `delay` from spawn, and returns
/// the landed id only when a complete JSON line named it.
fn land_then_kill(cwd: &Path, config_root: &Path, delay: Duration) -> LandEnd {
    let bin = assert_cmd::cargo::cargo_bin("graft");
    let mut child = Command::new(bin)
        .current_dir(cwd)
        .args(["land", "--json"])
        .env_remove("GRAFT_ENDPOINT")
        .env_remove("GRAFT_TOKEN")
        .env_remove("VOGT_SESSION_ID")
        .env_remove("VOGT_WORK_ITEM")
        .env("GRAFT_AUTHOR_NAME", "Test Author")
        .env("GRAFT_AUTHOR_EMAIL", "author@example.com")
        .env("GRAFT_AUTHOR_KIND", "agent")
        .env("GRAFT_AUTHOR_DATE", "1700000001000")
        .env("GRAFT_CONFIG", config_root.join(".graft-test-config.toml"))
        .env("USER", "graft-test")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn land");
    let started = Instant::now();
    loop {
        if child.try_wait().expect("poll").is_some() {
            break;
        }
        if started.elapsed() >= delay {
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let status = child.wait().expect("wait");
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout.read_to_string(&mut out);
    }
    let id = serde_json::from_str::<serde_json::Value>(out.trim())
        .ok()
        .filter(|value| value.get("outcome").and_then(|v| v.as_str()) == Some("landed"))
        .and_then(|value| {
            value
                .get("landed")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        });
    if status.success() {
        LandEnd::Ack(id.expect("acknowledged land prints landed"))
    } else if status.code().is_none() {
        LandEnd::Killed { id }
    } else {
        LandEnd::Failed(status.code().unwrap_or(1))
    }
}

fn integrity_check(meta: &Path) -> String {
    let conn = rusqlite::Connection::open(meta).expect("open meta");
    conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check")
}
