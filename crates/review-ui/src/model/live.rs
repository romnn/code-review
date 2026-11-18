//! Following a working-tree review live: watching the working tree, capturing it when it
//! changes, and showing each capture in place of the snapshot on screen.
//!
//! A capture is compared with the snapshot on screen when it is taken, and lands only on that
//! snapshot: the correspondence between the two carries the reviewer's place and folds over,
//! and tells which hunks changed.
//! It waits while the reviewer is in the middle of something it would pull the rows from
//! under: marks being saved, a picker or overlay, a drag, a search being typed.
//! Only the newest waiting capture is kept, and one compared with a snapshot no longer on
//! screen, or read before a mark was saved, is taken again.
//!
//! A review key never acts on a hunk that changed under it: a mark that covers a hunk is
//! refused for a moment after the hunk changed, and for as long as a key pressed before the
//! change keeps repeating.
//! A terminal sends a held key as repeated presses, so a press is told from a held key by the
//! time between them.
//!
//! Every change the watch reports, and every change of the branch's pull request, counts one
//! more observation, and a capture carries the count it was taken at.
//! The watch reports a burst of changes twice: as soon as its first change arrives, and once it
//! settles, when the capture of it is taken.
//! One taken before the newest observation may miss it, so it never shows, and neither does one
//! that lands while a burst is under way: the view on screen stays, marked as updating, and
//! refuses marks until a capture of the newest observation lands.
//! From a burst's first change, the view on screen refuses marks too, since the capture it shows
//! may have caught the change half-made.
//! Without a watch, as while it starts or when the working tree cannot be watched, and once the
//! watch could not watch a directory, each capture looks twice and shows only when both looks
//! saw the same.
//! Nor does a view take marks while git is busy in the checkout, or once the checkout is
//! another clone.
//!
//! What a capture was of, its subject, shows with it: the branch, its pull request and the
//! base, which the header names and the status line announces when they change.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use review_core::correspond::{Link, LinkKind, correspond};
use review_core::{
    Busy, Capture, Commit, History, LiveCapture, ObjectId, ReviewInfo, ReviewTarget,
    RevisionRecord, RevisionView, Session, UnitId, UnitIndex,
};

use super::buffer::Origin;
use super::effects::{Because, Completion, Outcome, Subject};
use super::present::{Anchor, RowKey, diff_anchor, diff_offset};
use super::pulls::Pulls;
use super::sync::Tick;
use super::{Effect, Grant, Mode, Model, Phase, is_regular};
use crate::frame;
use crate::rows::{FullText, Layout};
use crate::state::{self, Snapshot};
use crate::watch::{Waited, Waiter, Watch};

/// How often the working tree is captured whatever the watch saw, catching what it cannot see:
/// a ref moving without the index changing, a submodule's commit, events the system dropped.
const RECONCILE: Duration = Duration::from_secs(30);

/// How often the working tree is captured when it cannot be watched.
const POLL: Duration = Duration::from_secs(5);

/// How often the periodic capture walks the watched tree again first, catching a directory
/// whose events were lost without a word; a walk asks git about every directory, about half a
/// second on a checkout of 13,000 files, so it runs far less often than the capture.
const REWALK: Duration = Duration::from_secs(300);

/// How many times a capture is taken again at once when git moved while it was taken.
const MOVED_ATTEMPTS: u32 = 3;

/// How long a hunk that changed refuses review keys.
const SETTLE: Duration = Duration::from_secs(1);

/// Review keys closer together than this are one key held down.
const REPEAT: Duration = Duration::from_millis(500);

/// Following the working tree.
#[derive(Default)]
pub(crate) struct Live {
    /// The session that pins the captures on screen until the window closes.
    session: Option<Arc<Session>>,
    /// The watch on the working tree, once it started; `None` while it starts or when the
    /// working tree cannot be watched, which captures it every [`POLL`] instead.
    watch: Option<Arc<Watch>>,
    /// Whether a capture runs.
    capturing: bool,
    /// Whether another capture follows the running one, and whether it walks the working
    /// tree again first.
    again: Option<bool>,
    /// The newest capture that waits for the reviewer.
    waiting: Option<Box<Captured>>,
    /// When each unit on screen last changed, for those that changed since the review opened.
    changed: HashMap<UnitId, Instant>,
    /// The review key held or pressed last.
    press: Option<Press>,
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
    /// Whether the view on screen may miss an observation, so it takes no marks until a
    /// capture of the newest one lands.
    updating: bool,
    /// What git was busy with at the last capture, which keeps the view on screen.
    busy: Option<Busy>,
    /// Whether the checkout is another clone now, which ends the following for good.
    replaced: bool,
    /// What the capture on screen was of.
    subject: Option<review_core::Subject>,
    /// The paths the capture on screen kept at their staged version, under a filter driver.
    filtered: Vec<String>,
    /// The tree of the capture on screen, which a grant of trust names.
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
    /// Whether a capture looks at the working tree twice: without a watch, or with one that
    /// could not watch every directory since it started, nothing says a capture overlapped a
    /// change.
    pub(crate) fn confirms(&self) -> bool {
        self.watch.as_ref().is_none_or(|watch| !watch.covers_all())
    }

    /// What was found of the pull requests when the running capture was taken, while one runs.
    pub(crate) fn given(&self) -> Option<&[review_core::github::CheckoutPullRequest]> {
        self.given.as_deref()
    }

    /// Whether a capture runs, which review-gui's QA mode waits for.
    pub(crate) fn capturing(&self) -> bool {
        self.capturing
    }

    /// Whether the view on screen may miss a change and takes no marks until it is captured.
    pub(crate) fn updating(&self) -> bool {
        self.updating
    }

    /// What git is busy with in the checkout, which keeps the view on screen.
    pub(crate) fn busy(&self) -> Option<Busy> {
        self.busy
    }

    /// Whether the checkout is another clone now, so nothing is captured any more.
    pub(crate) fn replaced(&self) -> bool {
        self.replaced
    }

    /// What the capture on screen was of.
    pub(crate) fn subject(&self) -> Option<&review_core::Subject> {
        self.subject.as_ref()
    }

    /// The paths the capture on screen kept at their staged version, under a filter driver.
    pub(crate) fn filtered(&self) -> &[String] {
        &self.filtered
    }

    /// The tree of the capture on screen, which a grant of trust names.
    pub(crate) fn tree(&self) -> Option<&ObjectId> {
        self.tree.as_ref()
    }
}

/// How a review that follows the checkout starts: the session its opening capture is pinned
/// under, and what that capture saw; see [`super::Loaded::following`].
pub struct Following {
    /// The session, held for as long as the window shows the review.
    pub session: Session,
    /// What the opening capture was of, or `None` when git was busy and the review opened on
    /// its last recorded revision.
    pub subject: Option<review_core::Subject>,
    /// The paths the opening capture kept at their staged version.
    pub filtered: Vec<String>,
    /// What git was busy with when the review opened, if it was.
    pub busy: Option<Busy>,
}

/// What the background work of opening found out about a review that follows the checkout.
pub(super) struct Opening {
    following: Following,
    tree: Option<ObjectId>,
    association: Option<review_core::BranchIdentity>,
    commits: Option<Vec<Commit>>,
}

impl Opening {
    /// Reads what the window needs about `following`'s capture, which `history` ends in: the
    /// tree the trust names, the branch and upstream, and the commits.
    pub(super) fn read(
        review: &review_core::Review,
        history: &History,
        following: Following,
    ) -> (Self, Option<Granted>) {
        let tree = provisional_or_tip_tree(review, history);
        let checkout = matches!(review.target(), ReviewTarget::Checkout { .. });
        let granted = (checkout && following.subject.is_some())
            .then(|| tree.as_ref().map(|tree| granted(review, tree, 0)))
            .flatten();
        let subject = following.subject.as_ref().filter(|_| checkout);
        let association = subject
            .and_then(|subject| subject.branch.as_deref())
            .and_then(|branch| review.repo().branch_identity(branch).ok());
        let commits = subject.and_then(|subject| review.subject_commits(subject).ok());
        (
            Self {
                following,
                tree,
                association,
                commits,
            },
            granted,
        )
    }
}

impl Opening {
    /// How the review opened, for the status line: what it follows, or that git is busy.
    pub(super) fn describe(&self) -> Option<String> {
        if let Some(busy) = self.following.busy {
            return Some(format!(
                "git is busy ({busy}); the last recorded revision shows until it finishes"
            ));
        }
        let subject = self.following.subject.as_ref()?;
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

/// What a grant of trust lets run on a snapshot of the checkout, read in the background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Granted {
    /// The grant that applies, if any does.
    pub(super) grant: Option<Grant>,
    /// The window's decisions on trust counted when it was read; see
    /// [`Model::trust_decisions`].
    pub(super) decisions: u64,
}

/// Which grant lets tools run on the snapshot whose tree is `tree`, of a review of a checkout:
/// the whole checkout's before the snapshot's own; none when the log cannot be read.
/// `decisions` is the window's count of decisions on trust when it is read.
pub(super) fn granted(review: &review_core::Review, tree: &ObjectId, decisions: u64) -> Granted {
    let checkout = review.trust().is_ok_and(|trust| trust.trusted);
    let snapshot = review.trust_for(tree).is_ok_and(|trust| trust.trusted);
    Granted {
        grant: if checkout {
            Some(Grant::Checkout)
        } else if snapshot {
            Some(Grant::Snapshot)
        } else {
            None
        },
        decisions,
    }
}

/// The tree of the head of `history`'s current revision.
fn provisional_or_tip_tree(review: &review_core::Review, history: &History) -> Option<ObjectId> {
    let tip = history.current()?;
    let head = history.view(tip).ok()?.record()?.head.clone();
    review.repo().tree_of(&head).ok()
}

/// A review key pressed, and held for as long as it repeats.
#[derive(Debug, Clone, Copy)]
struct Press {
    started: Instant,
    last: Instant,
}

/// A watch that started, with its waiter.
pub(super) struct Started {
    watch: Watch,
    waiter: Waiter,
}

/// What a wait for the working tree saw, with the waiter for the next wait.
pub(super) struct WaitedFor {
    waiter: Waiter,
    waited: Waited,
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
    /// The grant of trust that applies to it, for a review of a checkout.
    granted: Option<Granted>,
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
    /// The checkout is another clone now.
    Replaced,
    /// The capture failed, for this reason.
    Failed(String),
}

impl Model {
    /// Starts following the working tree, for a review of it with `[review] live` on, under
    /// the session the review opened with, if it opened with one, or a new one.
    pub(super) fn start_live(&mut self, opening: Option<Opening>) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let follows = matches!(
            review.target(),
            ReviewTarget::Worktree { .. } | ReviewTarget::Checkout { .. }
        );
        // A checkout's review shows only captures, so it captures the working tree on `r`
        // whatever the configuration says, and follows it as it changes with `[review] live`.
        let checkout = matches!(review.target(), ReviewTarget::Checkout { .. });
        if !follows || !(self.settings.live || checkout) {
            return;
        }
        let (session, opening) = match opening {
            Some(Opening {
                following,
                tree,
                association,
                commits,
            }) => {
                let Following {
                    session,
                    subject,
                    filtered,
                    busy,
                } = following;
                (
                    session,
                    Some((subject, filtered, busy, tree, association, commits)),
                )
            }
            None => match review.start_session() {
                Ok(session) => (session, None),
                Err(err) => {
                    self.effect(Effect::Log(format!(
                        "cannot follow the working tree: {}",
                        causes(&err)
                    )));
                    return;
                }
            },
        };
        let mut live = Live {
            session: Some(Arc::new(session)),
            ..Live::default()
        };
        let mut association = None;
        if let Some((subject, filtered, busy, tree, found, commits)) = opening {
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
        let repo = review.repo().clone();
        self.spawn(Subject::Always(Because::Live), move || {
            Outcome::Watching(
                Watch::start(repo)
                    .map(|(watch, waiter)| Box::new(Started { watch, waiter }))
                    .map_err(|err| causes(&err)),
            )
        });
    }

    /// Whether the review follows the working tree.
    #[must_use]
    pub fn following(&self) -> bool {
        self.live.is_some()
    }

    /// Takes in the watch that started, or why the working tree cannot be watched, and takes
    /// the first capture, which sees every change made before the watch started.
    pub(super) fn watching(&mut self, started: Result<Box<Started>, String>) {
        let Some(live) = &mut self.live else {
            return;
        };
        let every = match started {
            Ok(started) => {
                let Started { watch, waiter } = *started;
                live.watch = Some(Arc::new(watch));
                live.walked = Some(Instant::now());
                self.wait_for_changes(waiter);
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

    /// Waits on a thread of its own for the working tree to change.
    fn wait_for_changes(&mut self, mut waiter: Waiter) {
        self.effect(Effect::Wait(Box::new(move || {
            let seen = waiter.wait();
            Completion {
                subject: Subject::Always(Because::Live),
                outcome: Box::new(Outcome::Waited(Box::new(WaitedFor {
                    waiter,
                    waited: seen,
                }))),
            }
        })));
    }

    /// Takes in a change of the working tree: captures it, and waits for the next.
    pub(super) fn waited(&mut self, waited: WaitedFor) {
        let WaitedFor { mut waiter, waited } = waited;
        let Some(live) = &mut self.live else {
            return;
        };
        if waited == Waited::Closed {
            return;
        }
        if let Some(trouble) = waiter.take_problem()
            && !std::mem::replace(&mut live.troubled, true)
        {
            self.effect(Effect::Log(format!(
                "the working tree's watch may miss changes, which the capture every {} s \
                 catches: {trouble}",
                RECONCILE.as_secs()
            )));
        }
        self.wait_for_changes(waiter);
        match waited {
            Waited::Stirred => self.stirred(),
            Waited::Changed | Waited::Closed => self.changed(),
        }
    }

    /// Takes in the first change of a burst, as soon as the watch sees it: counts it, and
    /// until the burst settles, no capture shows.
    ///
    /// The view on screen, and a capture running, may have caught the change half-made, as
    /// when an editor saved one file of several, so neither takes marks until the capture of
    /// the settled burst lands.
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
    /// A capture already running may have missed the change, so the view on screen takes no
    /// marks until the capture of the change lands.
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
        let Some(live) = &mut self.live else {
            return;
        };
        // A checkout that is another clone is never captured again.
        if live.replaced {
            return;
        }
        let every = if live.watch.is_some() {
            RECONCILE
        } else {
            POLL
        };
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
    /// One capture runs at a time, which keeps the pins of the captures in use safe from the
    /// next; a capture asked for meanwhile follows it.
    pub(super) fn capture(&mut self, rewalk: bool) {
        let (Some(review), Some(snapshot)) = (self.review.clone(), self.snapshot.clone()) else {
            return;
        };
        let known: HashSet<ObjectId> = self.blobs.texts.keys().cloned().collect();
        let from = self.shown;
        let Some(live) = &mut self.live else {
            return;
        };
        if live.replaced {
            return;
        }
        if live.capturing {
            live.again = Some(live.again.unwrap_or(false) || rewalk);
            return;
        }
        let Some(session) = live.session.clone() else {
            return;
        };
        live.capturing = true;
        let watch = live.watch.clone().filter(|_| rewalk);
        let confirm = live.confirms();
        let keep: Vec<RevisionRecord> = [
            Some(&snapshot.history),
            live.waiting.as_ref().map(|waiting| &waiting.history),
        ]
        .into_iter()
        .flatten()
        .filter_map(|history| provisional_record(history))
        .collect();
        let observed = live.observed;
        let pull_requests = live.pulls.pull_requests();
        live.given = Some(pull_requests.clone());
        let listed = live.listed.clone();
        let trust_decisions = self.trust_decisions;
        self.spawn(Subject::Always(Because::Live), move || {
            Outcome::Captured(capture(&CaptureInput {
                review: &review,
                session: &session,
                watch: watch.as_deref(),
                confirm,
                shown: &snapshot,
                from,
                observed,
                keep: &keep,
                known: &known,
                pull_requests: &pull_requests,
                listed: listed.as_ref(),
                trust_decisions,
            }))
        });
    }

    /// Captures again because what a capture compares with may have changed, such as the
    /// branch's pull request: the view on screen takes no marks until that capture lands.
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
        let Some(live) = &mut self.live else {
            return;
        };
        if live.replaced {
            self.fail("This checkout was replaced by another clone; reopen cr to review it.");
            return;
        }
        live.asked = true;
        self.set_status("Capturing the working tree again…");
        self.capture(false);
        self.look_up_pull_request_now();
    }

    /// Why the view on screen takes no marks now, if it takes none: it may miss a change, git
    /// is busy, or the checkout is another clone.
    pub(super) fn live_refusal(&self) -> Option<String> {
        let live = self.live.as_ref()?;
        if live.replaced {
            return Some(
                "Not marked: this checkout was replaced by another clone; reopen cr to review it."
                    .to_owned(),
            );
        }
        if let Some(busy) = live.busy {
            return Some(format!(
                "Not marked: git is busy ({busy}), so what shows may be out of date; mark \
                 again once it finishes."
            ));
        }
        live.updating.then(|| {
            "Not marked: the working tree changed while it was captured; it shows again in a \
             moment."
                .to_owned()
        })
    }

    /// Takes in a capture: shows it, or keeps it for when the reviewer is done, and starts the
    /// capture asked for meanwhile.
    ///
    /// A capture taken before the newest observation never shows: the view on screen stays,
    /// as updating, and the capture asked for by that observation follows.
    pub(super) fn captured(&mut self, captured: Result<Box<Captured>, Missed>) {
        let Some(live) = &mut self.live else {
            return;
        };
        live.capturing = false;
        live.given = None;
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
                if live.stirring {
                    // The capture of the burst follows once it settles.
                    live.updating = true;
                } else if captured.observed < live.observed {
                    live.updating = true;
                    again = Some(again.unwrap_or(false));
                } else {
                    if live.busy.take().is_some() {
                        self.set_status("Git is no longer busy.");
                    }
                    self.offer(captured);
                }
            }
            Err(Missed::Busy(busy)) => {
                if live.busy.replace(busy) != Some(busy) {
                    self.set_status(format!(
                        "Git is busy: {busy}. What shows stays until it finishes."
                    ));
                }
            }
            Err(Missed::Replaced) => {
                live.replaced = true;
                live.watch = None;
                again = None;
                self.fail("This checkout was replaced by another clone; reopen cr to review it.");
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

    /// Shows `captured` when it was compared with the snapshot on screen and nothing holds it
    /// back, keeps it while something does, and takes another capture when it was compared with
    /// another snapshot or read before a mark was saved.
    fn offer(&mut self, captured: Box<Captured>) {
        if captured.from != self.shown || captured.history.version() < self.version {
            self.capture(false);
            return;
        }
        if self.live_waits() {
            if let Some(live) = &mut self.live {
                live.waiting = Some(captured);
            }
            return;
        }
        self.show_capture(*captured);
    }

    /// Shows the waiting capture once nothing holds it back; runs after every update.
    ///
    /// One taken before a change seen since it waited never shows: the capture of that change
    /// is running or follows, and the view on screen is updating until it lands.
    pub(super) fn show_waiting_capture(&mut self) {
        if self.live_waits() {
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
        self.offer(waiting);
    }

    /// Whether the reviewer is in the middle of something a new snapshot would pull the rows
    /// from under.
    fn live_waits(&self) -> bool {
        self.marking
            || !self.marks.is_empty()
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
            granted,
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
            granted,
            commits,
        });
        self.associate(association);
        let asked = self
            .live
            .as_mut()
            .is_some_and(|live| std::mem::take(&mut live.asked));
        if links.is_empty() && same_snapshot(&old.history, &history, old.revision, tip) {
            // The same snapshot read again, such as after a mark recorded it, or with marks
            // another window saved since: what shows is derived again.
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
                Some(news) => self.set_status(news),
                None if asked => self.set_status(format!("Up to date. {}", self.summary())),
                None => {}
            }
            return;
        }
        let Ok(view) = history.view_under(tip, old.policy) else {
            return;
        };
        let moved = Moved::new(&links);
        let was = self.was_on();
        let old_target = self
            .review_target()
            .map(|(units, _)| units)
            .unwrap_or_default();
        let old_head = old
            .view()
            .and_then(|view| view.record().map(|record| record.head.clone()));

        self.snapshot = Some(Snapshot {
            history: Arc::clone(&history),
            revision: tip,
            policy: old.policy,
        });
        self.shown += 1;
        self.version = self.version.max(history.version());
        self.blobs.texts.extend(texts);
        self.blobs.retain_for(&view);
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
        self.keep_place(&was, &view, &moved);
        self.protect_changes(&old, &view, &moved, &old_target);
        // A grant of a review of the working tree covers the snapshot it was made for.
        self.read_snapshot_trust();

        // Language servers run on the worktree of the head no longer shown.
        if view.record().map(|record| &record.head) != old_head.as_ref()
            && let Some(mut servers) = self.servers.try_lock()
        {
            servers.clear();
        }
        self.request_syntax();
        let changed = self.what_changed(&view, moved_to.filter(|_| same_code));
        self.set_status(match news {
            Some(news) => format!("{news} {changed}"),
            None => changed,
        });
    }

    /// What the status line says about the capture `view` that shows now: why there is nothing
    /// to review in it, that the branch only moved to `moved_to` with the code the same, or
    /// that the working tree changed.
    fn what_changed(
        &self,
        view: &RevisionView<'_>,
        moved_to: Option<(String, ObjectId)>,
    ) -> String {
        let nothing = self
            .live
            .as_ref()
            .and_then(|live| live.subject.as_ref())
            .zip(view.record())
            .filter(|_| view.unit_ids().is_empty())
            .map(|(subject, record)| subject.nothing_to_review(&record.base, &record.head));
        match (nothing, moved_to) {
            (Some(nothing), _) => nothing,
            (None, Some((branch, head))) => format!(
                "{branch} is at {} now; the code is the same. {}",
                abbreviated(&head),
                self.summary()
            ),
            (None, None) => format!("The working tree changed. {}", self.summary()),
        }
    }

    /// Takes in what the capture about to show was of: the subject, which the header and the
    /// title show, the filtered paths, the trust that applies and the commits; returns what
    /// changed about the subject, for the status line.
    fn observe(&mut self, observation: Observation) -> Option<String> {
        let Observation {
            subject,
            filtered,
            tree,
            granted,
            commits,
        } = observation;
        let title_before = self.title();
        if let Some(granted) = granted {
            // A decision made since the grant was read, as while the capture waited behind
            // the trust question, is read again.
            let grant = match (&self.review, &tree) {
                (Some(review), Some(tree)) if granted.decisions != self.trust_decisions => {
                    self::granted(review, tree, self.trust_decisions).grant
                }
                _ => granted.grant,
            };
            self.set_grant(grant);
        }
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
            hunk: self.rows.hunks.get(hunk).cloned().unwrap_or_default(),
            hunk_line: self
                .hunk_rows
                .get(hunk)
                .and_then(|rows| self.rows.rows.get(rows.start))
                .and_then(|row| RowKey::of(row).head()),
            top: self.rows.rows.get(Anchor::at(offset).row).map(RowKey::of),
            anchor: diff_anchor(&self.rows, offset),
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

    /// Lays out the new snapshot where the reviewer `was`: the same file, or the one now where
    /// it was in the tree; the hunk its units went to, or the nearest; the same top row; the
    /// tree cursor on the same node; and a review buffer the reviewer was in, on the same hunk.
    ///
    /// Buffers of the old snapshot's files go, and so does the way back from a definition into
    /// one, since their change markers number the old snapshot's hunks.
    fn keep_place(&mut self, was: &WasOn, view: &RevisionView<'_>, moved: &Moved) {
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
        if let Some(path) = self.selected.clone() {
            self.places.remove(&path);
        }
        // Places kept for other files name units of the old snapshot; each follows its unit to
        // the new one, or forgets it when the unit is gone.
        let follow =
            |unit: review_core::UnitId| moved.any(unit.index()).map(|index| view.id(index));
        for place in self.places.values_mut() {
            place.unit = place.unit.and_then(follow);
            place.key = place
                .key
                .and_then(|key| follow(key.unit()).map(|unit| key.with_unit(unit)));
        }

        self.hunk = 0;
        self.build_selected();
        let followed = was
            .hunk
            .iter()
            .filter_map(|unit| moved.any(unit.index()))
            .map(|index| view.id(index))
            .find_map(|unit| self.rows.hunks.iter().position(|hunk| hunk.contains(&unit)));
        self.hunk = followed.unwrap_or_else(|| self.nearest_hunk(was.hunk_line));

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
            diff_offset(&self.rows, top),
            self.rows.rows.len(),
            viewport,
        ));

        if let Some(cursor) = was.buffer {
            // The buffer opens on the hunk the keys acted on; the old row stays only while it
            // is still on that hunk.
            self.enter_buffer();
            if let Some(start) = self.buffer.as_ref().map(|buffer| buffer.cursor) {
                self.set_buffer_cursor(cursor.row, cursor.col);
                if !was.hunk.is_empty() && self.buffer_hunk() != Some(self.hunk) {
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

    /// Records which units changed: those without an unchanged link from the snapshot `old`
    /// changed now, and the others when their source last changed.
    ///
    /// What the review keys act on now changed now too unless it is exactly where what they
    /// acted on before, `old_target`, went: a hunk, file or folder the reviewer was moved onto
    /// is new under the keys, however unchanged it is.
    fn protect_changes(
        &mut self,
        old: &Snapshot,
        view: &RevisionView<'_>,
        moved: &Moved,
        old_target: &[UnitId],
    ) {
        let now = Instant::now();
        let went: HashSet<UnitId> = old_target
            .iter()
            .filter_map(|unit| moved.any(unit.index()))
            .map(|index| view.id(index))
            .collect();
        let target = self
            .review_target()
            .map(|(units, _)| units)
            .unwrap_or_default();
        let target = if target.iter().all(|unit| went.contains(unit)) {
            Vec::new()
        } else {
            target
        };
        let Some(live) = &mut self.live else {
            return;
        };
        let previous: HashMap<UnitIndex, Instant> = std::mem::take(&mut live.changed)
            .into_iter()
            .filter(|(unit, _)| unit.revision() == old.revision)
            .map(|(unit, at)| (unit.index(), at))
            .collect();
        for unit in view.unit_ids() {
            let at = match moved.unchanged_from(unit.index()) {
                Some(older) => previous.get(&older).copied(),
                None => Some(now),
            };
            if let Some(at) = at {
                live.changed.insert(unit, at);
            }
        }
        for unit in target {
            live.changed.insert(unit, now);
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
                    .rows
                    .get(rows.start)
                    .and_then(|row| RowKey::of(row).head())?;
                Some((hunk, head.abs_diff(line)))
            })
            .min_by_key(|&(_, distance)| distance)
            .map_or(0, |(hunk, _)| hunk)
    }

    /// Refuses a review key on `units` while one of them changed under it: within [`SETTLE`]
    /// of the change, or since before the press that is still repeating; says why and returns
    /// whether it refused.
    pub(super) fn refuses_changed(&mut self, units: &[UnitId], what: &str) -> bool {
        let Some(live) = &mut self.live else {
            return false;
        };
        let now = Instant::now();
        let press = match live.press {
            Some(press) if now.duration_since(press.last) < REPEAT => Press { last: now, ..press },
            _ => Press {
                started: now,
                last: now,
            },
        };
        live.press = Some(press);
        let changed = units
            .iter()
            .filter_map(|unit| live.changed.get(unit))
            .any(|&at| at >= press.started || now.duration_since(at) < SETTLE);
        if changed {
            self.set_status(format!(
                "Not marked: {what} just changed in the working tree. Read it, then press \
                 again."
            ));
        }
        changed
    }
}

/// Where the reviewer was in the snapshot on screen, to keep the place in the next one.
struct WasOn {
    /// The units of the selected hunk.
    hunk: Vec<UnitId>,
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
    granted: Option<Granted>,
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

/// What a capture job needs.
struct CaptureInput<'a> {
    review: &'a review_core::Review,
    session: &'a Session,
    watch: Option<&'a Watch>,
    /// Whether the capture looks twice, for want of a watch.
    confirm: bool,
    shown: &'a Snapshot,
    from: u64,
    observed: u64,
    /// The provisional revisions whose pins stay.
    keep: &'a [RevisionRecord],
    known: &'a HashSet<ObjectId>,
    /// What was last found for each branch.
    pull_requests: &'a [review_core::github::CheckoutPullRequest],
    /// The base and head the commits were last listed between.
    listed: Option<&'a (Option<ObjectId>, Option<ObjectId>)>,
    /// The window's decisions on trust, counted; see [`Model::trust_decisions`].
    trust_decisions: u64,
}

/// Captures the working tree and compares it with the snapshot shown; runs in the background.
fn capture(input: &CaptureInput<'_>) -> Result<Box<Captured>, Missed> {
    let failed = |err: &dyn std::error::Error| Missed::Failed(causes(err));
    let trouble = input
        .watch
        .and_then(|watch| watch.rewalk().err())
        .map(|err| causes(&err));
    let mut attempts = 0;
    let LiveCapture {
        history,
        subject,
        filtered,
    } = loop {
        attempts += 1;
        let captured = if input.confirm {
            input
                .review
                .confirmed_live_history(input.session, input.keep, input.pull_requests)
        } else {
            input
                .review
                .live_history(input.session, input.keep, input.pull_requests)
        }
        .map_err(|err| failed(&err))?;
        match captured {
            Capture::Captured(captured) => break *captured,
            Capture::Moved if attempts < MOVED_ATTEMPTS => {}
            Capture::Moved => {
                return Err(Missed::Failed(
                    "git kept changing while it was captured".to_owned(),
                ));
            }
            Capture::Busy(busy) => return Err(Missed::Busy(busy)),
            Capture::Replaced => return Err(Missed::Replaced),
        }
    };
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
            .and_then(|view| view.analysis())
            .ok_or_else(|| missing("the review on screen has no analysis"))?;
        let captured = history
            .view(tip)
            .ok()
            .and_then(|view| view.analysis())
            .ok_or_else(|| missing("the capture has no analysis"))?;
        correspond(input.review.repo(), shown, captured)
            .map_err(|err| failed(&err))?
            .links
    };
    let texts = read_new_texts(input.review, &history, tip, input.known);
    let tree = provisional_or_tip_tree(input.review, &history);
    let checkout = matches!(input.review.target(), ReviewTarget::Checkout { .. });
    let (granted, association, commits) = if checkout {
        let granted = tree
            .as_ref()
            .map(|tree| granted(input.review, tree, input.trust_decisions));
        let association = subject
            .branch
            .as_deref()
            .and_then(|branch| input.review.repo().branch_identity(branch).ok());
        let pair = (subject.base.commit().cloned(), subject.head.clone());
        let commits = (input.listed != Some(&pair))
            .then(|| input.review.subject_commits(&subject).ok())
            .flatten();
        (granted, association, commits)
    } else {
        (None, None, None)
    };
    Ok(Box::new(Captured {
        from: input.from,
        observed: input.observed,
        subject: checkout.then_some(subject),
        filtered,
        tree,
        granted,
        association,
        commits,
        history: Arc::new(history),
        links,
        texts,
        trouble,
    }))
}

/// The text of revision `tip`'s head files that `known` does not hold, so the capture shows
/// whole, without its context lines arriving later.
fn read_new_texts(
    review: &review_core::Review,
    history: &History,
    tip: review_core::RevisionNumber,
    known: &HashSet<ObjectId>,
) -> HashMap<ObjectId, Arc<FullText>> {
    let mut texts = HashMap::new();
    let Some(analysis) = history.view(tip).ok().and_then(|view| view.analysis()) else {
        return texts;
    };
    let wanted: Vec<&ObjectId> = analysis
        .files
        .iter()
        .filter_map(|file| file.new.as_ref())
        .filter(|side| is_regular(side) && !known.contains(&side.oid))
        .map(|side| &side.oid)
        .collect();
    if wanted.is_empty() {
        return texts;
    }
    let Ok(mut reader) = review.repo().blob_reader() else {
        return texts;
    };
    for oid in wanted {
        if let Ok(review_core::git::Blob::Content(bytes)) = reader.read(oid)
            && !bytes.contains(&0)
        {
            texts.insert(
                oid.clone(),
                Arc::new(FullText::new(&String::from_utf8_lossy(&bytes))),
            );
        }
    }
    texts
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

/// `history`'s provisional revision, if it ends in one.
fn provisional_record(history: &History) -> Option<RevisionRecord> {
    let tip = history.current()?;
    if !history.is_provisional(tip) {
        return None;
    }
    history.view(tip).ok()?.record().cloned()
}

/// The correspondence from the old snapshot to the new one, by unit index.
struct Moved {
    /// Where each old unit went, by any link that is not ambiguous.
    any: HashMap<UnitIndex, UnitIndex>,
    /// Where each old unit went unchanged, at a trusted position.
    unchanged: HashMap<UnitIndex, UnitIndex>,
    /// Where each new unit came from unchanged.
    unchanged_from: HashMap<UnitIndex, UnitIndex>,
}

impl Moved {
    fn new(links: &[Link]) -> Self {
        let mut moved = Self {
            any: HashMap::new(),
            unchanged: HashMap::new(),
            unchanged_from: HashMap::new(),
        };
        for link in links {
            if link.kind != LinkKind::Ambiguous {
                moved.any.entry(link.older).or_insert(link.newer);
            }
            if link.kind == LinkKind::Unchanged {
                moved.unchanged.insert(link.older, link.newer);
                moved.unchanged_from.insert(link.newer, link.older);
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

    fn unchanged_from(&self, newer: UnitIndex) -> Option<UnitIndex> {
        self.unchanged_from.get(&newer).copied()
    }
}

/// `err` and every cause under it, joined by `: `.
fn causes(err: &dyn std::error::Error) -> String {
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
    /// Moves every change `by` into the past, as if that much time passed since.
    pub(super) fn age(&mut self, by: Duration) {
        for at in self.changed.values_mut() {
            *at = at.checked_sub(by).unwrap_or(*at);
        }
        self.press = None;
    }

    /// Holds a review key down since `ago`, repeating until now.
    pub(super) fn hold_since(&mut self, ago: Duration) {
        let now = Instant::now();
        self.press = Some(Press {
            started: now.checked_sub(ago).unwrap_or(now),
            last: now,
        });
    }

    /// Whether a capture waits for the reviewer.
    pub(super) fn has_waiting(&self) -> bool {
        self.waiting.is_some()
    }
}
