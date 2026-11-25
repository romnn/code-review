//! Trust: whether tools that run the reviewed code, such as language servers, may run on the
//! snapshot on screen, read and decided in the background.
//!
//! The window shows one [`Trust`]: [`Trust::Checking`] while a reading or a decision is under
//! way, during which nothing starts, then denied, or granted under the scope its review allows
//! ([`Grant`]).
//! A decision goes through the save queue the marks use, and quitting waits for it; once the
//! app waits to quit, it takes no new one.
//! One decision at most is under way, since trust is being checked until it is recorded, and
//! the save queue keeps the snapshot on screen meanwhile.
//! Every snapshot that shows, by a capture or `R`, has its trust read again.
//!
//! The engine's [`Servers`](super::engine::Servers) decide, under one lock, whether a language
//! server may run: they read trust, record decisions and admit lookups, and hold the servers of
//! the current showing of a head, which another showing replaces.
//! Wherever trust is found denied there, by a reading, a lookup or a revocation, recorded or
//! not, every server stops before the window hears of it.
//! A revocation that cannot be recorded keeps trust denied there until a grant is recorded.
//!
//! The trust epoch moves on with every decision, every other snapshot that shows and every
//! denial the window takes in, and a reading or an answer from an earlier epoch counts for
//! nothing.
//! A lookup is admitted only while its epoch lasts, its showing is the servers' and the stored
//! grant still applies, so a revocation stops lookups at the key press.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use review_core::{ObjectId, Review, ReviewTarget, TrustScope};

use super::buffer::Lookup;
use super::effects::{Because, Outcome, Subject};
use super::engine::{Parts, Ticket};
use super::marks::Save;
use super::{Effect, Model, Overlay};

/// Whether tools may run on the snapshot on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Being read or decided in the background; nothing starts meanwhile.
    Checking,
    /// No tool that runs the reviewed code may start.
    Denied,
    /// Tools may run, under this grant.
    Granted(Grant),
}

impl Trust {
    /// Whether tools may run.
    #[must_use]
    pub fn granted(self) -> bool {
        matches!(self, Self::Granted(_))
    }
}

/// What a grant of trust covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// Exactly the snapshot on screen, until the code changes: what a review of a range or of
    /// the working tree is trusted with, and one of a checkout's grants.
    Snapshot,
    /// Whatever the checkout holds, until revoked.
    Checkout,
    /// The whole review, as a pull request's.
    Review,
}

/// A decision on trust, queued with the marks.
pub(crate) struct Decision {
    /// What it grants or revokes.
    grant: Grant,
    /// Whether it grants.
    trusted: bool,
    /// The head of the snapshot it was made on, whose tree a snapshot grant names and whose
    /// trust is read after it.
    head: ObjectId,
}

/// A decision on trust recorded, or why it could not be.
pub(super) struct Decided {
    decision: Decision,
    /// The trust that applies to the decision's snapshot once it is recorded.
    trust: Result<Trust, review_core::Error>,
}

/// Whether tools may run on the snapshot whose tree is `tree`, and under which grant: for a
/// checkout, the whole checkout's before the snapshot's own, and for a pull request, the whole
/// review's; denied when the log cannot be read.
pub(super) fn trust_of(review: &Review, tree: &ObjectId) -> Trust {
    let trusted = |trust: Result<review_core::Trust, review_core::Error>| {
        trust.is_ok_and(|trust| trust.trusted)
    };
    let granted = match review.target() {
        ReviewTarget::Checkout { .. } if trusted(review.trust()) => Some(Grant::Checkout),
        ReviewTarget::PullRequest { .. } => trusted(review.trust()).then_some(Grant::Review),
        _ => trusted(review.trust_for(tree)).then_some(Grant::Snapshot),
    };
    granted.map_or(Trust::Denied, Trust::Granted)
}

/// Records `decision` and reads the trust that applies after it, under the servers' lock; a
/// revocation stops every server, recorded or not (see
/// [`Servers::decide`](super::engine::Servers::decide)); runs in the background.
fn record(parts: &Parts, decision: Decision) -> Decided {
    let review = &parts.review;
    let trust = parts.servers.decide(!decision.trusted, || {
        let tree = review.repo().tree_of(&decision.head)?;
        let trusted = decision.trusted;
        match decision.grant {
            Grant::Snapshot => {
                review.set_trust(TrustScope::Snapshot { tree: tree.clone() }, trusted)
            }
            Grant::Checkout => review.set_trust(TrustScope::Checkout, trusted),
            Grant::Review => review.set_trusted(trusted),
        }?;
        Ok(trust_of(review, &tree))
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

    /// The head of the snapshot on screen.
    pub(super) fn shown_head(&self) -> Option<ObjectId> {
        self.view()
            .and_then(|view| view.record().map(|record| record.head.clone()))
    }

    /// Takes in another snapshot on screen, whose trust is not known yet, so the epoch moves on;
    /// once another head shows, it is a showing of its own, whose servers replace those before
    /// as its trust is read.
    pub(super) fn shown_anew(&mut self, head_changed: bool) {
        // A confirmation asked for on another snapshot does not carry over.
        self.trust_pending = None;
        self.next_epoch();
        if head_changed {
            self.showing = self.shown;
        }
    }

    /// What a reading of trust or a lookup asks in now.
    pub(super) fn ticket(&self) -> Ticket {
        Ticket {
            showing: self.showing,
            epoch: self.epoch(),
        }
    }

    /// `Ctrl+T`: revokes the grant that applies, stopping the language servers; or, for a review
    /// of the checkout, asks which grant to make, and for any other, makes it after a
    /// confirming second press.
    pub(super) fn toggle_trust(&mut self) {
        if self.engine.is_none() {
            return;
        }
        match self.trust {
            Trust::Checking => {
                self.set_status("Trust is being checked; press Ctrl+T again in a moment.");
            }
            Trust::Granted(grant) => self.decide(grant, false),
            Trust::Denied if self.is_checkout() => self.ask_trust(false),
            Trust::Denied => self.confirm_trust(),
        }
    }

    /// Trusts a review other than the checkout's on a second press on the same snapshot, after
    /// the first said what trusting runs.
    fn confirm_trust(&mut self) {
        let shown = self.shown_head();
        if shown.is_some() && self.trust_pending.take() == shown {
            let grant = match self.meta.as_ref().map(|meta| &meta.target) {
                Some(ReviewTarget::PullRequest { .. }) => Grant::Review,
                _ => Grant::Snapshot,
            };
            self.decide(grant, true);
            return;
        }
        self.trust_pending = shown;
        self.set_status(
            "Trusting runs a language server in a worktree of the reviewed head, and through it \
             the review's own build configuration, which can run any program. Press Ctrl+T \
             again to trust.",
        );
    }

    /// Asks whether to trust the checkout's code, with a grant for this snapshot or the whole
    /// checkout; with `resume`, go to definition runs once trusted, unless another key came
    /// first.
    pub(super) fn ask_trust(&mut self, resume: bool) {
        if self.live.as_ref().and_then(|live| live.tree()).is_none() {
            self.set_status("Nothing to trust yet: the working tree has not been captured.");
            return;
        }
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

    /// `s` or `c` in the trust question: grants trust in this snapshot or the whole checkout.
    pub(super) fn grant_trust(&mut self, grant: Grant) {
        if self.overlay != Some(Overlay::Trust) {
            return;
        }
        self.overlay = None;
        self.decide(grant, true);
    }

    /// Queues a decision on `grant` with the marks: from now on, until it is recorded, trust is
    /// being checked, and no lookup started before goes on.
    fn decide(&mut self, grant: Grant, trusted: bool) {
        if self.quitting.waits() {
            self.set_status(format!(
                "Trust is not changed while quitting. {}",
                self.quitting_status()
            ));
            return;
        }
        let Some(head) = self.shown_head() else {
            return;
        };
        self.next_epoch();
        self.trust = Trust::Checking;
        self.set_status(if trusted {
            "Granting trust…"
        } else {
            "Revoking trust…"
        });
        self.saves.push_back(Save::Trust(Decision {
            grant,
            trusted,
            head,
        }));
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
        let revoked = match decision.grant {
            Grant::Snapshot => "Trust in this snapshot revoked",
            Grant::Checkout => "Trust in this checkout revoked",
            Grant::Review => "Trust revoked",
        };
        match (&trust, decision.trusted) {
            (Ok(Trust::Granted(_)), true) => {
                self.set_status(match decision.grant {
                    Grant::Snapshot => {
                        "Trusted this snapshot: tools may run on it until the code changes. \
                         Ctrl+T revokes."
                    }
                    Grant::Checkout => {
                        "Trusted this checkout: tools may run on whatever it holds until \
                         Ctrl+T revokes it."
                    }
                    Grant::Review => {
                        "Trusted this review: tools may run on it until Ctrl+T revokes it."
                    }
                });
            }
            (Ok(_), true) => self.fail("The grant was recorded but does not apply."),
            (Ok(Trust::Granted(Grant::Snapshot)), false) => self.set_status(format!(
                "{revoked}; this snapshot stays trusted, which Ctrl+T revokes."
            )),
            (Ok(_), false) => self.set_status(format!("{revoked}. {}", self.summary())),
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

    /// Asks the engine's servers in the background whether tools may run on the snapshot on
    /// screen, which is being checked until the reading lands; see
    /// [`Servers::check`](super::engine::Servers::check).
    pub(super) fn read_trust(&mut self) {
        // Once the engine stopped, as the app quits, nothing is trusted any more.
        let Some(head) = self.shown_head().filter(|_| self.engine.is_some()) else {
            self.take_trust(Trust::Denied);
            return;
        };
        self.trust = Trust::Checking;
        let ticket = self.ticket();
        let current = Arc::clone(&self.trust_epoch);
        self.spawn_with(Subject::Trust(ticket.epoch), move |parts| {
            let review = &parts.review;
            Outcome::TrustRead(parts.servers.check(ticket, &current, || {
                review
                    .repo()
                    .tree_of(&head)
                    .map_or(Trust::Denied, |tree| trust_of(review, &tree))
            }))
        });
    }

    /// Takes in `trust` as what applies to the snapshot on screen; a denial moves the epoch on,
    /// past the last lookup the servers found trust denied for.
    pub(super) fn take_trust(&mut self, trust: Trust) {
        self.trust = trust;
        if trust == Trust::Denied {
            self.next_epoch();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pull request's review is trusted as a whole: a grant of one snapshot of it, which the
    /// window never makes, leaves it untrusted, and a grant of the whole review trusts it.
    #[test]
    fn a_pull_request_review_is_trusted_as_a_whole() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let initialized = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(initialized.success());
        let review = Review::open(
            &dir.path().join("data"),
            review_core::Repo::discover(&repo).unwrap(),
            ReviewTarget::PullRequest {
                host: "github.com".to_owned(),
                owner: "owner".to_owned(),
                repo: "repo".to_owned(),
                number: 1,
            },
        )
        .unwrap();
        // The empty tree, which any snapshot's tree stands for here.
        let tree = ObjectId::parse("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();

        review
            .set_trust(TrustScope::Snapshot { tree: tree.clone() }, true)
            .unwrap();
        assert_eq!(trust_of(&review, &tree), Trust::Denied);
        review.set_trusted(true).unwrap();
        assert_eq!(trust_of(&review, &tree), Trust::Granted(Grant::Review));
    }
}
