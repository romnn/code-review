//! An in-process job scheduler: priority queues, cancellation, retries with backoff,
//! worker slot bookkeeping and a small set of counters.
//!
//! The scheduler starts no threads. A host submits jobs, asks for work with
//! [`Scheduler::dispatch`], reports outcomes with [`Scheduler::complete`] and
//! [`Scheduler::fail`], and calls [`Scheduler::tick`] so that retries whose
//! delay has passed become ready again. [`pump`] does all of that in one call.

use backoff::{Backoff, DEFAULT_BASE_MS, DEFAULT_CAP_MS};
use config::SchedulerConfig;
use queue::{PriorityQueue, QueueEntry};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::str::FromStr;
use worker::WorkerPool;

pub use config::parse_config;

/// Retry delays that grow exponentially, with jitter, up to a cap.
pub mod backoff {
    use std::time::Duration;

    /// The delay ceiling before the first retry, in milliseconds.
    pub const DEFAULT_BASE_MS: u64 = 250;

    /// The longest delay a schedule produces, in milliseconds.
    pub const DEFAULT_CAP_MS: u64 = 60_000;

    /// A xorshift generator that spreads retries apart without depending on `rand`.
    #[derive(Debug, Clone)]
    pub struct Jitter {
        state: u64,
    }

    impl Jitter {
        /// Creates a generator from `seed`. Zero is replaced, because xorshift
        /// would otherwise stay at zero forever.
        pub fn new(seed: u64) -> Self {
            let state = if seed == 0 {
                0x2545_F491_4F6C_DD1D
            } else {
                seed
            };
            Jitter { state }
        }

        /// Returns the next pseudo-random value.
        pub fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.state = x;
            x
        }
    }

    /// Exponential backoff with full jitter, capped at a fixed maximum.
    /// Each delay is drawn from zero up to the current ceiling.
    #[derive(Debug, Clone)]
    pub struct Backoff {
        base_ms: u64,
        cap_ms: u64,
        attempt: u32,
        jitter: Jitter,
    }

    impl Backoff {
        /// Creates a schedule that starts at `base_ms`, capped at `cap_ms`.
        /// A cap below `base_ms` is raised to `base_ms`.
        pub fn new(base_ms: u64, cap_ms: u64, seed: u64) -> Self {
            Backoff {
                base_ms,
                cap_ms: cap_ms.max(base_ms),
                attempt: 0,
                jitter: Jitter::new(seed),
            }
        }

        /// Returns the delay before the next attempt and advances the schedule.
        pub fn next_delay(&mut self) -> Duration {
            let ceiling = self.ceiling_ms();
            self.attempt = self.attempt.saturating_add(1);
            // A ceiling of one millisecond or less leaves no room to spread retries.
            if ceiling <= 1 {
                return Duration::ZERO;
            }
            Duration::from_millis(self.jitter.next_u64() % ceiling)
        }

        /// Returns the delay ceiling for the next attempt, before jitter.
        pub fn ceiling_ms(&self) -> u64 {
            let shift = self.attempt.min(20);
            self.base_ms.saturating_mul(1 << shift).min(self.cap_ms)
        }

        /// Starts the schedule over, for example after a success.
        pub fn reset(&mut self) {
            self.attempt = 0;
        }
    }
}

/// Reads the scheduler's settings from `key = value` text.
pub mod config {
    use super::backoff::{DEFAULT_BASE_MS, DEFAULT_CAP_MS};
    use super::{line_error, SchedulerError};
    use std::collections::BTreeMap;
    use std::str::FromStr;

    /// Settings the scheduler reads at start-up.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SchedulerConfig {
        /// Jobs that may run at the same time, across all queues.
        pub worker_slots: usize,
        /// Attempts a job gets before it is marked as failed.
        /// Counts the first run, so 1 means no retries.
        pub max_attempts: u32,
        /// The first retry delay ceiling, in milliseconds.
        pub retry_base_ms: u64,
        /// The longest retry delay, in milliseconds.
        pub retry_cap_ms: u64,
        /// Jobs that may wait in the ready queue before submissions are refused.
        pub queue_capacity: usize,
        /// Per-queue limits on how many of a queue's jobs may run at once.
        pub queue_limits: BTreeMap<String, usize>,
    }

    impl Default for SchedulerConfig {
        fn default() -> Self {
            SchedulerConfig {
                worker_slots: 4,
                max_attempts: 3,
                retry_base_ms: DEFAULT_BASE_MS,
                retry_cap_ms: DEFAULT_CAP_MS,
                queue_capacity: 1024,
                queue_limits: BTreeMap::new(),
            }
        }
    }

    /// Parses `key = value` lines into a [`SchedulerConfig`].
    ///
    /// Blank lines and lines that start with `#` are ignored. An unknown key is
    /// an error, so a typo never silently falls back to a default.
    /// Values are parsed as unsigned integers.
    pub fn parse_config(text: &str) -> Result<SchedulerConfig, SchedulerError> {
        let mut config = SchedulerConfig::default();
        for (index, raw) in text.lines().enumerate() {
            let line_no = index + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                let reason = "expected a `key = value` setting";
                return Err(line_error(line_no, reason));
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "worker_slots" => {
                    config.worker_slots = parse_value(line_no, value)?;
                    if config.worker_slots == 0 {
                        return Err(line_error(line_no, "worker_slots must be at least 1"));
                    }
                }
                "max_attempts" => config.max_attempts = parse_value(line_no, value)?,
                "retry_base_ms" => config.retry_base_ms = parse_value(line_no, value)?,
                "retry_cap_ms" => config.retry_cap_ms = parse_value(line_no, value)?,
                "queue_capacity" => config.queue_capacity = parse_value(line_no, value)?,
                key if key.starts_with("queue_limit.") => {
                    let queue = key.trim_start_matches("queue_limit.");
                    let limit = parse_value(line_no, value)?;
                    config.queue_limits.insert(queue.to_string(), limit);
                }
                other => return Err(line_error(line_no, format!("unknown key `{other}`"))),
            }
        }
        Ok(config)
    }

    /// Parses one value, naming the line in the error if it is not a number.
    fn parse_value<T>(line: usize, value: &str) -> Result<T, SchedulerError>
    where
        T: FromStr,
    {
        value
            .parse()
            .map_err(|_| line_error(line, format!("`{value}` is not a valid number")))
    }
}

/// Builds a configuration error for `line`.
fn line_error(line: usize, reason: impl Into<String>) -> SchedulerError {
    SchedulerError::Config {
        line,
        reason: reason.into(),
    }
}

/// Everything the scheduler can refuse or fail to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerError {
    /// No job with this id is known to the scheduler.
    UnknownJob(JobId),
    /// The job was cancelled and will not run again.
    Cancelled(JobId),
    /// The job's state does not accept the event.
    InvalidTransition {
        /// The job that was asked to change.
        job: JobId,
        /// The state the job was in.
        from: JobState,
        /// The event that was refused.
        event: Event,
    },
    /// A line of the configuration text is malformed.
    Config {
        /// The 1-based line number.
        line: usize,
        /// What is wrong with the line.
        reason: String,
    },
    /// A job names a handler that was never registered.
    NoHandler(String),
    /// The ready queue is full, at its configured capacity.
    QueueFull {
        /// The queue the refused job was submitted to.
        queue: String,
        /// The number of jobs the ready queue holds at most.
        capacity: usize,
    },
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedulerError::UnknownJob(id) => write!(f, "no job with id {id}"),
            SchedulerError::Cancelled(id) => write!(f, "job {id} was cancelled"),
            SchedulerError::InvalidTransition { job, from, event } => {
                write!(f, "job {job} is {from:?} and does not accept {event:?}")
            }
            SchedulerError::Config { line, reason } => {
                write!(f, "config line {line}: {reason}")
            }
            SchedulerError::NoHandler(name) => write!(f, "no handler named `{name}`"),
            SchedulerError::QueueFull { queue, capacity } => {
                write!(f, "queue `{queue}` is full")?;
                write!(f, ": capacity is {capacity} jobs")
            }
        }
    }
}

impl std::error::Error for SchedulerError {}

/// Identifies a job for as long as the scheduler keeps track of it.
/// Ids are never reused within one scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// How urgently a ready job should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Runs when nothing more urgent is waiting.
    Low,
    /// The default for jobs that do not choose one.
    Normal,
    /// Runs before normal jobs.
    High,
    /// Runs before everything else.
    Critical,
}

impl Priority {
    /// The weight of this priority in the backlog report.
    pub fn weight(self) -> u32 {
        match self {
            Priority::Low => 1,
            Priority::Normal => 2,
            Priority::High => 5,
            Priority::Critical => 8,
        }
    }
}

impl FromStr for Priority {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Ok(Priority::Low),
            "normal" => Ok(Priority::Normal),
            "high" => Ok(Priority::High),
            "critical" => Ok(Priority::Critical),
            other => Err(format!(
                "unknown priority `{other}`; try low, normal, high or critical"
            )),
        }
    }
}

/// Where a job is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for its first worker slot.
    Queued,
    /// Holding a worker slot; `attempt` counts from 1.
    Running { attempt: u32 },
    /// Failed, and waiting until `not_before` (in milliseconds) to run `attempt`.
    Retrying { attempt: u32, not_before: u64 },
    /// Finished successfully.
    Succeeded,
    /// Failed on its last attempt, with the handler's message.
    Failed { reason: String },
    /// Cancelled by its owner before it could finish.
    Cancelled,
}

impl JobState {
    /// Returns `true` once the job will never run again.
    pub fn is_terminal(&self) -> bool {
        matches!(self, JobState::Succeeded | JobState::Failed { .. })
    }
}

/// Something that happens to a job, and what [`transition`] does about it.
/// Events are the only way a job changes state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The job was given a worker slot.
    Started,
    /// The handler returned successfully.
    Finished,
    /// The handler returned an error with this message.
    Errored(String),
    /// The job was cancelled by its owner.
    Cancel,
}

/// Returns the state a job moves to when `event` happens in state `from`.
///
/// `retry_at` is the time, in milliseconds, at which a retry becomes ready. It
/// only matters when an error leaves attempts to spare. Returns `None` if the
/// state does not accept the event.
pub fn transition(
    from: &JobState,
    event: &Event,
    max_attempts: u32,
    retry_at: u64,
) -> Option<JobState> {
    // Cancellation is accepted from every state that can still run.
    let next = match (from, event) {
        (JobState::Queued, Event::Started) => JobState::Running { attempt: 1 },
        (JobState::Retrying { attempt, .. }, Event::Started) => {
            JobState::Running { attempt: *attempt }
        }
        (JobState::Running { .. }, Event::Finished) => JobState::Succeeded,
        (JobState::Running { attempt }, Event::Errored(reason)) if *attempt >= max_attempts => {
            JobState::Failed {
                reason: reason.clone(),
            }
        }
        (JobState::Running { attempt }, Event::Errored(_)) => JobState::Retrying {
            attempt: attempt + 1,
            not_before: retry_at,
        },
        (
            JobState::Queued | JobState::Retrying { .. } | JobState::Running { .. },
            Event::Cancel,
        ) => JobState::Cancelled,
        _ => return None,
    };
    Some(next)
}

/// A unit of work: what to run, which queue it waits in, and how far it has got.
#[derive(Debug, Clone)]
pub struct Job {
    /// The id the scheduler gave the job on submission.
    pub id: JobId,
    /// The queue the job was submitted to, used for reports and limits.
    pub queue: String,
    /// The name of the handler that runs the job.
    pub handler: String,
    /// How urgently the job runs once it is ready.
    pub priority: Priority,
    /// Where the job is now.
    pub state: JobState,
    /// Attempts the job may make before it is given up.
    /// The scheduler caps this at its configured maximum.
    pub max_attempts: u32,
    /// The time, in milliseconds, at which the job was submitted.
    pub submitted_at: u64,
    /// The delays between this job's retries.
    pub backoff: Backoff,
}

impl Job {
    /// Creates a job for `handler` in `queue`, at normal priority with two attempts.
    pub fn new(queue: impl Into<String>, handler: impl Into<String>) -> Self {
        Job {
            id: JobId(0),
            queue: queue.into(),
            handler: handler.into(),
            priority: Priority::Normal,
            state: JobState::Queued,
            max_attempts: 2,
            submitted_at: 0,
            backoff: Backoff::new(DEFAULT_BASE_MS, DEFAULT_CAP_MS, 0),
        }
    }

    /// Sets how urgently the job runs.
    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// Sets how many attempts the job may make. At least one is always allowed.
    pub fn with_max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }
}

/// A source of the current time, in milliseconds since an arbitrary epoch.
pub trait Clock {
    /// Returns the current time in milliseconds.
    fn now_ms(&self) -> u64;
}

/// Reads the operating system's wall clock, falling back to zero.
/// A clock set before the epoch reads as zero.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        since_epoch.as_millis() as u64
    }
}

/// A clock that moves only when a test or a simulation tells it to.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: std::cell::Cell<u64>,
}

impl ManualClock {
    /// Moves the clock forward by `millis`.
    pub fn advance(&self, millis: u64) {
        self.now.set(self.now.get().saturating_add(millis));
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
}

/// The ready queue: jobs that may run now, the most urgent first.
/// Jobs that wait for a retry live in the scheduler, not here.
pub mod queue {
    use super::{JobId, Priority};
    use std::cmp::Ordering;
    use std::collections::BTreeSet;

    /// One ready job, ordered by priority and then by submission sequence.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct QueueEntry {
        /// The job's priority.
        pub priority: Priority,
        /// The order in which the job was queued, oldest first.
        pub seq: u64,
        /// The job itself.
        pub id: JobId,
    }

    impl Ord for QueueEntry {
        fn cmp(&self, other: &Self) -> Ordering {
            // Among equal priorities, the earlier submission sorts higher.
            self.priority
                .cmp(&other.priority)
                .then_with(|| other.seq.cmp(&self.seq))
                .then_with(|| self.id.cmp(&other.id))
        }
    }

    impl PartialOrd for QueueEntry {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    /// Ready jobs, ordered by priority and then by submission order.
    #[derive(Debug, Default)]
    pub struct PriorityQueue {
        entries: BTreeSet<QueueEntry>,
    }

    impl PriorityQueue {
        /// Creates an empty queue.
        pub fn new() -> Self {
            Self::default()
        }

        /// Adds an entry. Returns `false` if an identical entry was already queued.
        pub fn push(&mut self, entry: QueueEntry) -> bool {
            self.entries.insert(entry)
        }

        /// Removes and returns the most urgent entry.
        pub fn pop_highest(&mut self) -> Option<QueueEntry> {
            self.entries.pop_last()
        }

        /// Removes the entry for `id`, returning `true` if it was queued.
        pub fn remove(&mut self, id: JobId) -> bool {
            let hit = self.entries.iter().find(|entry| entry.id == id).copied();
            match hit {
                Some(entry) => self.entries.remove(&entry),
                None => false,
            }
        }

        /// Iterates over the entries, most urgent first.
        pub fn iter_highest_first(&self) -> impl Iterator<Item = &QueueEntry> + '_ {
            self.entries.iter().rev()
        }

        /// Returns how many entries are queued.
        pub fn len(&self) -> usize {
            self.entries.len()
        }

        /// Returns `true` if nothing is queued.
        pub fn is_empty(&self) -> bool {
            self.entries.is_empty()
        }

        /// Returns `true` if an entry for `id` is queued.
        pub fn contains(&self, id: JobId) -> bool {
            self.entries.iter().any(|entry| entry.id == id)
        }
    }
}

/// Worker slot bookkeeping: which job holds which slot, and since when.
pub mod worker {
    use super::JobId;
    use std::collections::HashMap;

    /// What one worker slot is doing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SlotState {
        /// The slot is free for the next job.
        Idle,
        /// The slot is running this job.
        Busy(JobId),
    }

    /// The scheduler's fixed set of worker slots.
    #[derive(Debug)]
    pub struct WorkerPool {
        slots: Vec<SlotState>,
        busy_since: HashMap<JobId, u64>,
    }

    impl WorkerPool {
        /// Creates a pool with `size` idle slots.
        pub fn new(size: usize) -> Self {
            WorkerPool {
                slots: vec![SlotState::Idle; size],
                busy_since: HashMap::new(),
            }
        }

        /// Returns the number of slots that are free.
        pub fn free_slots(&self) -> usize {
            self.slots
                .iter()
                .filter(|slot| **slot == SlotState::Idle)
                .count()
        }

        /// Gives a free slot to `job` at time `now`, or returns `false` if all are busy.
        pub fn acquire(&mut self, job: JobId, now: u64) -> bool {
            let Some(slot) = self.slots.iter_mut().find(|s| **s == SlotState::Idle) else {
                return false;
            };
            *slot = SlotState::Busy(job);
            self.busy_since.insert(job, now);
            true
        }

        /// Frees the slot held by `job`, returning how long the job was busy.
        /// Returns `None` if no slot holds `job`.
        pub fn release(&mut self, job: JobId, now: u64) -> Option<u64> {
            let slot = self
                .slots
                .iter_mut()
                .find(|slot| **slot == SlotState::Busy(job))?;
            *slot = SlotState::Idle;
            let since = self.busy_since.remove(&job)?;
            Some(now.saturating_sub(since))
        }

        /// Returns the jobs that hold a slot, in slot order, for reports.
        pub fn running(&self) -> Vec<JobId> {
            self.slots
                .iter()
                .filter_map(|slot| match slot {
                    SlotState::Busy(job) => Some(*job),
                    SlotState::Idle => None,
                })
                .collect()
        }
    }

    /// Describes how full the worker slots are, for the status line.
    fn occupancy_line(busy: usize, slots: usize, waiting: usize) -> String {
        let free = slots.saturating_sub(busy);
        let share = if slots == 0 { 0 } else { busy * 100 / slots };
        let line =
            format!("{busy} of {slots} worker slots busy ({share}% in use), {free} still free");
        let waiting = if waiting == 0 {
            String::from("no jobs waiting")
        } else if waiting == 1 {
            String::from("one job waiting")
        } else {
            format!("{waiting} jobs waiting")
        };
        format!("{line}, {waiting}")
    }
}

/// Something that happened to a job, for [`Metrics::record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A job was accepted into the ready queue.
    Submitted,
    /// A submission was refused because the ready queue was full.
    Rejected,
    /// A job was given a worker slot.
    Started,
    /// A job finished after holding a slot for `busy_ms`.
    Succeeded { busy_ms: u64 },
    /// A job failed and was scheduled for another attempt.
    Retrying { delay_ms: u64 },
    /// A job failed on its last attempt.
    GaveUp { attempts: u32 },
    /// A job was cancelled before it could finish.
    Cancelled,
}

/// Counters for what the scheduler has done.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Metrics {
    /// Jobs accepted by `submit`.
    pub submitted: u64,
    /// Submissions refused because the queue was full.
    pub rejected: u64,
    /// Worker slots handed to jobs.
    pub started: u64,
    /// Jobs that finished successfully.
    pub succeeded: u64,
    /// Failures that were retried.
    pub retried: u64,
    /// Jobs that failed for good.
    pub failed: u64,
    /// Total milliseconds that successful jobs held a slot.
    pub busy_ms: u64,
    /// Total milliseconds of retry delay that were scheduled.
    pub retry_delay_ms: u64,
    /// Total attempts spent on jobs that gave up.
    pub attempts_exhausted: u64,
    /// Jobs cancelled before they could finish.
    pub cancelled: u64,
}

impl Metrics {
    /// Adds one outcome to the counters.
    pub fn record(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Submitted => self.submitted += 1,
            // Each arm bumps the counter for its outcome and nothing else.
            Outcome::Rejected => self.rejected += 1,
            Outcome::Started => self.started += 1,
            Outcome::Succeeded { busy_ms } => {
                self.succeeded += 1;
                self.busy_ms += busy_ms;
            }
            Outcome::Retrying { delay_ms } => {
                self.retried += 1;
                self.retry_delay_ms += delay_ms;
            }
            Outcome::Cancelled => self.cancelled += 1,
            Outcome::GaveUp { attempts } => {
                self.failed += 1;
                self.attempts_exhausted += u64::from(*attempts);
            }
        }
    }

    /// The share of started jobs that succeeded, as a fraction.
    /// It is zero before any job has started.
    pub fn success_ratio(&self) -> f64 {
        if self.started == 0 {
            return 0.0;
        }
        self.succeeded as f64 / self.started as f64
    }
}

impl fmt::Display for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "submitted={} started={} succeeded={} retried={} failed={} refused={}",
            self.submitted, self.started, self.succeeded, self.retried, self.failed, self.rejected
        )
    }
}

/// Runs the work a job describes. A host implements one handler per kind of job.
pub trait JobHandler {
    /// Runs the job. An error marks the attempt as failed, and it may be retried.
    fn run(&mut self, job: &Job) -> Result<(), String>;
}

/// Maps handler names, as jobs refer to them, to the handlers that run them.
#[derive(Default)]
pub struct HandlerRegistry {
    handlers: BTreeMap<String, Box<dyn JobHandler>>,
}

impl HandlerRegistry {
    /// Registers `handler` under `name`, replacing any handler registered before.
    pub fn register(&mut self, name: impl Into<String>, handler: Box<dyn JobHandler>) {
        self.handlers.insert(name.into(), handler);
    }

    /// Looks up the handler called `name`.
    pub fn get_mut(&mut self, name: &str) -> Result<&mut dyn JobHandler, SchedulerError> {
        match self.handlers.get_mut(name) {
            Some(handler) => Ok(handler.as_mut()),
            None => Err(SchedulerError::NoHandler(name.to_string())),
        }
    }
}

/// Owns every job, the queues they wait in, and the worker pool they run on.
#[derive(Debug)]
pub struct Scheduler {
    config: SchedulerConfig,
    jobs: BTreeMap<JobId, Job>,
    ready: PriorityQueue,
    delayed: BTreeSet<(u64, JobId)>,
    pool: WorkerPool,
    metrics: Metrics,
    next_id: u64,
    next_seq: u64,
}

impl Scheduler {
    /// Creates a scheduler with `config` and no jobs.
    pub fn new(config: SchedulerConfig) -> Self {
        let pool = WorkerPool::new(config.worker_slots);
        Scheduler {
            config,
            jobs: BTreeMap::new(),
            ready: PriorityQueue::new(),
            delayed: BTreeSet::new(),
            pool,
            metrics: Metrics::default(),
            next_id: 0,
            next_seq: 0,
        }
    }

    /// Accepts `job` into the ready queue at time `now` and returns its id.
    ///
    /// The job's attempt limit is capped at the configured maximum. Fails with
    /// [`SchedulerError::QueueFull`] when the ready queue is at capacity.
    pub fn submit(&mut self, mut job: Job, now: u64) -> Result<JobId, SchedulerError> {
        if self.pending() >= self.config.queue_capacity {
            self.metrics.record(&Outcome::Rejected);
            return Err(SchedulerError::QueueFull {
                queue: job.queue,
                capacity: self.config.queue_capacity,
            });
        }
        self.next_id += 1;
        let id = JobId(self.next_id);
        job.id = id;
        job.submitted_at = now;
        job.max_attempts = job.max_attempts.min(self.config.max_attempts).max(1);
        let seed = now;
        job.backoff = Backoff::new(self.config.retry_base_ms, self.config.retry_cap_ms, seed);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.ready.push(QueueEntry {
            priority: job.priority,
            seq,
            id,
        });
        self.jobs.insert(id, job);
        self.metrics.record(&Outcome::Submitted);
        Ok(id)
    }

    /// Gives free worker slots to the most urgent ready jobs at time `now`.
    ///
    /// A job whose queue already runs its configured limit waits, and does not
    /// hold back the jobs behind it. Returns the ids of the jobs that started,
    /// most urgent first.
    pub fn dispatch(&mut self, now: u64) -> Vec<JobId> {
        let mut started = Vec::new();
        let mut deferred = Vec::new();
        let mut running: HashMap<String, usize> = HashMap::new();
        for id in self.pool.running() {
            if let Some(job) = self.jobs.get(&id) {
                *running.entry(job.queue.clone()).or_default() += 1;
            }
        }
        while self.pool.free_slots() > 0 {
            let Some(entry) = self.ready.pop_highest() else {
                break;
            };
            let Some(job) = self.jobs.get_mut(&entry.id) else {
                continue;
            };
            let used = running.get(&job.queue).copied().unwrap_or(0);
            let limit = self.config.queue_limits.get(&job.queue).copied();
            if limit.is_some_and(|limit| used >= limit) {
                deferred.push(entry);
                continue;
            }
            let next = transition(&job.state, &Event::Started, job.max_attempts, now);
            let Some(next) = next else {
                continue;
            };
            job.state = next;
            *running.entry(job.queue.clone()).or_default() += 1;
            self.pool.acquire(entry.id, now);
            self.metrics.record(&Outcome::Started);
            started.push(entry.id);
        }
        for entry in deferred {
            self.ready.push(entry);
        }
        started
    }

    /// Moves job `id` through `event` at time `now` and returns its new state.
    fn apply(&mut self, id: JobId, event: Event, now: u64) -> Result<JobState, SchedulerError> {
        let job = self
            .jobs
            .get_mut(&id)
            .ok_or(SchedulerError::UnknownJob(id))?;
        if job.state == JobState::Cancelled && event != Event::Cancel {
            return Err(SchedulerError::Cancelled(id));
        }
        let retry_at = match event {
            Event::Errored(_) => now + job.backoff.next_delay().as_millis() as u64,
            _ => now,
        };
        let next = transition(&job.state, &event, job.max_attempts, retry_at);
        let Some(next) = next else {
            return Err(SchedulerError::InvalidTransition {
                job: id,
                from: job.state.clone(),
                event,
            });
        };
        job.state = next.clone();
        Ok(next)
    }

    /// Records that job `id` finished without error at time `now`.
    /// The slot the job held is released, and its backoff starts over.
    pub fn complete(&mut self, id: JobId, now: u64) -> Result<(), SchedulerError> {
        self.apply(id, Event::Finished, now)?;
        let busy_ms = self.pool.release(id, now).unwrap_or_default();
        if let Some(job) = self.jobs.get_mut(&id) {
            job.backoff.reset();
        }
        self.metrics.record(&Outcome::Succeeded { busy_ms });
        Ok(())
    }

    /// Records that job `id` failed at time `now`, with `reason`.
    /// The slot the job held is released either way.
    ///
    /// The job waits out a backoff delay and is retried if it has attempts left.
    /// Otherwise it fails for good.
    pub fn fail(&mut self, id: JobId, reason: String, now: u64) -> Result<(), SchedulerError> {
        let next = self.apply(id, Event::Errored(reason), now)?;
        self.pool.release(id, now);
        match next {
            JobState::Retrying { not_before, .. } => {
                self.delayed.insert((not_before, id));
                let wait_ms = not_before.saturating_sub(now);
                self.metrics
                    .record(&Outcome::Retrying { delay_ms: wait_ms });
            }
            JobState::Failed { .. } => {
                let attempts = self.jobs.get(&id).map_or(0, |job| job.max_attempts);
                self.metrics.record(&Outcome::GaveUp { attempts });
            }
            other => unreachable!("a failed attempt cannot leave a job {other:?}"),
        }
        Ok(())
    }

    /// Cancels job `id` at time `now`, whatever state it is in.
    ///
    /// A queued job leaves the ready queue and a retrying job leaves the retry
    /// schedule, so neither will run. A running job gives up its worker slot,
    /// and the function returns `true`: the host must then stop the handler,
    /// because the scheduler cannot interrupt it.
    ///
    /// # Errors
    ///
    /// Cancelling a job that was already cancelled returns
    /// [`SchedulerError::Cancelled`]. A job that succeeded or failed for good
    /// cannot be cancelled, and returns [`SchedulerError::InvalidTransition`].
    /// Neither error changes anything.
    pub fn cancel(&mut self, id: JobId, now: u64) -> Result<bool, SchedulerError> {
        let previous = self
            .job(id)
            .map(|job| job.state.clone())
            .ok_or(SchedulerError::UnknownJob(id))?;
        match &previous {
            JobState::Cancelled => return Err(SchedulerError::Cancelled(id)),
            JobState::Succeeded | JobState::Failed { .. } => {
                return Err(SchedulerError::InvalidTransition {
                    job: id,
                    from: previous.clone(),
                    event: Event::Cancel,
                });
            }
            _ => {}
        }
        // Validation above guarantees the job is queued, retrying or running.
        self.apply(id, Event::Cancel, now)?;
        let was_running = match previous {
            JobState::Queued => {
                // A queued job has never held a slot, so only the ready queue changes.
                debug_assert!(self.ready.contains(id));
                self.ready.remove(id);
                false
            }
            JobState::Retrying { not_before, .. } => {
                // Retries are keyed by their wake-up time, so the key finds them.
                self.delayed.remove(&(not_before, id));
                false
            }
            JobState::Running { .. } => {
                // The slot returns to the pool; the host stops the handler.
                self.pool.release(id, now);
                true
            }
            other => unreachable!("cancel was accepted in state {other:?}"),
        };
        if let Some(job) = self.jobs.get_mut(&id) {
            job.backoff.reset();
        }
        self.metrics.record(&Outcome::Cancelled);
        Ok(was_running)
    }

    /// Makes every retry whose delay has passed at time `now` ready again.
    ///
    /// Returns how many jobs became ready.
    pub fn tick(&mut self, now: u64) -> usize {
        let due: Vec<(u64, JobId)> = self
            .delayed
            .iter()
            .take_while(|(not_before, _)| *not_before <= now)
            .copied()
            .collect();
        // A retry that is due re-enters the ready queue at the back of its priority.
        for &(not_before, id) in &due {
            self.delayed.remove(&(not_before, id));
            let Some(job) = self.jobs.get(&id) else {
                continue;
            };
            let seq = self.next_seq;
            self.next_seq += 1;
            self.ready.push(QueueEntry {
                priority: job.priority,
                seq,
                id,
            });
        }
        due.len()
    }

    /// Returns how many jobs wait for a worker slot or for a retry.
    pub fn pending(&self) -> usize {
        self.ready.len() + self.delayed.len()
    }

    /// Lists each queue's concurrency limit, for the status line.
    pub fn limits_line(&self) -> String {
        let limits: Vec<String> = self
            .config
            .queue_limits
            .iter()
            .map(|(queue, limit)| format!("{queue}<={limit}"))
            .collect();
        limits.join(",")
    }

    /// Returns the job with `id`, if the scheduler still holds it.
    pub fn job(&self, id: JobId) -> Option<&Job> {
        self.jobs.get(&id)
    }

    /// Returns the counters the scheduler keeps.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Returns the ids of the jobs that hold a worker slot.
    pub fn running(&self) -> Vec<JobId> {
        self.pool.running()
    }

    /// Summarizes the scheduler in one line, for logs.
    pub fn status_line(&self) -> String {
        format!(
            "running={} pending={} {} | {}",
            self.pool.running().len(),
            self.pending(),
            self.metrics,
            self.limits_line(),
        )
    }
}

/// Runs one round of work at the clock's time.
///
/// Retries that are due become ready, free slots take the most urgent jobs,
/// and each started job runs its handler to completion. A handler error counts as
/// a failed attempt and does not stop the round. Returns how many jobs ran.
pub fn pump<C: Clock>(
    scheduler: &mut Scheduler,
    registry: &mut HandlerRegistry,
    clock: &C,
) -> Result<usize, SchedulerError> {
    let now = clock.now_ms();
    scheduler.tick(now);
    // Due retries become ready first, so they can start in this round.
    let started = scheduler.dispatch(now);
    for id in &started {
        let job = scheduler
            .job(*id)
            .cloned()
            .ok_or(SchedulerError::UnknownJob(*id))?;
        let outcome = registry.get_mut(&job.handler)?.run(&job);
        let now = clock.now_ms();
        // A handler error is a failed attempt, not a failed round.
        match outcome {
            Ok(()) => scheduler.complete(*id, now)?,
            Err(reason) => scheduler.fail(*id, reason, now)?,
        }
    }
    Ok(started.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler_with(slots: usize) -> Scheduler {
        let config = SchedulerConfig {
            worker_slots: slots,
            ..SchedulerConfig::default()
        };
        Scheduler::new(config)
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let mut schedule = Backoff::new(100, 1_000, 7);
        let ceilings: Vec<u64> = (0..5)
            .map(|_| {
                let ceiling = schedule.ceiling_ms();
                schedule.next_delay();
                ceiling
            })
            .collect();
        assert_eq!(ceilings, vec![100, 200, 400, 800, 1_000]);
    }

    #[test]
    fn jitter_stays_under_the_ceiling() {
        let mut schedule = Backoff::new(50, 4_000, 99);
        for _ in 0..200 {
            let ceiling = schedule.ceiling_ms();
            let delay = schedule.next_delay().as_millis() as u64;
            assert!(delay < ceiling, "{delay} is not below {ceiling}");
        }
    }

    #[test]
    fn parse_config_reads_known_keys() {
        let text = "# pool settings
worker_slots = 8
max_attempts = 5

queue_capacity = 64
";
        let config = parse_config(text).expect("valid config");
        assert_eq!(config.worker_slots, 8);
        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.queue_capacity, 64);
        assert_eq!(config.retry_cap_ms, backoff::DEFAULT_CAP_MS);
        assert_eq!(config.retry_base_ms, backoff::DEFAULT_BASE_MS);
    }

    #[test]
    fn parse_config_names_the_bad_line() {
        let error = parse_config(
            "worker_slots = 2
retry_cap_ms = soon
",
        )
        .unwrap_err();
        assert_eq!(
            error,
            SchedulerError::Config {
                line: 2,
                reason: "`soon` is not a valid number".to_string(),
            }
        );
    }

    #[test]
    fn priority_queue_orders_by_priority_then_submission() {
        let mut queue = PriorityQueue::new();
        // Same priority: the earlier submission comes out first.
        queue.push(QueueEntry {
            priority: Priority::Normal,
            seq: 0,
            id: JobId(1),
        });
        queue.push(QueueEntry {
            priority: Priority::High,
            seq: 1,
            id: JobId(2),
        });
        queue.push(QueueEntry {
            priority: Priority::Normal,
            seq: 2,
            id: JobId(3),
        });
        let order: Vec<u64> = std::iter::from_fn(|| queue.pop_highest())
            .map(|entry| entry.id.0)
            .collect();
        assert_eq!(order, vec![2, 1, 3]);
    }

    #[test]
    fn worker_pool_hands_out_and_reclaims_slots() {
        let mut pool = WorkerPool::new(2);
        assert!(pool.acquire(JobId(1), 0));
        assert!(pool.acquire(JobId(2), 0));
        assert!(!pool.acquire(JobId(3), 0));
        assert_eq!(pool.release(JobId(1), 55), Some(55));
        assert_eq!(pool.free_slots(), 1);
    }

    #[test]
    fn transition_refuses_finish_before_start() {
        let next = transition(&JobState::Queued, &Event::Finished, 3, 0);
        assert_eq!(next, None);
    }

    #[test]
    fn failed_job_is_retried_then_given_up() {
        let clock = ManualClock::default();
        let config = SchedulerConfig {
            max_attempts: 2,
            retry_base_ms: 10,
            retry_cap_ms: 10,
            ..SchedulerConfig::default()
        };
        // Two attempts: the first fails, and the retry fails for good.
        let mut scheduler = Scheduler::new(config);
        let id = scheduler
            .submit(Job::new("mail", "send"), clock.now_ms())
            .expect("queue has room");

        assert_eq!(scheduler.dispatch(clock.now_ms()), vec![id]);
        scheduler
            .fail(id, "smtp down".to_string(), clock.now_ms())
            .unwrap();

        clock.advance(10);
        assert_eq!(scheduler.tick(clock.now_ms()), 1);
        assert_eq!(scheduler.dispatch(clock.now_ms()), vec![id]);
        scheduler
            .fail(id, "smtp down".to_string(), clock.now_ms())
            .unwrap();
        let state = scheduler.job(id).map(|job| job.state.clone());
        assert!(matches!(state, Some(JobState::Failed { .. })));
        assert_eq!(scheduler.metrics().attempts_exhausted, 2);
        assert_eq!(scheduler.metrics().retried, 1);
    }

    #[test]
    fn pump_turns_handler_errors_into_retries() {
        struct AlwaysFails;

        impl JobHandler for AlwaysFails {
            fn run(&mut self, _job: &Job) -> Result<(), String> {
                Err("disk full".to_string())
            }
        }

        let clock = ManualClock::default();
        let mut scheduler = scheduler_with(1);
        let mut registry = HandlerRegistry::default();
        registry.register("flaky", Box::new(AlwaysFails));
        let id = scheduler
            .submit(Job::new("default", "flaky"), clock.now_ms())
            .unwrap();

        assert_eq!(pump(&mut scheduler, &mut registry, &clock).unwrap(), 1);
        let state = scheduler.job(id).map(|job| job.state.clone());
        assert!(matches!(state, Some(JobState::Retrying { attempt: 2, .. })));
    }

    #[test]
    fn cancelled_job_never_reaches_a_worker() {
        let clock = ManualClock::default();
        let mut scheduler = scheduler_with(1);
        let id = scheduler
            .submit(Job::new("default", "noop"), clock.now_ms())
            .unwrap();
        assert_eq!(scheduler.cancel(id, clock.now_ms()), Ok(false));
        assert!(scheduler.dispatch(clock.now_ms()).is_empty());
        let state = scheduler.job(id).map(|job| job.state.clone());
        assert_eq!(state, Some(JobState::Cancelled));
        assert_eq!(scheduler.metrics().cancelled, 1);
    }

    #[test]
    fn metrics_display_lists_every_counter() {
        let mut metrics = Metrics::default();
        metrics.record(&Outcome::Submitted);
        metrics.record(&Outcome::Started);
        metrics.record(&Outcome::Succeeded { busy_ms: 5 });
        let line = metrics.to_string();
        assert!(
            line.starts_with("submitted=1 started=1 succeeded=1 retried=0"),
            "{line}"
        );
    }
}
