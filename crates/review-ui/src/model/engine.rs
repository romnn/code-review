//! The engine handle: the review, the checkout's review that holds the trust in it, the watch
//! on the working tree and the language servers, which only background work can reach.
//!
//! [`Servers`] is the one authority on whether a language server may start or be used: see the
//! `trust` module.
//!
//! Everything it holds can block: the review runs git and reads the event log, the watch runs
//! git and joins threads as it ends, and the servers are processes.
//! The model holds one [`Engine`], which it can neither look into nor clone: all it can do with
//! it is queue a job that runs with its [`Parts`] in the background ([`Model::spawn_with`]),
//! whose outcome comes back as plain data, and hand it to the job that stops it
//! ([`Model::dispose_engine`]), which quitting waits for.
//! The wait for the working tree to change holds only the watch's [`Waiting`], which the model
//! can only hand on to the next wait ([`Model::wait_for_changes`]).
//! So no code on the UI thread compiles that blocks on the engine.
//!
//! A job holds the parts from when it is queued until it ends, and with them the review, whose
//! lease keeps every snapshot it reads, a capture no longer shown included, from being compacted
//! away; a capture holds the watch too.
//! So the last of them may let go anywhere, on the UI thread too, as when the model is dropped or
//! a job is dropped unrun: each then hands what blocks to a thread of its own.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use review_core::Review;

use super::buffer::Unanswered;
use super::effects::{Because, Completion, Job, Outcome, Subject};
use super::live::WaitedFor;
use super::trust::Trust;
use super::{Effect, Model};
use crate::lsp::{LanguageServer, Stopper};
use crate::watch::{Waiter, Watch};

/// The model's handle on the engine; see the module documentation.
pub(super) struct Engine {
    parts: Arc<Parts>,
}

/// What the engine holds, which only a job reaches.
pub(super) struct Parts {
    /// The review.
    pub(super) review: Review,
    /// The review of the checkout, which holds the trust to run tools in it, bound to the clone
    /// there when the review opened; `None` for a review whose code is not on disk (see
    /// [`Review::checkout_review`]).
    pub(super) checkout: Option<Review>,
    /// The watch on the working tree; a capture holds it too while it walks the tree again.
    pub(super) watch: Mutex<WatchSlot>,
    /// The running language servers.
    pub(super) servers: Servers,
}

/// Where the engine's watch on the working tree is.
///
/// The watch starts outside the lock and is installed under it, and it closes under the same
/// lock, for good, as the review ends in the window or the engine stops, so a watch that
/// finishes starting after that is dropped rather than kept.
#[derive(Default)]
pub(super) enum WatchSlot {
    /// No watch started yet, or none can.
    #[default]
    Unstarted,
    /// The watch that started.
    Started(Arc<Watch>),
    /// The review ended in the window or the engine stopped: no watch is ever kept again.
    Closed,
}

impl WatchSlot {
    /// The watch that started, while it is kept.
    pub(super) fn started(&self) -> Option<&Arc<Watch>> {
        match self {
            Self::Started(watch) => Some(watch),
            Self::Unstarted | Self::Closed => None,
        }
    }

    /// Closes the slot for good and returns the watch it kept, for the caller to drop outside
    /// the lock.
    fn close(&mut self) -> Option<Arc<Watch>> {
        match std::mem::replace(self, Self::Closed) {
            Self::Started(watch) => Some(watch),
            Self::Unstarted | Self::Closed => None,
        }
    }
}

/// What waits for the watch to see a change, handed from one wait to the next.
///
/// It holds the watch only weakly, so a wait never keeps the engine alive: once the watch is
/// dropped, the wait ends.
pub(super) struct Waiting(Waiter);

/// The language servers running in the checkout, with what decides whether they may run.
///
/// It holds one set of servers, which outlives the captures that show, so a server indexes the
/// checkout once, and its lock serializes the readings of trust, the decisions on trust and the
/// admission of lookups.
/// It is held only for those, never across a handshake or a request.
#[derive(Default)]
pub(super) struct Servers(Mutex<Set>);

/// The servers running in the checkout, by program.
#[derive(Default)]
struct Set {
    /// The first trust epoch a lookup is admitted in: the one after the last denial found here.
    floor: u64,
    running: HashMap<&'static str, Running>,
    /// Whether a revocation could not be recorded, which keeps trust denied here until a grant
    /// is recorded, whatever the stored grant says.
    withdrawn: bool,
}

/// A running language server, locked while a lookup uses it, with what stops it without that
/// lock.
///
/// It stops as it leaves the set, however it leaves, and its process is gone by the time it
/// left: a lookup still holding the server sees its wait fail rather than keeping it running
/// unlisted, where no revocation finds it.
struct Running {
    server: Arc<Mutex<LanguageServer>>,
    stopper: Stopper,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stopper.stop();
    }
}

impl Servers {
    /// Reads, with `read`, the trust in the checkout, as every snapshot that shows asks, in trust
    /// `epoch`; `current` is the window's trust epoch now, and `on_disk` whether the window
    /// shows the code the checkout holds rather than, say, an older revision.
    ///
    /// A reading whose epoch no longer counts changes nothing and reads nothing, since reading
    /// may wait for another writer of the review.
    /// Without the checkout's code on screen, every server stops, as no lookup needs one.
    /// Trust found denied stops every server before it is returned, and no lookup of the
    /// reading's epoch or before is admitted any more.
    pub(super) fn check(
        &self,
        epoch: u64,
        current: &AtomicU64,
        on_disk: bool,
        read: impl FnOnce() -> Trust,
    ) -> Trust {
        let mut set = self.0.lock();
        if current.load(Ordering::SeqCst) != epoch {
            return Trust::Denied;
        }
        if !on_disk {
            // Each server stops as it leaves the set.
            set.running.clear();
        }
        let trust = if set.withdrawn { Trust::Denied } else { read() };
        if !trust.granted() {
            set.deny(epoch);
        }
        trust
    }

    /// Reads, with `read`, the trust in the checkout while any server runs, as the window's
    /// watch on the grant does whatever shows; `current` is the window's trust epoch now.
    ///
    /// Trust found denied, as when another window revoked it or another clone took the
    /// checkout's path, stops every server before it is returned, and no lookup of the current
    /// epoch or before is admitted any more.
    /// Without a server running, it reads nothing and returns `None`.
    pub(super) fn guard(&self, current: &AtomicU64, read: impl FnOnce() -> Trust) -> Option<Trust> {
        let mut set = self.0.lock();
        if set.running.is_empty() {
            return None;
        }
        let trust = if set.withdrawn { Trust::Denied } else { read() };
        if !trust.granted() {
            set.deny(current.load(Ordering::SeqCst));
        }
        Some(trust)
    }

    /// Records a decision on trust with `record`, which returns the trust that applies after it.
    ///
    /// A revocation, as `revokes` says this is, stops every server, recorded or not and
    /// whatever grant still applies, and one that cannot be recorded keeps trust denied here
    /// until a grant is recorded.
    pub(super) fn decide<E>(
        &self,
        revokes: bool,
        record: impl FnOnce() -> Result<Trust, E>,
    ) -> Result<Trust, E> {
        let mut set = self.0.lock();
        let recorded = record();
        if revokes {
            set.running.clear();
            set.withdrawn |= recorded.is_err();
        } else if recorded.is_ok() {
            set.withdrawn = false;
        }
        recorded.map(|trust| if set.withdrawn { Trust::Denied } else { trust })
    }

    /// Hands a lookup of trust `epoch` the server `program`, launching it with `launch` when
    /// none runs, while the stored grant, which `trusted` reads, still applies; `current` is the
    /// window's trust epoch now.
    ///
    /// All under the lock, so a decision or a reading of trust either comes first and is seen
    /// here, or waits for the launch, which only spawns the process, and stops what it
    /// launched.
    /// A lookup whose epoch no longer counts is refused before the stored grant is read, which
    /// may wait for another writer of the review, and the epoch is asked for again after it,
    /// since the reviewer may revoke trust meanwhile.
    /// Trust found denied since the lookup's epoch, here or by a reading, refuses it as denied,
    /// and so does a stored grant that no longer applies, as in another clone, which stops
    /// every server.
    /// The server is in the set before the lock is released, so a revocation finds it.
    pub(super) fn admit(
        &self,
        epoch: u64,
        current: &AtomicU64,
        trusted: impl FnOnce() -> bool,
        program: &'static str,
        launch: impl FnOnce() -> Result<LanguageServer, String>,
    ) -> Result<Arc<Mutex<LanguageServer>>, Unanswered> {
        let lasts = || current.load(Ordering::SeqCst) == epoch;
        let mut set = self.0.lock();
        if !lasts() {
            return Err(Unanswered::Stale);
        }
        if epoch < set.floor || set.withdrawn || !trusted() {
            set.deny(epoch);
            return Err(Unanswered::Denied);
        }
        if !lasts() {
            return Err(Unanswered::Stale);
        }
        if let Some(running) = set.running.get(program) {
            return Ok(Arc::clone(&running.server));
        }
        let server = launch().map_err(Unanswered::Failed)?;
        let running = Running {
            stopper: server.stopper(),
            server: Arc::new(Mutex::new(server)),
        };
        let server = Arc::clone(&running.server);
        set.running.insert(program, running);
        Ok(server)
    }

    /// Stops `server`, which runs as `program`, unless another took its place.
    pub(super) fn forget(&self, program: &'static str, server: &Arc<Mutex<LanguageServer>>) {
        let mut set = self.0.lock();
        if set
            .running
            .get(program)
            .is_some_and(|running| Arc::ptr_eq(&running.server, server))
        {
            set.running.remove(program);
        }
    }

    /// Stops every server, as the engine stops or the checkout turns out to be another clone,
    /// whose code no grant covers; a lookup admitted after reads the grant that no longer
    /// applies.
    pub(super) fn stop(&self) {
        self.0.lock().running.clear();
    }
}

impl Set {
    /// Stops every server, its process gone by the time this returns, and admits no lookup of
    /// `epoch` or before any more: the window moves its epoch on as it takes the denial in.
    fn deny(&mut self, epoch: u64) {
        // Each server stops as it leaves the set.
        self.running.clear();
        self.floor = self.floor.max(epoch + 1);
    }
}

impl Engine {
    /// The handle on `review`, with the review of its `checkout`, if it has code on disk; made
    /// by the job that opens the review.
    pub(super) fn new(review: Review, checkout: Option<Review>) -> Self {
        Self {
            parts: Arc::new(Parts {
                review,
                checkout,
                watch: Mutex::default(),
                servers: Servers::default(),
            }),
        }
    }
}

impl Drop for Parts {
    fn drop(&mut self) {
        let servers = std::mem::take(&mut self.servers.0.get_mut().running);
        let watch = self.watch.get_mut().close();
        // Stopping a server waits for its process and the watch for its thread, and the last
        // holder of the parts may be the UI thread.
        if !servers.is_empty() || watch.is_some() {
            std::thread::spawn(move || {
                drop(servers);
                drop(watch);
            });
        }
    }
}

impl Model {
    /// Queues `job` to run in the background with the engine's parts, for `subject`, as
    /// [`Self::spawn`] does; nothing runs before the review opens or once the engine stopped.
    pub(super) fn spawn_with(
        &mut self,
        subject: Subject,
        job: impl FnOnce(&Parts) -> Outcome + Send + 'static,
    ) {
        let Some(engine) = &self.engine else {
            return;
        };
        let parts = Arc::clone(&engine.parts);
        self.spawn(subject, move || job(&parts));
    }

    /// Waits in the background for the working tree to change, with `waiting` alone, and hands
    /// it back with what it saw for the next wait.
    pub(super) fn wait_for_changes(&mut self, waiting: Waiting) {
        let Waiting(mut waiter) = waiting;
        self.effect(Effect::Spawn(Job::waiting(move || {
            let seen = waiter.wait();
            Completion {
                subject: Subject::Watch,
                outcome: Box::new(Outcome::Waited(Box::new(WaitedFor {
                    waited: seen,
                    problem: waiter.take_problem(),
                    waiting: Waiting(waiter),
                }))),
            }
        })));
    }

    /// Queues the job that stops watching the working tree, which ends the wait on it, and
    /// stops the language servers, as the review ends in this window and no lookup runs any
    /// more.
    pub(super) fn unwatch(&mut self) {
        self.spawn_with(Subject::Always(Because::Live), |parts| {
            parts.close_watch();
            parts.servers.stop();
            Outcome::Unwatched
        });
    }

    /// Takes the engine and queues the job that stops it, the language servers and the watch,
    /// as the app quits; returns whether there was one to stop.
    ///
    /// The trust epoch moves on, so a lookup still running starts no server after the job
    /// stopped them; a job still running keeps the review, and a capture the watch, until it
    /// ends.
    pub(super) fn dispose_engine(&mut self) -> bool {
        let Some(Engine { parts }) = self.engine.take() else {
            return false;
        };
        self.next_epoch();
        self.spawn(Subject::Always(Because::Stop), move || {
            parts.servers.stop();
            parts.close_watch();
            Outcome::Disposed
        });
        true
    }
}

impl Parts {
    /// Starts watching the working tree, keeping the watch unless the slot closed meanwhile,
    /// and returns what waits on it, which sees the watch closed if it was not kept, or why
    /// the tree cannot be watched.
    pub(super) fn watch(&self) -> Result<Waiting, crate::watch::WatchError> {
        let (watch, waiter) = Watch::start(self.review.ignore_rules())?;
        self.install(watch);
        Ok(Waiting(waiter))
    }

    /// Keeps `watch`, which started outside the lock, unless the slot closed meanwhile, when
    /// it is dropped here, on the job's thread.
    pub(super) fn install(&self, watch: Watch) {
        let mut slot = self.watch.lock();
        if matches!(*slot, WatchSlot::Closed) {
            drop(slot);
            drop(watch);
            return;
        }
        *slot = WatchSlot::Started(Arc::new(watch));
    }

    /// Closes the watch slot for good, as the review ends in the window or the engine stops,
    /// and drops the watch it kept outside the lock, on the job's thread.
    fn close_watch(&self) {
        let watch = self.watch.lock().close();
        drop(watch);
    }
}

#[cfg(test)]
impl Model {
    /// The engine's parts, for a test to look into or to stand in for a language server.
    pub(super) fn parts(&self) -> &Parts {
        &self.engine.as_ref().expect("the review is open").parts
    }

    /// The engine's parts, held weakly, so a test can tell when the last holder let go of them
    /// and of the review they hold.
    pub(super) fn weak_parts(&self) -> std::sync::Weak<Parts> {
        self.engine.as_ref().expect("the review is open").weak()
    }
}

#[cfg(test)]
impl Engine {
    /// The parts, held weakly; see [`Model::weak_parts`].
    pub(super) fn weak(&self) -> std::sync::Weak<Parts> {
        Arc::downgrade(&self.parts)
    }
}

#[cfg(test)]
impl Servers {
    /// Puts `server` in the set as `program`, as a lookup that launched it does, and returns it
    /// as that lookup holds it.
    #[cfg(target_os = "linux")]
    pub(super) fn insert(
        &self,
        program: &'static str,
        server: LanguageServer,
    ) -> Arc<Mutex<LanguageServer>> {
        let running = Running {
            stopper: server.stopper(),
            server: Arc::new(Mutex::new(server)),
        };
        let held = Arc::clone(&running.server);
        self.0.lock().running.insert(program, running);
        held
    }

    /// How many servers run.
    pub(super) fn len(&self) -> usize {
        self.0.lock().running.len()
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use super::*;

    /// A process standing in for a language server in `dir`, which runs until it is stopped.
    fn stand_in(dir: &Path) -> LanguageServer {
        LanguageServer::stand_in("cat", &[], dir).unwrap()
    }

    /// Whether the process `id` is gone, reaped.
    fn gone(id: u32) -> bool {
        !Path::new(&format!("/proc/{id}")).exists()
    }

    /// Admits a lookup of trust `epoch` for `rust`, the stored grant applying as `trusted` says,
    /// launching a stand-in in `dir`; returns the process it got.
    fn admit(servers: &Servers, epoch: u64, trusted: bool, dir: &Path) -> Result<u32, Unanswered> {
        let current = AtomicU64::new(epoch);
        let server = servers.admit(epoch, &current, || trusted, "rust", || Ok(stand_in(dir)))?;
        let process = server.lock().process_id();
        Ok(process)
    }

    /// A revocation while a lookup launches a server, under the servers' lock, waits for the
    /// launch and stops the server it launched; the lookup refuses from then on.
    #[test]
    fn a_revocation_waits_for_a_server_starting_and_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Arc::new(Servers::default());
        let revoked = Arc::new(AtomicBool::new(false));
        let launched = Arc::new(AtomicBool::new(false));
        let (launching, launch_begun) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let servers = Arc::clone(&servers);
            let revoked = Arc::clone(&revoked);
            let launched = Arc::clone(&launched);
            let root = dir.path().to_owned();
            move || {
                let launch = || {
                    launching.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                    let server = stand_in(&root);
                    launched.store(true, Ordering::SeqCst);
                    Ok(server)
                };
                let current = AtomicU64::new(0);
                let trusted = || !revoked.load(Ordering::SeqCst);
                servers
                    .admit(0, &current, trusted, "rust", launch)
                    .map(|server| server.lock().process_id())
            }
        });

        launch_begun.recv().unwrap();
        servers
            .decide(true, || {
                // The launch under the lock finished before the revocation recorded itself.
                assert!(launched.load(Ordering::SeqCst));
                revoked.store(true, Ordering::SeqCst);
                Ok::<_, ()>(Trust::Denied)
            })
            .unwrap();
        let process = worker.join().unwrap().unwrap();
        assert!(gone(process), "the server launched still runs");
        assert_eq!(servers.len(), 0);
        assert!(matches!(
            admit(&servers, 1, false, dir.path()),
            Err(Unanswered::Denied)
        ));
    }

    /// What the handshake test hears first: the server saying it got the handshake, or the
    /// handshake's end.
    enum Handshake {
        Heard,
        Ended(Result<(), String>),
    }

    /// A revocation while a server is in its handshake, which runs outside the servers' lock,
    /// stops it without waiting for the handshake, once the server, which ignores the request to
    /// exit, is killed after the exit deadline, and the handshake fails.
    ///
    /// The server says it got the handshake, then neither answers nor ends until it is killed,
    /// within ten minutes at the latest, so the handshake fails as stopped only when the
    /// revocation stops it while it waits.
    #[test]
    fn a_revocation_during_a_slow_handshake_waits_only_for_the_exit_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let heard = dir.path().join("heard");
        let made = std::process::Command::new("mkfifo").arg(&heard).status();
        assert!(made.unwrap().success());
        let servers = Arc::new(Servers::default());
        let (event, events) = std::sync::mpsc::channel();
        std::thread::spawn({
            let servers = Arc::clone(&servers);
            let root = dir.path().to_owned();
            let heard = heard.to_str().unwrap().to_owned();
            let event = event.clone();
            move || {
                let script = r#"head -c 1 >/dev/null; : >"$1"; exec sleep 600"#;
                let launch = || {
                    LanguageServer::stand_in(
                        "timeout",
                        &["600", "sh", "-c", script, "sh", &heard],
                        &root,
                    )
                    .map_err(|err| err.to_string())
                };
                let current = AtomicU64::new(0);
                let shaken = servers
                    .admit(0, &current, || true, "rust", launch)
                    .map_err(|err| format!("{err:?}"))
                    .and_then(|server| {
                        server
                            .lock()
                            .initialize(&root)
                            .map_err(|err| err.to_string())
                    });
                event.send(Handshake::Ended(shaken)).ok();
            }
        });
        std::thread::spawn(move || {
            // The server opens `heard` once it read the start of the handshake, and closes it.
            let mut heard = std::fs::File::open(heard).unwrap();
            std::io::Read::read_to_end(&mut heard, &mut Vec::new()).unwrap();
            event.send(Handshake::Heard).ok();
        });
        if let Handshake::Ended(shaken) = events.recv().unwrap() {
            panic!("the handshake ended before the server got it: {shaken:?}");
        }

        servers.decide(true, || Ok::<_, ()>(Trust::Denied)).unwrap();
        let Handshake::Ended(shaken) = events.recv().unwrap() else {
            panic!("the server said it got the handshake twice");
        };
        assert!(
            shaken.is_err_and(|err| err.contains("stopped")),
            "the handshake fails"
        );
        assert_eq!(servers.len(), 0);
    }

    /// A revocation stops every server, whatever grant still applies after it, and so does one
    /// that cannot be recorded, which keeps trust denied whatever is stored until a grant is
    /// recorded; a grant stops none.
    #[test]
    fn a_revocation_stops_every_server() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let granted = || Trust::Granted;

        let first = admit(&servers, 0, true, dir.path()).unwrap();
        servers.decide(false, || Ok::<_, ()>(granted())).unwrap();
        assert_eq!(servers.len(), 1, "a grant stops none");
        let still = servers.decide(true, || Ok::<_, ()>(granted())).unwrap();
        assert_eq!(still, granted());
        assert!(gone(first), "another grant still applies");

        let second = admit(&servers, 1, true, dir.path()).unwrap();
        let failed = servers.decide(true, || Err::<Trust, _>("the log is read-only"));
        assert!(failed.is_err());
        assert!(gone(second), "a failed revocation stops them");
        assert_eq!(
            servers.check(2, &AtomicU64::new(2), true, granted),
            Trust::Denied
        );
        assert!(matches!(
            admit(&servers, 2, true, dir.path()),
            Err(Unanswered::Denied)
        ));
        assert_eq!(
            servers.decide(false, || Ok::<_, ()>(granted())).unwrap(),
            granted()
        );
        assert_eq!(
            servers.check(3, &AtomicU64::new(3), true, granted),
            granted()
        );
    }

    /// Trust found denied, as another window revoked it, stops every server before the reading
    /// returns; once trust is granted again, a lookup launches a server of its own rather than
    /// using one from before, and one from before the denial is refused.
    #[test]
    fn a_denial_leaves_no_server_from_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let before = admit(&servers, 0, true, dir.path()).unwrap();
        assert_eq!(
            servers.check(0, &AtomicU64::new(0), true, || Trust::Denied),
            Trust::Denied
        );
        assert!(gone(before), "the server from before the denial runs");

        // A lookup of the epoch the denial was found in comes after it, the grant stored again.
        assert!(matches!(
            admit(&servers, 0, true, dir.path()),
            Err(Unanswered::Denied)
        ));
        // The window took the denial in and trust is granted again, in a later epoch.
        let after = admit(&servers, 2, true, dir.path()).unwrap();
        assert_ne!(after, before);
        assert!(!gone(after));
    }

    /// A lookup whose epoch no longer counts is refused before the stored grant is read, which
    /// may wait for another writer of the review; one whose epoch moves on while it reads is
    /// refused after.
    #[test]
    fn admission_reads_the_grant_only_for_a_lookup_that_counts() {
        let servers = Servers::default();
        let unread = || panic!("the stored grant is read");
        let launch = || panic!("a server launches");
        let current = AtomicU64::new(1);
        let refused = servers.admit(0, &current, unread, "rust", launch);
        assert!(
            matches!(refused, Err(Unanswered::Stale)),
            "an earlier epoch"
        );

        let revoking = || {
            current.fetch_add(1, Ordering::SeqCst);
            true
        };
        let refused = servers.admit(1, &current, revoking, "rust", launch);
        assert!(
            matches!(refused, Err(Unanswered::Stale)),
            "a revocation meanwhile"
        );
        assert_eq!(servers.len(), 0);
    }

    /// The servers outlive the snapshots that show while the window shows the checkout's code,
    /// stop once it shows code the checkout no longer holds, and a reading that no longer
    /// counts, as once the app closes, reads nothing and stops none.
    #[test]
    fn servers_run_while_the_checkouts_code_shows() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let granted = || Trust::Granted;
        let running = admit(&servers, 0, true, dir.path()).unwrap();

        // Another capture of the checkout shows.
        assert_eq!(
            servers.check(1, &AtomicU64::new(1), true, granted),
            granted()
        );
        assert!(!gone(running), "a capture stopped the server");

        // A late reading of a snapshot no longer on screen stops nothing.
        let read = servers.check(1, &AtomicU64::new(2), false, || {
            panic!("the stored grant is read")
        });
        assert_eq!(read, Trust::Denied);
        assert!(!gone(running), "a late reading stopped the server");

        // An older revision shows.
        assert_eq!(
            servers.check(2, &AtomicU64::new(2), false, granted),
            granted()
        );
        assert!(gone(running), "the server runs for an older revision");
        assert_eq!(servers.len(), 0);
    }
}
