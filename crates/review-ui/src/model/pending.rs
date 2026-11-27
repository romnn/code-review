//! The checkout's review as its log last recorded it, shown at once while its review state is
//! checked behind it.
//!
//! The pending view reads nothing derived: the log's last complete revision record, and the
//! revision's changes when the cache holds their analysis in this version's format; when it
//! does not, as after the format changed, the files one comparison of its trees finds, while a
//! job analyzes the revision.
//! Behind it, one job captures the working tree and reads the history, starting from what the
//! view read of the log, so it takes in what was appended since and finds the log replaced; it
//! starts once the revision's analysis is in, which it takes rather than computing it again.
//! The analysis and the verified review each show with the next frame, as an opened review
//! does, keeping the selected file.
//!
//! The pending view is no [`Snapshot`], and only a snapshot has a [`Model::view`], which
//! everything that counts, folds or approves by review state, shows what changed since a review
//! or marks reads: until the review is verified nothing counts as reviewed, and no key finds
//! anything to mark.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use review_core::analysis::Analysis;
use review_core::tree::NodeKind;
use review_core::{
    Capture, Changes, FileStatus, History, LiveCapture, ObjectId, Review, RevisionNumber,
    RevisionRecord,
};

use super::buffer::Origin;
use super::effects::{Because, Outcome, Subject};
use super::live::read_new_texts;
use super::live::{Opening, Replaced, causes, replaced_log};
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

/// What shows first of an opened review.
pub(super) enum Shows {
    /// The review, verified.
    Verified(Box<Verified>),
    /// The checkout's last recorded revision, while the review is verified behind it.
    Pending(Box<Pending>),
}

/// What shows of the checkout's review while its review state is checked.
pub(super) struct Pending {
    /// The revision, as the log records it.
    record: RevisionRecord,
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
    /// The revision, as the log records it.
    pub record: &'a RevisionRecord,
    /// Whether its changes show, rather than only its changed files.
    pub changes: bool,
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
    /// It failed, for this reason.
    Failed(String),
}

impl Unverified {
    /// What to say about it.
    pub(super) fn message(self) -> String {
        match self {
            Self::Replaced(replaced) => replaced.message().to_owned(),
            Self::Failed(why) => why,
        }
    }

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
    /// the cache holds their analysis, or else its changed files; `None` before the first
    /// revision.
    ///
    /// With its changes come the head text of the file it shows first: the one at `place`,
    /// where the reviewer last was, or else the first in the tree.
    ///
    /// # Errors
    ///
    /// Returns why the log or git could not be read.
    #[tracing::instrument(name = "read_pending", skip_all)]
    pub(super) fn read(review: &Review, place: Option<&str>) -> Result<Option<Self>, String> {
        let failed = |err: review_core::Error| causes(&err);
        let Some(record) = review.last_recorded().map_err(failed)? else {
            return Ok(None);
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
        Ok(Some(Self {
            record,
            files,
            analysis,
            texts,
            arrived: None,
            failed: None,
        }))
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
            number: self.record.number,
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

/// Verifies the checkout's review: captures its working tree and reads the history with the
/// capture on top, and the head text of its files that `known` does not hold; runs in the
/// background.
///
/// No watch tells the opening of a change made while it captures, so the capture looks twice.
/// While git is busy in the checkout or cr's data is full, the review shows its last recorded
/// revision, which takes marks; without one, it waits a moment for git.
///
/// # Errors
///
/// Returns why the review cannot be shown.
#[tracing::instrument(skip_all)]
pub(super) fn verify(review: &Review, known: &HashSet<ObjectId>) -> Result<Verified, Unverified> {
    let started = Instant::now();
    let mut moved = 0;
    let recorded = |busy| {
        let history = review.history().map_err(|err| Unverified::of(&err))?;
        let opening = Opening::read(review, &history, None, Vec::new(), busy);
        Ok((history, opening))
    };
    let (history, opening) = loop {
        let captured = match review.confirmed_live_history(&[]) {
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
                let opening = Opening::read(review, &history, Some(subject), filtered, None);
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
    /// when the cache did not hold them.
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
        self.checking = Some(first);
        self.shown += 1;
        // The text of every other file is asked for before any file lays out, so none lays out
        // without its context before the text lands.
        self.request_texts();
        self.refresh_tree();
        self.restore_view_state(view_state);
        self.say_opened(
            found,
            &format!("Revision {} · checking the review state…", record.number),
        );
        self.request_syntax();
        if analyzed {
            self.verify();
            return;
        }
        let selected = self.selected.clone();
        self.spawn_with(Subject::Always(Because::Opening), move |parts| {
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
        });
    }

    /// Verifies the review behind the pending view, reading the text of no file the window
    /// holds or is reading already.
    fn verify(&mut self) {
        let known: HashSet<ObjectId> = self
            .blobs
            .texts
            .keys()
            .chain(self.requested.claims())
            .cloned()
            .collect();
        self.spawn_with(Subject::Always(Because::Opening), move |parts| {
            Outcome::Verified(verify(&parts.review, &known).map(Box::new))
        });
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
            .filter(|checking| checking.record == record)
        else {
            return;
        };
        match analysis {
            Ok(analysis) => checking.arrived = Some(Arrived::Analysis { analysis, texts }),
            Err(err) => {
                self.effect(Effect::Log(format!(
                    "could not analyze the last recorded revision: {err}"
                )));
                self.verify();
            }
        }
    }

    /// Takes in the verified review, or why it could not be verified, which shows with the next
    /// frame in place of the pending view.
    pub(super) fn verified(&mut self, verified: Result<Box<Verified>, Unverified>) {
        if let Some(checking) = &mut self.checking {
            checking.arrived = Some(Arrived::Verified(verified));
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
            Arrived::Verified(Ok(verified)) => return self.show_verified(*verified, None, None),
            Arrived::Verified(Err(unverified)) => unverified,
        };
        let why = match unverified {
            // The status line says the review ended.
            Unverified::Replaced(replaced) => {
                self.stop(replaced, false);
                replaced.message().to_owned()
            }
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
        checking.files = Changes::of(&analysis, checking.record.number).files();
        checking.analysis = Some(analysis);
        self.blobs.texts.extend(texts);
        self.shown += 1;
        self.request_texts();
        self.refresh_tree();
        self.keep_file();
        self.request_syntax();
        self.verify();
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
            record: &checking.record,
            changes: checking.analysis.is_some(),
        })
    }
}
