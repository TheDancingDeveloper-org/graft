//! The `graft` binary against an in-process server: human and JSON output of
//! every read command, each JSON document checked against its schema, the
//! exit codes of design §8.2, and the token rules.

mod support;

use std::fs;
use std::path::Path;

use graft_proto::PROTOCOL_VERSION;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::{Value, json};
use support::*;

/// `graft clone demo app` into `cwd`, with extra arguments. Returns the working copy.
fn clone_app(s: &Served, cwd: &Path, extra: &[&str]) -> std::path::PathBuf {
    graft(cwd)
        .args(["--endpoint", &s.url, "clone", REPO, "app"])
        .args(extra)
        .assert()
        .success();
    cwd.join("app")
}

/// Runs `graft --endpoint … --repo demo <args>` in a fresh directory.
fn run_on(s: &Served, args: &[&str]) -> assert_cmd::assert::Assert {
    run_on_stdin(s, args, "")
}

/// [`run_on`] with `stdin` as the command's standard input.
fn run_on_stdin(
    s: &Served,
    args: &[&str],
    stdin: impl Into<Vec<u8>>,
) -> assert_cmd::assert::Assert {
    let dir = scratch();
    graft(dir.path())
        .args(["--endpoint", &s.url, "--repo", REPO])
        .args(args)
        .write_stdin(stdin)
        .assert()
}

fn text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn version_human_and_json() {
    let dir = scratch();
    let out = graft(dir.path()).arg("version").assert().success();
    let human = text(out.get_output());
    assert!(human.starts_with(concat!("graft ", env!("CARGO_PKG_VERSION"), "+")));
    assert!(human.ends_with(&format!(" (protocol {PROTOCOL_VERSION})\n")));

    let out = graft(dir.path())
        .args(["--json", "version"])
        .assert()
        .success();
    let value = json_out(out.get_output());
    assert_valid("version", &value);
    let version = value["version"].as_str().unwrap();
    assert!(version.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")));
    assert_ne!(version, "0.0.0");
    assert_eq!(
        value["protocol"],
        json!({"major": PROTOCOL_VERSION.major, "minor": PROTOCOL_VERSION.minor})
    );
}

#[test]
fn help_is_public_facing_for_every_command() {
    const COMMANDS: &[&[&str]] = &[
        &[],
        &["clone"],
        &["status"],
        &["diff"],
        &["log"],
        &["show"],
        &["blame"],
        &["cat"],
        &["ls"],
        &["sparse"],
        &["sparse", "list"],
        &["sparse", "add"],
        &["sparse", "remove"],
        &["commit"],
        &["workspace"],
        &["workspace", "show"],
        &["workspace", "list"],
        &["workspace", "open"],
        &["workspace", "renew"],
        &["workspace", "close"],
        &["line"],
        &["line", "create"],
        &["line", "merge"],
        &["line", "merges"],
        &["line", "list"],
        &["tag"],
        &["tag", "create"],
        &["tag", "list"],
        &["stack"],
        &["stack", "list"],
        &["stack", "squash"],
        &["stack", "reorder"],
        &["stack", "split"],
        &["stack", "drop"],
        &["sync"],
        &["land"],
        &["revert"],
        &["pick"],
        &["attest"],
        &["ci"],
        &["ci", "exchange"],
        &["api"],
        &["version"],
        &["login"],
        &["logout"],
        &["config"],
        &["doctor"],
        &["init"],
        &["mcp"],
        &["lsp"],
        &["serve"],
        &["serve", "admin"],
        &["node"],
        &["mirror"],
        &["mirror", "import"],
        &["mirror", "follow"],
        &["mirror", "pr-batch"],
        &["mirror", "export"],
        &["mirror", "authority"],
    ];

    for command in COMMANDS {
        let dir = scratch();
        let output = graft(dir.path())
            .args(*command)
            .arg("--help")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let help = String::from_utf8(output).unwrap();
        assert_no_internal_references(command, &help);
    }
}

#[test]
fn top_level_help_snapshot() {
    let dir = scratch();
    let out = graft(dir.path()).arg("--help").assert().success();
    insta::assert_snapshot!("top_level_help", text(out.get_output()));
}

#[test]
fn completions_help_snapshot() {
    let dir = scratch();
    let out = graft(dir.path())
        .args(["completions", "--help"])
        .assert()
        .success();
    insta::assert_snapshot!("completions_help", text(out.get_output()));
}

#[test]
fn experimental_commands_are_hidden_and_gated() {
    let dir = scratch();
    let help = graft(dir.path()).arg("--help").assert().success();
    let help = text(help.get_output());
    assert!(!help.contains("attest"));
    assert!(!help.contains("ci "));

    graft(dir.path())
        .args(["attest", "main", "--check", "test", "--pass"])
        .assert()
        .code(2)
        .stderr(contains("experimental; enable with GRAFT_EXPERIMENTAL=1"));
    graft(dir.path())
        .args(["ci", "exchange", "--provider", "github", "--out", "token"])
        .assert()
        .code(2)
        .stderr(contains("experimental; enable with GRAFT_EXPERIMENTAL=1"));
}

#[test]
fn clone_human_and_json() {
    let s = serve();
    let dir = scratch();
    let out = graft(dir.path())
        .args(["--endpoint", &s.url, "clone", REPO, "app"])
        .assert()
        .success();
    insta::assert_snapshot!("clone_human", text(out.get_output()));

    let dir = scratch();
    let out = graft(dir.path())
        .args(["--json", "--endpoint", &s.url, "clone", REPO, "app"])
        .assert()
        .success();
    let value = json_out(out.get_output());
    assert_valid("clone", &value);
    assert_eq!(value["endpoint"], json!(s.url));
    let mut stable = value.clone();
    stable["endpoint"] = json!("http://127.0.0.1:9");
    insta::assert_json_snapshot!("clone_json", stable);
    assert_eq!(value["base"], json!(s.commits.third.to_string()));
    assert_eq!(value["files"], json!(4));
}

#[test]
fn status_reports_added_modified_and_deleted_files() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path(), &[]);

    let clean = graft(&app).arg("status").assert().success();
    insta::assert_snapshot!("status_clean", text(clean.get_output()));

    fs::write(app.join("README.md"), "hello\nchanged\n").unwrap();
    fs::remove_file(app.join("assets/logo.bin")).unwrap();
    fs::write(app.join("new.txt"), "fresh\n").unwrap();

    let out = graft(&app).arg("status").assert().success();
    insta::assert_snapshot!("status_changed", text(out.get_output()));

    let out = graft(&app).args(["--json", "status"]).assert().success();
    let value = json_out(out.get_output());
    assert_valid("status", &value);
    insta::assert_json_snapshot!("status_changed_json", value);
    assert_eq!(value["clean"], json!(false));
    assert_eq!(
        value["changes"],
        json!([
            {"path": "README.md", "status": "modified"},
            {"path": "assets/logo.bin", "status": "deleted"},
            {"path": "new.txt", "status": "added"},
        ])
    );
}

#[test]
fn status_outside_a_working_copy_is_a_usage_error() {
    let dir = scratch();
    graft(dir.path())
        .arg("status")
        .assert()
        .code(2)
        .stderr(contains("not inside a working copy"));
}

#[test]
fn log_human_and_json() {
    let s = serve();
    let out = run_on(&s, &["log"]).success();
    insta::assert_snapshot!("log_human", text(out.get_output()));

    let out = run_on(&s, &["--json", "log", "--limit", "2"]).success();
    let value = json_out(out.get_output());
    assert_valid("log", &value);
    insta::assert_json_snapshot!("log_json", value);
    assert_eq!(value["changesets"].as_array().map(Vec::len), Some(2));
    assert_eq!(value["next"], json!("more"));
}

#[test]
fn log_defaults_to_the_working_copys_base() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path(), &["--at", &s.commits.second.to_string()]);
    let out = graft(&app).args(["--json", "log"]).assert().success();
    let value = json_out(out.get_output());
    assert_valid("log", &value);
    insta::assert_json_snapshot!("log_base_json", value);
    assert_eq!(
        value["changesets"][0]["id"],
        json!(s.commits.second.to_string())
    );
}

#[test]
fn show_human_and_json() {
    let s = serve();
    let rev = s.commits.second.to_string();
    let out = run_on(&s, &["show", &rev]).success();
    insta::assert_snapshot!("show_human", text(out.get_output()));

    let out = run_on(&s, &["--json", "show", &rev]).success();
    let value = json_out(out.get_output());
    assert_valid("show", &value);
    insta::assert_json_snapshot!("show_json", value);
    assert_eq!(value["files"][0]["path"], json!("src/lib.rs"));
    assert_eq!(value["files"][0]["op"], json!("put"));
}

#[test]
fn show_a_missing_ref_fails_with_not_found() {
    let s = serve();
    run_on(&s, &["show", "no-such-ref"])
        .code(1)
        .stderr(contains("not found"));
}

#[test]
fn diff_between_two_changesets_human_and_json() {
    let s = serve();
    let from = s.commits.first.to_string();
    let to = s.commits.third.to_string();
    let out = run_on(&s, &["diff", &from, &to]).success();
    insta::assert_snapshot!("diff_human", text(out.get_output()));

    let out = run_on(&s, &["--json", "diff", &from, &to]).success();
    let value = json_out(out.get_output());
    assert_valid("diff", &value);
    insta::assert_json_snapshot!("diff_json", value);
    assert_eq!(value["from"], json!(from));
    assert_eq!(value["to"], json!(to));
}

#[test]
fn diff_of_one_changeset_is_a_usage_error() {
    let s = serve();
    run_on(&s, &["diff", "main"])
        .code(2)
        .stderr(contains("give two changesets"));
}

#[test]
fn diff_stat_counts_added_and_removed_lines() {
    let s = serve();
    let from = s.commits.first.to_string();
    let to = s.commits.third.to_string();
    let out = run_on(&s, &["diff", "--stat", &from, &to]).success();
    let text = text(out.get_output());
    assert!(text.contains("src/lib.rs"), "{text}");
    assert!(text.contains("file"), "{text}");

    let out = run_on(&s, &["--json", "diff", "--stat", &from, &to]).success();
    let value = json_out(out.get_output());
    assert!(value.get("stat").is_some(), "{value}");
}

#[test]
fn diff_stat_of_the_working_copy_counts_an_edit() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path(), &[]);
    fs::write(app.join("README.md"), "edited\n").unwrap();
    let out = graft(&app).args(["diff", "--stat"]).assert().success();
    let text = text(out.get_output());
    assert!(text.contains("README.md |     2 +1     -1"), "{text}");
    assert!(text.contains("1 file, 1 insertion, 1 deletion"), "{text}");
}

#[test]
fn diff_stat_marks_oversized_working_copy_files_truncated() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path(), &[]);
    fs::write(app.join("README.md"), vec![b'x'; 64 * 1024 + 1]).unwrap();
    graft(&app)
        .args(["diff", "--stat"])
        .assert()
        .success()
        .stdout(contains("README.md | ? (truncated)"));
}

#[test]
fn dash_c_runs_in_the_named_directory() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path(), &[]);
    let elsewhere = scratch();
    graft(elsewhere.path())
        .args(["-C", app.to_str().unwrap(), "status"])
        .assert()
        .success()
        .stdout(contains("README.md").not());
    fs::write(app.join("README.md"), "edited\n").unwrap();
    graft(elsewhere.path())
        .args(["-C", app.to_str().unwrap(), "status"])
        .assert()
        .success()
        .stdout(contains("README.md"));
}

#[test]
fn dash_c_of_a_missing_directory_fails() {
    let s = serve();
    run_on(&s, &["-C", "/no/such/graft/dir", "status"])
        .code(1)
        .stderr(contains("cannot enter"));
}

#[test]
fn diff_of_the_working_copy_lists_its_changes() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path(), &[]);
    fs::write(app.join("README.md"), "edited\n").unwrap();
    let out = graft(&app).arg("diff").assert().success();
    insta::assert_snapshot!("diff_working_copy", text(out.get_output()));

    let out = graft(&app).args(["--json", "diff"]).assert().success();
    let value = json_out(out.get_output());
    assert_valid("diff", &value);
    insta::assert_json_snapshot!("diff_working_copy_json", value);
    assert_eq!(value["to"], Value::Null);
}

#[test]
fn blame_human_and_json() {
    let s = serve();
    let out = run_on(&s, &["blame", "src/lib.rs"]).success();
    insta::assert_snapshot!("blame_human", text(out.get_output()));

    let out = run_on(&s, &["--json", "blame", "src/lib.rs", "--lines", "1:2"]).success();
    let value = json_out(out.get_output());
    assert_valid("blame", &value);
    insta::assert_json_snapshot!("blame_json", value);
    assert_eq!(value["ranges"][0]["start"], json!(1));
    assert_eq!(value["ranges"][0]["end"], json!(2));
    assert_eq!(
        value["ranges"][0]["origin"]["id"],
        json!(s.commits.second.to_string())
    );
}

#[test]
fn blame_rejects_an_empty_line_range() {
    let s = serve();
    run_on(&s, &["blame", "src/lib.rs", "--lines", "3:3"])
        .code(2)
        .stderr(contains("START < END"));
}

#[test]
fn cat_text_and_binary_json() {
    let s = serve();
    let out = run_on(&s, &["cat", "README.md"]).success();
    assert_eq!(text(out.get_output()), "hello\n");

    let out = run_on(&s, &["--json", "cat", "README.md"]).success();
    let value = json_out(out.get_output());
    assert_valid("cat", &value);
    insta::assert_json_snapshot!("cat_text_json", value);
    assert_eq!(value["encoding"], json!("utf8"));
    assert_eq!(value["content"], json!("hello\n"));

    let out = run_on(&s, &["--json", "cat", "assets/logo.bin"]).success();
    let value = json_out(out.get_output());
    assert_valid("cat", &value);
    insta::assert_json_snapshot!("cat_binary_json", value);
    assert_eq!(value["encoding"], json!("base64"));
    assert_eq!(value["content"], json!("/wB/"));
    assert_eq!(value["size"], json!(3));
}

#[test]
fn cat_a_missing_file_as_json_reports_the_error() {
    let s = serve();
    let out = run_on(&s, &["--json", "cat", "missing.txt"]).code(1);
    let value = json_out(out.get_output());
    assert_valid("error", &value);
    insta::assert_json_snapshot!("cat_missing_error_json", value);
    assert_eq!(value["error"]["kind"], json!("not_found"));
}

#[test]
fn ls_lists_direct_children_and_descendants() {
    let s = serve();
    let out = run_on(&s, &["ls"]).success();
    insta::assert_snapshot!("ls_human", text(out.get_output()));

    let out = run_on(&s, &["--json", "ls"]).success();
    let value = json_out(out.get_output());
    assert_valid("ls", &value);
    insta::assert_json_snapshot!("ls_json", value);
    assert_eq!(value["entries"].as_array().map(Vec::len), Some(4));

    let out = run_on(&s, &["--json", "ls", "--recursive"]).success();
    let value = json_out(out.get_output());
    assert_valid("ls", &value);
    insta::assert_json_snapshot!("ls_recursive_json", value);
    let paths: Vec<&str> = value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"src/lib.rs"), "{paths:?}");
}

#[test]
fn sparse_lists_adds_and_removes_prefixes() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path(), &["--path", "src"]);
    assert!(!app.join("docs").exists());

    let out = graft(&app).args(["sparse"]).assert().success();
    insta::assert_snapshot!("sparse_list", text(out.get_output()));

    graft(&app)
        .args(["sparse", "add", "docs"])
        .assert()
        .success();
    assert!(app.join("docs/guide.md").exists(), "docs was fetched");

    let out = graft(&app).args(["--json", "sparse"]).assert().success();
    let value = json_out(out.get_output());
    assert_valid("sparse", &value);
    insta::assert_json_snapshot!("sparse_json", value);
    assert_eq!(value["prefixes"], json!(["docs", "src"]));

    graft(&app)
        .args(["sparse", "remove", "src"])
        .assert()
        .success();
    assert!(
        app.join("src/lib.rs").exists(),
        "removed files stay on disk"
    );
    graft(&app)
        .args(["sparse", "remove", "docs"])
        .assert()
        .code(2)
        .stderr(contains("cannot remove every prefix"));
}

#[test]
fn sparse_refuses_to_overwrite_local_changes() {
    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path(), &["--path", "src"]);
    fs::write(app.join("src/lib.rs"), "local edit\n").unwrap();
    graft(&app)
        .args(["sparse", "add", "docs"])
        .assert()
        .code(3)
        .stderr(contains("local changes"));
    assert_eq!(
        fs::read_to_string(app.join("src/lib.rs")).unwrap(),
        "local edit\n"
    );
    assert!(!app.join("docs").exists(), "the slice did not change");
}

#[test]
fn api_lists_every_operation_and_how_it_is_surfaced() {
    let dir = scratch();
    let out = graft(dir.path()).args(["api", "--list"]).assert().success();
    let value = json_out(out.get_output());
    assert_valid("api-list", &value);
    assert_no_internal_references(&["api", "--list"], &value.to_string());
    insta::assert_json_snapshot!("api_list_json", value);
    assert_eq!(value.as_array().map(Vec::len), Some(33));
}

#[test]
fn api_calls_a_read_operation_with_json_in_and_out() {
    let s = serve();
    let request = json!({"name": "main"}).to_string();
    let out = run_on_stdin(&s, &["api", "resolve_ref", "--input", "-"], request).success();
    let value = json_out(out.get_output());
    assert_valid("api", &value);
    insta::assert_json_snapshot!("api_resolve_ref_json", value);
    assert_eq!(value["target"], json!(s.commits.third.to_string()));

    let request = json!({"at": "main", "path": "src"}).to_string();
    let out = run_on_stdin(&s, &["api", "list_tree", "--input", "-"], request).success();
    let value = json_out(out.get_output());
    assert_valid("api", &value);
    insta::assert_json_snapshot!("api_list_tree_json", value);
    assert_eq!(value["entries"][0]["path"], json!("src/lib.rs"));
}

#[test]
fn api_refuses_writes_and_unknown_operations() {
    let s = serve();
    run_on(&s, &["api", "create_changeset"])
        .code(2)
        .stderr(contains("not available"));
    run_on(&s, &["api", "frobnicate"])
        .code(2)
        .stderr(contains("unknown operation"));
}

#[test]
fn api_refuses_a_request_field_it_does_not_know() {
    let s = serve();
    run_on_stdin(
        &s,
        &["api", "resolve_ref", "--input", "-"],
        json!({"nmae": "main"}).to_string(),
    )
    .code(2)
    .stderr(contains("bad request"));
}

#[test]
fn an_unreachable_endpoint_is_unavailable() {
    let dir = scratch();
    graft(dir.path())
        .args(["--endpoint", "http://127.0.0.1:9", "--repo", REPO, "log"])
        .assert()
        .code(7);
}

#[test]
fn a_command_without_a_repository_or_endpoint_is_a_usage_error() {
    let dir = scratch();
    graft(dir.path())
        .arg("log")
        .assert()
        .code(2)
        .stderr(contains("no repository"));
    graft(dir.path())
        .args(["--repo", REPO, "log"])
        .assert()
        .code(2)
        .stderr(contains("no endpoint"));
}

#[test]
fn json_errors_go_to_standard_output_as_an_object() {
    let dir = scratch();
    let out = graft(dir.path()).args(["--json", "log"]).assert().code(2);
    let value = json_out(out.get_output());
    assert_valid("error", &value);
    insta::assert_json_snapshot!("usage_error_json", value);
    assert_eq!(value["error"]["kind"], json!("usage"));
}

#[test]
fn the_token_comes_from_a_file_or_the_environment_and_is_never_printed() {
    let s = serve();
    let dir = scratch();
    let token = dir.path().join("token");
    fs::write(&token, "  s3cret-token\n").unwrap();
    let out = graft(dir.path())
        .args(["--endpoint", &s.url, "--repo", REPO, "--token-file"])
        .arg(&token)
        .arg("log")
        .assert()
        .success();
    assert!(!text(out.get_output()).contains("s3cret"));

    let out = graft(dir.path())
        .env("GRAFT_TOKEN", "s3cret-env")
        .args(["--endpoint", &s.url, "--repo", REPO, "log"])
        .assert()
        .success();
    assert!(!text(out.get_output()).contains("s3cret"));

    let empty = dir.path().join("empty");
    fs::write(&empty, "\n").unwrap();
    graft(dir.path())
        .args(["--endpoint", &s.url, "--repo", REPO, "--token-file"])
        .arg(&empty)
        .arg("log")
        .assert()
        .code(1)
        .stderr(contains("is empty"));
}

#[test]
fn unknown_commands_say_so() {
    let dir = scratch();
    graft(dir.path())
        .arg("frobnicate")
        .assert()
        .code(2)
        .stderr(contains("unknown command"));
}

#[test]
fn every_schema_file_compiles() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("schemas directory") {
        let path = entry.expect("entry").path();
        let schema: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("JSON");
        jsonschema::validator_for(&schema)
            .unwrap_or_else(|e| panic!("{} does not compile: {e}", path.display()));
        count += 1;
    }
    assert_eq!(
        count, 29,
        "one schema per command, plus error, api-list, line-list, line-merge, line-merges, tag-list and workspace-list"
    );
}

#[test]
fn a_reader_that_closes_early_ends_the_command_quietly() {
    use std::process::Stdio;

    let s = serve();
    let dir = scratch();
    let app = clone_app(&s, dir.path(), &[]);
    // Enough new files that the status listing overflows the pipe buffer, so
    // the write fails once the reader has gone.
    for i in 0..3000 {
        fs::write(app.join(format!("new-{i:04}.txt")), "x\n").unwrap();
    }
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(&app)
        .arg("status")
        .env_remove("GRAFT_ENDPOINT")
        .env_remove("GRAFT_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn graft");
    drop(child.stdout.take());
    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_clap_error_under_json_prints_the_error_object() {
    let dir = scratch();
    let out = graft(dir.path())
        .args(["--json", "frobnicate-now"])
        .assert()
        .code(2);
    let value = json_out(out.get_output());
    assert_valid("error", &value);
    assert_eq!(value["error"]["kind"], json!("usage"));
}

#[test]
fn line_create_and_list_json_and_human() {
    let s = serve();
    let made = json_out(
        run_on(&s, &["--json", "line", "create", "side", "--from", "main"])
            .success()
            .get_output(),
    );
    assert_valid("line", &made);
    insta::assert_json_snapshot!("line_create_json", made);
    let list = json_out(
        run_on(&s, &["--json", "line", "list"])
            .success()
            .get_output(),
    );
    assert_valid("line-list", &list);
    insta::assert_json_snapshot!("line_list_json", list);
    let human = run_on(&s, &["line", "list"]).success();
    insta::assert_snapshot!("line_list_human", text(human.get_output()));
    let again = run_on(&s, &["--json", "line", "create", "side", "--from", "main"]).code(3);
    let err = json_out(again.get_output());
    assert_valid("error", &err);
    assert_eq!(err["error"]["kind"], json!("ref_conflict"));
}

#[test]
fn login_persists_named_server_and_clone_uses_it_without_overrides() {
    use std::os::unix::fs::PermissionsExt;

    let served = serve();
    let home = scratch();
    let config_path = home.path().join("config.toml");
    let token = "login-test-secret";

    let login = graft(home.path())
        .env("GRAFT_CONFIG", &config_path)
        .args(["login", "dev", "--endpoint", &served.url])
        .write_stdin(format!("{token}\n"))
        .assert()
        .success();
    assert!(!text(login.get_output()).contains(token));

    let config_text = fs::read_to_string(&config_path).unwrap();
    assert!(config_text.contains("default_server = \"dev\""));
    assert!(config_text.contains(&served.url));
    assert!(!config_text.contains(token));
    assert_eq!(
        fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let parsed: toml::Value = toml::from_str(&config_text).unwrap();
    let token_path = Path::new(parsed["servers"]["dev"]["token_file"].as_str().unwrap());
    assert_eq!(fs::read_to_string(token_path).unwrap().trim(), token);
    assert_eq!(
        fs::metadata(token_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let clone_root = home.path().join("fresh");
    fs::create_dir(&clone_root).unwrap();
    graft(&clone_root)
        .env("GRAFT_CONFIG", &config_path)
        .args(["clone", REPO, "app"])
        .assert()
        .success();
    let working_config = fs::read_to_string(clone_root.join("app/.graft/config")).unwrap();
    assert!(working_config.contains("server = dev"));
    assert!(!working_config.contains("endpoint ="));

    let show = graft(&clone_root.join("app"))
        .env("GRAFT_CONFIG", &config_path)
        .args(["--json", "config", "show"])
        .assert()
        .success();
    let output = text(show.get_output());
    assert!(output.contains("<redacted>"));
    assert!(!output.contains(token));

    graft(home.path())
        .env("GRAFT_CONFIG", &config_path)
        .args(["logout", "dev"])
        .assert()
        .success();
    assert!(!token_path.exists());
}

#[test]
fn clone_by_url_uses_the_matching_named_server() {
    let served = serve();
    let home = scratch();
    let config_path = home.path().join("config.toml");
    graft(home.path())
        .env("GRAFT_CONFIG", &config_path)
        .args(["login", "dev", "--endpoint", &served.url])
        .write_stdin("url-clone-secret\n")
        .assert()
        .success();
    let host = served
        .url
        .strip_prefix("http://")
        .expect("testkit serves http");
    let clone_root = home.path().join("fresh");
    fs::create_dir(&clone_root).unwrap();
    let url = format!("graft://{host}/{REPO}?line=main");
    let out = graft(&clone_root)
        .env("GRAFT_CONFIG", &config_path)
        .args(["--json", "clone", &url, "app"])
        .assert()
        .success();
    let value = json_out(out.get_output());
    assert_eq!(value["repo"], json!(REPO));
    assert_eq!(value["endpoint"], json!(served.url));
    assert_eq!(value["line"], json!("main"));
    let working_config = fs::read_to_string(clone_root.join("app/.graft/config")).unwrap();
    assert!(working_config.contains("server = dev"));
    assert!(!working_config.contains("url-clone-secret"));

    let bad = graft(&clone_root)
        .args(["clone", "graft://token@host/repo"])
        .assert()
        .code(2);
    let err = String::from_utf8_lossy(&bad.get_output().stderr);
    assert!(err.contains("userinfo"), "{err}");
}

#[test]
fn endpoint_override_clone_records_the_override_not_the_default_server() {
    let default_server = serve();
    let override_server = serve();
    let home = scratch();
    let config_path = home.path().join("config.toml");
    let token_path = home.path().join("override-token");
    fs::write(&token_path, "override-secret\n").unwrap();

    graft(home.path())
        .env("GRAFT_CONFIG", &config_path)
        .args(["login", "dev", "--endpoint", &default_server.url])
        .write_stdin("default-secret")
        .assert()
        .success();
    graft(home.path())
        .env("GRAFT_CONFIG", &config_path)
        .arg("--endpoint")
        .arg(&override_server.url)
        .arg("--token-file")
        .arg(&token_path)
        .args(["clone", REPO, "app"])
        .assert()
        .success();

    let working_config = fs::read_to_string(home.path().join("app/.graft/config")).unwrap();
    assert!(working_config.contains(&format!("endpoint = {}", override_server.url)));
    assert!(!working_config.contains("server = dev"));
}

#[test]
fn explicit_connection_overrides_a_missing_working_copy_server() {
    let served = serve();
    let dir = scratch();
    let app = clone_app(&served, dir.path(), &[]);
    let token_path = dir.path().join("override-token");
    fs::write(&token_path, "override-token\n").unwrap();
    let path = app.join(".graft/config");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{text}server = removed\n")).unwrap();

    graft(&app)
        .arg("--endpoint")
        .arg(&served.url)
        .arg("--token-file")
        .arg(&token_path)
        .args(["status", "--upstream"])
        .assert()
        .success();
}

#[test]
fn login_json_never_prints_the_token() {
    let served = serve();
    let dir = scratch();
    let token = "json-login-secret";
    let output = graft(dir.path())
        .args(["--json", "login", "dev", "--endpoint", &served.url])
        .write_stdin(token)
        .assert()
        .success()
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains(token));
    assert!(combined.contains("<redacted>"));
}

#[test]
fn serve_dev_then_clone_by_the_printed_url() {
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Hang bound only. Readiness is the printed clone line, or process exit.
    const HANG: Duration = Duration::from_secs(120);

    let home = scratch();
    let config_path = home.path().join("config.toml");
    let data = home.path().join("data");
    let state = home.path().join("state");
    let mut child = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(home.path())
        .env("GRAFT_CONFIG", &config_path)
        .env_remove("GRAFT_ENDPOINT")
        .env_remove("GRAFT_TOKEN")
        .args([
            "serve",
            "--dev",
            "--data",
            data.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "--repo",
            "demo",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve --dev");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let (url_tx, url_rx) = std::sync::mpsc::channel();
    let captured_out = Arc::new(Mutex::new(String::new()));
    let captured_err = Arc::new(Mutex::new(String::new()));
    let out_buf = Arc::clone(&captured_out);
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            out_buf.lock().unwrap().push_str(&line);
            out_buf.lock().unwrap().push('\n');
            if let Some(url) = line.trim().strip_prefix("graft clone ") {
                let _ = url_tx.send(url.to_owned());
            }
        }
    });
    let err_buf = Arc::clone(&captured_err);
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            err_buf.lock().unwrap().push_str(&line);
            err_buf.lock().unwrap().push('\n');
        }
    });
    let started = Instant::now();
    let url = loop {
        match url_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(url) => break url,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.wait();
                panic!(
                    "serve --dev exited before printing the clone command\nstderr:\n{}",
                    captured_err.lock().unwrap()
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().expect("poll serve --dev") {
                    panic!(
                        "serve --dev exited {status} before printing the clone command\nstderr:\n{}",
                        captured_err.lock().unwrap()
                    );
                }
                if started.elapsed() >= HANG {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "serve --dev hung past {HANG:?} without a clone command\nstderr:\n{}",
                        captured_err.lock().unwrap()
                    );
                }
            }
        }
    };
    let config_wait = Instant::now();
    while !config_path.exists() {
        if config_wait.elapsed() >= HANG {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "serve --dev printed a clone command but never wrote the user config\nstderr:\n{}",
                captured_err.lock().unwrap()
            );
        }
        if let Some(status) = child.try_wait().expect("poll serve --dev") {
            if config_path.exists() {
                break;
            }
            panic!(
                "serve --dev exited {status} before writing the user config\nstderr:\n{}",
                captured_err.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let config_text = fs::read_to_string(&config_path).expect("user config");
    let parsed: toml::Value = toml::from_str(&config_text).unwrap();
    let token_path = Path::new(parsed["servers"]["demo"]["token_file"].as_str().unwrap());
    let token = fs::read_to_string(token_path).unwrap();
    let token = token.trim();
    assert!(!token.is_empty());
    assert!(!captured_out.lock().unwrap().contains(token));
    assert!(!captured_err.lock().unwrap().contains(token));
    assert!(!config_text.contains(token));
    assert_eq!(
        fs::metadata(token_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let key = state.join("signing.key");
    assert_eq!(
        fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let clone_root = home.path().join("fresh");
    fs::create_dir(&clone_root).unwrap();
    let mut clone = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(&clone_root)
        .env("GRAFT_CONFIG", &config_path)
        .args(["clone", &url, "app"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clone");
    let mut clone_out = clone.stdout.take().expect("clone stdout");
    let mut clone_err = clone.stderr.take().expect("clone stderr");
    let clone_stdout = Arc::new(Mutex::new(Vec::new()));
    let clone_stderr = Arc::new(Mutex::new(Vec::new()));
    let out_sink = Arc::clone(&clone_stdout);
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = clone_out.read_to_end(&mut buf);
        *out_sink.lock().unwrap() = buf;
    });
    let err_sink = Arc::clone(&clone_stderr);
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = clone_err.read_to_end(&mut buf);
        *err_sink.lock().unwrap() = buf;
    });
    let clone_started = Instant::now();
    let clone_status = loop {
        if let Some(status) = clone.try_wait().expect("poll clone") {
            break status;
        }
        if clone_started.elapsed() >= HANG {
            let _ = clone.kill();
            let _ = clone.wait();
            panic!(
                "clone hung past {HANG:?}\nclone stderr:\n{}\nserve stderr:\n{}",
                String::from_utf8_lossy(&clone_stderr.lock().unwrap()),
                captured_err.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let clone_text = format!(
        "{}{}",
        String::from_utf8_lossy(&clone_stdout.lock().unwrap()),
        String::from_utf8_lossy(&clone_stderr.lock().unwrap())
    );
    assert!(
        clone_status.success(),
        "clone failed {clone_status}\n{clone_text}\nserve stderr:\n{}",
        captured_err.lock().unwrap()
    );
    assert!(!clone_text.contains(token));
    assert!(clone_root.join("app/.graft/config").is_file());

    let _ = child.kill();
    let _ = child.wait();
}
