//! The write commands (G2.5) against an in-process server with workspaces:
//! `commit [--amend]`, `workspace`, `stack list`, `sync` and `land`, their
//! human and JSON output (each JSON document checked against its schema),
//! and the exit codes of design §8.2.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::assert::Assert;
use graft_proto::ApiError;
use predicates::str::contains;
use serde_json::{Value, json};
use support::*;

/// `graft clone demo app` in a fresh directory.
fn clone_app(s: &Served, dir: &Path) -> PathBuf {
    graft(dir)
        .args(["--endpoint", &s.url, "clone", REPO, "app"])
        .assert()
        .success();
    dir.join("app")
}

/// A `graft` command in the working copy with a fixed author and time, so
/// changeset ids are reproducible.
fn wc_cmd(app: &Path) -> assert_cmd::Command {
    let mut cmd = graft(app);
    cmd.env("GRAFT_AUTHOR_NAME", "Test Author")
        .env("GRAFT_AUTHOR_EMAIL", "author@example.com")
        .env("GRAFT_AUTHOR_KIND", "agent")
        .env("GRAFT_AUTHOR_DATE", "1700000000000");
    cmd
}

fn run(app: &Path, args: &[&str]) -> Assert {
    wc_cmd(app).args(args).assert()
}

fn json_run(app: &Path, args: &[&str]) -> Value {
    let out = wc_cmd(app).arg("--json").args(args).assert().success();
    json_out(out.get_output())
}

fn text(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn error_kind(a: &Assert) -> Value {
    let value = json_out(a.get_output());
    assert_valid("error", &value);
    value["error"]["kind"].clone()
}

/// The fixture's `main` files after its three commits, with `extra` on top.
fn main_files(extra: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert("README.md".into(), b"hello\n".to_vec());
    files.insert("src/lib.rs".into(), b"fn a() {}\nfn b() {}\n".to_vec());
    files.insert("assets/logo.bin".into(), vec![0xff, 0x00, 0x7f]);
    files.insert("docs/guide.md".into(), b"read me\n".to_vec());
    for (k, v) in extra {
        files.insert((*k).to_owned(), v.to_vec());
    }
    files
}

#[test]
fn commit_human_and_json() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path());

    fs::write(app.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    let out = run(&app, &["commit", "-m", "add c"]).success();
    insta::assert_snapshot!("commit_human", text(&out));

    fs::write(app.join("src/new.rs"), "new\n").unwrap();
    fs::remove_file(app.join("docs/guide.md")).unwrap();
    let value = json_run(
        &app,
        &["commit", "-m", "more", "--meta", "vogt.work_item=WI-1"],
    );
    assert_valid("commit", &value);
    insta::assert_json_snapshot!("commit_json", value);
    assert_eq!(value["upload"]["uploaded"], json!(1));
    assert_eq!(value["workspace"], json!("w-1"));

    // The working copy is clean against its new head, and status says so.
    let status = json_run(&app, &["status"]);
    assert_eq!(status["clean"], json!(true));
    assert_eq!(status["head"], value["id"]);
}

#[test]
fn commit_needs_a_message_and_a_change() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    run(&app, &["commit"])
        .code(2)
        .stderr(contains("needs a message"));
    let a = run(&app, &["--json", "commit", "-m", "nothing"]).code(1);
    assert_eq!(error_kind(&a), json!("nothing_to_commit"));
    let a = run(&app, &["--json", "commit", "--amend"]).code(1);
    assert_eq!(error_kind(&a), json!("no_draft"));
    // Outside a working copy: a usage error.
    let elsewhere = scratch();
    run(elsewhere.path(), &["commit", "-m", "x"])
        .code(2)
        .stderr(contains("not inside a working copy"));
}

#[test]
fn commit_refuses_the_working_copys_own_state_in_any_spelling() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    fs::create_dir(app.join(".GRAFT")).unwrap();
    fs::write(app.join(".GRAFT/config"), "repo = evil\n").unwrap();
    let a = run(&app, &["--json", "commit", "-m", "evil"]).code(2);
    assert_eq!(error_kind(&a), json!("invalid_path"));
}

#[test]
fn amend_supersedes_the_head_and_the_stack_lists_drafts() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    fs::write(app.join("README.md"), "hello again\n").unwrap();
    let first = json_run(&app, &["commit", "-m", "first"]);
    fs::write(app.join("src/lib.rs"), "changed\n").unwrap();
    let second = json_run(&app, &["commit", "-m", "second"]);

    let out = run(&app, &["stack", "list"]).success();
    insta::assert_snapshot!("stack_list_human", text(&out));

    fs::write(app.join("src/lib.rs"), "changed again\n").unwrap();
    let amended = json_run(&app, &["commit", "--amend"]);
    assert_valid("commit", &amended);
    assert_eq!(amended["superseded"], second["id"]);
    assert_eq!(amended["parent"], first["id"]);
    insta::assert_json_snapshot!("commit_amend_json", amended);

    let stack = json_run(&app, &["stack"]);
    assert_valid("stack", &stack);
    let ids: Vec<&Value> = stack["drafts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| &d["id"])
        .collect();
    assert_eq!(ids, vec![&first["id"], &amended["id"]]);
    assert_eq!(stack["drafts"][1]["subject"], json!("second"));
    insta::assert_json_snapshot!("stack_json", stack);
}

#[test]
fn workspace_show_list_renew_and_close() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path());

    // Before any write the working copy has no workspace: open makes one.
    let opened = json_run(&app, &["workspace", "open"]);
    assert_valid("workspace", &opened);
    assert_eq!(opened["opened"], json!(true));
    assert_eq!(opened["name"], json!("w-1"));
    assert_eq!(opened["base"], json!(s.commits.third.to_string()));
    let again = json_run(&app, &["workspace", "open"]);
    assert_eq!(again["opened"], json!(false));
    run(&app, &["workspace", "open", "other"])
        .code(2)
        .stderr(contains("already has workspace w-1"));

    let mut shown = json_run(&app, &["workspace"]);
    assert_valid("workspace", &shown);
    shown["lease_until_ms"] = json!(0);
    insta::assert_json_snapshot!("workspace_show_json", shown);

    let list = json_run(&app, &["workspace", "list"]);
    assert_valid("workspace-list", &list);
    assert_eq!(list["workspaces"][0]["name"], json!("w-1"));
    assert_eq!(list["next"], Value::Null);

    let renewed = json_run(&app, &["workspace", "renew", "--lease", "60"]);
    assert_valid("workspace", &renewed);

    // Outside a working copy, by name.
    let elsewhere = scratch();
    let out = graft(elsewhere.path())
        .args(["--json", "--endpoint", &s.url, "--repo", REPO])
        .args(["workspace", "open", "ci-job", "--line", "main"])
        .assert()
        .success();
    let value = json_out(out.get_output());
    assert_valid("workspace", &value);
    assert_eq!(value["name"], json!("ci-job"));

    let closed = json_run(&app, &["workspace", "close"]);
    assert_valid("workspace", &closed);
    assert_eq!(closed, json!({"name": "w-1", "closed": true}));
    let a = run(&app, &["--json", "workspace"]).code(2);
    assert_eq!(error_kind(&a), json!("usage"));
    assert!(s.fixture.repo().workspace("w-1").is_none());
}

#[test]
fn sync_fast_forwards_and_refuses_to_overwrite_local_changes() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    let tip = s.rt.block_on(
        s.fixture
            .repo()
            .commit("main", &main_files(&[("README.md", b"theirs\n")])),
    );

    fs::write(app.join("README.md"), "mine\n").unwrap();
    let a = run(&app, &["--json", "sync"]).code(3);
    assert_eq!(error_kind(&a), json!("dirty"));
    assert_eq!(fs::read_to_string(app.join("README.md")).unwrap(), "mine\n");

    fs::write(app.join("README.md"), "hello\n").unwrap();
    let value = json_run(&app, &["sync"]);
    assert_valid("sync", &value);
    assert_eq!(value["base"], json!(tip.to_string()));
    insta::assert_json_snapshot!("sync_json", value);
    assert_eq!(
        fs::read_to_string(app.join("README.md")).unwrap(),
        "theirs\n"
    );

    let out = run(&app, &["sync"]).success();
    insta::assert_snapshot!("sync_up_to_date", text(&out));
}

#[test]
fn sync_with_a_conflicting_stack_exits_3_and_writes_the_conflicts() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    fs::write(app.join("README.md"), "mine\n").unwrap();
    run(&app, &["commit", "-m", "mine"]).success();
    s.rt.block_on(
        s.fixture
            .repo()
            .commit("main", &main_files(&[("README.md", b"theirs\n")])),
    );
    let a = run(&app, &["--json", "sync"]).code(3);
    assert_eq!(error_kind(&a), json!("conflicted"));
    assert_eq!(
        fs::read_to_string(app.join("README.md")).unwrap(),
        "<<<<<<< ours\nmine\n||||||| base\nhello\n=======\ntheirs\n>>>>>>> theirs\n"
    );
}

#[test]
fn land_on_a_direct_line() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    let a = run(&app, &["--json", "land"]).code(1);
    assert_eq!(error_kind(&a), json!("no_draft"));

    fs::write(app.join("README.md"), "landed\n").unwrap();
    let draft = json_run(&app, &["commit", "-m", "land me"]);
    let value = json_run(&app, &["land"]);
    assert_valid("land", &value);
    assert_eq!(value["landed"], draft["id"]);
    insta::assert_json_snapshot!("land_landed_json", value);
    let status = json_run(&app, &["status"]);
    assert_eq!(status["base"], draft["id"]);
    assert_eq!(status["head"], Value::Null);
}

#[test]
fn land_exit_codes_follow_the_design() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    fs::write(app.join("README.md"), "mine\n").unwrap();
    let draft = json_run(&app, &["commit", "-m", "mine"]);
    let tip = s.commits.third;

    // Contended past the wait (none): 4.
    s.fixture.repo().fail_next(
        "Land",
        ApiError::Contended {
            line: "main".into(),
            tip,
        },
    );
    let a = run(&app, &["--json", "land", "--wait", "0"]).code(4);
    assert_eq!(error_kind(&a), json!("contended"));

    // A policy refusal: 5. Not authorized: 6. Unavailable: 7.
    let cases = [
        (
            ApiError::Rejected {
                policy: "attested".into(),
                detail: "missing ci/test".into(),
            },
            5,
            "rejected",
        ),
        (ApiError::Unauthenticated, 6, "unauthenticated"),
        (
            ApiError::Unavailable { retry_after_ms: 0 },
            7,
            "unavailable",
        ),
    ];
    for (err, code, kind) in cases {
        s.fixture.repo().fail_next("Land", err);
        let a = run(&app, &["--json", "land"]).code(code);
        assert_eq!(error_kind(&a), json!(kind));
    }

    // The line moved under the stack: the fake does not rebase, so 3.
    s.rt.block_on(
        s.fixture
            .repo()
            .commit("main", &main_files(&[("docs/guide.md", b"theirs\n")])),
    );
    let a = run(&app, &["--json", "land"]).code(3);
    assert_eq!(error_kind(&a), json!("conflict"));
    let status = json_run(&app, &["status"]);
    assert_eq!(status["head"], draft["id"], "the draft is untouched");
}

#[test]
fn land_through_the_pr_bridge() {
    let s = serve();
    s.fixture
        .repo()
        .set_git_primary("main", "https://github.example/o/demo");
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    fs::write(app.join("README.md"), "via PR\n").unwrap();
    run(&app, &["commit", "-m", "via PR"]).success();

    let out = run(&app, &["land"]).success();
    insta::assert_snapshot!("land_queued_human", text(&out));
    let value = json_run(&app, &["land"]);
    assert_valid("land", &value);
    insta::assert_json_snapshot!("land_queued_json", value);

    // Still open when the wait runs out: 4, retryable.
    let a = run(&app, &["--json", "land", "--wait", "1"]).code(4);
    assert_eq!(error_kind(&a), json!("queued"));

    let url = "https://github.example/o/demo/pull/1";
    let merged = s.rt.block_on(s.fixture.repo().merge_pr(url));
    let value = json_run(&app, &["land", "--wait", "5"]);
    assert_valid("land", &value);
    assert_eq!(value["landed"], json!(merged.to_string()));
    let status = json_run(&app, &["status"]);
    assert_eq!(status["base"], json!(merged.to_string()));
    assert_eq!(status["clean"], json!(true));

    // A closed PR is a refusal: 5.
    fs::write(app.join("README.md"), "second PR\n").unwrap();
    run(&app, &["commit", "-m", "second"]).success();
    run(&app, &["land"]).success();
    s.fixture
        .repo()
        .close_pr("https://github.example/o/demo/pull/2");
    let a = run(&app, &["--json", "land"]).code(5);
    assert_eq!(error_kind(&a), json!("rejected"));
}
