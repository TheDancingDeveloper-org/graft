//! WI-2571: with `[github] token_file` set, every git network command the
//! mirror runs (ls-remote, fetch and push, as pr-batch and follow run them)
//! carries the token as an `http.extraHeader` in the environment, never in
//! argv. Without it, git runs with no credential, as before. WI-3492: a
//! `[forgejo] token_file` goes the same way, only to an upstream on the
//! Forgejo API's host.
//!
//! A fake `git` on `PATH` records each call's argv and header. The test
//! re-runs itself in a child process with that `PATH`, since this crate
//! forbids the `unsafe` that changing its own environment needs.

use std::path::Path;

use base64::Engine;
use graft_git::GitSha;
use graft_mirror::config::FileConfig;

const TOKEN: &str = "ghs_wi2571_fake_token_value";

fn config(dir: &Path, token_file: Option<&Path>) -> FileConfig {
    config_at(dir, "https://github.com/o/r.git", &github_table(token_file))
}

fn github_table(token_file: Option<&Path>) -> String {
    token_file
        .map(|p| {
            format!(
                "[github]\nowner = \"o\"\nrepo = \"r\"\ntoken_file = \"{}\"\n",
                p.display()
            )
        })
        .unwrap_or_default()
}

fn forgejo_table(api: &str, token_file: &Path) -> String {
    format!(
        "[forgejo]\napi = \"{api}\"\nowner = \"o\"\nrepo = \"r\"\ntoken_file = \"{}\"\n",
        token_file.display()
    )
}

fn config_at(dir: &Path, upstream: &str, forge: &str) -> FileConfig {
    FileConfig::parse(&format!(
        "server = \"http://graft:7878\"\nrepo = \"r\"\n\
         upstream = \"{upstream}\"\n\
         mirror_dir = \"{}\"\ndb = \"{}\"\n{forge}\
         [[lines]]\nbranch = \"main\"\nline = \"main\"\n",
        dir.join("m.git").display(),
        dir.join("m.sqlite").display(),
    ))
    .unwrap()
}

/// Runs ls-remote, fetch and a push (pr-batch's three network commands)
/// and returns the fake git's log of them.
async fn network_calls(file: &FileConfig, log: &Path) -> String {
    std::fs::write(log, "").unwrap();
    let mirror = file.mirror();
    mirror.ls_remote(&["graft/batch-1"]).await.unwrap();
    mirror.fetch(&["main"]).await.unwrap();
    let sha = GitSha::from_hex(&"ab".repeat(20)).unwrap();
    mirror
        .push_lease("graft/batch-1", sha, None, None)
        .await
        .unwrap();
    std::fs::read_to_string(log).unwrap()
}

const CHILD: &str = "GRAFT_WI2571_CHILD";
const TEST: &str = "every_network_command_carries_the_token_file_in_env_only";

#[test]
fn every_network_command_carries_the_token_file_in_env_only() {
    if let Ok(dir) = std::env::var(CHILD) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        return rt.block_on(check(Path::new(&dir)));
    }
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path();
    let bin = tmp.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = tmp.join("git.log");
    let git = bin.join("git");
    std::fs::write(
        &git,
        format!(
            "#!/bin/sh\nh=none; i=0; while [ \"$i\" -lt \"${{GIT_CONFIG_COUNT:-0}}\" ]; do eval k=\\\"\\$GIT_CONFIG_KEY_$i\\\"; eval v=\\\"\\$GIT_CONFIG_VALUE_$i\\\"; [ \"$k\" = http.extraHeader ] && h=$v; i=$((i+1)); done; printf 'call argv=%s header=%s\\n' \"$*\" \"$h\" >>'{}'\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env("PATH", path)
        .env(CHILD, tmp)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The child ran the checks: a filter that matched nothing also exits 0.
    assert!(
        tmp.join("checked").exists(),
        "the child did not run the checks"
    );
}

async fn check(tmp: &Path) {
    let log = tmp.join("git.log");

    let token_file = tmp.join("github-token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{TOKEN}"));
    let header = format!("Authorization: Basic {basic}");

    // With the token file: all three calls carry the header; argv never
    // holds the token or its encoding.
    let calls = network_calls(&config(tmp, Some(&token_file)), &log).await;
    let lines: Vec<&str> = calls.lines().collect();
    assert_eq!(lines.len(), 3, "{calls}");
    for (line, cmd) in lines.iter().zip(["ls-remote", "fetch", "push"]) {
        let (argv, got) = line.split_once(" header=").unwrap();
        assert!(argv.contains(cmd), "{line}");
        assert_eq!(got, header, "{cmd} lacked the credential");
        assert!(
            !argv.contains(TOKEN) && !argv.contains(&basic),
            "{cmd} put the token in argv"
        );
    }

    // No [github]: the same calls run with no credential (public upstreams).
    let calls = network_calls(&config(tmp, None), &log).await;
    assert_eq!(calls.lines().count(), 3, "{calls}");
    assert!(
        calls.lines().all(|l| l.ends_with(" header=none")),
        "{calls}"
    );

    // [forgejo] alone: its token goes to an upstream on the API's host,
    // header for header as GitHub's, never in argv.
    let fj_token = tmp.join("forgejo-token");
    std::fs::write(&fj_token, format!("{TOKEN}\n")).unwrap();
    let fj = config_at(
        tmp,
        "https://git.example/o/r.git",
        &forgejo_table("https://git.example/api/v1", &fj_token),
    );
    let calls = network_calls(&fj, &log).await;
    assert_eq!(calls.lines().count(), 3, "{calls}");
    for line in calls.lines() {
        let (argv, got) = line.split_once(" header=").unwrap();
        assert_eq!(got, header, "forgejo: {line} lacked the credential");
        assert!(!argv.contains(TOKEN) && !argv.contains(&basic), "{line}");
    }

    // An upstream on another host never gets the Forgejo token.
    let elsewhere = config_at(
        tmp,
        "https://elsewhere.example/o/r.git",
        &forgejo_table("https://git.example/api/v1", &fj_token),
    );
    let calls = network_calls(&elsewhere, &log).await;
    assert_eq!(calls.lines().count(), 3, "{calls}");
    assert!(
        calls.lines().all(|l| l.ends_with(" header=none")),
        "{calls}"
    );

    // An empty token file: a warning, and no credential rather than a failure.
    std::fs::write(&token_file, "").unwrap();
    let calls = network_calls(&config(tmp, Some(&token_file)), &log).await;
    assert!(
        calls.lines().all(|l| l.ends_with(" header=none")),
        "{calls}"
    );
    std::fs::write(tmp.join("checked"), "").unwrap();
}
