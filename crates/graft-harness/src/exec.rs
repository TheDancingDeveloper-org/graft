//! A single-threaded executor that polls its tasks in a seeded order.
//!
//! Nothing the harness runs waits on a real thread or timer: the memory
//! store answers at once, and the fault wrappers, the land backoff and the
//! GC pace only yield. So every task can always be polled, and the order in
//! which the scheduler polls them is the whole interleaving.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use graft_engine::Sleeper;
use graft_gc::Pace;

use crate::rng::Rng;

/// A task the scheduler drives.
pub type Task = Pin<Box<dyn Future<Output = ()>>>;

/// Polls `task` once.
pub fn poll_once(task: &mut Task) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    task.as_mut().poll(&mut cx).is_ready()
}

/// Runs `future` to completion on this thread. It must only yield (as
/// everything in the harness does), never wait on another thread.
pub fn run_local<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
    }
}

/// A future that is pending `n` times, then ready: a point where the
/// scheduler may run other tasks first.
#[derive(Debug)]
pub struct Yield(pub u32);

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 == 0 {
            Poll::Ready(())
        } else {
            self.0 -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// The land backoff as one yield. The engine draws its jitter from a
/// random hasher, so the length is ignored: it must not steer the run.
#[derive(Clone, Copy, Debug, Default)]
pub struct YieldSleeper;

impl Sleeper for YieldSleeper {
    fn sleep(&self, _ms: u64) -> graft_engine::policy::HookFuture<'static, ()> {
        Box::pin(Yield(1))
    }
}

/// The GC pause as one yield.
#[derive(Clone, Copy, Debug, Default)]
pub struct YieldPace;

impl Pace for YieldPace {
    fn pause(&self, _pause: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(Yield(1))
    }
}

/// Live tasks, polled one at a time in an order drawn from a seed.
pub struct Scheduler {
    tasks: Vec<Option<Task>>,
    rng: Rng,
}

impl Scheduler {
    /// An empty scheduler.
    pub fn new(rng: Rng) -> Self {
        Self {
            tasks: Vec::new(),
            rng,
        }
    }

    /// Adds a task.
    pub fn spawn(&mut self, task: Task) {
        self.tasks.push(Some(task));
    }

    /// Whether every task has finished.
    pub fn idle(&self) -> bool {
        self.tasks.iter().all(Option::is_none)
    }

    /// Polls one live task, chosen at random. Returns false when none is
    /// left.
    pub fn step(&mut self) -> bool {
        self.tasks.retain(Option::is_some);
        if self.tasks.is_empty() {
            return false;
        }
        let i = self.rng.below(self.tasks.len() as u64) as usize;
        if let Some(task) = self.tasks[i].as_mut()
            && poll_once(task)
        {
            self.tasks[i] = None;
        }
        true
    }

    /// Drops every task mid-flight: the process died.
    pub fn kill_all(&mut self) {
        self.tasks.clear();
    }
}
