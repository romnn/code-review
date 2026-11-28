//! Following a working-tree review live: watching the working tree, capturing it when it
//! changes, and showing each capture in place of the snapshot on screen.
//!
//! A capture is compared with the snapshot on screen when it is taken, and lands only on that
//! snapshot: the correspondence between the two carries the reviewer's place and folds over,
//! and tells which hunks changed.
//! It shows only in the turn that draws the next frame, which the front end runs with
//! [`Model::publish`], so every key acts on what the last frame showed.
//! It waits longer while the reviewer is in the middle of something it would pull the rows from
//! under: marks being saved, a picker or overlay, a drag, a search being typed.
//! Only the newest waiting capture is kept, and one compared with a snapshot no longer on
//! screen, or read before a mark was saved, is taken again.
//!
//! A review key acts on the capture on screen, however recently its code changed: the mark
//! names that capture, and the next one carries it only where the code stayed the same.
//! A key held down since before a capture never acts on what it moved the selection onto, as
//! the model's guard on review keys tells a repeat.
//! A capture carries the selected change of a unit it links unchanged as that very change, its
//! lines moved with the unit.
//!
//! Every change the watch reports, and every change of the branch's pull request, counts one
//! more observation, and a capture carries the count it was taken at.
//! The watch reports a burst of changes twice: as soon as its first change arrives, and once it
//! settles, when the capture of it is taken.
//! One taken before the newest observation may miss it, so it never shows, and neither does one
//! that lands while a burst is under way: the view on screen stays, marked as updating, until a
//! capture of the newest observation lands.
//! A mark on it is still taken: it marks what the reviewer saw, the capture on screen, which
//! the engine records as shown and checks under the review's lock.
//! Without a watch, as while it starts or when the working tree cannot be watched, and once the
//! watch could not watch a directory, each capture looks twice and shows only when both looks
//! saw the same.
//! While git is busy in the checkout, the view stays and still takes marks; once the checkout is
//! another clone, it takes none.
//!
//! What a capture was of, its subject, shows with it: the branch, its pull request and the
//! base, which the header names and the status line announces when they change.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use review_core::correspond::{Link, LinkKind, correspond};
use review_core::{
    Busy, Capture, Commit, History, LiveCapture, ObjectId, ReviewInfo, ReviewTarget, RevisionView,
    ShownRun, UnitId, UnitIndex,
};

use super::buffer::Origin;
use super::effects::{Because, Outcome, Subject};
use super::engine::{Parts, Waiting};
use super::guard::Aim;
use super::pool::Lane;
use super::present::{Anchor, DiffAnchor, RowKey, diff_anchor, diff_offset};
use super::pulls::Pulls;
use super::sync::Tick;
use super::{Effect, Mode, Model, Phase, Quitting};
use crate::frame;
use crate::rows::{FullText, Layout};
use crate::state::{self, Snapshot};
use crate::watch::Waited;

/// How often the working tree is captured whatever the watch saw, catching what it cannot see:
/// a ref moving without the index changing, a submodule's commit, events the system dropped.
pub(super) const RECONCILE: Duration = Duration::from_secs(30);

/// How often the working tree is captured when it cannot be watched.
const POLL: Duration = Duration::from_secs(5);

/// How often the periodic capture walks the watched tree again first, catching a directory
/// whose events were lost without a word; a walk asks git about every directory, about half a
/// second on a checkout of 13,000 files, so it runs far less often than the capture.
const REWALK: Duration = Duration::from_secs(300);

/// How many times a capture is taken again at once when git moved while it was taken.
const MOVED_ATTEMPTS: u32 = 3;

/// What the status line says while a capture the reviewer asked for runs; the capture answers
/// only while it still says so.
const CAPTURING_AGAIN: &str = "Capturing the working tree again…";

/// Following the working tree.
#[derive(Default)]
pub(crate) struct Live {
    /// Whether the engine watches the working tree; not while the watch starts or when the
    /// working tree cannot be watched, which is captured every [`POLL`] instead.
    watched: bool,
    /// Whether a capture runs.
    capturing: bool,
    /// Whether another capture follows the running one, and whether it walks the working
    /// tree again first.
    again: Option<bool>,
    /// The newest capture that waits for the reviewer.
    waiting: Option<Box<Captured>>,
    /// Whether the window said cr's data is full since the last capture that showed, so it
    /// says so once.
    full: bool,
    /// Whether the watch's trouble was logged already, so it is logged once.
    troubled: bool,
    /// When the watched tree was last walked, by the watch's start or a periodic capture.
    walked: Option<Instant>,
    /// The observations counted so far: each change the watch reported, at the start of its
    /// burst and once it settled, and each change of the pull request; a capture taken at an
    /// older count never shows.
    observed: u64,
    /// Whether a burst of changes the watch reported is still under way, so no capture shows
    /// until it settles.
    stirring: bool,
    /// Whether the view on screen may miss an observation, so it shows as updating until a
    /// capture of the newest one lands.
    updating: bool,
    /// What git was busy with at the last capture, which keeps the view on screen.
    busy: Option<Busy>,
    /// What the capture on screen was of.
    subject: Option<review_core::Subject>,
    /// The paths the capture on screen kept at their staged version, under a filter driver.
    filtered: Vec<String>,
    /// The tree of the capture on screen, which tells a capture of the same code.
    tree: Option<ObjectId>,
    /// The base and head the review's commits were last listed between.
    listed: Option<(Option<ObjectId>, Option<ObjectId>)>,
    /// Whether the reviewer asked for a capture with `r`, so its landing says so.
    asked: bool,
    /// The branch's pull request.
    pub(crate) pulls: Pulls,
    /// What was found of the pull requests when the running capture was taken, which it
    /// resolves its base with.
    given: Option<Vec<review_core::github::CheckoutPullRequest>>,
}

impl Live {
    /// What was found of the pull requests when the running capture was taken, while one runs.
    pub(crate) fn given(&self) -> Option<&[review_core::github::CheckoutPullRequest]> {
        self.given.as_deref()
    }

    /// Whether a capture runs, which review-gui's QA mode waits for.
    pub(crate) fn capturing(&self) -> bool {
        self.capturing
    }

    /// Whether the view on screen may miss a change, until it is captured.
    pub(crate) fn updating(&self) -> bool {
        self.updating
    }

    /// What git is busy with in the checkout, which keeps the view on screen.
    pub(crate) fn busy(&self) -> Option<Busy> {
        self.busy
    }

    /// What the capture on screen was of.
    pub(crate) fn subject(&self) -> Option<&review_core::Subject> {
        self.subject.as_ref()
    }

    /// What the capture waiting to show was of.
    pub(crate) fn waiting_subject(&self) -> Option<&review_core::Subject> {
        self.waiting.as_ref()?.subject.as_ref()
    }

    /// The paths the capture on screen kept at their staged version, under a filter driver.
    pub(crate) fn filtered(&self) -> &[String] {
        &self.filtered
    }
}

/// What the background work of opening found out about a review that follows the checkout.
pub(super) struct Opening {
    /// What the opening capture was of, or `None` when git was busy or cr's data full, and the
    /// review opened on its last recorded revision.
    subject: Option<review_core::Subject>,
    /// The paths the opening capture kept at their staged version.
    filtered: Vec<String>,
    /// What git was busy with when the review opened, if it was.
    busy: Option<Busy>,
    tree: Option<ObjectId>,
    association: Option<review_core::BranchIdentity>,
    commits: Option<Vec<Commit>>,
}

impl Opening {
    /// Reads what the window needs about the opening capture `history` ends in, of `subject`,
    /// which kept `filtered` at their staged version, or about its last recorded revision when
    /// git was `busy` or cr's data full: its tree, the branch and upstream, and the commits.
    #[tracing::instrument(name = "read_opening", skip_all)]
    pub(super) fn read(
        review: &review_core::Review,
        history: &History,
        subject: Option<review_core::Subject>,
        filtered: Vec<String>,
        busy: Option<Busy>,
    ) -> Self {
        let tree = provisional_or_tip_tree(review, history);
        let checkout = matches!(review.target(), ReviewTarget::Checkout { .. });
        let observed = subject.as_ref().filter(|_| checkout);
        let association = observed
            .and_then(|subject| subject.branch.as_deref())
            .and_then(|branch| review.repo().branch_identity(branch).ok());
        let commits = observed.and_then(|subject| review.subject_commits(subject).ok());
        Self {
            subject,
            filtered,
            busy,
            tree,
            association,
            commits,
        }
    }
}

impl Opening {
    /// How the review opened, for the status line: what it follows, or that git is busy.
    pub(super) fn describe(&self) -> Option<String> {
        if let Some(busy) = self.busy {
            return Some(format!(
                "git is busy ({busy}); the last recorded revision shows until it finishes"
            ));
        }
        let subject = self.subject.as_ref()?;
        Some(format!("following {}", branch_and_base(subject)))
    }
}

/// The branch checked out and what it is compared with, as the header names them:
/// `feat/x → origin/main`, `detached 1a2b3c4 → origin/main`, `main → empty tree` before the
/// first commit.
#[must_use]
pub fn branch_and_base(subject: &review_core::Subject) -> String {
    let head = match (&subject.branch, &subject.head) {
        (Some(branch), _) => branch.clone(),
        (None, Some(commit)) => format!("detached {}", abbreviated(commit)),
        (None, None) => "detached".to_owned(),
    };
    let base = subject.base.name().unwrap_or("empty tree");
    format!("{head} → {base}")
}

/// The first seven digits of `commit`, as git abbreviates it and the bottom bar shows it.
#[must_use]
pub fn abbreviated(commit: &ObjectId) -> &str {
    commit.as_str().get(..7).unwrap_or(commit.as_str())
}

/// The tree of the head of `history`'s current revision.
fn provisional_or_tip_tree(review: &review_core::Review, history: &History) -> Option<ObjectId> {
    let tip = history.current()?;
    let head = history.view(tip).ok()?.record()?.head.clone();
    review.snapshots().tree_of(&head).ok()
}

/// What a wait for the working tree saw, with what waits on for the next wait.
pub(super) struct WaitedFor {
    pub(super) waited: Waited,
    /// Why the watch may have missed changes since the last wait, if it may have.
    pub(super) problem: Option<String>,
    pub(super) waiting: Waiting,
}

/// A capture of the working tree, compared with the snapshot on screen when it was taken.
pub(super) struct Captured {
    /// The generation of the snapshot it was compared with; see [`Model::shown`].
    from: u64,
    /// The observations counted when it was taken; see [`Live::observed`].
    observed: u64,
    /// What it was of; `None` for a review that does not observe one.
    subject: Option<review_core::Subject>,
    /// The paths it kept at their staged version.
    filtered: Vec<String>,
    /// Its tree.
    tree: Option<ObjectId>,
    /// Its branch and upstream, for a review of a checkout on a branch.
    association: Option<review_core::BranchIdentity>,
    /// The commits between its base and head, when they are not the ones listed already.
    commits: Option<Vec<Commit>>,
    /// The history, ending in the capture.
    history: Arc<History>,
    /// The correspondence from the snapshot it was compared with to the capture; empty when
    /// the two are the same snapshot.
    links: Vec<Link>,
    /// The text of the capture's files that were not cached yet.
    texts: HashMap<ObjectId, Arc<FullText>>,
    /// Why the watch could not be walked again, if it could not.
    trouble: Option<String>,
}

/// Why a capture shows nothing new.
pub(super) enum Missed {
    /// Git is busy in the checkout.
    Busy(Busy),
    /// cr's data is full, so nothing was captured; the message says to drop reviews.
    Full(String),
    /// What the review was read from was replaced.
    Replaced(Replaced),
    /// The capture failed, for this reason.
    Failed(String),
}

/// What took the place of what a review was read from, which ends the review in a window: it
/// captures, looks up and saves nothing any more, and says to open the review again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replaced {
    /// The checkout is another clone now, whose history is not this review's.
    Clone,
    /// The review's event log is another one now, such as a backup restored; see
    /// [`review_core::Error::LogReplaced`].
    Log,
}

impl Replaced {
    /// Says what was replaced and that the review is to be opened again.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Clone => "This checkout was replaced by another clone; reopen cr to review it.",
            Self::Log => "This review's event log was replaced; reopen cr to review it.",
        }
    }

    /// Says why a review key marked nothing.
    fn refusal(self) -> &'static str {
        match self {
            Self::Clone => {
                "Not marked: this checkout was replaced by another clone; reopen cr to review it."
            }
            Self::Log => {
                "Not marked: this review's event log was replaced; reopen cr to review it."
            }
        }
    }
}

/// Whether `err` says the review's log was replaced, which ends the review rather than failing
/// what ran into it.
pub(super) fn replaced_log(err: &review_core::Error) -> bool {
    matches!(err, review_core::Error::LogReplaced { .. })
}

impl Model {
    /// Starts following the working tree, when the review `follows` it; `opening` is what the
    /// checkout's opening capture saw.
    pub(super) fn start_live(&mut self, follows: bool, opening: Option<Opening>) {
        if !follows {
            return;
        }
        let mut live = Live::default();
        let mut association = None;
        if let Some(Opening {
            subject,
            filtered,
            busy,
            tree,
            association: found,
            commits,
        }) = opening
        {
            live.listed = subject
                .as_ref()
                .map(|subject| (subject.base.commit().cloned(), subject.head.clone()));
            live.subject = subject;
            live.filtered = filtered;
            live.busy = busy;
            live.tree = tree;
            association = found;
            if let Some(commits) = commits {
                self.info = Some(ReviewInfo::Commits { commits });
            }
        }
        self.live = Some(live);
        self.effect(Effect::Title(self.title()));
        self.associate(association);
        // Without `[review] live`, the checkout's review is captured again on `r` only.
        if !self.settings.live {
            return;
        }
        self.spawn_with(Lane::Shown, Subject::Watch, |parts| {
            Outcome::Watching(parts.watch().map(Box::new).map_err(|err| causes(&err)))
        });
    }

    /// Whether the review follows the working tree.
    #[must_use]
    pub fn following(&self) -> bool {
        self.live.is_some()
    }

    /// Takes in the watch that started, or why the working tree cannot be watched, and takes
    /// the first capture, which sees every change made before the watch started.
    pub(super) fn watching(&mut self, started: Result<Box<Waiting>, String>) {
        let Some(live) = &mut self.live else {
            return;
        };
        let every = match started {
            Ok(waiting) => {
                live.watched = true;
                live.walked = Some(Instant::now());
                self.wait_for_changes(*waiting);
                RECONCILE
            }
            Err(err) => {
                self.effect(Effect::Log(format!(
                    "cannot watch the working tree, so it is captured every {} s: {err}",
                    POLL.as_secs()
                )));
                POLL
            }
        };
        self.capture(false);
        self.after(
            every,
            Subject::Always(Because::Live),
            Outcome::Tick(Tick::Reconcile),
        );
    }

    /// Takes in a change of the working tree: captures it, and waits for the next.
    pub(super) fn waited(&mut self, waited: WaitedFor) {
        let WaitedFor {
            waited,
            problem,
            waiting,
        } = waited;
        let Some(live) = &mut self.live else {
            return;
        };
        if waited == Waited::Closed {
            return;
        }
        if let Some(trouble) = problem
            && !std::mem::replace(&mut live.troubled, true)
        {
            self.effect(Effect::Log(format!(
                "the working tree's watch may miss changes, which the capture every {} s \
                 catches: {trouble}",
                RECONCILE.as_secs()
            )));
        }
        self.wait_for_changes(waiting);
        match waited {
            Waited::Stirred => self.stirred(),
            Waited::Changed | Waited::Closed => self.changed(),
        }
    }

    /// Takes in the first change of a burst, as soon as the watch sees it: counts it, and
    /// until the burst settles, no capture shows.
    ///
    /// The view on screen, and a capture running, may have caught the change half-made, as
    /// when an editor saved one file of several, so the view shows as updating until the
    /// capture of the settled burst lands.
    pub(super) fn stirred(&mut self) {
        let Some(live) = &mut self.live else {
            return;
        };
        live.observed += 1;
        live.stirring = true;
        live.updating = true;
    }

    /// Takes in a change the watch saw, once its burst settled: counts it, and captures it.
    ///
    /// A capture already running may have missed the change, so the view on screen shows as
    /// updating until the capture of the change lands.
    pub(super) fn changed(&mut self) {
        let Some(live) = &mut self.live else {
            return;
        };
        live.observed += 1;
        live.stirring = false;
        if live.capturing {
            live.updating = true;
        }
        self.capture(false);
    }

    /// The periodic capture, which walks the watched tree again first every [`REWALK`].
    pub(super) fn reconcile(&mut self) {
        // A review whose checkout or log was replaced is never captured again.
        if self.replaced.is_some() {
            return;
        }
        let Some(live) = &mut self.live else {
            return;
        };
        let every = if live.watched { RECONCILE } else { POLL };
        let rewalk = live.walked.is_none_or(|walked| walked.elapsed() >= REWALK);
        if rewalk {
            live.walked = Some(Instant::now());
        }
        self.capture(rewalk);
        self.after(
            every,
            Subject::Always(Because::Live),
            Outcome::Tick(Tick::Reconcile),
        );
    }

    /// Captures the working tree in the background and compares it with the snapshot on
    /// screen; with `rewalk`, walks the watched tree again first.
    ///
    /// One capture runs at a time, so captures show in the order they were taken; a capture
    /// asked for meanwhile follows it.
    pub(super) fn capture(&mut self, rewalk: bool) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        if self.engine.is_none() {
            return;
        }
        if self.replaced.is_some() {
            return;
        }
        let known: HashSet<ObjectId> = self.blobs.texts.keys().cloned().collect();
        let from = self.shown;
        let Some(live) = &mut self.live else {
            return;
        };
        if live.capturing {
            live.again = Some(live.again.unwrap_or(false) || rewalk);
            return;
        }
        live.capturing = true;
        let observed = live.observed;
        let pull_requests = live.pulls.pull_requests();
        live.given = Some(pull_requests.clone());
        let listed = live.listed.clone();
        self.spawn_with(Lane::Shown, Subject::Always(Because::Live), move |parts| {
            Outcome::Captured(capture(
                parts,
                &CaptureInput {
                    rewalk,
                    shown: &snapshot,
                    from,
                    observed,
                    known: &known,
                    pull_requests: &pull_requests,
                    listed: listed.as_ref(),
                },
            ))
        });
    }

    /// Captures again because what a capture compares with may have changed, such as the
    /// branch's pull request: the view on screen shows as updating until that capture lands.
    pub(super) fn recapture_for_subject(&mut self) {
        let Some(live) = &mut self.live else {
            return;
        };
        live.observed += 1;
        live.updating = true;
        self.capture(false);
    }

    /// `r` on a review that follows the working tree: captures it again now, and looks up the
    /// branch's pull request; nothing is recorded.
    pub(super) fn recapture(&mut self) {
        if let Some(replaced) = self.replaced {
            self.fail(replaced.message());
            return;
        }
        let Some(live) = &mut self.live else {
            return;
        };
        live.asked = true;
        self.set_status(CAPTURING_AGAIN);
        self.capture(false);
        self.look_up_pull_request_now();
    }

    /// Why the view on screen takes no marks now, if it takes none: the checkout or the
    /// review's log was replaced.
    ///
    /// Neither a capture under way nor busy git stops a mark: it acts on the capture on screen,
    /// which the engine records as shown, and checks under the review's lock.
    pub(super) fn refusal(&self) -> Option<&'static str> {
        self.replaced.map(Replaced::refusal)
    }

    /// Ends the review in this window for good, as `replaced` says what was read from was
    /// replaced: nothing is captured, looked up or saved any more, the watch on the working
    /// tree stops, and what waits to show is dropped; another clone is never trusted.
    /// For another log, so are the marks not saved yet, which could land only in it.
    /// `unsaved` says a mark being saved was not; when marks were lost, quitting that waited
    /// for them stops, so the reviewer learns of it.
    pub(super) fn stop(&mut self, replaced: Replaced, unsaved: bool) {
        let stopped = self.replaced.is_some();
        let replaced = *self.replaced.get_or_insert(replaced);
        if replaced == Replaced::Clone && !stopped {
            self.take_trust(super::Trust::Denied);
        }
        if let Some(live) = &mut self.live {
            live.watched = false;
            live.waiting = None;
            live.again = None;
        }
        if !stopped {
            self.unwatch();
        }
        let mut unsaved = unsaved;
        if replaced == Replaced::Log {
            unsaved |= self
                .saves
                .iter()
                .any(|save| matches!(save, super::marks::Save::Mark(_)));
            self.saves
                .retain(|save| matches!(save, super::marks::Save::Trust(_)));
            self.pending.clear();
            self.refresh_tree();
            self.rebuild_in_place();
        }
        if stopped && !unsaved {
            return;
        }
        let mut status = vec![replaced.message()];
        if unsaved {
            status.push("Marks not saved yet are lost.");
            if let Some(hint) = self.stop_quitting() {
                status.push(hint);
            }
        }
        self.fail(status.join(" "));
    }

    /// Takes in a capture: keeps it to show with the next frame, and starts the capture asked
    /// for meanwhile.
    ///
    /// A capture taken before the newest observation never shows: the view on screen stays,
    /// as updating, and the capture asked for by that observation follows.
    pub(super) fn captured(&mut self, captured: Result<Box<Captured>, Missed>) {
        let Some(live) = &mut self.live else {
            return;
        };
        live.capturing = false;
        live.given = None;
        // A review that ended while the capture ran shows nothing more.
        if self.replaced.is_some() {
            return;
        }
        let mut again = live.again.take();
        match captured {
            Ok(mut captured) => {
                if let Some(trouble) = captured.trouble.take() {
                    self.effect(Effect::Log(format!(
                        "cannot walk the working tree again to watch it: {trouble}"
                    )));
                }
                let Some(live) = &mut self.live else {
                    return;
                };
                live.full = false;
                if live.stirring {
                    // The capture of the burst follows once it settles.
                    live.updating = true;
                } else if captured.observed < live.observed {
                    live.updating = true;
                    again = Some(again.unwrap_or(false));
                } else {
                    live.busy = None;
                    self.offer(captured);
                }
            }
            // The overview says what git is doing; what shows stays, and takes marks.
            Err(Missed::Busy(busy)) => live.busy = Some(busy),
            // What shows stays, and the status line says why nothing new does, once.
            Err(Missed::Full(full)) => {
                if !std::mem::replace(&mut live.full, true) {
                    self.fail(full);
                }
            }
            Err(Missed::Replaced(replaced)) => {
                again = None;
                self.stop(replaced, false);
            }
            Err(Missed::Failed(err)) => {
                self.effect(Effect::Log(format!(
                    "could not capture the working tree: {err}"
                )));
                self.sync_failed();
            }
        }
        if let Some(rewalk) = again {
            self.capture(rewalk);
        }
    }

    /// Keeps `captured` to show with the next frame when it was compared with the snapshot on
    /// screen, and takes another capture when it was compared with another snapshot or read
    /// before a mark was saved.
    fn offer(&mut self, captured: Box<Captured>) {
        if captured.from != self.shown || captured.history.version() < self.version {
            self.capture(false);
            return;
        }
        if let Some(live) = &mut self.live {
            live.waiting = Some(captured);
        }
    }

    /// Shows the opened review, or else, unless the reviewer is in the middle of something
    /// they would show under or pull the rows from under, the rows of a file whose text landed
    /// and what a job brought for its pending view or the waiting capture.
    ///
    /// The front end calls it in the turn that draws the next frame, and nowhere else, so a key
    /// always acts on what the last frame showed: never on a review, its verification or a
    /// capture that landed after it.
    /// One taken before a change seen since it waited never shows: the capture of that change
    /// is running or follows, and the view on screen is updating until it lands.
    pub fn publish(&mut self) {
        // Before the review opened, no rows showed that it could pull from under the reviewer.
        if let Some(opened) = self.opened.take() {
            self.show_opened(opened);
            return;
        }
        if self.live_waits() {
            return;
        }
        if std::mem::take(&mut self.text_waits) {
            self.rebuild();
        }
        if self.arrived() {
            self.show_arrived();
            return;
        }
        let Some(live) = &mut self.live else {
            return;
        };
        let Some(waiting) = live.waiting.take() else {
            return;
        };
        if waiting.observed < live.observed || live.stirring {
            live.updating = true;
            return;
        }
        if waiting.from != self.shown || waiting.history.version() < self.version {
            self.capture(false);
            return;
        }
        self.show_capture(*waiting);
    }

    /// Whether the opened review, a file's rows that waited for its text, what a job brought
    /// for its pending view, or a capture waits to show with the next frame and nothing holds
    /// it back, so the front end must draw one.
    #[must_use]
    pub fn publishes(&self) -> bool {
        let opened = self.opened.is_some() && !self.quitting.over();
        let waiting = self.text_waits
            || self.arrived()
            || self
                .live
                .as_ref()
                .is_some_and(|live| live.waiting.is_some());
        opened || (waiting && !self.live_waits())
    }

    /// Whether the reviewer is in the middle of something a new snapshot, or what a job brought
    /// for the pending view, would pull the rows from under.
    fn live_waits(&self) -> bool {
        self.saving
            || !self.saves.is_empty()
            || self.quitting.over()
            || self.overlay.is_some()
            || self.picker.is_some()
            || self.settings_panel.is_some()
            || self.dragging.is_some()
            || self.find.as_ref().is_some_and(|find| find.editing)
            || self.filter.as_ref().is_some_and(|filter| filter.editing)
            || !matches!(self.phase, Phase::Ready)
    }

    /// Shows the capture in place of the snapshot on screen, keeping the reviewer's place:
    /// the file, the hunk and the diff's top row, through the correspondence between the two;
    /// folds only of changes that stayed the same at a trusted position.
    fn show_capture(&mut self, captured: Captured) {
        let Captured {
            history,
            links,
            texts,
            subject,
            filtered,
            tree,
            association,
            commits,
            ..
        } = captured;
        let (Some(old), Some(tip)) = (self.snapshot.clone(), history.current()) else {
            return;
        };
        let old_tree = self.live.as_ref().and_then(|live| live.tree.as_ref());
        let same_code = same_code(&old, old_tree, &history, tip, tree.as_ref());
        let moved_to = subject
            .as_ref()
            .and_then(|subject| Some((subject.branch.clone()?, subject.head.clone()?)));
        let news = self.observe(Observation {
            subject,
            filtered,
            tree,
            commits,
        });
        self.associate(association);
        let asked = self
            .live
            .as_mut()
            .is_some_and(|live| std::mem::take(&mut live.asked));
        if links.is_empty() && same_snapshot(&old.history, &history, old.revision, tip) {
            // The same snapshot read again, such as after a mark recorded it, or with marks or
            // trust another window saved since: what shows is derived again, and its trust is
            // read again.
            self.read_trust();
            if history.version() > self.version {
                self.version = history.version();
                self.revisions = super::overview::summarize(&history, old.policy);
                if let Some(snapshot) = &mut self.snapshot {
                    snapshot.history = history;
                }
                self.refresh_tree();
                self.rebuild();
            }
            match news {
                // Nothing replaces a failure that stopped the quitting.
                _ if self.quitting == Quitting::Stopped => {}
                Some(news) => self.set_status(news),
                None if asked && self.status == CAPTURING_AGAIN => {
                    self.set_view_status(format!("Up to date. {}", self.summary()));
                }
                None => {}
            }
            return;
        }
        let Ok(view) = history.view_under(tip, old.policy) else {
            return;
        };
        let moved = Moved::new(&links);
        let was = self.was_on();

        self.snapshot = Some(Snapshot {
            history: Arc::clone(&history),
            revision: tip,
            policy: old.policy,
        });
        self.shown += 1;
        self.version = self.version.max(history.version());
        self.blobs.texts.extend(texts);
        self.blobs.retain_for(&crate::rows::Source::Verified(view));
        let carry = |units: &HashSet<UnitId>| -> HashSet<UnitId> {
            units
                .iter()
                .filter_map(|&unit| moved.unchanged(unit.index()))
                .map(|index| view.id(index))
                .collect()
        };
        self.folds.units = carry(&self.folds.units);
        self.folds.opened = carry(&self.folds.opened);
        self.folds.expanded = carry(&self.folds.expanded);
        // The snapshot on screen was laid out from its view, so it has one.
        let old_view = old.view();
        self.keep_place(&was, old_view.as_ref(), &view, &moved);
        // Lookups of the snapshot before end, and trust is read again, as another window may
        // have revoked it.
        self.next_epoch();
        self.read_trust();
        self.request_syntax();
        let changed = self.what_changed(&view, moved_to.filter(|_| same_code));
        self.announce_capture(news, changed, asked);
    }

    /// Says on the status line what the capture that shows now brought: `news` about its
    /// subject and what `changed` about it.
    ///
    /// A capture that only brings edits says nothing, so the status line keeps the outcome of
    /// the reviewer's last action; what it said about the snapshot before, such as that a file
    /// is done, clears, since the header's progress tells how this one stands. A refresh the
    /// reviewer `asked` for answers while its request still shows.
    /// Nothing replaces a failure that stopped the quitting, which the reviewer must see.
    fn announce_capture(&mut self, news: Option<String>, changed: Option<String>, asked: bool) {
        if self.quitting == Quitting::Stopped {
            return;
        }
        let status = match (news, changed) {
            (Some(news), Some(changed)) => Some(format!("{news} {changed}")),
            (news, changed) => news.or(changed),
        };
        match status {
            Some(status) => self.set_status(status),
            None if asked && self.status == CAPTURING_AGAIN => {
                self.set_view_status(format!("Updated. {}", self.summary()));
            }
            None if self.about_view => self.set_status(""),
            None => {}
        }
    }

    /// What the status line says about the capture `view` that shows now: why there is nothing
    /// to review in it, or that the branch only moved to `moved_to` with the code the same.
    fn what_changed(
        &self,
        view: &RevisionView<'_>,
        moved_to: Option<(String, ObjectId)>,
    ) -> Option<String> {
        let nothing = self
            .live
            .as_ref()
            .and_then(|live| live.subject.as_ref())
            .zip(view.record())
            .filter(|_| view.unit_ids().is_empty())
            .map(|(subject, record)| subject.nothing_to_review(&record.base, &record.head));
        match (nothing, moved_to) {
            (Some(nothing), _) => Some(nothing),
            (None, Some((branch, head))) => Some(format!(
                "{branch} is at {} now; the code is the same. {}",
                abbreviated(&head),
                self.summary()
            )),
            (None, None) => None,
        }
    }

    /// Takes in what the capture about to show was of: the subject, which the header and the
    /// title show, the filtered paths, the tree and the commits; returns what changed about the
    /// subject, for the status line.
    fn observe(&mut self, observation: Observation) -> Option<String> {
        let Observation {
            subject,
            filtered,
            tree,
            commits,
        } = observation;
        let title_before = self.title();
        if let Some(commits) = commits {
            self.info = Some(ReviewInfo::Commits { commits });
        }
        let live = self.live.as_mut()?;
        live.updating = false;
        live.tree = tree;
        let filtered_news = (filtered.len() > live.filtered.len())
            .then(|| review_core::filtered_note(filtered.len()))
            .flatten()
            .map(|note| format!("{note}."));
        live.filtered = filtered;
        let subject = subject?;
        live.listed = Some((subject.base.commit().cloned(), subject.head.clone()));
        let news = live
            .subject
            .as_ref()
            .and_then(|before| subject_news(before, &subject));
        live.subject = Some(subject);
        // The title names the branch and the pull request the capture used.
        let title = self.title();
        if title != title_before {
            self.effect(Effect::Title(title));
        }
        match (news, filtered_news) {
            (Some(news), Some(filtered)) => Some(format!("{news} {filtered}")),
            (news, filtered) => news.or(filtered),
        }
    }

    /// Where the reviewer is in the snapshot on screen.
    fn was_on(&self) -> WasOn {
        let offset = self.diff_scroll.target();
        let in_review_buffer = self.mode == Mode::Buffer
            && self
                .buffer
                .as_ref()
                .is_some_and(|buffer| matches!(buffer.origin, Origin::Review(_)));
        // In a review buffer, the hunk under its cursor is the one the keys act on.
        let hunk = match self.buffer_hunk() {
            Some(hunk) if in_review_buffer => hunk,
            _ => self.hunk,
        };
        WasOn {
            hunk: self.rows.hunks.get(hunk).cloned(),
            hunk_line: self
                .hunk_rows
                .get(hunk)
                .and_then(|rows| self.rows.row_on_screen(rows.start))
                .and_then(|row| RowKey::of(row).head()),
            top: self
                .rows
                .row_on_screen(Anchor::at(offset).row)
                .map(RowKey::of),
            anchor: diff_anchor(&self.rows, offset).top,
            buffer: self
                .buffer
                .as_ref()
                .filter(|_| in_review_buffer)
                .map(|buffer| buffer.cursor),
            tree: self.tree_node().map(|node| TreeSpot {
                kind: node.kind,
                path: node.path.clone(),
            }),
        }
    }

    /// Lays out the new snapshot where the reviewer `was`, coming from the snapshot `old`: the
    /// same file, or the one now where it was in the tree; the same hunk, or the nearest; the
    /// same top row; the tree cursor on the same node; and a review buffer the reviewer was
    /// in, on the same hunk.
    ///
    /// Buffers of the old snapshot's files go, and so does the way back from a definition into
    /// one, since their change markers number the old snapshot's hunks.
    fn keep_place(
        &mut self,
        was: &WasOn,
        old: Option<&RevisionView<'_>>,
        view: &RevisionView<'_>,
        moved: &Moved,
    ) {
        self.selection = None;
        let reviewed = |buffer: &super::buffer::Buffer| matches!(buffer.origin, Origin::Review(_));
        if self.buffer.as_ref().is_some_and(reviewed) {
            self.set_buffer(None);
        } else if let Some(buffer) = &mut self.buffer {
            buffer.previous = None;
        }
        self.rows = Arc::new(Layout::default());
        self.hunk_rows.clear();
        self.refresh_tree();
        if let Some(spot) = &was.tree {
            self.put_tree_cursor(spot.kind, &spot.path);
        }

        if !self
            .selected
            .as_deref()
            .is_some_and(|path| state::is_file(&self.tree, path))
        {
            self.selected = self.file_near_cursor();
        }
        // The place taken from the screen is the shown file's; behind a folder's diff, the
        // selected file keeps its own, which follows its units below as every other does.
        if let Some(path) = self.selected.clone().filter(|_| self.folder.is_none()) {
            self.places.remove(&path);
        }
        // Places kept for other files name units of the old snapshot; each follows its unit to
        // the new one, or forgets it when the unit is gone.
        let follow =
            |unit: review_core::UnitId| moved.any(unit.index()).map(|index| view.id(index));
        for place in self.places.values_mut() {
            place.hunk = place
                .hunk
                .take()
                .and_then(|hunk| carried(&hunk, old, view, moved));
            place.key = place
                .key
                .and_then(|key| follow(key.unit()).map(|unit| key.with_unit(unit)));
        }

        self.hunk = 0;
        self.build_selected();
        let followed = was
            .hunk
            .as_ref()
            .and_then(|hunk| carried(hunk, old, view, moved))
            .and_then(|hunk| self.rows.hunk_like(&hunk));
        self.hunk = followed.unwrap_or_else(|| self.nearest_hunk(was.hunk_line));
        // A review key held on the old snapshot goes on acting on what it began on, there too.
        if let Some(press) = &mut self.press {
            press.target = press
                .target
                .take()
                .and_then(|aim| carried_aim(aim, old, view, moved));
        }

        let top = was
            .top
            .and_then(|key| {
                let unit = moved.any(key.unit().index()).map(|index| view.id(index));
                key.find_moved(unit, &self.rows)
            })
            .map_or(was.anchor, |row| Anchor { row, ..was.anchor });
        let viewport = self.grid().viewports.diff;
        self.diff_scroll.stop();
        self.diff_scroll.jump(frame::clamp(
            diff_offset(&self.rows, DiffAnchor::line(top)),
            self.rows.screen.len(),
            viewport,
        ));

        if let Some(cursor) = was.buffer {
            // The buffer opens on the hunk the keys acted on; the old row stays only while it
            // is still on that hunk.
            self.enter_buffer();
            if let Some(start) = self.buffer.as_ref().map(|buffer| buffer.cursor) {
                self.set_buffer_cursor(cursor.row, cursor.col);
                if was.hunk.is_some() && self.buffer_hunk() != Some(self.hunk) {
                    self.set_buffer_cursor(start.row, start.col);
                }
            }
        }
        if self.selected.is_none() {
            self.enter_mode(Mode::Tree);
        } else if self.mode == Mode::Buffer && self.buffer.is_none() {
            self.enter_mode(Mode::Diff);
        }
        // Outside the tree, its cursor stays on the file that shows.
        if self.mode != Mode::Tree
            && let Some(path) = self.selected.clone()
        {
            self.put_tree_cursor(review_core::tree::NodeKind::File, &path);
        }
        // The folder under the cursor shows the files it has now, before anything reads what
        // the keys act on.
        self.sync_folder_preview();
    }

    /// Puts the tree cursor on the visible node of `kind` at `path`, when there is one.
    fn put_tree_cursor(&mut self, kind: review_core::tree::NodeKind, path: &str) {
        let visible = self.visible_tree();
        if let Some(position) = visible.iter().position(|&index| {
            self.tree
                .get(index)
                .is_some_and(|node| node.kind == kind && node.path == path)
        }) {
            self.tree_cursor = position;
        }
    }

    /// The file at the tree cursor, or the nearest file before it, or the first.
    fn file_near_cursor(&self) -> Option<String> {
        let visible = self.visible_tree();
        let cursor = self.tree_cursor.min(visible.len().saturating_sub(1));
        let file = |index: &usize| {
            self.tree
                .get(*index)
                .filter(|node| node.kind == review_core::tree::NodeKind::File)
                .map(|node| node.path.clone())
        };
        visible
            .get(..=cursor)
            .and_then(|before| before.iter().rev().find_map(file))
            .or_else(|| visible.iter().find_map(file))
    }

    /// The hunk whose first row is nearest the head line `line`, or the first.
    fn nearest_hunk(&self, line: Option<usize>) -> usize {
        let Some(line) = line else {
            return 0;
        };
        self.hunk_rows
            .iter()
            .enumerate()
            .filter_map(|(hunk, rows)| {
                let head = self
                    .rows
                    .row_on_screen(rows.start)
                    .and_then(|row| RowKey::of(row).head())?;
                Some((hunk, head.abs_diff(line)))
            })
            .min_by_key(|&(_, distance)| distance)
            .map_or(0, |(hunk, _)| hunk)
    }
}

/// Where the reviewer was in the snapshot on screen, to keep the place in the next one.
struct WasOn {
    /// The selected hunk.
    hunk: Option<crate::rows::Hunk>,
    /// The head line the selected hunk starts on.
    hunk_line: Option<usize>,
    /// The diff's top row.
    top: Option<RowKey>,
    /// Where the diff is scrolled.
    anchor: Anchor,
    /// The cursor of the buffer of a file under review, while the reviewer is in it.
    buffer: Option<crate::text::TextPos>,
    /// The tree node under the cursor.
    tree: Option<TreeSpot>,
}

/// A node of the file tree, by what it is and where.
struct TreeSpot {
    kind: review_core::tree::NodeKind,
    path: String,
}

/// What a capture brings besides its history, for [`Model::observe`].
struct Observation {
    subject: Option<review_core::Subject>,
    filtered: Vec<String>,
    tree: Option<ObjectId>,
    commits: Option<Vec<Commit>>,
}

/// What changed from the subject `before` to `now` that the status line says: another branch,
/// a detached `HEAD`, another base, or a shallow history that keeps the base out of reach;
/// `None` when only the commits moved.
fn subject_news(before: &review_core::Subject, now: &review_core::Subject) -> Option<String> {
    if before.branch != now.branch {
        return Some(match (&now.branch, &now.head) {
            (Some(branch), _) => format!("Now on {branch}."),
            (None, Some(head)) => format!("HEAD is detached at {}.", abbreviated(head)),
            (None, None) => "HEAD is detached.".to_owned(),
        });
    }
    if before.missing_base != now.missing_base
        && let Some(missing) = &now.missing_base
    {
        return Some(format!(
            "The pull request's base {missing} is not available locally; comparing with {}.",
            now.base.name().unwrap_or("the empty tree")
        ));
    }
    if let review_core::BaseRef::Shallow { name } = &now.base
        && before.base != now.base
    {
        return Some(format!(
            "The history is shallow; fetch more to compare with {name}."
        ));
    }
    if before.base.name() != now.base.name() {
        return Some(match now.base.name() {
            Some(name) => format!("The base is now {name}."),
            None => "The base is now the empty tree.".to_owned(),
        });
    }
    None
}

/// What a capture job needs besides the engine.
struct CaptureInput<'a> {
    /// Whether the watched tree is walked again first.
    rewalk: bool,
    shown: &'a Snapshot,
    from: u64,
    observed: u64,
    known: &'a HashSet<ObjectId>,
    /// What was last found for each branch.
    pull_requests: &'a [review_core::github::CheckoutPullRequest],
    /// The base and head the commits were last listed between.
    listed: Option<&'a (Option<ObjectId>, Option<ObjectId>)>,
}

/// Captures the working tree and compares it with the snapshot shown; runs in the background.
fn capture(parts: &Parts, input: &CaptureInput<'_>) -> Result<Box<Captured>, Missed> {
    let review = &parts.review;
    let failed = |err: &dyn std::error::Error| Missed::Failed(causes(err));
    // Without a watch, or with one that could not watch every directory since it started,
    // nothing says a capture overlapped a change, so it looks twice.
    let watch = parts.watch.lock().started().cloned();
    let trouble = watch
        .as_ref()
        .filter(|_| input.rewalk)
        .and_then(|watch| watch.rewalk().err())
        .map(|err| causes(&err));
    let confirm = watch.as_ref().is_none_or(|watch| !watch.covers_all());
    drop(watch);
    let captured = live_capture(parts, input, confirm);
    let LiveCapture {
        history,
        subject,
        filtered,
    } = captured?;
    let missing = |what: &str| Missed::Failed(what.to_owned());
    let tip = history
        .current()
        .ok_or_else(|| missing("the review has no revision"))?;
    let links = if same_snapshot(&input.shown.history, &history, input.shown.revision, tip) {
        Vec::new()
    } else {
        let shown = input
            .shown
            .view()
            .map(|view| view.analysis())
            .ok_or_else(|| missing("the review on screen has no revision"))?;
        let captured = history
            .view(tip)
            .map(|view| view.analysis())
            .map_err(|err| failed(&err))?;
        correspond(review.snapshots(), shown, captured)
            .map_err(|err| failed(&err))?
            .links
    };
    let texts = read_new_texts(review, &history, tip, input.known);
    let tree = provisional_or_tip_tree(review, &history);
    let checkout = matches!(review.target(), ReviewTarget::Checkout { .. });
    let (association, commits) = if checkout {
        let association = subject
            .branch
            .as_deref()
            .and_then(|branch| review.repo().branch_identity(branch).ok());
        let pair = (subject.base.commit().cloned(), subject.head.clone());
        let commits = (input.listed != Some(&pair))
            .then(|| review.subject_commits(&subject).ok())
            .flatten();
        (association, commits)
    } else {
        (None, None)
    };
    Ok(Box::new(Captured {
        from: input.from,
        observed: input.observed,
        subject: checkout.then_some(subject),
        filtered,
        tree,
        association,
        commits,
        history: Arc::new(history),
        links,
        texts,
        trouble,
    }))
}

/// Captures the working tree, looking twice with `confirm`, and again while git moves under
/// it, a few times at most.
fn live_capture(
    parts: &Parts,
    input: &CaptureInput<'_>,
    confirm: bool,
) -> Result<LiveCapture, Missed> {
    let review = &parts.review;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let captured = if confirm {
            review.confirmed_live_history(input.pull_requests)
        } else {
            review.live_history(input.pull_requests)
        }
        .map_err(|err| {
            if replaced_log(&err) {
                Missed::Replaced(Replaced::Log)
            } else if matches!(err, review_core::Error::StorageFull { .. }) {
                Missed::Full(err.to_string())
            } else {
                Missed::Failed(causes(&err))
            }
        })?;
        match captured {
            Capture::Captured(captured) => return Ok(*captured),
            Capture::Moved if attempts < MOVED_ATTEMPTS => {}
            Capture::Moved => {
                return Err(Missed::Failed(
                    "git kept changing while it was captured".to_owned(),
                ));
            }
            Capture::Busy(busy) => return Err(Missed::Busy(busy)),
            Capture::Replaced => {
                // No grant covers another clone's code, so its language servers stop at once.
                parts.servers.stop();
                return Err(Missed::Replaced(Replaced::Clone));
            }
        }
    }
}

/// The text of revision `tip`'s head files that `known` does not hold, so what shows of it
/// shows whole, without its context lines arriving later.
pub(super) fn read_new_texts(
    review: &review_core::Review,
    history: &History,
    tip: review_core::RevisionNumber,
    known: &HashSet<ObjectId>,
) -> HashMap<ObjectId, Arc<FullText>> {
    history.view(tip).map_or_else(
        |_| HashMap::new(),
        |view| {
            super::head_texts(review, view.analysis(), |file| {
                file.new
                    .as_ref()
                    .is_some_and(|side| !known.contains(&side.oid))
            })
        },
    )
}

/// Whether revision `old` of `before` and revision `new` of `after` are the same snapshot:
/// the same number, base and head.
pub(super) fn same_snapshot(
    before: &History,
    after: &History,
    old: review_core::RevisionNumber,
    new: review_core::RevisionNumber,
) -> bool {
    let record = |history: &History, number| {
        history.view(number).ok().and_then(|view| {
            view.record()
                .map(|record| (record.base.clone(), record.head.clone()))
        })
    };
    old == new && record(before, old).is_some() && record(before, old) == record(after, new)
}

/// Whether revision `tip` of `history`, whose tree is `tree`, holds the same code as the
/// snapshot `old`, whose tree is `old_tree`: the same tree against the same base, as after a
/// commit.
fn same_code(
    old: &Snapshot,
    old_tree: Option<&ObjectId>,
    history: &History,
    tip: review_core::RevisionNumber,
    tree: Option<&ObjectId>,
) -> bool {
    let base =
        |view: Option<RevisionView<'_>>| view.and_then(|view| Some(view.record()?.base.clone()));
    tree.is_some() && tree == old_tree && base(old.view()) == base(history.view(tip).ok())
}

/// `hunk` of the snapshot `old` as the snapshot `new` has it, `moved` linking their units.
///
/// The unit it went to along an unchanged link shows the same changes since review, their lines
/// moved as the unit's did, so the hunk is found again as itself. A hunk of a unit that changed
/// keeps its lines, a place to read on from that no review key takes for the same change.
fn carried(
    hunk: &crate::rows::Hunk,
    old: Option<&RevisionView<'_>>,
    new: &RevisionView<'_>,
    moved: &Moved,
) -> Option<crate::rows::Hunk> {
    let unit = new.id(moved.any(hunk.unit.index())?);
    let change = hunk.change.as_ref().map(|change| {
        carried_change(change, old, new, moved).unwrap_or_else(|| ShownRun {
            unit,
            ..change.clone()
        })
    });
    Some(crate::rows::Hunk { unit, change })
}

/// `change` of the snapshot `old` as the snapshot `new` has it, when its unit is linked
/// unchanged: its lines moved as far as the unit's.
fn carried_change(
    change: &ShownRun,
    old: Option<&RevisionView<'_>>,
    new: &RevisionView<'_>,
    moved: &Moved,
) -> Option<ShownRun> {
    let unit = new.id(moved.unchanged(change.unit.index())?);
    let start = |view: &RevisionView<'_>, unit| Some(view.unit(unit).ok()?.chunk()?.new.start);
    let mut carried = ShownRun {
        unit,
        ..change.clone()
    };
    if let (Some(from), Some(to)) = (
        old.and_then(|old| start(old, change.unit)),
        start(new, unit),
    ) {
        carried.new.start = (carried.new.start + to).saturating_sub(from);
    }
    Some(carried)
}

/// What a review key acted on in the snapshot `old` as the snapshot `new` has it: only along
/// unchanged links, since anything else is new under the key.
fn carried_aim(
    aim: Aim,
    old: Option<&RevisionView<'_>>,
    new: &RevisionView<'_>,
    moved: &Moved,
) -> Option<Aim> {
    match aim {
        Aim::Change(change) => carried_change(&change, old, new, moved).map(Aim::Change),
        Aim::Units(units) => units
            .iter()
            .map(|unit| Some(new.id(moved.unchanged(unit.index())?)))
            .collect::<Option<_>>()
            .map(Aim::Units),
    }
}

/// The correspondence from the old snapshot to the new one, by unit index.
struct Moved {
    /// Where each old unit went, by any link that is not ambiguous.
    any: HashMap<UnitIndex, UnitIndex>,
    /// Where each old unit went unchanged, at a trusted position.
    unchanged: HashMap<UnitIndex, UnitIndex>,
}

impl Moved {
    fn new(links: &[Link]) -> Self {
        let mut moved = Self {
            any: HashMap::new(),
            unchanged: HashMap::new(),
        };
        for link in links {
            if link.kind != LinkKind::Ambiguous {
                moved.any.entry(link.older).or_insert(link.newer);
            }
            if link.kind == LinkKind::Unchanged {
                moved.unchanged.insert(link.older, link.newer);
            }
        }
        moved
    }

    fn any(&self, older: UnitIndex) -> Option<UnitIndex> {
        self.any.get(&older).copied()
    }

    fn unchanged(&self, older: UnitIndex) -> Option<UnitIndex> {
        self.unchanged.get(&older).copied()
    }
}

/// `err` and every cause under it, joined by `: `.
pub(super) fn causes(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
impl Live {
    /// Whether a capture waits for the reviewer.
    pub(super) fn has_waiting(&self) -> bool {
        self.waiting.is_some()
    }
}
