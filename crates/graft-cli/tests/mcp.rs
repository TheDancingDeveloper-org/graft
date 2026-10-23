//! `graft mcp` (WI-2300): the MCP server over stdio against a served
//! repository, inside a working copy (the working-copy tools) and outside
//! one (the repository tools only).

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
