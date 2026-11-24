//! A thread of its own that does what a front end hands it, in the order handed, so the UI
//! thread passes on work that may block, such as writing a diagnostic or starting a clipboard
//! command, without waiting for it.
//!
//! The [`Worker`] stays with the code that runs after the event loop, the only code that may
//! wait for the thread; the UI gets a [`Sender`], which can only hand work on.

use std::sync::mpsc;
use std::time::Duration;

/// The thread, which runs until every handle to it is gone.
#[derive(Debug)]
pub struct Worker<T> {
    sender: mpsc::Sender<Request<T>>,
}

/// Hands work to a [`Worker`]'s thread without ever waiting for it; every clone hands work to
/// the same thread.
#[derive(Debug)]
pub struct Sender<T> {
    sender: mpsc::Sender<Request<T>>,
}

/// What the thread is handed.
#[derive(Debug)]
enum Request<T> {
    /// Something to do.
    Run(T),
    /// Says on the sender once everything handed before is done.
    Flush(mpsc::Sender<()>),
}

impl<T: Send + 'static> Worker<T> {
    /// Starts the thread, which runs `run` on each item it is handed, one at a time in the
    /// order handed.
    pub fn start(mut run: impl FnMut(T) + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for request in receiver {
                match request {
                    Request::Run(item) => run(item),
                    Request::Flush(done) => {
                        // The flusher may have stopped waiting, past its deadline.
                        done.send(()).ok();
                    }
                }
            }
        });
        Self { sender }
    }

    /// A handle that hands work to the thread.
    #[must_use]
    pub fn sender(&self) -> Sender<T> {
        Sender {
            sender: self.sender.clone(),
        }
    }

    /// Waits until everything handed before is done, or `deadline` has passed, whichever comes
    /// first, so a thread stuck in its work cannot hang the caller.
    pub fn flush(&self, deadline: Duration) {
        let (done, finished) = mpsc::channel();
        if self.sender.send(Request::Flush(done)).is_ok() {
            // Past the deadline the work is stuck, and the caller goes on without it.
            finished.recv_timeout(deadline).ok();
        }
    }
}

impl<T> Sender<T> {
    /// Hands `item` to the thread, after everything handed before, without waiting.
    pub fn send(&self, item: T) {
        // The thread ends only when `run` panics, and what it would have done is lost with it.
        self.sender.send(Request::Run(item)).ok();
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    /// The thread does what it is handed in the order handed, from every clone, and a flush
    /// returns once all of it is done.
    #[test]
    fn work_runs_in_order_before_a_flush_returns() {
        let done = Arc::new(Mutex::new(Vec::new()));
        let worker = Worker::start({
            let done = Arc::clone(&done);
            move |item: u32| done.lock().push(item)
        });
        let sender = worker.sender();
        let clone = sender.clone();
        for item in 0..100 {
            if item % 2 == 0 {
                sender.send(item);
            } else {
                clone.send(item);
            }
        }
        worker.flush(Duration::from_secs(10));
        assert_eq!(*done.lock(), (0..100).collect::<Vec<_>>());
    }

    /// A flush gives up at its deadline while the thread is stuck.
    #[test]
    fn a_flush_gives_up_on_a_stuck_thread() {
        let (unstick, stuck) = mpsc::channel::<()>();
        let worker = Worker::start(move |()| stuck.recv().unwrap());
        worker.sender().send(());
        let started = std::time::Instant::now();
        worker.flush(Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(5));
        unstick.send(()).unwrap();
    }
}
