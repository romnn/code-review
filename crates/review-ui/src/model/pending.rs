//! The checkout's review as its log last recorded it, shown at once while its review state is
//! checked behind it.
//!
//! The pending view reads nothing derived: the log's last complete revision record, and the
//! revision's changes when the cache holds their analysis in this version's format; when it
//! does not, as after the format changed, the files one comparison of its trees finds, while a
//! job analyzes the revision.
//! Behind it, one job reads the history, starting from what the view read of the log, so it
//! takes in what was appended since and finds the log replaced; it starts once the revision's
//! analysis is in, which it takes rather than computing it again.
//! The first lookup of the branch's pull request runs meanwhile, and the model, which waits for
//! both, then queues the one confirmed capture of the working tree, compared with the base that
//! lookup decides; neither job waits for the other.
//! A capture of another branch than the lookup was for is taken again once that branch is
//! looked up.
//! A checkout with no recorded revision shows empty panes and waits for the lookup alone.
//! The bottom bar counts the revisions replayed, as the replay reports them, with the frames
//! that show anyway and at [`PROGRESS`]'s pace, then says what the capture waits for.
//! The analysis and the verified review each show with the next frame, as an opened review
//! does, keeping the selected file.
//!
//! The pending view is no [`Snapshot`], and only a snapshot has a [`Model::view`], which
//! everything that counts, folds or approves by review state, shows what changed since a review
//! or marks reads: until the review is verified nothing counts as reviewed, and no key finds
//! anything to mark.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use review_core::analysis::Analysis;
use review_core::tree::NodeKind;
use review_core::{
    BranchIdentity, Capture, Changes, FileStatus, History, LiveCapture, ObjectId, Replay, Review,
    RevisionNumber, RevisionRecord,
};

use super::buffer::Origin;
use super::effects::{Because, Outcome, Subject};
use super::live::read_new_texts;
use super::live::{Opening, Replaced, causes, replaced_log};
use super::pool::Lane;
use super::pulls::Given;
use super::sync::Tick;
use super::{Effect, Mode, Model, head_texts};
use crate::folders::ViewState;
use crate::rows::{self, FullText};
use crate::state::Snapshot;
use crate::text::TextPos;

/// What a key that would record something answers while the review state is checked.
pub(super) const CHECKING: &str =
    "The review state is still being checked; marks work once it shows.";

/// What the overview, the pickers and go to definition answer while the review state is
/// checked, since they read the verified review.
pub(super) const WAITS: &str = "The review state is still being checked; this works once it shows.";

/// How many times the opening capture is taken again when git moved while it was taken.
const ATTEMPTS: u32 = 5;

/// How long the opening waits for git to finish what it is busy with, when the review has no
/// revision to show meanwhile.
const BUSY_WAIT: Duration = Duration::from_secs(10);

/// How often a frame shows the count of revisions replayed while nothing else draws one.
const PROGRESS: Duration = Duration::from_millis(250);

/// What shows first of an opened review.
pub(super) enum Shows {
    /// The review, verified.
    Verified(Box<Verified>),
    /// The checkout's last recorded revision, while the review is verified behind it.
    Pending(Box<Pending>),
}

/// What shows of the checkout's review while its review state is checked.
pub(super) struct Pending {
    /// The revision, as the log records it; `None` before the first, when the panes show
    /// empty.
    record: Option<RevisionRecord>,
    /// Its changed files, with nothing counted.
    files: Vec<FileStatus>,
    /// Its analysis, once read or made, whose changes then show.
    analysis: Option<Arc<Analysis>>,
    /// The head text of the file it shows first, read with the analysis so that file's diff
    /// lays out once, with its context; taken when the view shows.
    texts: HashMap<ObjectId, Arc<FullText>>,
    /// What a job brought that shows in its place with the next frame.
    arrived: Option<Arrived>,
    /// Why the review could not be verified, once it could not.
    failed: Option<String>,
    /// How many revisions the verification replayed, out of how many, as it reported last.
    progress: Progress,
    /// Whether the history is replayed, or there is none to replay.
    replayed: bool,
    /// What the confirmed capture that runs, or ran and waits to show, was given of the
    /// lookups; one given anything else no longer counts.
    comparing: Option<Given>,
    /// The newest confirmed capture asked for, by the count of those asked for so far: only
    /// its outcome lands.
    request: u64,
    /// Where the reviewer was, for a review with no recorded revision to restore it in: it is
    /// restored once the verified review shows.
    view_state: Option<ViewState>,
}

/// How far the verification's replay of the history got, which its job sets as it replays
/// and the UI thread reads with each frame.
///
/// Both counts share one atomic, the total in its high half, so a frame never reads a count
/// of one report with the total of another, and the UI thread never waits for the job.
#[derive(Debug, Clone)]
struct Progress(Arc<AtomicU64>);

impl Progress {
    /// None of `total` revisions replayed yet.
    fn new(total: u32) -> Self {
        Self(Arc::new(AtomicU64::new(u64::from(total) << 32)))
    }

    /// Sets how far the replay got.
    fn set(&self, replay: Replay) {
        let half = |count: usize| u64::from(u32::try_from(count).unwrap_or(u32::MAX));
        self.0.store(
            (half(replay.total) << 32) | half(replay.replayed),
            Ordering::Relaxed,
        );
    }

    /// How far the replay got, as last set.
    fn get(&self) -> Replay {
        let packed = self.0.load(Ordering::Relaxed);
        let half = |half: u64| usize::try_from(half & u64::from(u32::MAX)).unwrap_or(usize::MAX);
        Replay {
            replayed: half(packed),
            total: half(packed >> 32),
        }
    }
}

/// What a job brought for the pending view.
enum Arrived {
    /// The revision's analysis, whose changes then show, with the head text of the selected
    /// file.
    Analysis {
        analysis: Arc<Analysis>,
        texts: HashMap<ObjectId, Arc<FullText>>,
    },
    /// The review verified, which takes the pending view's place, or why it could not be.
    Verified(Result<Box<Verified>, Unverified>),
}

/// What the tree and the diff lay out, held apart from the model while it lays them out: the
/// snapshot on screen, or the changes of the pending view's revision.
#[derive(Clone)]
pub(super) enum Laid {
    /// The snapshot on screen.
    Verified(Snapshot),
    /// The pending view's revision, `number`, as `analysis` found it.
    Pending {
        analysis: Arc<Analysis>,
        number: RevisionNumber,
    },
}

impl Laid {
    /// What rows are laid out from.
    pub(super) fn source(&self) -> Option<rows::Source<'_>> {
        match self {
            Self::Verified(snapshot) => snapshot.view().map(rows::Source::Verified),
            Self::Pending { analysis, number } => {
                Some(rows::Source::Pending(Changes::of(analysis, *number)))
            }
        }
    }
}

/// What the pending view shows while the review state is checked; see [`Model::checking`].
#[derive(Debug, Clone, Copy)]
pub struct Checking<'a> {
    /// The revision, as the log records it; `None` before the first.
    pub record: Option<&'a RevisionRecord>,
    /// Whether its changes show, rather than only its changed files.
    pub changes: bool,
    /// What the verification does, until it fails; `None` once it failed.
    pub step: Option<Step>,
}

/// What the verification of the checkout's review does, as the bottom bar says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// It replays the history, this far.
    Replaying(Replay),
    /// The confirmed capture waits for the branch's pull request to be looked up.
    LookingUp,
    /// The confirmed capture runs.
    Comparing,
}

/// A review verified: its history read, with the working tree captured on top for a review of
/// the checkout.
pub(super) struct Verified {
    history: Arc<History>,
    /// The revision to show, the current one.
    revision: RevisionNumber,
    /// Whether the review follows the working tree.
    follows: bool,
    /// For a review of the checkout, what its capture saw.
    opening: Option<Opening>,
    /// The head text of its files that the window did not hold or read when it was asked for.
    texts: HashMap<ObjectId, Arc<FullText>>,
}

/// Why a review could not be verified.
pub(super) enum Unverified {
    /// What it was read from was replaced, which ends the review in the window.
    Replaced(Replaced),
    /// The capture found the checkout on another branch or upstream than the pull request was
    /// looked up for, this one, so it is taken again once that is looked up.
    Moved(Option<BranchIdentity>),
    /// It failed, for this reason.
    Failed(String),
}

impl Unverified {
    /// `err`, which ends the review when it says the log was replaced.
    fn of(err: &review_core::Error) -> Self {
        if replaced_log(err) {
            Self::Replaced(Replaced::Log)
        } else {
            Self::Failed(causes(err))
        }
    }
}

/// The analysis of the pending view's revision, made in the background.
pub(super) struct Analyzed {
    /// The revision it is of.
    record: RevisionRecord,
    analysis: Result<Arc<Analysis>, String>,
    /// The head text of the file selected when it was asked for.
    texts: HashMap<ObjectId, Arc<FullText>>,
}

impl Pending {
    /// Reads the pending view of `review`: its last recorded revision, with its changes when
    /// the cache holds their analysis, or else its changed files; empty before the first
    /// revision.
    ///
    /// With its changes come the head text of the file it shows first: the one at `place`,
    /// where the reviewer last was, or else the first in the tree.
    ///
    /// # Errors
    ///
    /// Returns why the log or git could not be read.
    #[tracing::instrument(name = "read_pending", skip_all)]
    pub(super) fn read(review: &Review, place: Option<&str>) -> Result<Self, String> {
        let failed = |err: review_core::Error| causes(&err);
        let Some(record) = review.last_recorded().map_err(failed)? else {
            return Ok(Self {
                record: None,
                files: Vec::new(),
                analysis: None,
                texts: HashMap::new(),
                arrived: None,
                failed: None,
                progress: Progress::new(0),
                // Nothing recorded, so nothing is replayed.
                replayed: true,
                comparing: None,
                request: 0,
                view_state: None,
            });
        };
        let analysis = review.cached_analysis(&record);
        let files = match &analysis {
            Some(analysis) => Changes::of(analysis, record.number).files(),
            None => review.changed_files(&record).map_err(failed)?,
        };
        let texts = analysis.as_ref().map_or_else(HashMap::new, |analysis| {
            let tree = review_core::tree::file_tree(&files);
            let first = tree.iter().find(|node| node.kind == NodeKind::File);
            let first = first.map(|node| node.path.as_str());
            head_texts(review, analysis, |file| {
                let path = Some(file.display_path());
                path == place || path == first
            })
        });
        Ok(Self {
            progress: Progress::new(record.number.get()),
            record: Some(record),
            files,
            analysis,
            texts,
            arrived: None,
            failed: None,
            replayed: false,
            comparing: None,
            request: 0,
            view_state: None,
        })
    }

    /// Whether the review is still being verified behind it.
    pub(super) fn verifying(&self) -> bool {
        self.failed.is_none()
    }

    /// What a key that would record something answers.
    pub(super) fn refusal(&self) -> String {
        match &self.failed {
            Some(why) => format!("Not marked: the review state could not be checked: {why}"),
            None => CHECKING.to_owned(),
        }
    }

    /// The changed files, with nothing counted.
    pub(super) fn files(&self) -> Vec<FileStatus> {
        self.files.clone()
    }

    /// What the tree and the diff lay out, once the changes are in.
    pub(super) fn laid_out(&self) -> Option<Laid> {
        Some(Laid::Pending {
            analysis: Arc::clone(self.analysis.as_ref()?),
            number: self.record.as_ref()?.number,
        })
    }

    /// What the verification does, until it fails.
    fn step(&self) -> Option<Step> {
        Some(match (self.verifying(), self.replayed, &self.comparing) {
            (false, ..) => return None,
            (true, false, _) => Step::Replaying(self.progress.get()),
            (true, true, None) => Step::LookingUp,
            (true, true, Some(_)) => Step::Comparing,
        })
    }
}

impl Verified {
    /// `history` of `review`, which `follows` the working tree or not and whose capture saw
    /// `opening`, with the head text of its current revision's files that `known` does not
    /// hold, so each file's diff lays out once, with its context.
    ///
    /// # Errors
    ///
    /// Returns why there is nothing to show: no revision yet.
    pub(super) fn of(
        review: &Review,
        history: History,
        follows: bool,
        opening: Option<Opening>,
        known: &HashSet<ObjectId>,
    ) -> Result<Self, String> {
        let revision = history.current().ok_or("the review has no revision yet")?;
        let texts = read_new_texts(review, &history, revision, known);
        Ok(Self {
            history: Arc::new(history),
            revision,
            follows,
            opening,
            texts,
        })
    }
}

/// Reads the checkout's review's history, telling `progress` how far its replay got, for the
/// confirmed capture to read on from; runs in the background.
///
/// # Errors
///
/// Returns why the history cannot be read.
#[tracing::instrument(skip_all)]
fn replay(review: &Review, progress: &dyn Fn(Replay)) -> Result<(), Unverified> {
    review
        .history_reporting(progress)
        .map(|_| ())
        .map_err(|err| Unverified::of(&err))
}

/// Verifies the checkout's review once its history is replayed: captures the working tree with
/// what it is `given` of the lookups, puts the capture on top of the history, and reads the head
/// text of its files that `known` does not hold; runs in the background.
///
/// No watch tells the opening of a change made while it captures, so the capture looks twice.
/// While git is busy in the checkout or cr's data is full, the review shows its last recorded
/// revision, which takes marks; without one, it waits a moment for git.
///
/// # Errors
///
/// Returns why the review cannot be shown, and [`Unverified::Moved`] when the capture saw
/// another branch or upstream than the one looked up.
#[tracing::instrument(skip_all)]
fn confirm(
    review: &Review,
    known: &HashSet<ObjectId>,
    given: &Given,
) -> Result<Verified, Unverified> {
    let started = Instant::now();
    let mut moved = 0;
    let recorded = |busy| {
        let history = review.history().map_err(|err| Unverified::of(&err))?;
        let opening = Opening::read(review, &history, None, Vec::new(), busy, false);
        Ok((history, opening))
    };
    let (history, opening) = loop {
        let captured = match review.confirmed_live_history(given.pull_requests()) {
            // At its limit, cr's data takes no capture; the window's first capture says why.
            Err(review_core::Error::StorageFull { .. }) => break recorded(None)?,
            captured => captured.map_err(|err| Unverified::of(&err))?,
        };
        match captured {
            Capture::Captured(captured) => {
                let LiveCapture {
                    history,
                    subject,
                    filtered,
                } = *captured;
                let opening = Opening::read(
                    review,
                    &history,
                    Some(subject),
                    filtered,
                    None,
                    given.unknown(),
                );
                if opening.association.as_ref() != given.asked() {
                    return Err(Unverified::Moved(opening.association));
                }
                break (history, opening);
            }
            Capture::Moved => {
                moved += 1;
                if moved >= ATTEMPTS {
                    return Err(Unverified::Failed(
                        "git kept changing the checkout while it was captured; open it again"
                            .to_owned(),
                    ));
                }
            }
            Capture::Busy(busy) => {
                let (history, opening) = recorded(Some(busy))?;
                if history.current().is_some() {
                    break (history, opening);
                }
                if started.elapsed() >= BUSY_WAIT {
                    return Err(Unverified::Failed(format!(
                        "git is busy in this checkout ({busy}); open it again once that is done"
                    )));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Capture::Replaced => return Err(Unverified::Replaced(Replaced::Clone)),
        }
    };
    Verified::of(review, history, true, Some(opening), known).map_err(Unverified::Failed)
}

impl Model {
    /// Shows `first`, the checkout's last recorded revision, restoring the selection saved with
    /// the review, and starts verifying the review behind it, once its changes are analyzed
    /// when the cache did not hold them; without a recorded revision, shows empty panes and
    /// restores the selection once the verified review shows.
    #[tracing::instrument(skip_all)]
    pub(super) fn show_pending(
        &mut self,
        mut first: Pending,
        view_state: ViewState,
        found: Option<&str>,
    ) {
        let analyzed = first.analysis.is_some();
        let record = first.record.clone();
        self.blobs.texts.extend(std::mem::take(&mut first.texts));
        if record.is_none() {
            first.view_state = Some(view_state.clone());
        }
        self.checking = Some(first);
        self.shown += 1;
        // The other files' texts are read from now on; the selected file's is asked for as it
        // lays out.
        self.request_texts();
        self.refresh_tree();
        let Some(record) = record else {
            self.say_opened(found, "checking the review state…");
            self.confirm_opening();
            return;
        };
        self.restore_view_state(view_state);
        self.say_opened(
            found,
            &format!("Revision {} · checking the review state…", record.number),
        );
        self.request_syntax();
        if analyzed {
            self.replay_history();
            return;
        }
        let selected = self.selected.clone();
        self.spawn_with(
            Lane::Selected,
            Subject::Always(Because::Opening),
            move |parts| {
                let analysis = parts
                    .review
                    .analyze_recorded(&record)
                    .map_err(|err| causes(&err));
                let texts = analysis.as_ref().map_or_else(
                    |_| HashMap::new(),
                    |analysis| {
                        head_texts(&parts.review, analysis, |file| {
                            selected.as_deref() == Some(file.display_path())
                        })
                    },
                );
                Outcome::Analyzed(Box::new(Analyzed {
                    record,
                    analysis,
                    texts,
                }))
            },
        );
    }

    /// Replays the history behind the pending view, counting the revisions on the bottom bar.
    fn replay_history(&mut self) {
        let Some(checking) = &self.checking else {
            return;
        };
        let progress = checking.progress.clone();
        self.spawn_with(
            Lane::Background,
            Subject::Always(Because::Opening),
            move |parts| {
                let replayed = |replay| progress.set(replay);
                Outcome::Replayed(replay(&parts.review, &replayed))
            },
        );
        self.after(
            PROGRESS,
            Subject::Always(Because::Clock),
            Outcome::Tick(Tick::Checking),
        );
    }

    /// Takes in the replayed history, after which the confirmed capture runs once the pull
    /// request is looked up, or why it could not be read, which shows with the next frame.
    pub(super) fn replayed(&mut self, replayed: Result<(), Unverified>) {
        let Some(checking) = &mut self.checking else {
            return;
        };
        match replayed {
            Ok(()) => {
                checking.replayed = true;
                self.confirm_opening();
            }
            Err(unverified) => checking.arrived = Some(Arrived::Verified(Err(unverified))),
        }
    }

    /// Takes the confirmed capture behind the pending view once the history is replayed and
    /// the first lookup of the branch's pull request ended, with what the lookups found, and
    /// takes it anew when they found something else since, which the one taken before no
    /// longer shows; the capture reads the text of no file the window holds or reads itself,
    /// as it reads every file the pending view shows.
    pub(super) fn confirm_opening(&mut self) {
        let Some(checking) = &mut self.checking else {
            return;
        };
        let given = Given::of(&self.pulls);
        if checking
            .comparing
            .as_ref()
            .is_some_and(|comparing| *comparing != given)
        {
            checking.comparing = None;
            if matches!(checking.arrived, Some(Arrived::Verified(_))) {
                checking.arrived = None;
            }
        }
        let ready = checking.replayed && checking.verifying() && self.pulls.settled();
        if !ready || checking.comparing.is_some() {
            return;
        }
        checking.comparing = Some(given.clone());
        checking.request += 1;
        let request = checking.request;
        let shown = checking
            .analysis
            .as_ref()
            .map_or_else(Vec::new, |analysis| {
                analysis
                    .files
                    .iter()
                    .filter_map(|file| file.new.as_ref())
                    .filter(|side| super::is_regular(side))
                    .map(|side| side.oid.clone())
                    .collect()
            });
        let known: HashSet<ObjectId> = self
            .blobs
            .texts
            .keys()
            .chain(self.requested.claims())
            .cloned()
            .chain(shown)
            .collect();
        self.spawn_with(
            Lane::Background,
            Subject::Always(Because::Opening),
            move |parts| {
                let verified = confirm(&parts.review, &known, &given);
                Outcome::Verified(verified.map(Box::new), request)
            },
        );
    }

    /// Takes in the tick that draws a frame for the count of revisions replayed, and asks for
    /// the next while the history is replayed.
    pub(super) fn checking_ticked(&mut self) {
        let runs = self.checking.as_ref().is_some_and(|checking| {
            checking.verifying() && !checking.replayed && checking.arrived.is_none()
        });
        if runs {
            self.after(
                PROGRESS,
                Subject::Always(Because::Clock),
                Outcome::Tick(Tick::Checking),
            );
        }
    }

    /// Takes in the analysis of the pending view's revision, which shows with the next frame
    /// and then starts verifying the review, which takes the analysis rather than making it
    /// again; without one, verifies the review at once.
    pub(super) fn analyzed(&mut self, analyzed: Analyzed) {
        let Analyzed {
            record,
            analysis,
            texts,
        } = analyzed;
        let Some(checking) = self
            .checking
            .as_mut()
            .filter(|checking| checking.record.as_ref() == Some(&record))
        else {
            return;
        };
        match analysis {
            Ok(analysis) => checking.arrived = Some(Arrived::Analysis { analysis, texts }),
            Err(err) => {
                self.effect(Effect::Log(format!(
                    "could not analyze the last recorded revision: {err}"
                )));
                self.replay_history();
            }
        }
    }

    /// Takes in the verified review, or why it could not be verified, which shows with the next
    /// frame in place of the pending view, when it is of `request`, the newest asked for and
    /// still given what the lookups found; a capture of a checkout that moved to another branch
    /// or upstream since its pull request was looked up is taken again once that is looked up.
    pub(super) fn verified(&mut self, verified: Result<Box<Verified>, Unverified>, request: u64) {
        let Some(checking) = self
            .checking
            .as_mut()
            .filter(|checking| checking.comparing.is_some() && checking.request == request)
        else {
            return;
        };
        match verified {
            Err(Unverified::Moved(association)) => {
                checking.comparing = None;
                self.associate(association);
                self.confirm_opening();
            }
            verified => checking.arrived = Some(Arrived::Verified(verified)),
        }
    }

    /// Whether what a job brought for the pending view waits for the next frame.
    pub(super) fn arrived(&self) -> bool {
        self.checking
            .as_ref()
            .is_some_and(|checking| checking.arrived.is_some())
    }

    /// Shows what a job brought for the pending view: the revision's changes, or the verified
    /// review in its place, keeping the selected file; or else why the review could not be
    /// verified, over the pending view, which stays.
    pub(super) fn show_arrived(&mut self) {
        let Some(arrived) = self
            .checking
            .as_mut()
            .and_then(|checking| checking.arrived.take())
        else {
            return;
        };
        let unverified = match arrived {
            Arrived::Analysis { analysis, texts } => return self.show_changes(analysis, texts),
            Arrived::Verified(Ok(verified)) => {
                let view_state = self
                    .checking
                    .as_mut()
                    .and_then(|checking| checking.view_state.take());
                return self.show_verified(*verified, view_state, None);
            }
            Arrived::Verified(Err(unverified)) => unverified,
        };
        let why = match unverified {
            // The status line says the review ended.
            Unverified::Replaced(replaced) => {
                self.stop(replaced, false);
                replaced.message().to_owned()
            }
            // Taken again as it lands, never kept to show.
            Unverified::Moved(_) => return,
            Unverified::Failed(why) => {
                self.effect(Effect::Log(format!(
                    "could not check the review state: {why}"
                )));
                self.fail(format!("Could not check the review state: {why}"));
                why
            }
        };
        if let Some(checking) = &mut self.checking {
            checking.failed = Some(why);
        }
    }

    /// Shows the changes `analysis` found in the pending view's revision in place of its
    /// changed files, with `texts`, the head text of the selected file, then verifies the
    /// review, which reads no text the window now holds or reads.
    #[tracing::instrument(skip_all)]
    fn show_changes(&mut self, analysis: Arc<Analysis>, texts: HashMap<ObjectId, Arc<FullText>>) {
        let Some(checking) = &mut self.checking else {
            return;
        };
        let Some(number) = checking.record.as_ref().map(|record| record.number) else {
            return;
        };
        checking.files = Changes::of(&analysis, number).files();
        checking.analysis = Some(analysis);
        self.blobs.texts.extend(texts);
        self.shown += 1;
        self.request_texts();
        self.refresh_tree();
        self.keep_file();
        self.request_syntax();
        self.replay_history();
    }

    /// Shows `verified`, as the review opens with `view_state`, the folded folders and the
    /// place saved with the review, or in place of the pending view, keeping the file it
    /// showed; `found` is how the review was found, when its capture says nothing.
    #[tracing::instrument(skip_all)]
    pub(super) fn show_verified(
        &mut self,
        verified: Verified,
        view_state: Option<ViewState>,
        found: Option<String>,
    ) {
        let Verified {
            history,
            revision,
            follows,
            opening,
            texts,
        } = verified;
        self.checking = None;
        // Places kept in the pending view name units the review state never decided.
        self.places.clear();
        // A whole file opened from the pending view shows that revision's text, and opens again
        // from the verified review below.
        let buffer = self
            .buffer
            .as_ref()
            .filter(|buffer| matches!(buffer.origin, Origin::Review(_)))
            .map(|buffer| buffer.cursor);
        if buffer.is_some() {
            self.set_buffer(None);
        }
        self.revisions = super::overview::summarize(&history, self.settings.policy);
        self.shown += 1;
        self.version = history.version();
        // The policy in use when the review shows, which the settings list may have changed
        // while it opened.
        self.snapshot = Some(Snapshot {
            history,
            revision,
            policy: self.settings.policy,
        });
        self.blobs.texts.extend(texts);
        self.request_texts();
        self.refresh_tree();
        match view_state {
            Some(view_state) => self.restore_view_state(view_state),
            None => self.keep_file(),
        }
        if self.mode == Mode::Buffer {
            self.reopen_buffer(buffer);
        }
        let found = match &opening {
            Some(opening) => opening.describe(),
            None => found,
        };
        self.say_opened(found.as_deref(), &self.summary());
        self.request_syntax();
        self.start_live(follows, opening);
    }

    /// Shows the file, or the folder, that showed before, laid out anew, while the tree still
    /// has it, and else the first file that needs the reviewer; with no file, nothing.
    fn keep_file(&mut self) {
        if self
            .selected
            .as_deref()
            .is_some_and(|path| crate::state::is_file(&self.tree, path))
        {
            self.rebuild();
            return;
        }
        self.selected = None;
        self.folder = None;
        self.select_first_file();
        if self.selected.is_none() {
            self.rebuild();
            self.enter_mode(Mode::Tree);
        }
    }

    /// Opens the whole file again with the cursor where it was, `cursor`, once the verified
    /// review shows; or else the diff, while that review has not read the file's text yet.
    fn reopen_buffer(&mut self, cursor: Option<TextPos>) {
        if let Some(TextPos { row, col }) = cursor {
            self.enter_buffer();
            if self.buffer.is_some() {
                self.set_buffer_cursor(row, col);
                return;
            }
        }
        self.enter_mode(Mode::Diff);
    }

    /// While the review state is checked, what the pending view shows.
    #[must_use]
    pub fn checking(&self) -> Option<Checking<'_>> {
        let checking = self.checking.as_ref()?;
        Some(Checking {
            record: checking.record.as_ref(),
            changes: checking.analysis.is_some(),
            step: checking.step(),
        })
    }
}
