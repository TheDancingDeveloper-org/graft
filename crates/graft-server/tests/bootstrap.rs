//! The bootstrap admin token (`bootstrap_admin_token_file`): written once on
//! first start, mode 0600, never logged, and left alone afterwards.

mod common;

use std::fs;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{Site, command};

/// The longest any test here may take. A server that should have stopped
/// but keeps serving fails the test here instead of hanging the run.
const LIMIT: Duration = Duration::from_secs(120);

/// Aborts the test process if it is still running after `LIMIT`. Dropping
/// the guard disarms it.
struct Watchdog(Option<mpsc::Sender<()>>);

impl Watchdog {
    fn arm(name: &'static str) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(LIMIT) {
                eprintln!("{name}: exceeded {LIMIT:?}; aborting");
                std::process::abort();
            }
        });
        Self(Some(tx))
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

/// Runs `serve` with `config`, expecting it to exit by itself. A server
/// still running at `LIMIT` is killed and the test fails.
fn serve_expecting_exit(config: &Path, logs: &Path) -> (ExitStatus, String, String) {
    let out = logs.join("serve.out");
    let err = logs.join("serve.err");
    let mut child = command()
        .arg("--config")
        .arg(config)
        .stdout(Stdio::from(fs::File::create(&out).expect("stdout file")))
        .stderr(Stdio::from(fs::File::create(&err).expect("stderr file")))
        .spawn()
        .expect("spawn serve");
    let deadline = Instant::now() + LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("serve kept running past {LIMIT:?}; start-up should have stopped");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let read = |p: &Path| fs::read_to_string(p).expect("read output");
    (status, read(&out), read(&err))
}

fn site(tag: &str, token_file: &std::path::Path) -> Site {
    Site::new(
        tag,
        &format!(
            "bootstrap_admin_token_file = \"{}\"\nbootstrap_admin_repos = [\"boot\"]\n",
            token_file.display()
        ),
    )
}

#[tokio::test]
async fn first_start_writes_a_private_admin_token_that_is_never_logged() {
    let _watchdog = Watchdog::arm("first_start");
    let probe = graft_testkit::crash::TempDir::new("server-bootstrap-paths");
    let token_file = probe.path().join("secrets/admin.token");
    let site = site("server-bootstrap", &token_file);
    let log = site.dir.path().join("first.log");

    let server = site.start_with_stderr(Stdio::from(fs::File::create(&log).expect("log")));
    let first = fs::read(&token_file).expect("token file written");
    let token = String::from_utf8(first.clone())
        .expect("utf8")
        .trim()
        .to_owned();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode =
            |p: &std::path::Path| fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&token_file), 0o600);
        assert_eq!(mode(token_file.parent().expect("dir")), 0o700);
    }
    // An admin call the server accepts with the bootstrap token.
    server
        .clients(Some(&token))
        .await
        .create_repo("boot")
        .await
        .expect("admin call with the bootstrap token");
    assert!(server.stop().success());

    let text = fs::read_to_string(&log).expect("read log");
    assert!(
        text.contains(&format!(
            "wrote bootstrap admin token to {}",
            token_file.display()
        )),
        "{text}"
    );
    assert!(!token.is_empty() && !text.contains(&token), "token in log");

    // Second start: the file is byte-identical, and no token is logged.
    let log2 = site.dir.path().join("second.log");
    let again = site.start_with_stderr(Stdio::from(fs::File::create(&log2).expect("log")));
    assert!(again.stop().success());
    assert_eq!(fs::read(&token_file).expect("read again"), first);
    let text = fs::read_to_string(&log2).expect("read log");
    assert!(!text.contains("wrote bootstrap admin token"), "{text}");
    assert!(!text.contains(&token), "token in log");
}

#[test]
fn an_unwritable_token_path_stops_start_up_naming_the_path() {
    let _watchdog = Watchdog::arm("unwritable");
    // The parent of the token path is a regular file, so creating the
    // directory fails for every user, root included.
    let probe = graft_testkit::crash::TempDir::new("server-bootstrap-ro-paths");
    let blocker = probe.path().join("not-a-dir");
    fs::write(&blocker, b"").expect("write blocker");
    let token_file = blocker.join("admin.token");
    let site = site("server-bootstrap-ro", &token_file);
    let (status, stdout, stderr) = serve_expecting_exit(&site.config, site.dir.path());
    assert!(!status.success(), "start-up must stop");
    assert!(
        stderr.contains(&format!("bootstrap admin token {}", token_file.display())),
        "{stderr}"
    );
    assert!(!stdout.contains("listening on"), "{stdout}");
    assert!(!token_file.exists());
    assert_eq!(
        fs::read(&blocker).expect("blocker"),
        b"",
        "blocker untouched"
    );
    // No token was minted into any output.
    for text in [&stdout, &stderr] {
        assert!(!text.contains("wrote bootstrap admin token"), "{text}");
        assert!(!text.contains("v4.public."), "token in output: {text}");
    }
}

#[test]
fn the_key_needs_a_repository() {
    let _watchdog = Watchdog::arm("needs_repo");
    let probe = graft_testkit::crash::TempDir::new("server-bootstrap-norepo");
    let site = Site::new(
        "server-bootstrap-norepo",
        &format!(
            "bootstrap_admin_token_file = \"{}\"\n",
            probe.path().join("t").display()
        ),
    );
    let (status, _, stderr) = serve_expecting_exit(&site.config, site.dir.path());
    assert!(!status.success());
    assert!(stderr.contains("bootstrap_admin_repos"), "{stderr}");
    assert!(!probe.path().join("t").exists());
}
