//! Process smoke tests for the server roles shipped in the `graft` binary.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use assert_cmd::Command as AssertCommand;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_graft")
}

fn server_bin() -> PathBuf {
    std::env::var_os("GRAFT_TEST_SERVER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .expect("current test executable")
                .parent()
                .and_then(Path::parent)
                .expect("workspace target profile directory")
                .join("graft-server")
        })
}

struct Running {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

impl Running {
    fn start(program: impl AsRef<OsStr>, args: &[&str]) -> Self {
        let mut child = Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start graft role");
        let stdout = BufReader::new(child.stdout.take().expect("role stdout"));
        Self { child, stdout }
    }

    fn wait_for_addr(&mut self, prefix: &str) -> SocketAddr {
        let mut line = String::new();
        loop {
            line.clear();
            assert!(
                self.stdout.read_line(&mut line).expect("read role stdout") > 0,
                "role exited before printing {prefix:?}"
            );
            if let Some(addr) = line.trim().strip_prefix(prefix) {
                return addr.parse().expect("role address");
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

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve port")
        .local_addr()
        .expect("reserved address")
}

fn metrics(addr: SocketAddr) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect metrics");
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: graft\r\nConnection: close\r\n\r\n")
        .expect("request metrics");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read metrics");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    response
}

fn metric_names(response: &str) -> BTreeSet<&str> {
    response
        .split_once("\r\n\r\n")
        .map_or(response, |(_, body)| body)
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once(' ').map(|(sample, _)| sample))
        .map(|sample| sample.split('{').next().unwrap_or(sample))
        .collect()
}

fn init_git_repo(path: &Path) {
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(path.join("README.md"), "one binary\n").expect("fixture file");
    assert!(
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "-c",
                "user.name=Graft Test",
                "-c",
                "user.email=graft@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ])
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn one_binary_starts_every_role() {
    let dir = tempfile::tempdir().expect("temp dir");
    let listen = free_addr();
    let metrics_addr = free_addr();
    std::fs::write(
        dir.path().join("server.toml"),
        format!(
            "listen = \"{listen}\"\nmetrics = \"{metrics_addr}\"\nobjects_dir = {:?}\nmeta_db = {:?}\njson_logs = false\nmin_free_fraction = 0.0\nshutdown_grace_s = 1\n",
            dir.path().join("objects"),
            dir.path().join("meta/meta.db"),
        ),
    )
    .expect("server config");
    let config = dir.path().join("server.toml");
    let config_str = config.to_str().expect("utf8 config path");
    let token_file = dir.path().join("mirror.token");
    let token = AssertCommand::new(bin())
        .args([
            "serve",
            "--config",
            config_str,
            "admin",
            "token",
            "mint",
            "--subject",
            "smoke",
            "--scope",
            "repo:smoke:admin",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    std::fs::write(&token_file, token).expect("mirror token file");

    let unified_metrics = {
        let mut server = Running::start(bin(), &["serve", "--config", config_str]);
        assert_eq!(server.wait_for_addr("graft-server listening on "), listen);
        assert_eq!(
            server.wait_for_addr("graft-server metrics on "),
            metrics_addr
        );
        let fresh_metrics = metrics(metrics_addr);

        let git_repo = dir.path().join("fixture");
        std::fs::create_dir(&git_repo).expect("fixture directory");
        init_git_repo(&git_repo);
        AssertCommand::new(bin())
            .args(["mirror", "import"])
            .arg(git_repo.join(".git"))
            .args(["--repo", "smoke", "--server"])
            .arg(format!("http://{listen}"))
            .arg("--db")
            .arg(dir.path().join("mirror.sqlite"))
            .arg("--token-file")
            .arg(&token_file)
            .assert()
            .success()
            .stdout(predicates::str::contains("imported 1 commits"));

        fresh_metrics
    };

    let legacy_metrics = {
        let server_bin = server_bin();
        assert!(
            server_bin.is_file(),
            "legacy graft-server binary not found at {}; build it first or set GRAFT_TEST_SERVER_BIN",
            server_bin.display()
        );
        let mut server = Running::start(&server_bin, &["--config", config_str, "serve"]);
        assert_eq!(server.wait_for_addr("graft-server listening on "), listen);
        assert_eq!(
            server.wait_for_addr("graft-server metrics on "),
            metrics_addr
        );
        metrics(metrics_addr)
    };
    assert_eq!(
        metric_names(&unified_metrics),
        metric_names(&legacy_metrics)
    );

    std::fs::write(
        dir.path().join("node.toml"),
        format!(
            "socket = {:?}\ncache_dir = {:?}\nupstream = \"http://127.0.0.1:9\"\nserver_public_key = \"0000000000000000000000000000000000000000000000000000000000000000\"\njson_logs = false\nmin_free_bytes = 0\nshutdown_grace_s = 1\n",
            dir.path().join("node.sock"),
            dir.path().join("cache"),
        ),
    )
    .expect("node config");
    let mut node = Running::start(
        bin(),
        &[
            "node",
            "--config",
            dir.path().join("node.toml").to_str().unwrap(),
        ],
    );
    node.wait_for_addr("graft-node listening on ");
}
