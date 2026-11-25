//! Flags other tools have (git above all): `log -n`, `--version`, paths after
//! `--` on `diff`, and a "did you mean" hint for each flag graft spells
//! differently or lacks (WI-3738).

mod support;

use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::json;
use support::*;

/// Runs `graft <args>` outside any working copy and server; a usage error
/// is decided before anything connects.
fn usage(args: &[&str]) -> assert_cmd::assert::Assert {
    let dir = scratch();
    graft(dir.path()).args(args).assert()
}

#[test]
fn log_dash_n_is_limit() {
    let s = serve();
    let dir = scratch();
    let run = |flag: &[&str]| {
        let out = graft(dir.path())
            .args(["--endpoint", &s.url, "--repo", REPO, "--json", "log"])
            .args(flag)
            .assert()
            .success();
        json_out(out.get_output())
    };
    let short = run(&["-n", "1"]);
    assert_eq!(short["changesets"].as_array().map(Vec::len), Some(1));
    assert_eq!(short["next"], json!("more"));
    assert_eq!(short, run(&["--limit", "1"]));
    assert_eq!(short, run(&["-n1"]));
}

#[test]
fn version_flags_print_the_version() {
    let dir = scratch();
    for flag in ["--version", "-V"] {
        let out = graft(dir.path()).arg(flag).assert().success();
        let shown = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
        assert!(
            shown.starts_with(concat!("graft ", env!("CARGO_PKG_VERSION"), "+")),
            "{shown}"
        );
    }
    let out = graft(dir.path())
        .args(["--version", "--json"])
        .assert()
        .success();
    let value = json_out(out.get_output());
    assert_valid("version", &value);
}

#[test]
fn diff_takes_paths_after_dashes() {
    let s = serve();
    let dir = scratch();
    let app = dir.path().join("app");
    graft(dir.path())
        .args(["--endpoint", &s.url, "clone", REPO, "app"])
        .assert()
        .success();
    std::fs::write(app.join("README.md"), "edited\n").unwrap();

    let names = |args: &[&str]| {
        let out = graft(&app).args(args).assert().success();
        String::from_utf8_lossy(&out.get_output().stdout).into_owned()
    };
    assert_eq!(
        names(&["diff", "--name-only", "--", "README.md"]),
        "README.md\n"
    );
    assert_eq!(
        names(&["diff", "--name-only", "--", "other.rs", "README.md"]),
        "README.md\n"
    );
    assert_eq!(names(&["diff", "--name-only", "--", "other.rs"]), "");
    assert_eq!(
        names(&["diff", "--name-only", "--path", "README.md"]),
        "README.md\n"
    );
}

#[test]
fn diff_paths_after_dashes_filter_two_changesets() {
    let s = serve();
    let dir = scratch();
    let (from, to) = (s.commits.first.to_string(), s.commits.second.to_string());
    let run = |extra: &[&str]| {
        let out = graft(dir.path())
            .args([
                "--endpoint",
                &s.url,
                "--repo",
                REPO,
                "--json",
                "diff",
                &from,
                &to,
            ])
            .args(extra)
            .assert()
            .success();
        json_out(out.get_output())
    };
    let all = run(&[]);
    let first = all["files"][0]["path"].as_str().unwrap().to_owned();
    let only = run(&["--", &first]);
    assert_eq!(only, run(&["--path", &first]));
    assert!(
        only["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["path"] == first.as_str())
    );
    let none = run(&["--", "no/such/file"]);
    assert_eq!(none["files"].as_array().map(Vec::len), Some(0));
}

/// Each unknown flag, and the text its hint must contain.
const HINTED: &[(&[&str], &str)] = &[
    (&["log", "--max-count", "3"], "--limit"),
    (&["log", "--oneline"], "one line per changeset"),
    (&["diff", "--check"], "--stat"),
    (&["diff", "--cached"], "no staging area"),
    (&["diff", "--staged"], "no staging area"),
    (&["diff", "--name-status"], "--name-only"),
    (&["diff", "--numstat"], "--stat"),
    (&["diff", "--shortstat"], "--stat"),
    (&["status", "--short"], "--json"),
    (&["status", "-s"], "--json"),
    (&["status", "--porcelain"], "--json"),
    (&["status", "--workspace", "dir"], "-C <dir>"),
    (&["land", "--line", "main"], "graft line merge"),
    (&["land", "--no-verify"], "--no-check"),
    (&["land", "--force"], "--sync"),
    (&["cat", "-r", "main", "README.md"], "--at <REV>"),
    (&["cat", "--rev", "main", "README.md"], "--at <REV>"),
    (&["cat", "--revision", "main", "README.md"], "--at <REV>"),
];

#[test]
fn every_known_git_flag_gets_a_hint() {
    for (args, expect) in HINTED {
        usage(args)
            .code(2)
            .stderr(contains("did you mean").and(contains(*expect)));
    }
}

#[test]
fn hint_is_in_the_json_error_too() {
    let out = usage(&["--json", "cat", "-r", "main", "README.md"]).code(2);
    let value = json_out(out.get_output());
    let message = value["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("did you mean") && message.contains("--at"),
        "{value}"
    );
}

#[test]
fn hints_follow_global_flags_and_leave_other_flags_alone() {
    usage(&["--endpoint", "http://127.0.0.1:1", "cat", "-r", "x", "f"])
        .code(2)
        .stderr(contains("--at <REV>"));
    // `-r` is only a hint on `cat`; elsewhere it is a plain unknown flag.
    usage(&["log", "-r", "x"])
        .code(2)
        .stderr(contains("unexpected argument").and(contains("did you mean").not()));
}
