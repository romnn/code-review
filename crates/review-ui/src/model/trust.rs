//! Trust: whether tools that run the code under review, such as language servers, may run in
//! the checkout, read and decided in the background.
//!
//! Only a review of the working tree has code on disk to run tools in.
//! The review of its checkout holds the one grant, which covers whatever the checkout holds
//! after later edits and branch switches until it is revoked
//! ([`review_core::Review::checkout_review`]).
//! The window shows one [`Trust`]: [`Trust::Checking`] while a reading or a decision is under
//! way, during which nothing starts, then denied or granted.
//! A decision goes through the save queue the marks use, and quitting waits for it; once the
//! app waits to quit, it takes no new one.
//! One decision at most is under way, since trust is being checked until it is recorded, and
//! the save queue keeps the snapshot on screen meanwhile.
//! Every snapshot that shows, by a capture or `R`, has its trust read again, and while a
//! language server may run, the grant is read every [`RECONCILE`] whatever shows, even with no
//! capture showing, so a revocation in another window or another clone at the checkout's path
//! stops this window's servers within that time.
//!
//! The engine's [`Servers`](super::engine::Servers) decide, under one lock, whether a language
//! server may run: they read trust, record decisions and admit lookups, and hold the servers
//! running in the checkout.
//! Wherever trust is found denied there, by a reading, a lookup, the watch on the grant or a
//! revocation, recorded or not, every server stops before the window hears of it, and so does
//! every server once the window shows code the checkout no longer holds.
//! A revocation that cannot be recorded keeps trust denied there until a grant is recorded.
//!
//! The trust epoch moves on with every decision, every other snapshot that shows and every
//! denial the window takes in, and a reading or an answer from an earlier epoch counts for
//! nothing.
//! A lookup is admitted only while its epoch lasts and the stored grant still applies, so a
//! revocation stops lookups at the key press.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use review_core::{Review, ReviewTarget};

use super::buffer::Lookup;
use super::effects::{Because, Outcome, Subject};
use super::engine::Parts;
use super::live::RECONCILE;
use super::marks::Save;
use super::sync::Tick;
use super::{Effect, Model, Overlay};

/// Whether tools may run in the checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Being read or decided in the background; nothing starts meanwhile.
    Checking,
    /// No tool that runs the code may start.
    Denied,
    /// Tools may run in the checkout until the grant is revoked.
    Granted,
}

impl Trust {
    /// Whether tools may run.
    #[must_use]
    pub fn granted(self) -> bool {
        self == Self::Granted
    }
}

/// A decision on trust, queued with the marks.
pub(crate) struct Decision {
    /// Whether it grants trust, rather than revoking it.
    trusted: bool,
}

/// A decision on trust recorded, or why it could not be.
pub(super) struct Decided {
    decision: Decision,
    /// The trust that applies once it is recorded.
    trust: Result<Trust, review_core::Error>,
}

/// The grant read while language servers may run, whatever shows.
#[derive(Debug, Clone, Copy)]
pub(super) struct Guarded {
    /// The trust epoch the reading was asked in.
    epoch: u64,
    /// What it read, or `None` when no server ran, which reads nothing.
    trust: Option<Trust>,
}

/// Whether tools may run in the checkout whose review is `checkout`; denied without one, or
/// when its log cannot be read.
pub(super) fn trust_of(checkout: Option<&Review>) -> Trust {
    if checkout.is_some_and(|checkout| checkout.trusted().unwrap_or_default()) {
        Trust::Granted
    } else {
        Trust::Denied
    }
}

/// Records `decision` and reads the trust that applies after it, under the servers' lock; a
/// revocation stops every server, recorded or not (see
/// [`Servers::decide`](super::engine::Servers::decide)); runs in the background.
fn record(parts: &Parts, decision: Decision) -> Decided {
    let checkout = parts.checkout.as_ref();
    let trust = parts.servers.decide(!decision.trusted, || {
        let checkout = checkout.ok_or_else(|| {
            review_core::Error::InvalidTarget("this review has no code on disk".to_owned())
        })?;
        checkout.set_trusted(decision.trusted)?;
        Ok(trust_of(Some(checkout)))
    });
    Decided { decision, trust }
}

impl Model {
    /// The trust epoch now; see the module documentation.
    pub(super) fn epoch(&self) -> u64 {
        self.trust_epoch.load(Ordering::SeqCst)
    }

    /// Moves the trust epoch on, which stops every lookup started before it, and returns the
    /// new one.
    pub(super) fn next_epoch(&mut self) -> u64 {
        self.trust_epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Whether the review is of a working tree, whose checkout tools can run in.
    #[must_use]
    pub fn has_checkout(&self) -> bool {
        self.meta.as_ref().is_some_and(|meta| {
            matches!(
                meta.target,
                ReviewTarget::Checkout { .. } | ReviewTarget::Worktree { .. }
            )
        })
    }

    /// Why tools cannot run in the checkout for this review, if they cannot: it has no code on
    /// disk, or the checkout is another clone now.
    fn without_checkout(&self) -> Option<&'static str> {
        if !self.has_checkout() {
            return Some(
                "Language servers run only in a checkout, on the code on disk; check out the \
                 branch and review it there with cr.",
            );
        }
        self.replaced.map(super::Replaced::message)
    }

    /// Why go to definition cannot run on what shows, if it cannot: there is no checkout to run
    /// it in, or what shows is a revision older than the code on disk.
    ///
    /// Whether the code on disk is what shows otherwise, the lookup finds out.
    pub(super) fn not_on_disk(&self) -> Option<String> {
        if let Some(why) = self.without_checkout() {
            return Some(format!("Go to definition needs the code on disk. {why}"));
        }
        let newer = self
            .snapshot
            .as_ref()
            .and_then(crate::state::Snapshot::newer)?;
        Some(format!(
            "Go to definition needs the code on disk, which revision {newer} holds; press R to \
             switch to it."
        ))
    }

    /// Whether the window shows the code its checkout holds, as far as it knows: the newest
    /// revision of a review of the working tree, in the clone it opened in.
    #[must_use]
    pub fn shows_checkout(&self) -> bool {
        self.not_on_disk().is_none()
    }

    /// `Ctrl+T`: revokes the grant, stopping the language servers, or asks whether to trust the
    /// checkout.
    pub(super) fn toggle_trust(&mut self) {
        if self.engine.is_none() {
            return;
        }
        if let Some(why) = self.without_checkout() {
            self.set_status(why);
            return;
        }
        match self.trust {
            Trust::Checking => {
                self.set_status("Trust is being checked; press Ctrl+T again in a moment.");
            }
            Trust::Granted => self.decide(false),
            Trust::Denied => self.ask_trust(false),
        }
    }

    /// Asks whether to trust the checkout's code; with `resume`, go to definition runs once
    /// trusted, unless another key came first.
    pub(super) fn ask_trust(&mut self, resume: bool) {
        self.trust_resumes = self
            .buffer
            .as_ref()
            .filter(|_| resume)
            .map(|buffer| Lookup {
                buffer: buffer.id,
                shown: self.shown,
                epoch: self.epoch(),
            });
        self.overlay = Some(Overlay::Trust);
    }

    /// The trust question's yes: trusts the checkout until revoked.
    pub(super) fn grant_trust(&mut self) {
        if self.overlay != Some(Overlay::Trust) {
            return;
        }
        self.overlay = None;
        self.decide(true);
    }

    /// Queues a decision with the marks, granting trust or revoking it: from now on, until it
    /// is recorded, trust is being checked, and no lookup started before goes on.
    fn decide(&mut self, trusted: bool) {
        if self.quitting.waits() {
            self.set_status(format!(
                "Trust is not changed while quitting. {}",
                self.quitting_status()
            ));
            return;
        }
        self.next_epoch();
        self.trust = Trust::Checking;
        self.set_status(if trusted {
            "Granting trust…"
        } else {
            "Revoking trust…"
        });
        self.saves.push_back(Save::Trust(Decision { trusted }));
        self.next_save();
    }

    /// Queues `decision`'s job, as the save queue's next save.
    pub(super) fn save_decision(&mut self, decision: Decision) {
        self.spawn_with(Subject::Always(Because::Save), move |parts| {
            Outcome::Decided(record(parts, decision))
        });
    }

    /// Takes in a decision on trust recorded, or one that could not be, with the trust that
    /// applies after it, and starts the next save; a grant goes on to the definition that asked
    /// for it.
    pub(super) fn decided(&mut self, decided: Decided) {
        let Decided { decision, trust } = decided;
        match (&trust, decision.trusted) {
            (Ok(Trust::Granted), true) => self.set_status(
                "Trusted this checkout: language servers may run and build in it until Ctrl+T \
                 revokes it.",
            ),
            (Ok(_), true) => self.fail("The grant was recorded but does not apply."),
            (Ok(_), false) => self.set_status(format!(
                "Trust in this checkout revoked; its language servers stopped. {}",
                self.summary()
            )),
            (Err(err), true) => self.fail(format!("Cannot record trust: {err}")),
            (Err(err), false) => {
                self.effect(Effect::Log(format!(
                    "could not record a revocation of trust: {err:#}"
                )));
                let mut failure = format!(
                    "Could not revoke trust: {err}. The language servers stopped, but the stored \
                     grant may still apply when the review opens again."
                );
                if let Some(hint) = self.stop_quitting() {
                    failure = format!("{failure} {hint}");
                }
                self.fail(failure);
            }
        }
        self.take_trust(trust.unwrap_or(Trust::Denied));
        let resume = self.trust_resumes.take().filter(|_| decision.trusted);
        let here = self.buffer.as_ref().map(|buffer| (buffer.id, self.shown));
        if self.trust.granted()
            && resume.is_some_and(|resume| Some((resume.buffer, resume.shown)) == here)
        {
            self.go_to_definition();
        }
        self.next_save();
    }

    /// Asks the engine's servers in the background whether tools may run in the checkout,
    /// which is being checked until the reading lands; see
    /// [`Servers::check`](super::engine::Servers::check).
    pub(super) fn read_trust(&mut self) {
        // Once the engine stopped, as the app quits, nothing is trusted any more.
        if self.engine.is_none() {
            self.take_trust(Trust::Denied);
            return;
        }
        self.trust = Trust::Checking;
        let epoch = self.epoch();
        let on_disk = self.shows_checkout();
        let current = Arc::clone(&self.trust_epoch);
        self.spawn_with(Subject::Trust(epoch), move |parts| {
            Outcome::TrustRead(parts.servers.check(epoch, &current, on_disk, || {
                trust_of(parts.checkout.as_ref())
            }))
        });
    }

    /// Reads the grant every [`RECONCILE`] for as long as a language server runs, from now on;
    /// see the module documentation.
    pub(super) fn guard_servers(&mut self) {
        if self.guarding || self.engine.is_none() {
            return;
        }
        self.guarding = true;
        self.after(
            RECONCILE,
            Subject::Always(Because::Clock),
            Outcome::Tick(Tick::Guard),
        );
    }

    /// Reads the grant in the background, as the watch on it is due, while any language server
    /// runs; see [`Servers::guard`](super::engine::Servers::guard).
    pub(super) fn guard_due(&mut self) {
        self.guarding = false;
        let epoch = self.epoch();
        let current = Arc::clone(&self.trust_epoch);
        self.spawn_with(Subject::Always(Because::Guard), move |parts| {
            let trust = parts
                .servers
                .guard(&current, || trust_of(parts.checkout.as_ref()));
            Outcome::Guarded(Guarded { epoch, trust })
        });
    }

    /// Takes in the grant the watch read: keeps watching while servers run, and shows trust
    /// found denied, whose servers stopped, unless trust was read or decided since.
    pub(super) fn guarded(&mut self, guarded: Guarded) {
        match guarded.trust {
            Some(Trust::Granted) => self.guard_servers(),
            Some(_) if guarded.epoch == self.epoch() => {
                self.take_trust(Trust::Denied);
                self.fail(
                    "Trust in this checkout no longer applies, as when another window revoked it \
                     or another clone took its place, so its language servers stopped.",
                );
            }
            // No server ran, or a newer reading or decision applies.
            None | Some(_) => {}
        }
    }

    /// Takes in `trust` as what applies; a denial moves the epoch on, past the last lookup the
    /// servers found trust denied for.
    pub(super) fn take_trust(&mut self, trust: Trust) {
        self.trust = trust;
        if trust == Trust::Denied {
            self.next_epoch();
        }
    }
}
