//! Process smoke tests for the server roles shipped in the `graft` binary.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};

use assert_cmd::Command as AssertCommand;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_graft")
}

struct Running {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

impl Running {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(bin())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start graft role");
        let stdout = BufReader::new(child.stdout.take().expect("role stdout"));
        Self { child, stdout }
    }

    fn wait_for(&mut self, prefix: &str) {
        let mut line = String::new();
        loop {
            line.clear();
            assert!(
                self.stdout.read_line(&mut line).expect("read role stdout") > 0,
                "role exited before printing {prefix:?}"
            );
            if line.starts_with(prefix) {
                return;
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let _ = self.child.wait();
    }
}

#[test]
fn one_binary_starts_every_role() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("server.toml"),
        format!(
            "listen = \"127.0.0.1:0\"\nobjects_dir = {:?}\nmeta_db = {:?}\njson_logs = false\nmin_free_fraction = 0.0\nshutdown_grace_s = 1\n",
            dir.path().join("objects"),
            dir.path().join("meta/meta.db"),
        ),
    )
    .expect("server config");
    let directory = dir.path().to_str().expect("utf8 temp path");
    let mut server = Running::start(&["-C", directory, "serve", "--config", "server.toml"]);
    server.wait_for("graft-server listening on ");

    std::fs::write(
        dir.path().join("node.toml"),
        format!(
            "socket = {:?}\ncache_dir = {:?}\nupstream = \"http://127.0.0.1:9\"\nserver_public_key = \"0000000000000000000000000000000000000000000000000000000000000000\"\njson_logs = false\nmin_free_bytes = 0\nshutdown_grace_s = 1\n",
            dir.path().join("node.sock"),
            dir.path().join("cache"),
        ),
    )
    .expect("node config");
    let mut node = Running::start(&[
        "node",
        "--config",
        dir.path().join("node.toml").to_str().unwrap(),
    ]);
    node.wait_for("graft-node listening on ");

    AssertCommand::new(bin())
        .arg("mirror")
        .assert()
        .code(64)
        .stderr(predicates::str::contains("usage: graft-mirror"));
}
