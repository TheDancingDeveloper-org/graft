//! The crash harness (work item G1.5, design §6.6).
//!
//! A crash test is one `#[test]` function that plays two roles. Run normally
//! it is the *parent*: for each iteration it re-runs its own test binary as a
//! *child* ([`run_child`]), lets the child write, kills it with `SIGKILL` at
//! a random point, then reopens the store and checks every write the child
//! acknowledged. Run as the child ([`child`] returns `Some`), it writes and
//! calls [`ack`] after each write returns.
//!
//! Acknowledgements travel over the child's stdout pipe. A line the child
//! wrote before it died is still in the pipe, so the parent sees exactly
//! the writes that returned; a line cut short by the kill is discarded.
//!
//! ```ignore
//! #[test]
//! fn acknowledged_writes_survive_kill_9() {
//!     if let Some(child) = crash::child() {
//!         return write_until_killed(&child.dir, child.iteration);
//!     }
//!     let dir = crash::TempDir::new("store-crash");
//!     let mut delays = crash::Delays::new(SEED);
//!     for iteration in 1..=20 {
//!         let run = crash::run_child(
//!             "acknowledged_writes_survive_kill_9",
//!             dir.path(),
//!             iteration,
//!             delays.next(Duration::ZERO, Duration::from_millis(500)),
//!         );
//!         verify(dir.path(), &run.acks);
//!     }
//! }
//! ```

#![cfg(unix)]

use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::Duration;

/// The child's working directory (set by [`run_child`]).
const DIR_ENV: &str = "GRAFT_CRASH_DIR";

/// The child's iteration number (set by [`run_child`]).
const ITERATION_ENV: &str = "GRAFT_CRASH_ITERATION";

/// The prefix of an acknowledgement line on the child's stdout.
const ACK_PREFIX: &str = "GRAFT-ACK ";

/// How long the parent waits for the child's first acknowledgement.
const FIRST_ACK_TIMEOUT: Duration = Duration::from_secs(60);

/// What a child is told to do.
#[derive(Clone, Debug)]
pub struct ChildEnv {
    /// The directory the parent owns and verifies.
    pub dir: PathBuf,
    /// The iteration, from 1; children use it to make their writes unique.
    pub iteration: u32,
}

/// `Some` when this process is a crash child started by [`run_child`].
pub fn child() -> Option<ChildEnv> {
    let dir = std::env::var_os(DIR_ENV)?;
    let iteration = std::env::var(ITERATION_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .expect("crash child without an iteration");
    Some(ChildEnv {
        dir: dir.into(),
        iteration,
    })
}

/// Acknowledges one write from a child. Call it only after the write has
/// returned. `line` must not contain a newline. Safe from many threads.
pub fn ack(line: &str) {
    debug_assert!(!line.contains('\n'));
    let mut out = std::io::stdout().lock();
    // Bypass the test harness's output capture: write the bytes directly.
    out.write_all(format!("{ACK_PREFIX}{line}\n").as_bytes())
        .and_then(|()| out.flush())
        .expect("write acknowledgement");
}

/// The result of one child run.
#[derive(Clone, Debug)]
pub struct ChildRun {
    /// Acknowledged writes, in the order the child printed them.
    pub acks: Vec<String>,
    /// Whether the kill landed (`false` if the child finished first).
    pub killed: bool,
}

/// Runs `test` from the current test binary as a crash child working in
/// `dir`, waits for its first acknowledgement, sleeps `delay`, and kills it
/// with `SIGKILL`. Returns what it acknowledged. Panics if the child
/// acknowledges nothing or fails on its own.
pub fn run_child(test: &str, dir: &Path, iteration: u32, delay: Duration) -> ChildRun {
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(DIR_ENV, dir)
        .env(ITERATION_ENV, iteration.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child");
    let stdout = child.stdout.take().expect("child stdout");
    let (first, first_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut acks = Vec::new();
        let mut lines = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match lines.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                // A line cut short by the kill was never acknowledged.
                Ok(_) if !line.ends_with('\n') => break,
                Ok(_) => {}
            }
            // The test harness may print "test <name> ... " on the same line
            // before the child's first output, so look past it.
            let Some(at) = line.find(ACK_PREFIX) else {
                continue;
            };
            let ack = line[at + ACK_PREFIX.len()..].trim_end_matches('\n');
            if acks.is_empty() {
                let _ = first.send(());
            }
            acks.push(ack.to_owned());
        }
        acks
    });

    if first_rx.recv_timeout(FIRST_ACK_TIMEOUT).is_err() {
        let _ = child.kill();
        let status = child.wait().expect("wait for crash child");
        panic!("crash child {iteration} acknowledged nothing ({status})");
    }
    thread::sleep(delay);
    let killed = match child.try_wait().expect("poll crash child") {
        Some(status) => {
            assert!(status.success(), "crash child {iteration} failed: {status}");
            false
        }
        None => {
            child.kill().expect("kill crash child");
            let status = child.wait().expect("wait for crash child");
            // It may have exited on its own between the poll and the kill.
            status.signal() == Some(9)
        }
    };
    let acks = reader.join().expect("acknowledgement reader");
    ChildRun { acks, killed }
}

/// Seeded kill delays, so a failing run reproduces from its seed.
#[derive(Clone, Debug)]
pub struct Delays(u64);

impl Delays {
    /// A generator from `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next delay, uniform in `min..=max` milliseconds.
    pub fn next(&mut self, min: Duration, max: Duration) -> Duration {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let (lo, hi) = (min.as_millis() as u64, max.as_millis() as u64);
        let span = hi.saturating_sub(lo) + 1;
        Duration::from_millis(lo + (self.0 >> 33) % span)
    }
}

/// A fresh directory under the system temp dir, removed when dropped.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    /// Creates `<tmp>/graft-<tag>-<pid>-<n>`, empty.
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("graft-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Unpark(thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives a future to completion on this thread, parking between polls.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_stay_in_range_and_repeat_from_the_seed() {
        let (min, max) = (Duration::from_millis(5), Duration::from_millis(9));
        let mut a = Delays::new(7);
        let mut b = Delays::new(7);
        for _ in 0..100 {
            let d = a.next(min, max);
            assert!((min..=max).contains(&d));
            assert_eq!(d, b.next(min, max));
        }
    }

    #[test]
    fn temp_dirs_are_distinct_and_removed() {
        let a = TempDir::new("testkit");
        let b = TempDir::new("testkit");
        assert_ne!(a.path(), b.path());
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists());
    }

    /// The harness end to end: this test is its own child.
    #[test]
    fn crash_child_acknowledgements_reach_the_parent() {
        if let Some(env) = child() {
            // Acknowledges until killed.
            let mut n = 0u64;
            loop {
                ack(&format!("{} {n}", env.iteration));
                n += 1;
                thread::sleep(Duration::from_millis(1));
            }
        }
        let dir = TempDir::new("testkit-crash");
        let run = run_child(
            "crash::tests::crash_child_acknowledgements_reach_the_parent",
            dir.path(),
            3,
            Duration::from_millis(50),
        );
        assert!(run.killed, "the endless child is killed");
        assert!(!run.acks.is_empty());
        for (n, ack) in run.acks.iter().enumerate() {
            assert_eq!(ack, &format!("3 {n}"), "every line arrives whole, in order");
        }
    }
}
