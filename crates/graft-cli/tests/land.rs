//! Native land, conflicts, revert, stack operations and attest (G3.5),
//! driving the `graft` binary against the testkit server in front of the
//! real engine (G3.1), with races injected: another writer lands on the
//! line just before our `Land` runs, or a call fails or loses its reply.
//!
//! `land_results_map_to_exit_codes_and_output` is the table of design §8.2:
//! every result, its exit code, and what the command prints, by snapshot.

mod support;

#[path = "../../graft-client/tests/support/engine.rs"]
mod engine;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use assert_cmd::assert::Assert;
use engine::{EngineRepo, Fault, Race};
use graft_model::ChangesetId;
use graft_proto::ApiError;
use graft_testkit::server::TestServer;
use serde_json::{Value, json};
use support::{assert_valid, graft, json_out, scratch};
use tokio::runtime::Runtime;

const LIB: &str = "fn a() {}\nfn b() {}\nfn c() {}\n";

/// The engine behind a loopback server, and the runtime that serves it.
struct Served {
    rt: Runtime,
    repo: Arc<EngineRepo>,
    _server: TestServer,
    url: String,
}

#[derive(Clone, Copy)]
enum Policy {
    /// `main` attested on `ci/build` and `ci/test` (decision D6).
    D6,
    Direct,
}

fn serve(policy: Policy) -> Served {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let files: Vec<(&str, &[u8])> = vec![
        ("README.md", b"hello\n"),
        ("src/lib.rs", LIB.as_bytes()),
        ("assets/logo.bin", &[0xff, 0x00, 0x7f]),
        ("docs/guide.md", b"guide\n"),
    ];
    let repo = rt.block_on(async {
        match policy {
            Policy::D6 => EngineRepo::d6("demo", &files).await,
            Policy::Direct => EngineRepo::direct("demo", &files).await,
        }
    });
    let server = rt.block_on(TestServer::tcp_workspaces(repo.clone()));
    let url = server.url().to_owned();
    Served {
        rt,
        repo,
        _server: server,
        url,
    }
}

fn put(path: &str, data: &str) -> Race {
    vec![(path.to_owned(), Some(data.as_bytes().to_vec()))]
}

/// `graft clone demo app` in `dir`.
fn clone_app(s: &Served, dir: &Path) -> PathBuf {
    graft(dir)
        .args(["--endpoint", &s.url, "clone", "demo", "app"])
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

fn error_of(a: &Assert) -> Value {
    let value = json_out(a.get_output());
    assert_valid("error", &value);
    value["error"].clone()
}

fn id(value: &Value) -> ChangesetId {
    ChangesetId::parse(value.as_str().expect("an id")).expect("a changeset id")
}

fn stdout(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn stderr(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

/// A working copy with one draft on `main`: `README.md` changed, and
/// `src/lib.rs` too when `lib` is given.
fn with_draft(s: &Served, dir: &Path, lib: Option<&str>) -> (PathBuf, ChangesetId) {
    let app = clone_app(s, dir);
    fs::write(app.join("README.md"), "mine\n").unwrap();
    if let Some(lib) = lib {
        fs::write(app.join("src/lib.rs"), lib).unwrap();
    }
    let draft = json_run(&app, &["commit", "-m", "mine"]);
    (app, id(&draft["id"]))
}

/// Attests every D6 check on `rev` through `graft attest`.
fn attest_all(app: &Path, rev: &str) {
    for check in ["ci/build", "ci/test"] {
        run(app, &["attest", rev, "--check", check, "--pass"]).success();
    }
}

/// One land result: how to get there.
struct Case {
    name: &'static str,
    policy: Policy,
    arrange: fn(&Served, &Path, ChangesetId),
    args: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        name: "landed",
        policy: Policy::Direct,
        arrange: |_, _, _| {},
        args: &[],
    },
    Case {
        name: "landed, rebased over a racing land",
        policy: Policy::Direct,
        arrange: |s, _, _| s.repo.race(put("docs/guide.md", "theirs\n")),
        args: &[],
    },
    Case {
        name: "conflict with a racing land",
        policy: Policy::Direct,
        arrange: |s, _, _| s.repo.race(put("README.md", "theirs\n")),
        args: &[],
    },
    Case {
        name: "conflict with --sync: the conflicts are written",
        policy: Policy::Direct,
        arrange: |s, _, _| s.repo.race(put("README.md", "theirs\n")),
        args: &["--sync"],
    },
    Case {
        name: "contended past the wait",
        policy: Policy::Direct,
        arrange: |s, _, head| {
            s.repo.fail(
                "Land",
                Fault::Before(ApiError::Contended {
                    line: "main".into(),
                    tip: head,
                }),
            )
        },
        args: &["--wait", "0"],
    },
    Case {
        name: "rejected: no attestation (D6)",
        policy: Policy::D6,
        arrange: |_, _, _| {},
        args: &[],
    },
    Case {
        name: "rejected: a failing attestation (D6)",
        policy: Policy::D6,
        arrange: |_, app, head| {
            let rev = head.to_string();
            run(app, &["attest", &rev, "--check", "ci/build", "--pass"]).success();
            run(
                app,
                &[
                    "attest",
                    &rev,
                    "--check",
                    "ci/test",
                    "--fail",
                    "--url",
                    "https://ci.example/run/7",
                ],
            )
            .success();
        },
        args: &[],
    },
    Case {
        name: "permission denied: no land scope",
        policy: Policy::Direct,
        arrange: |s, _, _| s.repo.set_scopes(&["repo:demo:write"]),
        args: &[],
    },
    Case {
        name: "unauthenticated",
        policy: Policy::Direct,
        arrange: |s, _, _| {
            s.repo
                .fail("Land", Fault::Before(ApiError::Unauthenticated))
        },
        args: &[],
    },
    Case {
        name: "unavailable: the reply was lost after the line moved",
        policy: Policy::Direct,
        arrange: |s, _, _| {
            s.repo.fail(
                "Land",
                Fault::After(ApiError::Unavailable { retry_after_ms: 0 }),
            )
        },
        args: &[],
    },
];

/// Runs `case` in a fresh server, as people see it (`json` off) or as
/// JSON, and returns the result.
fn land_case(case: &Case, json: bool) -> Assert {
    let s = serve(case.policy);
    let dir = scratch();
    let (app, head) = with_draft(&s, dir.path(), None);
    (case.arrange)(&s, &app, head);
    let mut cmd = wc_cmd(&app);
    if json {
        cmd.arg("--json");
    }
    cmd.arg("land").args(case.args).assert()
}

#[test]
fn land_results_map_to_exit_codes_and_output() {
    let mut table = String::new();
    for case in CASES {
        let human = land_case(case, false);
        let machine = land_case(case, true);
        let code = human.get_output().status.code().expect("exit code");
        assert_eq!(
            machine.get_output().status.code(),
            Some(code),
            "{}",
            case.name
        );
        let value = json_out(machine.get_output());
        if code == 0 {
            assert_valid("land", &value);
        } else {
            assert_valid("error", &value);
        }
        writeln!(table, "== {} (land {})", case.name, case.args.join(" ")).unwrap();
        writeln!(table, "exit {code}").unwrap();
        writeln!(table, "stdout:\n{}", stdout(&human)).unwrap();
        writeln!(table, "stderr:\n{}", stderr(&human)).unwrap();
        writeln!(
            table,
            "json:\n{}\n",
            serde_json::to_string_pretty(&value).unwrap()
        )
        .unwrap();
    }
    insta::assert_snapshot!("land_results", table);
}

#[test]
fn a_lost_land_reply_is_retried_by_running_land_again() {
    let s = serve(Policy::Direct);
    let dir = scratch();
    let (app, head) = with_draft(&s, dir.path(), None);
    s.repo.fail(
        "Land",
        Fault::After(ApiError::Unavailable { retry_after_ms: 0 }),
    );
    let a = wc_cmd(&app).args(["--json", "land"]).assert().code(7);
    assert_eq!(error_of(&a)["kind"], json!("unavailable"));
    let value = json_run(&app, &["land"]);
    assert_eq!(value["landed"], json!(head.to_string()));
    let status = json_run(&app, &["status"]);
    assert_valid("status", &status);
    assert_eq!(status["base"], json!(head.to_string()));
    assert_eq!(status["clean"], json!(true));
}

#[test]
fn a_half_done_land_is_finished_by_the_next_command() {
    let s = serve(Policy::Direct);
    let dir = scratch();
    let (app, _) = with_draft(&s, dir.path(), None);
    s.repo.race(put("docs/guide.md", "theirs\n"));
    s.repo.fail(
        "GetObjects",
        Fault::Before(ApiError::Unavailable { retry_after_ms: 0 }),
    );
    run(&app, &["land"]).code(7);
    let status = json_run(&app, &["status"]);
    assert_valid("status", &status);
    assert_eq!(status["unfinished"], json!(true));
    let out = run(&app, &["status"]).success();
    assert!(stdout(&out).contains("an update is unfinished"));

    let value = json_run(&app, &["land"]);
    assert_valid("land", &value);
    assert_eq!(value["outcome"], json!("landed"));
    assert_eq!(value["rebased"], json!(true));
    let tip = s.rt.block_on(s.repo.tip());
    assert_eq!(value["landed"], json!(tip.to_string()));
    assert_eq!(
        fs::read_to_string(app.join("docs/guide.md")).unwrap(),
        "theirs\n"
    );
    let status = json_run(&app, &["status"]);
    assert_eq!(status["unfinished"], json!(false));
    assert_eq!(status["clean"], json!(true));
}

#[test]
fn after_a_conflict_sync_writes_markers_and_edit_commit_land_succeeds() {
    let s = serve(Policy::D6);
    let dir = scratch();
    let (app, head) = with_draft(&s, dir.path(), Some("fn a() { 1 }\nfn b() {}\nfn c() {}\n"));
    attest_all(&app, &head.to_string());
    s.repo
        .race(put("src/lib.rs", "fn a() { 2 }\nfn b() {}\nfn c() {}\n"));

    // Land: 3, and nothing changes.
    let a = wc_cmd(&app).args(["--json", "land"]).assert().code(3);
    let error = error_of(&a);
    assert_eq!(error["kind"], json!("conflict"));
    assert_eq!(
        error["conflicts"],
        json!([{"path": "src/lib.rs", "mode": null}])
    );
    let tip = s.rt.block_on(s.repo.tip());
    assert_eq!(error["onto"], json!(tip.to_string()));
    let status = json_run(&app, &["status"]);
    assert_eq!(status["head"], json!(head.to_string()));
    assert_eq!(status["clean"], json!(true));

    // Sync: 3, with the conflicts written into the files.
    let a = run(&app, &["sync"]).code(3);
    insta::assert_snapshot!("sync_conflicted_stderr", stderr(&a));
    let lib = fs::read_to_string(app.join("src/lib.rs")).unwrap();
    assert!(lib.starts_with("<<<<<<< ours\nfn a() { 1 }\n"), "{lib}");
    assert_eq!(fs::read_to_string(app.join("README.md")).unwrap(), "mine\n");
    let status = json_run(&app, &["status"]);
    assert_valid("status", &status);
    assert_eq!(status["base"], json!(tip.to_string()));
    assert_eq!(status["head"], Value::Null);
    assert_eq!(
        status["conflicts"],
        json!([{"path": "src/lib.rs", "mode": "markers"}])
    );
    // Sync again: the same conflicts, still 3.
    let a = wc_cmd(&app).args(["--json", "sync"]).assert().code(3);
    let error = error_of(&a);
    assert_eq!(error["kind"], json!("unresolved"));

    // Commit with the markers in: refused.
    let a = wc_cmd(&app).args(["--json", "commit"]).assert().code(3);
    assert_eq!(error_of(&a)["kind"], json!("unresolved"));

    // Edit, commit (the stack's message), attest, land.
    fs::write(
        app.join("src/lib.rs"),
        "fn a() { 3 }\nfn b() {}\nfn c() {}\n",
    )
    .unwrap();
    let draft = json_run(&app, &["commit"]);
    assert_valid("commit", &draft);
    let stack = json_run(&app, &["stack"]);
    assert_eq!(stack["drafts"][0]["subject"], json!("mine"));
    attest_all(&app, draft["id"].as_str().unwrap());
    let value = json_run(&app, &["land"]);
    assert_valid("land", &value);
    let landed = id(&value["landed"]);
    let files = s.rt.block_on(s.repo.files(landed));
    assert_eq!(files["src/lib.rs"], b"fn a() { 3 }\nfn b() {}\nfn c() {}\n");
    assert_eq!(files["README.md"], b"mine\n");
    let status = json_run(&app, &["status"]);
    assert_eq!(status["conflicts"], json!([]));
    assert_eq!(status["clean"], json!(true));
}

#[test]
fn a_binary_conflict_gets_a_sidecar() {
    let s = serve(Policy::Direct);
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    fs::write(app.join("assets/logo.bin"), [0u8, 1, 2]).unwrap();
    run(&app, &["commit", "-m", "new logo"]).success();
    s.repo
        .race(vec![("assets/logo.bin".into(), Some(vec![0, 9, 9]))]);
    let a = wc_cmd(&app)
        .args(["--json", "land", "--sync"])
        .assert()
        .code(3);
    let error = error_of(&a);
    assert_eq!(error["kind"], json!("conflicted"));
    assert_eq!(
        error["conflicts"],
        json!([{"path": "assets/logo.bin", "mode": "sidecar"}])
    );
    let sidecar = app.join("assets/logo.bin.graft-conflict");
    let note = fs::read_to_string(&sidecar).unwrap();
    assert!(note.contains("graft cat assets/logo.bin --at"), "{note}");
    assert_eq!(fs::read(app.join("assets/logo.bin")).unwrap(), [0, 1, 2]);

    run(&app, &["commit"]).code(3);
    fs::remove_file(&sidecar).unwrap();
    run(&app, &["commit"]).success();
    json_run(&app, &["land"]);
    let tip = s.rt.block_on(s.repo.tip());
    assert_eq!(
        s.rt.block_on(s.repo.files(tip))["assets/logo.bin"],
        [0, 1, 2]
    );
}

#[test]
fn revert_then_land_restores_the_file() {
    let s = serve(Policy::Direct);
    let dir = scratch();
    let (app, head) = with_draft(&s, dir.path(), None);
    json_run(&app, &["land"]);

    let value = json_run(&app, &["revert", "main"]);
    assert_valid("revert", &value);
    assert_eq!(value["reverted"], json!(head.to_string()));
    assert_eq!(
        fs::read_to_string(app.join("README.md")).unwrap(),
        "hello\n"
    );
    let out = run(&app, &["stack"]).success();
    assert!(stdout(&out).contains("Revert \"mine\""), "{}", stdout(&out));
    json_run(&app, &["land"]);
    let tip = s.rt.block_on(s.repo.tip());
    assert_eq!(s.rt.block_on(s.repo.files(tip))["README.md"], b"hello\n");

    // Reverting it again, onto a line that changed the file since: 3.
    s.rt.block_on(s.repo.advance(put("README.md", "later\n")));
    run(&app, &["sync"]).success();
    let a = wc_cmd(&app)
        .args(["--json", "revert", &head.to_string()])
        .assert()
        .code(3);
    assert_eq!(error_of(&a)["kind"], json!("conflict"));
}

#[test]
fn stack_operations_rewrite_the_drafts() {
    let s = serve(Policy::Direct);
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    let mut ids = Vec::new();
    for (path, msg) in [
        ("README.md", "one"),
        ("docs/guide.md", "two"),
        ("src/lib.rs", "three"),
    ] {
        fs::write(app.join(path), format!("{msg}\n")).unwrap();
        let draft = json_run(&app, &["commit", "-m", msg]);
        ids.push(draft["id"].as_str().unwrap().to_owned());
    }
    let subjects = |v: &Value| -> Vec<String> {
        v["drafts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["subject"].as_str().unwrap().to_owned())
            .collect()
    };
    let drafts = |v: &Value| -> Vec<String> {
        v["drafts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap().to_owned())
            .collect()
    };

    // A dirty working copy is refused.
    fs::write(app.join("README.md"), "dirty\n").unwrap();
    let a = wc_cmd(&app)
        .args(["--json", "stack", "drop", &ids[1]])
        .assert()
        .code(3);
    assert_eq!(error_of(&a)["kind"], json!("dirty"));
    fs::write(app.join("README.md"), "one\n").unwrap();

    // Drop by a short prefix, as `stack list` prints it.
    let out = run(&app, &["stack", "drop", &ids[1][..13]]).success();
    assert!(
        stdout(&out).starts_with("dropped: head "),
        "{}",
        stdout(&out)
    );
    let v = json_run(&app, &["stack"]);
    assert_valid("stack", &v);
    assert_eq!(subjects(&v), ["one", "three"]);
    assert_eq!(
        fs::read_to_string(app.join("docs/guide.md")).unwrap(),
        "guide\n"
    );

    let now = drafts(&v);
    let v = json_run(&app, &["stack", "reorder", &now[1], &now[0]]);
    assert_valid("stack", &v);
    assert_eq!(subjects(&v), ["three", "one"]);

    let v = json_run(&app, &["stack", "squash"]);
    assert_eq!(subjects(&v), ["three"]);
    assert_eq!(v["drafts"][0]["changes"], json!(2));

    let now = drafts(&v);
    let v = json_run(&app, &["stack", "split", &now[0], "--part", "README.md"]);
    assert_eq!(v["drafts"].as_array().unwrap().len(), 2);
    assert_eq!(fs::read_to_string(app.join("README.md")).unwrap(), "one\n");
    assert_eq!(
        fs::read_to_string(app.join("src/lib.rs")).unwrap(),
        "three\n"
    );

    // Unknown and ambiguous names are usage errors.
    let a = wc_cmd(&app)
        .args(["--json", "stack", "drop", "c000000000"])
        .assert()
        .code(2);
    assert_eq!(error_of(&a)["kind"], json!("usage"));

    let landed = json_run(&app, &["land"]);
    assert_eq!(landed["outcome"], json!("landed"));
    let status = json_run(&app, &["status"]);
    assert_eq!(status["clean"], json!(true));
}

#[test]
fn attest_records_a_check_for_ci() {
    let s = serve(Policy::D6);
    let dir = scratch();
    let (app, head) = with_draft(&s, dir.path(), None);
    let rev = head.to_string();
    let value = json_run(
        &app,
        &[
            "attest",
            &rev,
            "--check",
            "ci/build",
            "--pass",
            "--url",
            "https://ci.example/1",
        ],
    );
    assert_valid("attest", &value);
    assert_eq!(
        value,
        json!({"changeset": rev, "check": "ci/build", "passed": true, "url": "https://ci.example/1"})
    );
    let out = run(&app, &["attest", &rev, "--check", "ci/test", "--pass"]).success();
    assert_eq!(
        stdout(&out),
        format!("attested ci/test passed on {}\n", &rev[..13])
    );
    // Outside a working copy, with --repo.
    let elsewhere = scratch();
    graft(elsewhere.path())
        .args([
            "--endpoint",
            &s.url,
            "--repo",
            "demo",
            "attest",
            "main",
            "--check",
            "ci/x",
            "--fail",
        ])
        .assert()
        .success();
    // Neither --pass nor --fail: usage.
    run(&app, &["attest", &rev, "--check", "ci/test"]).code(2);
    // Both checks attested: the land goes through.
    assert_eq!(json_run(&app, &["land"])["landed"], json!(rev));
}

/// Two working copies in their own directories edit `src/lib.rs` and
/// commit; the first, which changes the last function, lands. Returns the
/// second working copy, its draft (`second`) not yet landed.
fn two_writers_on_one_file(s: &Served, one: &Path, two: &Path, second: &str) -> PathBuf {
    let first = clone_app(s, one);
    let other = clone_app(s, two);
    fs::write(
        first.join("src/lib.rs"),
        "fn a() {}\nfn b() {}\nfn c() { 1 }\n",
    )
    .unwrap();
    json_run(&first, &["commit", "-m", "one"]);
    fs::write(other.join("src/lib.rs"), second).unwrap();
    json_run(&other, &["commit", "-m", "two"]);
    assert_eq!(json_run(&first, &["land"])["rebased"], json!(false));
    other
}

#[test]
fn land_line_merges_edits_to_separate_regions_of_one_file() {
    let s = serve(Policy::Direct);
    let (one, two) = (scratch(), scratch());
    let app = two_writers_on_one_file(
        &s,
        one.path(),
        two.path(),
        "fn a() { 2 }\nfn b() {}\nfn c() {}\n",
    );

    // `--merge file`: the stricter rebase refuses the file; nothing moves.
    let a = wc_cmd(&app)
        .args(["--json", "land", "--merge", "file"])
        .assert()
        .code(3);
    assert_eq!(error_of(&a)["kind"], json!("conflict"));

    // A plain land (no --sync) merges the file line by line (D16).
    let value = json_run(&app, &["land"]);
    assert_valid("land", &value);
    assert_eq!(value["outcome"], json!("landed"));
    assert_eq!(value["rebased"], json!(true));
    let tip = s.rt.block_on(s.repo.tip());
    assert_eq!(value["landed"], json!(tip.to_string()));
    let merged = "fn a() { 2 }\nfn b() {}\nfn c() { 1 }\n";
    let files = s.rt.block_on(s.repo.files(tip));
    assert_eq!(files["src/lib.rs"], merged.as_bytes());
    assert_eq!(fs::read_to_string(app.join("src/lib.rs")).unwrap(), merged);
    assert_eq!(json_run(&app, &["status"])["clean"], json!(true));
}

#[test]
fn land_line_merge_still_refuses_edits_to_the_same_line() {
    let s = serve(Policy::Direct);
    let (one, two) = (scratch(), scratch());
    let app = two_writers_on_one_file(
        &s,
        one.path(),
        two.path(),
        "fn a() {}\nfn b() {}\nfn c() { 2 }\n",
    );
    let tip = s.rt.block_on(s.repo.tip());
    let a = wc_cmd(&app).args(["--json", "land"]).assert().code(3);
    let error = error_of(&a);
    assert_eq!(error["kind"], json!("conflict"));
    assert_eq!(error["onto"], json!(tip.to_string()));
    assert_eq!(s.rt.block_on(s.repo.tip()), tip);
    assert_eq!(
        fs::read_to_string(app.join("src/lib.rs")).unwrap(),
        "fn a() {}\nfn b() {}\nfn c() { 2 }\n"
    );
}

/// WI-2309: `graft status --upstream` predicts the land without moving
/// anything: an uncommitted edit that merges, then one that conflicts on
/// the same line, each checked against an immediate land.
#[test]
fn status_upstream_predicts_the_land_and_moves_nothing() {
    let s = serve(Policy::Direct);
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    let state = || fs::read_to_string(app.join(".graft/state")).unwrap();

    let fresh = json_run(&app, &["status", "--upstream"]);
    assert_valid("status", &fresh);
    assert_eq!(fresh["upstream"]["up_to_date"], json!(true));

    fs::write(app.join("src/lib.rs"), "fn A() {}\nfn b() {}\nfn c() {}\n").unwrap();
    s.rt.block_on(
        s.repo
            .advance(put("src/lib.rs", "fn a() {}\nfn b() {}\nfn C() {}\n")),
    );
    let before = (state(), fs::read(app.join("src/lib.rs")).unwrap());
    let tip = s.rt.block_on(s.repo.tip());
    let clean = json_run(&app, &["status", "--upstream"]);
    assert_valid("status", &clean);
    assert_eq!(before, (state(), fs::read(app.join("src/lib.rs")).unwrap()));
    assert_eq!(s.rt.block_on(s.repo.tip()), tip);
    let up = &clean["upstream"];
    assert_eq!(up["behind"], json!(1));
    assert_eq!(
        up["changed"],
        json!([{"path": "src/lib.rs", "status": "changed"}])
    );
    assert_eq!(
        up["overlaps"],
        json!([{"path": "src/lib.rs", "uncommitted": true, "outcome": "clean", "hunks": [], "reason": null}])
    );
    let text = stdout(&run(&app, &["status", "--upstream"]).success());
    assert!(text.contains("1 changeset(s) ahead"), "{text}");
    assert!(text.contains("merges   src/lib.rs (uncommitted)"), "{text}");
    json_run(&app, &["commit", "-m", "mine"]);
    let landed = json_run(&app, &["land"]);
    assert_eq!(landed["outcome"], json!("landed"), "{landed}");

    fs::write(app.join("README.md"), "mine\n").unwrap();
    s.rt.block_on(s.repo.advance(put("README.md", "theirs\n")));
    let conflict = json_run(&app, &["status", "--upstream"]);
    assert_valid("status", &conflict);
    let o = &conflict["upstream"]["overlaps"][0];
    assert_eq!(o["outcome"], json!("conflict"), "{conflict}");
    assert_eq!(o["hunks"], json!([[1, 1]]));
    assert_eq!(conflict["upstream"]["clean"], json!(false));
    let text = stdout(&run(&app, &["status", "--upstream"]).success());
    assert!(
        text.contains("conflict README.md (uncommitted) lines 1"),
        "{text}"
    );
    json_run(&app, &["commit", "-m", "mine"]);
    run(&app, &["land"]).code(3);
}

/// WI-2310: `graft status --drafts` asks about the changed files, or the
/// PATHS named; without the drafts scope it fails alone, but with
/// `--upstream` the upstream answer still prints and the refusal is noted.
#[test]
fn status_drafts_asks_and_a_missing_scope_keeps_upstream() {
    let s = serve(Policy::Direct);
    let dir = scratch();
    let app = clone_app(&s, dir.path());
    fs::write(app.join("README.md"), "mine\n").unwrap();

    let found = json_run(&app, &["status", "--drafts"]);
    let d = &found["drafts"];
    assert_eq!(d["asked"], json!(1), "{found}");
    assert_eq!(d["overlaps"], json!([]));
    assert_eq!(d["truncated"], json!(false));
    assert_eq!(d["denied"], Value::Null);
    let named = json_run(&app, &["status", "--drafts", "src/", "docs"]);
    assert_eq!(named["drafts"]["asked"], json!(2), "{named}");
    let text = stdout(&run(&app, &["status", "--drafts"]).success());
    assert!(text.contains("no other live workspace"), "{text}");

    s.repo.set_scopes(&["repo:demo:read", "repo:demo:write"]);
    let both = json_run(&app, &["status", "--upstream", "--drafts"]);
    assert_eq!(both["upstream"]["up_to_date"], json!(true), "{both}");
    let denied = both["drafts"]["denied"].as_str().expect("denied scope");
    assert!(denied.contains("drafts"), "{both}");
    let text = stdout(&run(&app, &["status", "--upstream", "--drafts"]).success());
    assert!(text.contains("drafts: not shown"), "{text}");
    let alone = wc_cmd(&app)
        .args(["--json", "status", "--drafts"])
        .assert()
        .failure();
    assert_eq!(error_of(&alone)["kind"], json!("permission_denied"));
}

#[test]
fn line_create_then_list_then_land_on_the_new_line() {
    let s = serve(Policy::Direct);
    let dir = scratch();
    let outside = |args: &[&str]| {
        graft(dir.path())
            .args(["--endpoint", &s.url, "--repo", "demo"])
            .args(args)
            .assert()
    };

    let made = json_out(
        outside(&["--json", "line", "create", "batch-1", "--from", "main"])
            .success()
            .get_output(),
    );
    assert_valid("line", &made);
    assert_eq!(made["name"], json!("batch-1"));
    assert_eq!(made["version"], json!(1));
    let human = stdout(&outside(&["line", "create", "batch-2", "--from", "main"]).success());
    assert!(human.starts_with("created line batch-2 at "), "{human}");

    let list = json_out(outside(&["--json", "line", "list"]).success().get_output());
    assert_valid("line-list", &list);
    let names: Vec<_> = list["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|l| l["name"].as_str().expect("name").to_owned())
        .collect();
    assert_eq!(names, ["batch-1", "batch-2", "main"]);
    assert_eq!(list["lines"][0]["target"], list["lines"][2]["target"]);
    let page = json_out(
        outside(&["--json", "line", "list", "--limit", "1"])
            .success()
            .get_output(),
    );
    assert_valid("line-list", &page);
    assert_eq!(page["next"], json!("batch-1"));
    let rest = json_out(
        outside(&[
            "--json", "line", "list", "--limit", "5", "--cursor", "batch-1", "--prefix", "batch",
        ])
        .success()
        .get_output(),
    );
    assert_eq!(rest["lines"][0]["name"], json!("batch-2"));
    assert_eq!(rest["lines"].as_array().map(Vec::len), Some(1));

    // Creating it again is refused, and nothing moves.
    let again = outside(&["--json", "line", "create", "batch-1", "--from", "main"]).failure();
    assert_eq!(error_of(&again)["kind"], json!("ref_conflict"));
    let bad = outside(&["--json", "line", "create", "has space", "--from", "main"]).code(2);
    assert_eq!(error_of(&bad)["kind"], json!("invalid_change"));

    // A land on the new line moves it and leaves main alone.
    graft(dir.path())
        .args([
            "--endpoint",
            &s.url,
            "clone",
            "demo",
            "app",
            "--line",
            "batch-1",
        ])
        .assert()
        .success();
    let app = dir.path().join("app");
    fs::write(app.join("README.md"), "batch\n").unwrap();
    json_run(&app, &["commit", "-m", "batch work"]);
    let landed = json_run(&app, &["land"]);
    assert_valid("land", &landed);
    let after = json_out(outside(&["--json", "line", "list"]).success().get_output());
    assert_eq!(after["lines"][0]["version"], json!(2));
    assert_eq!(after["lines"][2], list["lines"][2], "main did not move");
}

/// D27: the pre-land check stops a land on a real conflict without sending
/// it, and `--no-check` sends it, so the server's land reports the conflict.
#[test]
fn the_pre_land_check_stops_a_real_conflict_and_no_check_skips_it() {
    let s = serve(Policy::Direct);
    let (one, two) = (scratch(), scratch());
    let app = two_writers_on_one_file(
        &s,
        one.path(),
        two.path(),
        "fn a() {}\nfn b() {}\nfn c() { 2 }\n",
    );
    let tip = s.rt.block_on(s.repo.tip());
    let a = wc_cmd(&app).args(["--json", "land"]).assert().code(3);
    let error = error_of(&a);
    assert_eq!(error["kind"], json!("conflict"));
    let pre = &error["preflight"];
    assert_eq!(pre["checked"], json!(true), "{error}");
    assert_eq!(pre["stop"], json!(true));
    assert_eq!(pre["conflicts"], json!(["src/lib.rs"]));
    assert_eq!(pre["tip"], json!(tip.to_string()));
    let text = stdout(&wc_cmd(&app).arg("land").assert().code(3));
    assert!(text.contains("pre-land: conflict src/lib.rs"), "{text}");

    let a = wc_cmd(&app)
        .args(["--json", "land", "--no-check"])
        .assert()
        .code(3);
    let error = error_of(&a);
    assert_eq!(error["kind"], json!("conflict"));
    assert_eq!(error["preflight"], json!({"checked": false}));
    assert_eq!(s.rt.block_on(s.repo.tip()), tip);
}

/// D27: a file both sides changed that merges cleanly is reported and the
/// land goes on; without the drafts scope the overlap read is skipped with
/// a note.
#[test]
fn the_pre_land_check_reports_clean_merges_and_draft_overlap() {
    let s = serve(Policy::Direct);
    let (one, two) = (scratch(), scratch());
    let app = two_writers_on_one_file(
        &s,
        one.path(),
        two.path(),
        "fn a() { 2 }\nfn b() {}\nfn c() {}\n",
    );

    let value = json_run(&app, &["land"]);
    assert_valid("land", &value);
    assert_eq!(value["outcome"], json!("landed"));
    let pre = &value["preflight"];
    assert_eq!(pre["stop"], json!(false), "{value}");
    assert_eq!(pre["conflicts"], json!([]));
    assert_eq!(pre["merges_cleanly"], json!(["src/lib.rs"]));
    // Every working copy here has the same owner, and the overlap leaves
    // out the caller's own workspaces (graft-mcp's tests cover a hit).
    assert_eq!(pre["drafts"]["checked"], json!(true));

    // Without the drafts scope the overlap is skipped, and the land goes on.
    s.repo
        .set_scopes(&["repo:demo:read", "repo:demo:write", "repo:demo:land:main"]);
    fs::write(app.join("README.md"), "mine\n").unwrap();
    json_run(&app, &["commit", "-m", "mine"]);
    let value = json_run(&app, &["land"]);
    assert_valid("land", &value);
    let drafts = &value["preflight"]["drafts"];
    assert_eq!(drafts["checked"], json!(false), "{value}");
    assert!(
        drafts["skipped"].as_str().expect("note").contains("drafts"),
        "{value}"
    );
}
