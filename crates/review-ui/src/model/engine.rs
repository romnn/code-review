//! The engine handle: the review, the session that pins what the window shows, the watch on
//! the working tree and the language servers, which only background work can reach.
//!
//! [`Servers`] is the one authority on whether a language server may start or be used: see the
//! `trust` module.
//!
//! Everything it holds can block: the review runs git and reads the event log, the session and
//! the watch run git and join threads as they end, and the servers are processes.
//! The model holds one [`Engine`], which it can neither look into nor clone: all it can do with
//! it is queue a job that runs with its [`Parts`] in the background ([`Model::spawn_with`]),
//! whose outcome comes back as plain data, and hand it to the job that stops it
//! ([`Model::dispose_engine`]), which quitting waits for.
//! The wait for the working tree to change holds only the watch's [`Waiting`], which the model
//! can only hand on to the next wait ([`Model::wait_for_changes`]).
//! So no code on the UI thread compiles that blocks on the engine.
//!
//! A job holds the parts while it runs, and a capture the session and the watch, so the last of
//! them may let go anywhere, on the UI thread too, as when the model is dropped or a job is
//! dropped unrun: each then hands what blocks to a thread of its own.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use review_core::{Review, Session};

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
    /// The session that pins the captures the window shows, for a review that follows the
    /// working tree, until the engine stops; a capture holds it too while it runs, and it ends
    /// once the last holder lets go.
    pub(super) session: Mutex<Option<Arc<Session>>>,
    /// The watch on the working tree, once it started, until the checkout is another clone or
    /// the engine stops; a capture holds it too while it walks the tree again.
    pub(super) watch: Mutex<Option<Arc<Watch>>>,
    /// The running language servers.
    pub(super) servers: Servers,
}

/// What waits for the watch to see a change, handed from one wait to the next.
///
/// It holds the watch only weakly, so a wait never keeps the engine alive: once the watch is
/// dropped, the wait ends.
pub(super) struct Waiting(Waiter);

/// The language servers of the head on screen, with what decides whether they may run.
///
/// It holds one set of servers, for the current showing of a head, and its lock serializes the
/// readings of trust, the decisions on trust and the admission of lookups.
/// It is held only for those, never across a handshake or a request.
#[derive(Default)]
pub(super) struct Servers(Mutex<Set>);

/// The servers of one showing of a head, all in the worktree of its tree, by program.
#[derive(Default)]
struct Set {
    /// The showing they run for; see [`Ticket::showing`].
    showing: u64,
    /// The first trust epoch a lookup is admitted in: the one after the last denial found here.
    floor: u64,
    running: HashMap<&'static str, Running>,
    /// Whether a revocation could not be recorded, which keeps trust denied here until a grant
    /// is recorded, whatever the stored grants say.
    withdrawn: bool,
}

/// What a reading of trust or a lookup was asked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Ticket {
    /// The showing on screen: the generation of the snapshot shown when its head became
    /// current; see [`Model::shown`]. So a head shown again is a showing of its own.
    pub(super) showing: u64,
    /// The trust epoch.
    pub(super) epoch: u64,
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
    /// Reads, with `read`, the trust in the head that `ticket`'s showing shows, as a capture or
    /// another revision showing asks; `current` is the window's trust epoch now.
    ///
    /// A newer showing replaces the set, stopping its servers, and a ticket that no longer
    /// counts, of an older showing or epoch, changes nothing and reads nothing, since reading
    /// may wait for another writer of the review.
    /// Trust found denied stops every server before it is returned, and no lookup of the
    /// ticket's epoch or before is admitted any more.
    pub(super) fn check(
        &self,
        ticket: Ticket,
        current: &AtomicU64,
        read: impl FnOnce() -> Trust,
    ) -> Trust {
        let mut set = self.0.lock();
        if ticket.showing < set.showing || current.load(Ordering::SeqCst) != ticket.epoch {
            return Trust::Denied;
        }
        if ticket.showing > set.showing {
            // Each server stops as it leaves the set.
            set.running.clear();
            set.showing = ticket.showing;
        }
        let trust = if set.withdrawn { Trust::Denied } else { read() };
        if !trust.granted() {
            set.deny(ticket);
        }
        trust
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

    /// Hands a lookup of `ticket` the server `program`, launching it with `launch` when none
    /// runs, while the stored grant, which `trusted` reads, still applies; `current` is the
    /// window's trust epoch now.
    ///
    /// All under the lock, so a decision or a reading of trust either comes first and is seen
    /// here, or waits for the launch, which only spawns the process, and stops what it
    /// launched.
    /// A ticket that no longer counts is refused before the stored grant is read, which may
    /// wait for another writer of the review, and the epoch is asked for again after it, since
    /// the reviewer may revoke trust meanwhile.
    /// Trust found denied since the ticket's epoch, here or by a reading, refuses it as denied,
    /// and so does a stored grant that no longer applies, which stops every server.
    /// The server is in the set before the lock is released, so a revocation finds it.
    pub(super) fn admit(
        &self,
        ticket: Ticket,
        current: &AtomicU64,
        trusted: impl FnOnce() -> bool,
        program: &'static str,
        launch: impl FnOnce() -> Result<LanguageServer, String>,
    ) -> Result<Arc<Mutex<LanguageServer>>, Unanswered> {
        let lasts = || current.load(Ordering::SeqCst) == ticket.epoch;
        let mut set = self.0.lock();
        if ticket.showing != set.showing || !lasts() {
            return Err(Unanswered::Stale);
        }
        if ticket.epoch < set.floor || set.withdrawn || !trusted() {
            set.deny(ticket);
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

    /// Stops every server, as the engine stops.
    fn stop(&self) {
        self.0.lock().running.clear();
    }
}

impl Set {
    /// Stops every server, its process gone by the time this returns, and admits no lookup of
    /// `ticket`'s epoch or before any more: the window moves its epoch on as it takes the
    /// denial in.
    fn deny(&mut self, ticket: Ticket) {
        // Each server stops as it leaves the set.
        self.running.clear();
        self.floor = self.floor.max(ticket.epoch + 1);
    }
}

impl Engine {
    /// The handle on `review`, with the session the window follows the working tree under, if
    /// it does; made by the job that opens the review.
    pub(super) fn new(review: Review, session: Option<Session>) -> Self {
        Self {
            parts: Arc::new(Parts {
                review,
                session: Mutex::new(session.map(Arc::new)),
                watch: Mutex::new(None),
                servers: Servers::default(),
            }),
        }
    }
}

impl Drop for Parts {
    fn drop(&mut self) {
        let servers = std::mem::take(&mut self.servers.0.get_mut().running);
        let watch = self.watch.get_mut().take();
        let session = self.session.get_mut().take();
        // Stopping a server waits for its process, the watch for its thread and the session
        // for git, and the last holder of the parts may be the UI thread.
        if !servers.is_empty() || watch.is_some() || session.is_some() {
            std::thread::spawn(move || {
                drop(servers);
                drop(watch);
                drop(session);
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
                subject: Subject::Always(Because::Live),
                outcome: Box::new(Outcome::Waited(Box::new(WaitedFor {
                    waited: seen,
                    problem: waiter.take_problem(),
                    waiting: Waiting(waiter),
                }))),
            }
        })));
    }

    /// Takes the engine and queues the job that stops it, the language servers, the watch and
    /// the session, as the app quits; returns whether there was one to stop.
    ///
    /// The trust epoch moves on, so a lookup still running starts no server after the job
    /// stopped them; a capture still running keeps the session and the watch until it ends.
    pub(super) fn dispose_engine(&mut self) -> bool {
        let Some(Engine { parts }) = self.engine.take() else {
            return false;
        };
        self.next_epoch();
        self.spawn(Subject::Always(Because::Stop), move || {
            parts.servers.stop();
            drop(parts.watch.lock().take());
            drop(parts.session.lock().take());
            Outcome::Disposed
        });
        true
    }
}

impl Parts {
    /// Starts watching the working tree, keeping the watch, and returns what waits on it, or
    /// why it cannot be watched.
    pub(super) fn watch(&self) -> Result<Waiting, crate::watch::WatchError> {
        let (watch, waiter) = Watch::start(self.review.repo().clone())?;
        *self.watch.lock() = Some(Arc::new(watch));
        Ok(Waiting(waiter))
    }
}

#[cfg(test)]
impl Model {
    /// The engine's parts, for a test to look into or to stand in for a language server.
    pub(super) fn parts(&self) -> &Parts {
        &self.engine.as_ref().expect("the review is open").parts
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
    use std::time::{Duration, Instant};

    use super::*;

    /// A process standing in for a language server in `dir`, which runs until it is stopped.
    fn stand_in(dir: &Path) -> LanguageServer {
        LanguageServer::stand_in("cat", &[], dir).unwrap()
    }

    /// Whether the process `id` is gone, reaped.
    fn gone(id: u32) -> bool {
        !Path::new(&format!("/proc/{id}")).exists()
    }

    /// The ticket of `showing` in `epoch`.
    fn ticket(showing: u64, epoch: u64) -> Ticket {
        Ticket { showing, epoch }
    }

    /// Admits a lookup of `ticket` for `rust`, the stored grant applying as `trusted` says,
    /// launching a stand-in in `dir`; returns the process it got.
    fn admit(
        servers: &Servers,
        ticket: Ticket,
        trusted: bool,
        dir: &Path,
    ) -> Result<u32, Unanswered> {
        let current = AtomicU64::new(ticket.epoch);
        let server = servers.admit(ticket, &current, || trusted, "rust", || Ok(stand_in(dir)))?;
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
                    .admit(ticket(0, 0), &current, trusted, "rust", launch)
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
            admit(&servers, ticket(0, 1), false, dir.path()),
            Err(Unanswered::Denied)
        ));
    }

    /// A revocation while a server is in its handshake, which runs outside the servers' lock,
    /// returns at once and stops it, and the handshake fails.
    #[test]
    fn a_revocation_during_a_slow_handshake_returns_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Arc::new(Servers::default());
        let worker = std::thread::spawn({
            let servers = Arc::clone(&servers);
            let root = dir.path().to_owned();
            move || {
                // A server that never answers the handshake.
                let launch = || {
                    LanguageServer::stand_in("sleep", &["30"], &root).map_err(|err| err.to_string())
                };
                let current = AtomicU64::new(0);
                let server = servers
                    .admit(ticket(0, 0), &current, || true, "rust", launch)
                    .unwrap();
                let command = crate::lsp::server_for(Path::new("lib.rs")).unwrap();
                server.lock().initialize(&command, &root)
            }
        });
        // The server is in the set, shaking hands.
        let waited = Instant::now();
        while servers.len() == 0 {
            assert!(
                waited.elapsed() < Duration::from_secs(10),
                "no server launched"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));

        let revoking = Instant::now();
        servers.decide(true, || Ok::<_, ()>(Trust::Denied)).unwrap();
        assert!(
            revoking.elapsed() < Duration::from_secs(1),
            "the revocation waited"
        );
        let shaken = worker.join().unwrap();
        assert!(
            shaken.is_err_and(|err| err.to_string().contains("stopped")),
            "the handshake fails"
        );
        assert!(waited.elapsed() < Duration::from_secs(10));
        assert_eq!(servers.len(), 0);
    }

    /// A revocation stops every server, whatever grant still applies after it, and so does one
    /// that cannot be recorded, which keeps trust denied whatever is stored until a grant is
    /// recorded; a grant stops none.
    #[test]
    fn a_revocation_stops_every_server() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let granted = || Trust::Granted(super::super::Grant::Snapshot);

        let first = admit(&servers, ticket(0, 0), true, dir.path()).unwrap();
        servers.decide(false, || Ok::<_, ()>(granted())).unwrap();
        assert_eq!(servers.len(), 1, "a grant stops none");
        let still = servers.decide(true, || Ok::<_, ()>(granted())).unwrap();
        assert_eq!(still, granted());
        assert!(gone(first), "another grant still applies");

        let second = admit(&servers, ticket(0, 1), true, dir.path()).unwrap();
        let failed = servers.decide(true, || Err::<Trust, _>("the log is read-only"));
        assert!(failed.is_err());
        assert!(gone(second), "a failed revocation stops them");
        assert_eq!(
            servers.check(ticket(0, 2), &AtomicU64::new(2), granted),
            Trust::Denied
        );
        assert!(matches!(
            admit(&servers, ticket(0, 2), true, dir.path()),
            Err(Unanswered::Denied)
        ));
        assert_eq!(
            servers.decide(false, || Ok::<_, ()>(granted())).unwrap(),
            granted()
        );
        assert_eq!(
            servers.check(ticket(0, 3), &AtomicU64::new(3), granted),
            granted()
        );
    }

    /// Trust found denied, as another window revoked it, stops every server before the reading
    /// returns; once trust is granted again on the same code, a lookup launches a server of its
    /// own rather than using one from before, and one from before the denial is refused.
    #[test]
    fn a_denial_leaves_no_server_from_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let before = admit(&servers, ticket(0, 0), true, dir.path()).unwrap();
        assert_eq!(
            servers.check(ticket(0, 0), &AtomicU64::new(0), || Trust::Denied),
            Trust::Denied
        );
        assert!(gone(before), "the server from before the denial runs");

        // A lookup of the epoch the denial was found in comes after it, the grant stored again.
        assert!(matches!(
            admit(&servers, ticket(0, 0), true, dir.path()),
            Err(Unanswered::Denied)
        ));
        // The window took the denial in and trust is granted again, in a later epoch.
        let after = admit(&servers, ticket(0, 2), true, dir.path()).unwrap();
        assert_ne!(after, before);
        assert!(!gone(after));
    }

    /// A lookup whose ticket no longer counts is refused before the stored grant is read, which
    /// may wait for another writer of the review; one whose epoch moves on while it reads is
    /// refused after.
    #[test]
    fn admission_reads_the_grant_only_for_a_ticket_that_counts() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let unread = || panic!("the stored grant is read");
        let launch = || panic!("a server launches");
        let current = AtomicU64::new(1);
        let refused = servers.admit(ticket(0, 0), &current, unread, "rust", launch);
        assert!(
            matches!(refused, Err(Unanswered::Stale)),
            "an earlier epoch"
        );
        let refused = servers.admit(ticket(1, 1), &current, unread, "rust", launch);
        assert!(matches!(refused, Err(Unanswered::Stale)), "another showing");

        let revoking = || {
            current.fetch_add(1, Ordering::SeqCst);
            true
        };
        let refused = servers.admit(ticket(0, 1), &current, revoking, "rust", launch);
        assert!(
            matches!(refused, Err(Unanswered::Stale)),
            "a revocation meanwhile"
        );
        let _ = dir;
        assert_eq!(servers.len(), 0);
    }

    /// A reading whose epoch no longer counts, as once the app closes, reads nothing and changes
    /// nothing, so it never waits for another writer of the review.
    #[test]
    fn a_reading_that_no_longer_counts_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let running = admit(&servers, ticket(0, 0), true, dir.path()).unwrap();
        let read = servers.check(ticket(0, 0), &AtomicU64::new(1), || {
            panic!("the stored grant is read")
        });
        assert_eq!(read, Trust::Denied);
        assert!(!gone(running));
    }

    /// Another head showing replaces the servers, and showing a head again is a showing of its
    /// own: from one head to another and back, the servers of the first showing stop, and a
    /// reading of the head between, running late, stops none started since.
    #[test]
    fn a_showing_has_servers_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let granted = || Trust::Granted(super::super::Grant::Snapshot);
        let first = admit(&servers, ticket(0, 0), true, dir.path()).unwrap();

        // The first head shows again, as showing 3, before the reading of showing 2 runs.
        assert_eq!(
            servers.check(ticket(3, 3), &AtomicU64::new(3), granted),
            granted()
        );
        assert!(gone(first), "the first showing's server runs");
        let again = admit(&servers, ticket(3, 3), true, dir.path()).unwrap();
        assert_eq!(
            servers.check(ticket(2, 2), &AtomicU64::new(3), || { Trust::Denied }),
            Trust::Denied
        );
        assert!(
            !gone(again),
            "the late reading stopped a server of a later showing"
        );
        assert!(matches!(
            admit(&servers, ticket(2, 3), true, dir.path()),
            Err(Unanswered::Stale)
        ));
    }
}
