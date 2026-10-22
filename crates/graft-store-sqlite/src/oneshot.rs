//! A one-shot reply channel from a worker thread to an async caller. The
//! store needs nothing more from an async runtime, so it depends on none.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

#[derive(Debug)]
struct Slot<T> {
    value: Option<T>,
    waker: Option<Waker>,
    closed: bool,
}

/// The worker's end. Dropping it unsent resolves the receiver to `None`.
#[derive(Debug)]
pub(crate) struct Sender<T>(Arc<Mutex<Slot<T>>>);

/// The caller's end: a future of the reply, or `None` if the worker went away.
#[derive(Debug)]
pub(crate) struct Receiver<T>(Arc<Mutex<Slot<T>>>);

pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let slot = Arc::new(Mutex::new(Slot {
        value: None,
        waker: None,
        closed: false,
    }));
    (Sender(slot.clone()), Receiver(slot))
}

impl<T> Sender<T> {
    pub(crate) fn send(self, value: T) {
        self.close(Some(value));
    }

    fn close(&self, value: Option<T>) {
        let waker = {
            let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.closed {
                return;
            }
            slot.closed = true;
            slot.value = value;
            slot.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.close(None);
    }
}

impl<T> Future for Receiver<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.closed {
            return Poll::Ready(slot.value.take());
        }
        match &slot.waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => slot.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Wake;
    use std::thread;

    struct Unpark(thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        let waker = Waker::from(Arc::new(Unpark(thread::current())));
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            thread::park();
        }
    }

    #[test]
    fn a_reply_from_another_thread_wakes_the_caller() {
        let (tx, rx) = channel();
        let worker = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(20));
            tx.send(7);
        });
        assert_eq!(block_on(rx), Some(7));
        worker.join().expect("worker");
    }

    #[test]
    fn a_dropped_sender_resolves_to_none() {
        let (tx, rx) = channel::<u8>();
        drop(tx);
        assert_eq!(block_on(rx), None);
    }
}
