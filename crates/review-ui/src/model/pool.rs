//! Where the model's jobs run: one pool of worker threads for the app, one per core, which
//! takes what the reviewer looks at first, and threads of their own for what nothing may hold
//! back.
//!
//! A worker takes the oldest job of the most urgent [`Lane`] that waits.
//! Background work never takes the last worker, so neither the history's verification nor a
//! lookup of a pull request keeps what shows waiting for a worker: at worst it waits for
//! another job that shows to end.
//! A worker starts only when a job finds every worker busy, and stays for the next job.
//!
//! The pool cancels nothing: a job that a newer request may supersede, such as a preview of a
//! file the reviewer moved on from, checks for itself whether it is still wanted when it
//! starts and at the boundaries of its work.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, LazyLock};

use parking_lot::{Condvar, Mutex, MutexGuard};

/// Where a job runs, and how soon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lane {
    /// In the pool, first: what the reviewer looks at, such as the opened review, the selected
    /// file's highlights and preview, or a picker's search.
    Selected,
    /// In the pool, after the selected: the rest of what shows, such as the other files'
    /// highlights, the captures of the working tree and the watch that asks for them.
    Shown,
    /// In the pool, last and never on its last worker: what checks or fetches behind what
    /// shows, such as the review's verification, a fetch or a lookup of a pull request.
    Background,
    /// On a thread of its own, which nothing in the pool holds back: a save, which quitting
    /// waits for, the jobs that stop the engine or end the review in the window, the watch on
    /// the grant, which stops language servers once trust is gone, and a wait for something
    /// outside the app, such as the working tree changing or a language server answering.
    Own,
}

/// The app's pool, which starts its workers with its first jobs.
pub(super) static POOL: LazyLock<Pool> = LazyLock::new(|| {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    // Two at least, so background work always leaves one worker for the rest.
    Pool::new(cores.max(2))
});

/// A pool of worker threads; see the module documentation.
pub(super) struct Pool {
    shared: Arc<Shared>,
}

/// What the pool's workers share.
struct Shared {
    queue: Mutex<Queue>,
    /// Wakes a waiting worker for a job.
    ready: Condvar,
    /// How many workers there are at most.
    workers: usize,
}

/// A job, as the pool runs it.
type Task = Box<dyn FnOnce() + Send>;

/// The jobs that wait, by lane, and the workers.
#[derive(Default)]
struct Queue {
    selected: VecDeque<Task>,
    shown: VecDeque<Task>,
    background: VecDeque<Task>,
    /// The workers started.
    started: usize,
    /// The workers waiting for a job that no job has woken yet.
    idle: usize,
    /// The background jobs running.
    running_background: usize,
}

/// A job a worker took, and whether it is background work.
struct Taken {
    task: Task,
    background: bool,
}

impl Pool {
    /// A pool of at most `workers` workers, none started yet.
    pub(super) fn new(workers: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::default(),
                ready: Condvar::new(),
                workers,
            }),
        }
    }

    /// Runs `task` in `lane`: queued in the pool, or on a thread of its own.
    pub(super) fn start(&self, lane: Lane, task: impl FnOnce() + Send + 'static) {
        let task: Task = Box::new(task);
        let mut queue = self.shared.queue.lock();
        match lane {
            Lane::Selected => queue.selected.push_back(task),
            Lane::Shown => queue.shown.push_back(task),
            Lane::Background => queue.background.push_back(task),
            Lane::Own => {
                drop(queue);
                std::thread::spawn(move || run(task));
                return;
            }
        }
        self.shared.wake(&mut queue);
    }
}

/// How long a test waits for a job to report before it fails, rather than hang.
#[cfg(test)]
pub(super) const PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
impl Pool {
    /// Takes a worker in `lane` until the returned sender sends or is dropped, as a slow job
    /// would, once a worker took it.
    pub(super) fn hold(&self, lane: Lane) -> std::sync::mpsc::Sender<()> {
        let (release, released) = std::sync::mpsc::channel();
        let (started, starts) = std::sync::mpsc::channel();
        self.start(lane, move || {
            started.send(()).ok();
            released.recv().ok();
        });
        starts
            .recv_timeout(PATIENCE)
            .expect("a worker takes the job");
        release
    }
}

impl Shared {
    /// Wakes a waiting worker, or starts one while there are fewer than the most, when a job
    /// waits that a worker may take.
    fn wake(self: &Arc<Self>, queue: &mut Queue) {
        if !queue.runnable(self.workers) {
            return;
        }
        if queue.idle > 0 {
            queue.idle -= 1;
            self.ready.notify_one();
        } else if queue.started < self.workers {
            queue.started += 1;
            let shared = Arc::clone(self);
            std::thread::spawn(move || shared.work());
        }
    }

    /// A worker's life: it takes the next job it may, and waits for one while there is none.
    fn work(self: Arc<Self>) {
        let mut queue = self.queue.lock();
        loop {
            let Some(Taken { task, background }) = queue.take(self.workers) else {
                queue.idle += 1;
                // Whoever wakes this worker takes it off the idle count.
                self.ready.wait(&mut queue);
                continue;
            };
            if background {
                queue.running_background += 1;
            }
            // Another job may wait that another worker can take meanwhile.
            self.wake(&mut queue);
            MutexGuard::unlocked(&mut queue, || run(task));
            if background {
                queue.running_background -= 1;
            }
        }
    }
}

impl Queue {
    /// Whether background work may take one more of `workers` workers: never the last.
    fn background_fits(&self, workers: usize) -> bool {
        self.running_background + 1 < workers
    }

    /// Whether a job waits that a worker may take.
    fn runnable(&self, workers: usize) -> bool {
        !self.selected.is_empty()
            || !self.shown.is_empty()
            || (!self.background.is_empty() && self.background_fits(workers))
    }

    /// Takes the job a worker runs next, the oldest of the most urgent lane, if it may take
    /// one.
    fn take(&mut self, workers: usize) -> Option<Taken> {
        if let Some(task) = self.selected.pop_front().or_else(|| self.shown.pop_front()) {
            return Some(Taken {
                task,
                background: false,
            });
        }
        if !self.background_fits(workers) {
            return None;
        }
        let task = self.background.pop_front()?;
        Some(Taken {
            task,
            background: true,
        })
    }
}

/// Runs `task`, which loses its outcome if it panics, as a job on a thread of its own would,
/// but never its worker.
fn run(task: Task) {
    if std::panic::catch_unwind(AssertUnwindSafe(task)).is_err() {
        tracing::error!("a background job panicked");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Sender};

    use super::*;

    /// A job that reports `name` on `sender` once it runs.
    fn reports(sender: &Sender<&'static str>, name: &'static str) -> impl FnOnce() + Send + use<> {
        let sender = sender.clone();
        move || sender.send(name).unwrap()
    }

    /// Jobs that wait for a worker run the selected first, then the shown, then the
    /// background, whatever order they were queued in.
    #[test]
    fn the_selected_runs_before_queued_background_work() {
        let pool = Pool::new(2);
        // Both workers are busy, so the three jobs queue.
        let first = pool.hold(Lane::Selected);
        let second = pool.hold(Lane::Selected);
        let (sender, ran) = mpsc::channel();
        pool.start(Lane::Background, reports(&sender, "replay"));
        pool.start(Lane::Shown, reports(&sender, "texts"));
        pool.start(Lane::Selected, reports(&sender, "preview"));
        // One worker frees up and runs them one after another, most urgent first.
        drop(first);
        let order: Vec<_> = (0..3)
            .map(|_| ran.recv_timeout(PATIENCE).unwrap())
            .collect();
        assert_eq!(order, ["preview", "texts", "replay"]);
        drop(second);
    }

    /// Background work never takes the last worker: with one running, a second waits, while
    /// the selected file's job runs at once on the worker kept for it.
    #[test]
    fn background_work_leaves_a_worker_for_the_rest() {
        let pool = Pool::new(2);
        let replay = pool.hold(Lane::Background);
        let (lookup, lookups) = mpsc::channel();
        pool.start(Lane::Background, reports(&lookup, "lookup"));
        let (sender, ran) = mpsc::channel();
        pool.start(Lane::Selected, reports(&sender, "preview"));
        assert_eq!(ran.recv_timeout(PATIENCE).unwrap(), "preview");
        // The second background job cannot have started: only the first's end lets it.
        assert!(lookups.try_recv().is_err());
        drop(replay);
        assert_eq!(lookups.recv_timeout(PATIENCE).unwrap(), "lookup");
    }

    /// A job on a thread of its own, such as a save, runs while every worker is busy.
    #[test]
    fn a_job_of_its_own_never_waits_for_a_worker() {
        let pool = Pool::new(2);
        let first = pool.hold(Lane::Selected);
        let second = pool.hold(Lane::Selected);
        let (sender, ran) = mpsc::channel();
        pool.start(Lane::Own, reports(&sender, "save"));
        assert_eq!(ran.recv_timeout(PATIENCE).unwrap(), "save");
        drop((first, second));
    }

    /// A background job that panics gives its place back, so the next one runs.
    #[test]
    fn a_panicking_job_gives_its_worker_back() {
        let pool = Pool::new(2);
        pool.start(Lane::Background, || panic!("on purpose"));
        let (sender, ran) = mpsc::channel();
        pool.start(Lane::Background, reports(&sender, "next"));
        assert_eq!(ran.recv_timeout(PATIENCE).unwrap(), "next");
    }
}
