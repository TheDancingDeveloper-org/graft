//! `graft mcp` (WI-2300): the MCP server over stdio against a served
//! repository, inside a working copy (the working-copy tools) and outside
//! one (the repository tools only).

#[path = "../../graft-client/tests/support/engine.rs"]
mod engine;
mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::Duration;

use predicates::str::contains;
use serde_json::{Value, json};
use support::*;

/// How long one response may take.
const WAIT: Duration = Duration::from_secs(60);

/// A running `graft mcp`, spoken to in newline-delimited JSON-RPC.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Mcp {
    /// Starts `graft mcp` in `cwd` with `args` before the subcommand, and
    /// runs the initialize handshake.
    fn start(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Mcp {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("graft"));
        cmd.current_dir(cwd)
            .env_remove("GRAFT_ENDPOINT")
            .env_remove("GRAFT_TOKEN")
            .env("GRAFT_AUTHOR_NAME", "Mcp Agent")
            .env("GRAFT_AUTHOR_EMAIL", "agent@example.com")
            .args(args)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn graft mcp");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        let mut mcp = Mcp {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        let init = mcp.request(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "graft-cli-test", "version": "0"},
            }),
        );
        assert_eq!(init["serverInfo"]["name"], "graft-mcp", "{init}");
        assert!(init["capabilities"]["tools"].is_object(), "{init}");
        mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        mcp
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").expect("write to graft mcp");
        stdin.flush().unwrap();
    }

    /// Sends a request and returns its `result`, failing on an error.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(WAIT)
                .unwrap_or_else(|_| panic!("no response to {method}"));
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout carried a non-JSON line ({e}): {line}"));
            if message["id"] == json!(id) {
                assert!(message["error"].is_null(), "{method} failed: {message}");
                return message["result"].clone();
            }
        }
    }

    /// Calls a tool; returns its structured content and whether it is an
    /// error result.
    fn call(&mut self, tool: &str, arguments: Value) -> (Value, bool) {
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        (
            result["structuredContent"].clone(),
            result["isError"] == json!(true),
        )
    }

    /// Calls a tool that must succeed.
    fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let (value, error) = self.call(tool, arguments);
        assert!(!error, "{tool} failed: {value}");
        value
    }

    /// Closes stdin and waits for a clean exit; returns standard error.
    fn finish(mut self) -> String {
        drop(self.stdin.take());
        let out = self.child.wait_with_output().expect("graft mcp exits");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "graft mcp exited {:?}: {stderr}",
            out.status
        );
        stderr
    }
}

fn tool_names(mcp: &mut Mcp) -> Vec<String> {
    let listed = mcp.request("tools/list", json!({}));
    listed["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn inside_a_working_copy_commit_and_land_move_the_line() {
    let s = serve();
    let dir = scratch();
    graft(dir.path())
        .args(["--endpoint", &s.url, "clone", REPO, "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    std::fs::write(app.join("README.md"), "from the agent\n").unwrap();

    let mut mcp = Mcp::start(&app, &["--endpoint", &s.url], &[]);

    let names = tool_names(&mut mcp);
    for tool in [
        "graft_log",
        "graft_show",
        "graft_diff",
        "graft_blame",
        "graft_read",
        "graft_ls",
        "graft_status",
        "graft_commit",
        "graft_land",
        "graft_sync",
        "graft_workspace",
    ] {
        assert!(names.iter().any(|n| n == tool), "{tool} in {names:?}");
    }

    // A read tool.
    let log = mcp.ok("graft_log", json!({"repo": REPO, "from": "main"}));
    let ids: Vec<&str> = log["changesets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3, "{log}");
    assert_eq!(ids[0], s.commits.third.to_string());

    // The working copy's status, served locally.
    let status = mcp.ok("graft_status", json!({"repo": REPO}));
    assert_eq!(status["repo"], REPO);
    assert_eq!(status["line"], "main");
    assert_eq!(status["base"], json!(s.commits.third.to_string()));
    assert_eq!(status["clean"], false);
    assert_eq!(
        status["changes"],
        json!([{"path": "README.md", "status": "modified"}])
    );
    let workspace = status["workspace"]
        .as_str()
        .map_or_else(|| "mcp".to_owned(), str::to_owned);

    // Another repository is not this working copy's.
    let (refused, error) = mcp.call("graft_status", json!({"repo": "other"}));
    assert!(error);
    assert_eq!(refused["kind"], "invalid_argument", "{refused}");

    // Commit the working copy, then land it.
    let draft = mcp.ok(
        "graft_commit",
        json!({
            "repo": REPO,
            "workspace": workspace,
            "message": "agent edit",
            "from_working_copy": true,
        }),
    );
    let draft_id = draft["id"].as_str().expect("draft id").to_owned();
    assert_eq!(draft["parent"], json!(s.commits.third.to_string()));
    assert_eq!(draft["changes"][0]["path"], "README.md");
    assert_eq!(draft["upload"]["uploaded"], 1, "{draft}");

    let landed = mcp.ok("graft_land", json!({"repo": REPO, "workspace": workspace}));
    assert_eq!(landed["outcome"], "landed", "{landed}");
    assert_eq!(landed["landed"], json!(draft_id));

    // The line moved, and the working copy with it.
    let tip = s.fixture.repo().tip("main").expect("main");
    assert_eq!(tip.to_string(), draft_id);
    let log = mcp.ok(
        "graft_log",
        json!({"repo": REPO, "from": "main", "limit": 1}),
    );
    assert_eq!(log["changesets"][0]["id"], json!(draft_id));
    let read = mcp.ok(
        "graft_read",
        json!({"repo": REPO, "at": "main", "path": "README.md"}),
    );
    assert_eq!(read["content"], "from the agent\n");
    let status = mcp.ok("graft_status", json!({"repo": REPO}));
    assert_eq!(status["base"], json!(draft_id));
    assert_eq!(status["head"], Value::Null);
    assert_eq!(status["clean"], true);

    let stderr = mcp.finish();
    assert!(stderr.is_empty(), "nothing to say on stderr: {stderr}");

    // The CLI agrees with what the MCP session left behind.
    let out = graft(&app).args(["--json", "status"]).assert().success();
    let cli = json_out(out.get_output());
    assert_eq!(cli["base"], json!(draft_id));
    assert_eq!(cli["clean"], true);
}

#[test]
fn status_lists_a_conflict_only_while_its_markers_remain() {
    let s = serve();
    let dir = scratch();
    graft(dir.path())
        .args(["--endpoint", &s.url, "clone", REPO, "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    std::fs::write(app.join("README.md"), "mine\n").unwrap();
    graft(&app)
        .args(["commit", "-m", "mine"])
        .assert()
        .success();
    let files: std::collections::BTreeMap<String, Vec<u8>> = [
        ("README.md", &b"theirs\n"[..]),
        ("src/lib.rs", b"fn a() {}\nfn b() {}\n"),
        ("assets/logo.bin", &[0xff, 0x00, 0x7f]),
        ("docs/guide.md", b"read me\n"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_vec()))
    .collect();
    s.rt.block_on(s.fixture.repo().commit("main", &files));
    graft(&app).arg("sync").assert().code(3);

    let mut mcp = Mcp::start(&app, &["--endpoint", &s.url], &[]);
    let status = mcp.ok("graft_status", json!({"repo": REPO}));
    assert_eq!(
        status["conflicts"],
        json!([{"path": "README.md", "mode": "markers"}]),
        "{status}"
    );

    std::fs::write(app.join("README.md"), "both\n").unwrap();
    let status = mcp.ok("graft_status", json!({"repo": REPO}));
    assert_eq!(status["conflicts"], json!([]), "{status}");
    assert_eq!(
        status["changes"],
        json!([{"path": "README.md", "status": "modified"}])
    );
    mcp.finish();
}

#[test]
fn outside_a_working_copy_only_the_repository_tools_answer() {
    let s = serve();
    let dir = scratch();
    let token = dir.path().join("token");
    std::fs::write(&token, "s3cret-file\n").unwrap();
    let token = token.to_str().unwrap().to_owned();
    let mut mcp = Mcp::start(
        dir.path(),
        &["--endpoint", &s.url, "--token-file", &token],
        &[],
    );

    let names = tool_names(&mut mcp);
    assert!(names.iter().any(|n| n == "graft_log"), "{names:?}");

    let log = mcp.ok("graft_log", json!({"repo": REPO, "from": "main"}));
    assert_eq!(log["changesets"].as_array().unwrap().len(), 3);

    let (status, error) = mcp.call("graft_status", json!({"repo": REPO}));
    assert!(error);
    assert_eq!(status["kind"], "needs_working_copy", "{status}");
    let (commit, error) = mcp.call(
        "graft_commit",
        json!({
            "repo": REPO,
            "workspace": "w",
            "message": "m",
            "from_working_copy": true,
        }),
    );
    assert!(error);
    assert_eq!(commit["kind"], "needs_working_copy", "{commit}");

    let stderr = mcp.finish();
    assert!(!stderr.contains("s3cret"), "{stderr}");
}

/// `graft_pick` on the real engine: a landed changeset re-applied onto an
/// older changeset (a release line's tip), then onto a tip that changed the
/// same line since, which is a conflict.
#[test]
fn pick_re_applies_a_landed_changeset() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let files: Vec<(&str, &[u8])> = vec![
        ("README.md", b"hello\n"),
        ("src/lib.rs", b"fn a() {}\nfn b() {}\nfn c() {}\n"),
    ];
    let repo = rt.block_on(engine::EngineRepo::direct(REPO, &files));
    let server = rt.block_on(graft_testkit::server::TestServer::tcp_workspaces(
        repo.clone(),
    ));
    let url = server.url().to_owned();
    let root = rt.block_on(repo.tip()).to_string();
    let dir = scratch();
    graft(dir.path())
        .args(["--endpoint", &url, "clone", REPO, "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    let land = |path: &str, content: &str| {
        std::fs::write(app.join(path), content).unwrap();
        for args in [&["commit", "-m", path][..], &["land"][..]] {
            graft(&app)
                .env("GRAFT_AUTHOR_NAME", "Test Author")
                .env("GRAFT_AUTHOR_EMAIL", "author@example.com")
                .args(args)
                .assert()
                .success();
        }
        rt.block_on(repo.tip()).to_string()
    };
    let readme = land("README.md", "mine\n");
    let lib = land("src/lib.rs", "fn a() {}\nfn b() {}\nfn c() { 1 }\n");
    land("README.md", "later\n");

    let mut mcp = Mcp::start(&app, &["--endpoint", &url], &[]);
    let picked = mcp.ok(
        "graft_pick",
        json!({"repo": REPO, "changeset": lib, "onto": root}),
    );
    assert_eq!(picked["picked"], json!(lib), "{picked}");
    assert_eq!(picked["onto"], json!(root), "{picked}");
    assert_eq!(picked["changed"], true, "{picked}");
    let read = mcp.ok(
        "graft_read",
        json!({"repo": REPO, "at": picked["id"], "path": "README.md"}),
    );
    assert_eq!(read["content"], "hello\n", "only the picked changes");

    let (conflict, error) = mcp.call(
        "graft_pick",
        json!({"repo": REPO, "changeset": readme, "onto": "main"}),
    );
    assert!(error, "{conflict}");
    assert_eq!(conflict["kind"], "conflict", "{conflict}");

    // A draft is refused, and nothing moves: no new changeset on any line.
    std::fs::write(app.join("docs.md"), "draft\n").unwrap();
    graft(&app)
        .env("GRAFT_AUTHOR_NAME", "Test Author")
        .env("GRAFT_AUTHOR_EMAIL", "author@example.com")
        .args(["commit", "-m", "draft"])
        .assert()
        .success();
    let cli_status = || {
        let out = graft(&app).args(["--json", "status"]).assert().success();
        json_out(out.get_output())
    };
    let status = cli_status();
    let draft = status["head"].clone();
    assert!(draft.is_string(), "{status}");
    let tip = rt.block_on(repo.tip());
    let (refused, error) = mcp.call(
        "graft_pick",
        json!({"repo": REPO, "changeset": draft, "onto": root}),
    );
    assert!(error, "{refused}");
    assert_eq!(refused["kind"], "invalid_argument", "{refused}");
    assert_eq!(rt.block_on(repo.tip()), tip);
    assert_eq!(
        cli_status()["head"],
        draft,
        "the workspace head did not move"
    );

    let (both, error) = mcp.call(
        "graft_pick",
        json!({"repo": REPO, "changeset": lib, "onto": root, "workspace": "w"}),
    );
    assert!(error);
    assert_eq!(both["kind"], "invalid_argument", "{both}");
    mcp.finish();
}

#[test]
fn the_token_never_reaches_the_output() {
    let s = serve();
    let dir = scratch();
    let mut mcp = Mcp::start(
        dir.path(),
        &["--endpoint", &s.url],
        &[("GRAFT_TOKEN", "s3cret-env")],
    );
    let log = mcp.ok("graft_log", json!({"repo": REPO, "from": "main"}));
    assert!(!log.to_string().contains("s3cret"));
    let stderr = mcp.finish();
    assert!(!stderr.contains("s3cret"), "{stderr}");
}

#[test]
fn without_an_endpoint_it_is_a_usage_error() {
    let dir = scratch();
    let out = graft(dir.path())
        .arg("mcp")
        .assert()
        .code(2)
        .stderr(contains("no endpoint"));
    assert!(out.get_output().stdout.is_empty(), "stdout stays clean");
}

/// Two working copies on the real engine (a `direct` line) edit separate
/// regions of `src/lib.rs`. The first lands through the CLI; the second
/// lands through `graft mcp`: with `merge: "file"` the rebase refuses the
/// file, and with no `merge` it line-merges (D16) and the working copy holds
/// the merged file.
#[test]
fn land_from_a_working_copy_line_merges_by_default() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let files: Vec<(&str, &[u8])> = vec![
        ("README.md", b"hello\n"),
        ("src/lib.rs", b"fn a() {}\nfn b() {}\nfn c() {}\n"),
    ];
    let repo = rt.block_on(engine::EngineRepo::direct(REPO, &files));
    let server = rt.block_on(graft_testkit::server::TestServer::tcp_workspaces(
        repo.clone(),
    ));
    let url = server.url().to_owned();
    let (one, two) = (scratch(), scratch());
    let clone = |dir: &Path| {
        graft(dir)
            .args(["--endpoint", &url, "clone", REPO, "app"])
            .assert()
            .success();
        dir.join("app")
    };
    let first = clone(one.path());
    let second = clone(two.path());
    let author = |cmd: &mut assert_cmd::Command| {
        cmd.env("GRAFT_AUTHOR_NAME", "Test Author")
            .env("GRAFT_AUTHOR_EMAIL", "author@example.com");
    };

    std::fs::write(
        first.join("src/lib.rs"),
        "fn a() {}\nfn b() {}\nfn c() { 1 }\n",
    )
    .unwrap();
    let mut cmd = graft(&first);
    author(&mut cmd);
    cmd.args(["commit", "-m", "one"]).assert().success();
    let mut cmd = graft(&first);
    author(&mut cmd);
    cmd.arg("land").assert().success();
    let first_tip = rt.block_on(repo.tip());

    std::fs::write(
        second.join("src/lib.rs"),
        "fn a() { 2 }\nfn b() {}\nfn c() {}\n",
    )
    .unwrap();
    let mut mcp = Mcp::start(&second, &["--endpoint", &url], &[]);
    let workspace = mcp.ok("graft_status", json!({"repo": REPO}))["workspace"]
        .as_str()
        .map_or_else(|| "mcp".to_owned(), str::to_owned);
    mcp.ok(
        "graft_commit",
        json!({
            "repo": REPO,
            "workspace": workspace,
            "message": "two",
            "from_working_copy": true,
        }),
    );

    // A file merge refuses the file both sides changed; nothing moves.
    let (refused, error) = mcp.call(
        "graft_land",
        json!({"repo": REPO, "workspace": workspace, "merge": "file"}),
    );
    assert!(error, "{refused}");
    assert_eq!(refused["kind"], "conflict", "{refused}");
    assert_eq!(rt.block_on(repo.tip()), first_tip);

    // No `merge`: a line merge, rebased onto the first land.
    let landed = mcp.ok("graft_land", json!({"repo": REPO, "workspace": workspace}));
    assert_eq!(landed["outcome"], "landed", "{landed}");
    assert_eq!(landed["rebased"], true, "{landed}");
    let tip = rt.block_on(repo.tip());
    assert_eq!(landed["landed"], json!(tip.to_string()));
    let merged = "fn a() { 2 }\nfn b() {}\nfn c() { 1 }\n";
    assert_eq!(
        rt.block_on(repo.files(tip))["src/lib.rs"],
        merged.as_bytes()
    );
    assert_eq!(
        std::fs::read_to_string(second.join("src/lib.rs")).unwrap(),
        merged
    );
    let status = mcp.ok("graft_status", json!({"repo": REPO}));
    assert_eq!(status["clean"], true, "{status}");
    assert_eq!(status["base"], json!(tip.to_string()));
    mcp.finish();
}
