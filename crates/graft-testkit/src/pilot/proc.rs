//! Child processes with a deadline on every step, the three graft binaries,
//! hermetic `git`, and a `graft-server` site the harness starts and kills.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::PilotError;

/// The deadline of one ordinary step (a git command, a CLI call, fsck).
pub const STEP_TIMEOUT: Duration = Duration::from_secs(120);

/// A hang bound for a server that never prints a listen address and never
/// exits. Readiness itself is the "listening on" line (or process exit).
/// This bound is not a speed check.
pub const START_TIMEOUT: Duration = Duration::from_secs(120);

/// What a finished step printed, and how long it ran (spawn to exit).
#[derive(Debug)]
pub struct Ran {
    /// Exit status.
    pub status: ExitStatus,
    /// Standard output.
    pub stdout: Vec<u8>,
    /// Standard error.
    pub stderr: Vec<u8>,
    /// Spawn to exit.
    pub elapsed: Duration,
}

impl Ran {
    /// Standard output as text (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

fn describe(cmd: &Command) -> String {
    // The program and its arguments only: tokens never travel in arguments,
    // and the environment is left out on purpose.
    let mut s = Path::new(cmd.get_program())
        .file_name()
        .unwrap_or(cmd.get_program())
        .to_string_lossy()
        .into_owned();
    for a in cmd.get_args().take(6) {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}

/// Sends SIGKILL to `pid`. Used only when a step overruns its deadline,
/// while another thread owns the child and waits on it.
fn kill9(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Runs `cmd` to completion, killing it if it runs past `timeout`. Fails
/// on a timeout, and on a non-zero exit unless `allow_failure`.
pub fn run_with(
    cmd: &mut Command,
    timeout: Duration,
    allow_failure: bool,
) -> Result<Ran, PilotError> {
    let what = describe(cmd);
    let start = Instant::now();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| PilotError::Step(format!("{what}: spawn: {e}")))?;
    let pid = child.id();
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    // The waiter owns the child, so the exit time is exact; the deadline is
    // a channel timeout here.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let status = child.wait();
        let _ = tx.send((status, start.elapsed()));
    });
    let (status, elapsed) = match rx.recv_timeout(timeout) {
        Ok(done) => done,
        Err(_) => {
            kill9(pid);
            let _ = rx.recv_timeout(Duration::from_secs(10));
            return Err(PilotError::Timeout(format!("{what} after {timeout:?}")));
        }
    };
    let status = status.map_err(|e| PilotError::Step(format!("{what}: wait: {e}")))?;
    let ran = Ran {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        elapsed,
    };
    if !allow_failure && !ran.status.success() {
        let stderr = String::from_utf8_lossy(&ran.stderr);
        let tail: String = stderr.lines().rev().take(5).collect::<Vec<_>>().join(" / ");
        return Err(PilotError::Step(format!("{what}: {}: {tail}", ran.status)));
    }
    Ok(ran)
}

/// [`run_with`] with the standard deadline, failing on a non-zero exit.
pub fn run(cmd: &mut Command) -> Result<Ran, PilotError> {
    run_with(cmd, STEP_TIMEOUT, false)
}

fn stderr_tail(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = buf.lock().map(|g| g.clone()).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    text.lines().rev().take(8).collect::<Vec<_>>().join(" / ")
}
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

/// The binaries under test.
#[derive(Clone, Debug)]
pub struct Bins {
    /// `graft-server`.
    pub server: PathBuf,
    /// `graft-mirror`.
    pub mirror: PathBuf,
    /// `graft` (graft-cli).
    pub cli: PathBuf,
}

impl Bins {
    /// Finds the binaries: `$GRAFT_SERVER_BIN`, `$GRAFT_MIRROR_BIN` and
    /// `$GRAFT_CLI_BIN` when set, else the target directory this process
    /// was built into, after `cargo build` of the three packages in the same
    /// profile (a no-op when they are fresh). The build is skipped when
    /// `$GRAFT_PILOT_NO_BUILD` is set.
    pub fn locate() -> Result<Bins, PilotError> {
        let from_env = |var: &str| std::env::var_os(var).map(PathBuf::from);
        if let (Some(server), Some(mirror), Some(cli)) = (
            from_env("GRAFT_SERVER_BIN"),
            from_env("GRAFT_MIRROR_BIN"),
            from_env("GRAFT_CLI_BIN"),
        ) {
            return Ok(Bins {
                server,
                mirror,
                cli,
            });
        }
        let exe =
            std::env::current_exe().map_err(|e| PilotError::Step(format!("current_exe: {e}")))?;
        // target/<profile>/deps/<test> or target/<profile>/<bin>.
        let mut dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        if dir.file_name() == Some(OsStr::new("deps")) {
            dir.pop();
        }
        if std::env::var_os("GRAFT_PILOT_NO_BUILD").is_none() {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut cmd = Command::new(cargo);
            cmd.args(["build", "--quiet", "--bins"]).args([
                "-p",
                "graft-server",
                "-p",
                "graft-mirror",
                "-p",
                "graft-cli",
            ]);
            if dir.file_name() == Some(OsStr::new("release")) {
                cmd.arg("--release");
            }
            if let Some(target) = dir.parent() {
                cmd.env("CARGO_TARGET_DIR", target);
            }
            run_with(&mut cmd, Duration::from_secs(1800), false)?;
        }
        let bins = Bins {
            server: dir.join("graft-server"),
            mirror: dir.join("graft-mirror"),
            cli: dir.join("graft"),
        };
        for b in [&bins.server, &bins.mirror, &bins.cli] {
            if !b.is_file() {
                return Err(PilotError::Step(format!(
                    "binary {} not found",
                    b.display()
                )));
            }
        }
        Ok(bins)
    }
}

/// `git` with no user or system config and fixed identities and dates, so
/// fixtures are the same on every machine.
pub fn git(dir: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Pilot")
        .env("GIT_AUTHOR_EMAIL", "pilot@example.invalid")
        .env("GIT_COMMITTER_NAME", "Pilot")
        .env("GIT_COMMITTER_EMAIL", "pilot@example.invalid")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args([
            "-c",
            "core.symlinks=true",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.quotepath=false",
        ]);
    cmd
}

/// A server's data directory and config file.
#[derive(Debug)]
pub struct Site {
    /// The data directory.
    pub dir: PathBuf,
    /// The config file.
    pub config: PathBuf,
    bins: Bins,
}

impl Site {
    /// A site in `dir` (created) for `bins.server`, on a free loopback port.
    pub fn new(bins: &Bins, dir: &Path) -> Result<Site, PilotError> {
        std::fs::create_dir_all(dir).map_err(|e| PilotError::io(dir, e))?;
        let config = dir.join("server.toml");
        let text = format!(
            "listen = \"127.0.0.1:0\"\nobjects_dir = \"{}\"\nmeta_db = \"{}\"\njson_logs = false\nshutdown_grace_s = 5\nmin_free_fraction = 0.0\n",
            dir.join("objects").display(),
            dir.join("meta/meta.db").display(),
        );
        std::fs::write(&config, text).map_err(|e| PilotError::io(&config, e))?;
        Ok(Site {
            dir: dir.to_path_buf(),
            config,
            bins: bins.clone(),
        })
    }

    /// The object store root.
    pub fn objects_dir(&self) -> PathBuf {
        self.dir.join("objects")
    }

    /// Starts `graft-server serve` and waits until it prints its listen
    /// address, or until it exits. [`START_TIMEOUT`] only catches a hang:
    /// a slow start that still prints the line is ready.
    pub fn start(&self) -> Result<Server, PilotError> {
        let started = Instant::now();
        let mut child = Command::new(&self.bins.server)
            .arg("--config")
            .arg(&self.config)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| PilotError::Step(format!("spawn graft-server: {e}")))?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = Arc::new(Mutex::new(Vec::new()));
        {
            let buf = Arc::clone(&stderr);
            let pipe = child.stderr.take();
            thread::spawn(move || {
                if let Some(mut p) = pipe {
                    let mut chunk = [0u8; 4096];
                    loop {
                        match p.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if let Ok(mut g) = buf.lock() {
                                    g.extend_from_slice(&chunk[..n]);
                                    if g.len() > 16 * 1024 {
                                        let drop_n = g.len() - 8 * 1024;
                                        g.drain(..drop_n);
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut lines = BufReader::new(stdout);
            let mut line = String::new();
            let mut sent = false;
            // Keep reading after the address, so the pipe never fills.
            while lines.read_line(&mut line).unwrap_or(0) > 0 {
                if !sent && let Some(a) = line.trim().strip_prefix("graft-server listening on ") {
                    let _ = tx.send(a.parse::<SocketAddr>().ok());
                    sent = true;
                }
                line.clear();
            }
            if !sent {
                let _ = tx.send(None);
            }
        });
        let mut server = Server {
            child: Some(child),
            addr: "127.0.0.1:0".parse().expect("addr"),
            startup: Duration::ZERO,
        };
        match rx.recv_timeout(START_TIMEOUT) {
            Ok(Some(addr)) => {
                server.addr = addr;
                server.startup = started.elapsed();
                Ok(server)
            }
            Ok(None) => {
                let status = server
                    .child
                    .as_mut()
                    .and_then(|c| c.try_wait().ok().flatten());
                let err = stderr_tail(&stderr);
                let how = match status {
                    Some(s) => format!("exited {s}"),
                    None => "stopped printing without a listen address".into(),
                };
                Err(PilotError::Step(format!("graft-server {how}: {err}")))
            }
            Err(_) => {
                kill9(pid);
                let tail = stderr_tail(&stderr);
                Err(PilotError::Timeout(format!(
                    "graft-server hung past {START_TIMEOUT:?}: {tail}"
                )))
            }
        }
    }

    /// Runs `graft-server admin <args>`.
    pub fn admin(&self, args: &[&str], allow_failure: bool) -> Result<Ran, PilotError> {
        let mut cmd = Command::new(&self.bins.server);
        cmd.arg("--config")
            .arg(&self.config)
            .arg("admin")
            .args(args);
        run_with(&mut cmd, STEP_TIMEOUT, allow_failure)
    }

    /// Mints a token with `scopes`, for an hour. The token goes from the
    /// child's stdout into memory; it is never printed or put in argv.
    pub fn mint(&self, subject: &str, scopes: &[&str]) -> Result<Token, PilotError> {
        let mut args = vec![
            "token",
            "mint",
            "--subject",
            subject,
            "--kind",
            "agent",
            "--ttl",
            "3600",
        ];
        for s in scopes {
            args.extend(["--scope", s]);
        }
        let ran = self.admin(&args, false)?;
        Ok(Token(ran.text().trim().to_owned()))
    }

    /// `admin fsck <repo>` (the server must be stopped). `Ok(true)` when clean.
    pub fn fsck(&self, repo: &str) -> Result<bool, PilotError> {
        Ok(self.admin(&["fsck", repo], true)?.status.success())
    }
}

/// A bearer token. Its `Debug` output is redacted.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Wraps a token value.
    pub fn new(value: impl Into<String>) -> Token {
        Token(value.into())
    }

    /// The secret, for a header or a child's environment only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// A running server; killed when dropped.
#[derive(Debug)]
pub struct Server {
    child: Option<Child>,
    /// The listen address.
    pub addr: SocketAddr,
    /// Spawn to listening.
    pub startup: Duration,
}

impl Server {
    /// The server URL.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// `kill -9`, then reaps it.
    pub fn kill9(mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
