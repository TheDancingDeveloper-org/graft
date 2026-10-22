//! The binary: config refusal, start-up on the socket, metrics, and a clean
//! stop on SIGTERM that removes the socket.

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use graft_node::NodeConfig;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_graft-node")
}

#[test]
fn the_shipped_example_config_parses() {
    let text = include_str!("../deploy/node.toml");
    NodeConfig::parse(text).expect("deploy/node.toml is valid");
}

#[test]
fn a_bad_config_exits_78() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.toml");
    std::fs::write(&path, "nonsense = true\n").unwrap();
    let out = Command::new(bin())
        .arg("--config")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
}

#[test]
fn starts_serves_metrics_and_stops_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = common::config(dir.path(), "http://127.0.0.1:9");
    cfg.metrics = Some("127.0.0.1:0".into());
    let path = dir.path().join("node.toml");
    let text = format!(
        "socket = {:?}\ncache_dir = {:?}\nupstream = {:?}\nserver_public_key = {:?}\nmetrics = \"127.0.0.1:0\"\njson_logs = false\nmin_free_bytes = 0\nshutdown_grace_s = 2\n",
        cfg.socket, cfg.cache_dir, cfg.upstream, cfg.server_public_key
    );
    std::fs::write(&path, text).unwrap();
    let started = Instant::now();
    let mut child = Command::new(bin())
        .arg("--config")
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut metrics = None;
    let mut listening = false;
    let mut line = String::new();
    while !(listening && metrics.is_some()) {
        line.clear();
        assert!(stdout.read_line(&mut line).unwrap() > 0, "exited early");
        if line.starts_with("graft-node listening on ") {
            listening = true;
        } else if let Some(a) = line.trim().strip_prefix("graft-node metrics on ") {
            metrics = Some(a.to_owned());
        }
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(cfg.socket.exists());

    let mut s = std::net::TcpStream::connect(metrics.unwrap()).unwrap();
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    assert!(body.contains("graft_node_cache_hits_total"), "{body}");

    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let code = loop {
        if let Some(code) = child.try_wait().unwrap() {
            break code;
        }
        assert!(Instant::now() < deadline, "did not stop");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(code.success(), "{code:?}");
    assert!(!cfg.socket.exists(), "the socket is removed on stop");
}
