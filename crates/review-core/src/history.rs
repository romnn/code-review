//! The full history of a review: every revision with its analysis, the correspondences between
//! consecutive revisions, and the marks, notes and walkthroughs from the event log.
//!
//! Review state is derived here.
//! Correspondences chain across revisions, so each unit knows its last reviewed predecessor even
//! when intermediate revisions were never reviewed.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::analysis::Analysis;
use crate::correspond::{Correspondence, Link, LinkKind};
use crate::error::Error;
use crate::ids::{ObjectId, RevisionNumber, UnitId, UnitIndex};
use crate::standing::ReviewPolicy;
use crate::store::events::{Event, MarkVerdict, NoteEvent, RevisionEvent, WalkthroughEvent};
use crate::view::RevisionView;

/// A verdict that counts as reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Looked at.
    Seen,
    /// Approved.
    Accepted,
    /// Needs attention.
    Flagged,
}

impl Verdict {
    /// Returns the verdict's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Seen => "seen",
            Self::Accepted => "accepted",
            Self::Flagged => "flagged",
        }
    }
}

/// The review state of one unit at one revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum UnitState {
    /// No marked predecessor: never reviewed.
    New,
    /// Explicitly marked `unseen` here or on its chain: requires review again.
    Unseen {
        /// The unit the `unseen` mark was made on.
        marked: UnitId,
    },
    /// Reviewed: marked here, or every link back to the mark is unchanged.
    Reviewed {
        /// The verdict.
        verdict: Verdict,
        /// The unit the mark was made on.
        marked: UnitId,
        /// Whether the mark was made on an earlier revision.
        inherited: bool,
    },
    /// A predecessor was reviewed, but the change differs since, or its identity is uncertain;
    /// or some of its changes since were approved and others were not.
    ChangedSinceReviewed {
        /// The verdict of the last review.
        verdict: Verdict,
        /// The unit that was last reviewed, for the interdiff: the unit a whole mark was made
        /// on, or the one the last partial approval was, whose approved text then stands for
        /// the reviewed change.
        reviewed: UnitId,
    },
}

impl UnitState {
    /// Returns whether the unit counts as reviewed.
    #[must_use]
    pub fn is_reviewed(self) -> bool {
        matches!(self, Self::Reviewed { .. })
    }

    /// Returns the state's short name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Unseen { .. } => "unseen",
            Self::Reviewed { verdict, .. } => verdict.name(),
            Self::ChangedSinceReviewed { .. } => "changed",
        }
    }
}

/// A revision as recorded in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RevisionRecord {
    /// The revision's number.
    pub number: RevisionNumber,
    /// The base snapshot.
    pub base: ObjectId,
    /// The head snapshot.
    pub head: ObjectId,
    /// The target branch commit, when there is one.
    pub target: Option<ObjectId>,
    /// Every merge base git reported.
    pub merge_bases: Vec<ObjectId>,
    /// Seconds since the Unix epoch.
    pub recorded_at: u64,
    /// The branch checked out when the working tree was captured, if one was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit `HEAD` was at when the working tree was captured, if it had one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<ObjectId>,
    /// The number of the branch's pull request when the working tree was captured, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<u64>,
}

impl From<&RevisionEvent> for RevisionRecord {
    fn from(event: &RevisionEvent) -> Self {
        Self {
            number: event.number,
            base: event.base.clone(),
            head: event.head.clone(),
            target: event.target.clone(),
            merge_bases: event.merge_bases.clone(),
            recorded_at: event.recorded_at,
            branch: event.branch.clone(),
            head_commit: event.head_commit.clone(),
            pull_request: event.pull_request,
        }
    }
}

/// The links touching one unit, toward the previous and the next revision.
#[derive(Debug, Clone, Default)]
pub(crate) struct UnitLinks {
    pub predecessors: Vec<Link>,
    pub successors: Vec<Link>,
}

/// One revision's data within a [`History`].
pub(crate) struct RevisionData {
    pub record: RevisionRecord,
    pub analysis: Arc<Analysis>,
    pub links: Vec<UnitLinks>,
    pub states: Vec<UnitState>,
}

/// Everything known about a review, with review state derived for every revision.
///
/// A history read for a live working-tree review may end in a provisional revision: the
/// working tree as captured just now, numbered after the last recorded revision but not
/// recorded, so it takes part in no other history; see [`Self::is_provisional`].
pub struct History {
    revisions: Vec<RevisionData>,
    /// The reviewed text of each unit whose latest review action is a partial approval, which
    /// every later unit whose state names that unit compares with.
    approved: HashMap<UnitId, String>,
    notes: Vec<NoteEvent>,
    walkthroughs: Vec<WalkthroughEvent>,
    warnings: Vec<String>,
    version: u64,
    provisional: bool,
}

/// The inputs to build a [`History`].
pub(crate) struct HistoryInput {
    /// The log's events, followed by the provisional revision's event when there is one.
    pub events: Vec<Event>,
    pub analyses: Vec<Arc<Analysis>>,
    pub correspondences: Vec<Arc<Correspondence>>,
    /// The length of the log the events were read from, in bytes.
    pub version: u64,
    /// Whether the last revision is provisional.
    pub provisional: bool,
}

impl History {
    /// Builds the history and derives every unit's state.
    pub(crate) fn new(input: HistoryInput) -> Self {
        let HistoryInput {
            events,
            analyses,
            correspondences,
            version,
            provisional,
        } = input;
        let mut records = Vec::new();
        let mut actions = Vec::new();
        let mut notes = Vec::new();
        let mut walkthroughs = Vec::new();
        for event in events {
            match event {
                Event::Revision(revision) => records.push(RevisionRecord::from(&revision)),
                Event::Mark(mark) => actions.push(LoggedAction {
                    revision: mark.revision,
                    unit: mark.unit,
                    evidence: mark.evidence,
                    action: Action::Mark(mark.verdict),
                }),
                Event::PartialApproval(approval) => actions.push(LoggedAction {
                    revision: approval.revision,
                    unit: approval.unit,
                    evidence: approval.evidence,
                    action: Action::Partial(approval.text),
                }),
                Event::Note(note) => notes.push(note),
                Event::Walkthrough(walkthrough) => walkthroughs.push(walkthrough),
                // Trust gates tools in the front ends; it does not change review state.
                Event::Trust(_) => {}
            }
        }
        let mut revisions: Vec<RevisionData> = records
            .into_iter()
            .zip(analyses)
            .map(|(record, analysis)| RevisionData {
                links: vec![UnitLinks::default(); analysis.units.len()],
                states: Vec::new(),
                record,
                analysis,
            })
            .collect();
        for (index, correspondence) in correspondences.iter().enumerate() {
            for link in &correspondence.links {
                if let Some(entry) = revisions
                    .get_mut(index)
                    .and_then(|older| older.links.get_mut(link.older.get()))
                {
                    entry.successors.push(*link);
                }
                if let Some(entry) = revisions
                    .get_mut(index + 1)
                    .and_then(|newer| newer.links.get_mut(link.newer.get()))
                {
                    entry.predecessors.push(*link);
                }
            }
        }
        let mut history = Self {
            revisions,
            approved: HashMap::new(),
            notes: Vec::new(),
            walkthroughs: Vec::new(),
            warnings: Vec::new(),
            version,
            provisional,
        };
        history.notes = notes
            .into_iter()
            .filter_map(|mut note| {
                note.unit =
                    history.check_evidence(note.revision, note.unit, &note.evidence, "note")?;
                Some(note)
            })
            .collect();
        history.walkthroughs = walkthroughs;
        let direct = history.direct_actions(actions);
        history.derive_states(&direct);
        history.approved = direct
            .into_iter()
            .filter_map(|(unit, action)| match action {
                Action::Partial(text) => Some((unit, text)),
                Action::Mark(_) => None,
            })
            .collect();
        history
    }

    /// Resolves a recorded unit reference, verifying its evidence digest.
    ///
    /// When the unit at the recorded index has different evidence (an analysis recomputed with a
    /// different numbering), the unit with that exact evidence is used instead; when there is
    /// none, the reference is dropped with a warning.
    fn check_evidence(
        &mut self,
        revision: RevisionNumber,
        unit: UnitIndex,
        evidence: &str,
        what: &str,
    ) -> Option<UnitIndex> {
        let analysis = &self.revisions.get(revision.index())?.analysis;
        if analysis.evidence_digest(unit).as_deref() == Some(evidence) {
            return Some(unit);
        }
        let found = analysis.find_by_digest(evidence);
        if found.is_none() {
            self.warnings.push(format!(
                "ignored a {what} on {}: the unit's evidence no longer matches",
                UnitId::new(revision, unit)
            ));
        }
        found
    }

    /// Returns the latest valid review action per unit, keyed by revision and unit: a later
    /// mark replaces a partial approval and the other way around.
    fn direct_actions(&mut self, actions: Vec<LoggedAction>) -> HashMap<UnitId, Action> {
        let mut direct = HashMap::new();
        for recorded in actions {
            let what = match recorded.action {
                Action::Mark(_) => "mark",
                Action::Partial(_) => "partial approval",
            };
            if let Some(unit) =
                self.check_evidence(recorded.revision, recorded.unit, &recorded.evidence, what)
            {
                direct.insert(UnitId::new(recorded.revision, unit), recorded.action);
            }
        }
        direct
    }

    /// Derives every unit's state, revision by revision.
    fn derive_states(&mut self, direct: &HashMap<UnitId, Action>) {
        for index in 0..self.revisions.len() {
            let Some(number) = RevisionNumber::from_index(index) else {
                continue;
            };
            let previous: Vec<UnitState> = index
                .checked_sub(1)
                .and_then(|previous| self.revisions.get(previous))
                .map(|previous| previous.states.clone())
                .unwrap_or_default();
            let Some(revision) = self.revisions.get_mut(index) else {
                continue;
            };
            revision.states = (0..revision.analysis.units.len())
                .map(|unit| {
                    let unit = UnitIndex::new(unit);
                    let links = revision
                        .links
                        .get(unit.get())
                        .map(|links| links.predecessors.as_slice());
                    let id = UnitId::new(number, unit);
                    state_of(id, direct.get(&id), links.unwrap_or_default(), &previous)
                })
                .collect();
        }
    }

    /// Returns every recorded revision, oldest first.
    #[must_use]
    pub fn revisions(&self) -> Vec<&RevisionRecord> {
        self.revisions
            .iter()
            .map(|revision| &revision.record)
            .collect()
    }

    /// Returns the current revision: the last one recorded, which is what the target pointed at
    /// when it was last observed.
    ///
    /// Revisions form a linear chain in observation order, so only the current revision accepts
    /// marks and review state always flows forward in time.
    #[must_use]
    pub fn current(&self) -> Option<RevisionNumber> {
        RevisionNumber::from_index(self.revisions.len().checked_sub(1)?)
    }

    /// Returns how much of the review's log this history was read from: the log's length in
    /// bytes.
    ///
    /// The log only grows, so of two histories of one review, the one with the larger version
    /// knows every mark and revision the other knows; one with an equal version knows the same,
    /// apart from a provisional revision.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Returns whether revision `number` is provisional: the working tree as captured for a live
    /// review, shown but not recorded.
    ///
    /// Only the [current](Self::current) revision can be provisional.
    /// [`Review::mark`](crate::Review::mark) records it before its marks, provided the log
    /// still ends at the revision before it, so a provisional revision is never marked unless
    /// it is recorded exactly as shown.
    #[must_use]
    pub fn is_provisional(&self, number: RevisionNumber) -> bool {
        self.provisional && self.current() == Some(number)
    }

    /// Returns warnings about marks, notes or walkthroughs that could not be applied.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Returns a view of revision `number` that decides by marks alone, under
    /// [`ReviewPolicy::MARKS_ONLY`].
    ///
    /// A front end that approves anything automatically builds its views with
    /// [`Self::view_under`] instead, so no embedder approves units it never asked to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownRevision`] when the revision does not exist.
    pub fn view(&self, number: RevisionNumber) -> Result<RevisionView<'_>, Error> {
        self.view_under(number, ReviewPolicy::MARKS_ONLY)
    }

    /// Returns a view of revision `number` that decides under `policy` which units still need
    /// the reviewer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownRevision`] when the revision does not exist.
    pub fn view_under(
        &self,
        number: RevisionNumber,
        policy: ReviewPolicy,
    ) -> Result<RevisionView<'_>, Error> {
        if number.index() >= self.revisions.len() {
            return Err(Error::UnknownRevision(number));
        }
        Ok(RevisionView::new(self, number, policy))
    }

    /// Returns a view of the [current](Self::current) revision that decides by marks alone, as
    /// [`Self::view`] does.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoRevisions`] when nothing was recorded yet.
    pub fn current_view(&self) -> Result<RevisionView<'_>, Error> {
        self.current_view_under(ReviewPolicy::MARKS_ONLY)
    }

    /// Returns a view of the [current](Self::current) revision that decides under `policy`, as
    /// [`Self::view_under`] does.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoRevisions`] when nothing was recorded yet.
    pub fn current_view_under(&self, policy: ReviewPolicy) -> Result<RevisionView<'_>, Error> {
        self.view_under(self.current().ok_or(Error::NoRevisions)?, policy)
    }

    pub(crate) fn revision(&self, number: RevisionNumber) -> Option<&RevisionData> {
        self.revisions.get(number.index())
    }

    pub(crate) fn notes(&self) -> &[NoteEvent] {
        &self.notes
    }

    pub(crate) fn walkthroughs(&self) -> &[WalkthroughEvent] {
        &self.walkthroughs
    }

    /// Returns the reviewed text a partial approval left on unit `unit`, when that is the
    /// unit's latest review action.
    pub(crate) fn approved_text(&self, unit: UnitId) -> Option<&str> {
        self.approved.get(&unit).map(String::as_str)
    }

    /// Returns the predecessor links of a unit.
    pub(crate) fn predecessors(&self, unit: UnitId) -> &[Link] {
        self.revision(unit.revision())
            .and_then(|revision| revision.links.get(unit.index().get()))
            .map_or(&[], |links| links.predecessors.as_slice())
    }

    /// Returns the successor links of a unit.
    pub(crate) fn successors(&self, unit: UnitId) -> &[Link] {
        self.revision(unit.revision())
            .and_then(|revision| revision.links.get(unit.index().get()))
            .map_or(&[], |links| links.successors.as_slice())
    }
}

/// A review action recorded in the log, before its unit reference is checked.
struct LoggedAction {
    revision: RevisionNumber,
    unit: UnitIndex,
    evidence: String,
    action: Action,
}

/// What a reviewer did to a unit.
enum Action {
    /// Marked it as a whole.
    Mark(MarkVerdict),
    /// Approved one of its changes since it was reviewed, leaving this reviewed text.
    Partial(String),
}

/// Derives one unit's state from its own latest review action and its predecessors' states.
fn state_of(
    unit: UnitId,
    direct: Option<&Action>,
    predecessors: &[Link],
    previous: &[UnitState],
) -> UnitState {
    let verdict = match direct {
        Some(Action::Mark(MarkVerdict::Unseen)) => return UnitState::Unseen { marked: unit },
        Some(Action::Mark(MarkVerdict::Seen)) => Some(Verdict::Seen),
        Some(Action::Mark(MarkVerdict::Accepted)) => Some(Verdict::Accepted),
        Some(Action::Mark(MarkVerdict::Flagged)) => Some(Verdict::Flagged),
        // Part of the unit is approved, so it stays open, with the approved text to compare
        // the rest with.
        Some(Action::Partial(_)) => {
            return UnitState::ChangedSinceReviewed {
                verdict: Verdict::Accepted,
                reviewed: unit,
            };
        }
        None => None,
    };
    if let Some(verdict) = verdict {
        return UnitState::Reviewed {
            verdict,
            marked: unit,
            inherited: false,
        };
    }
    let state = |link: &Link| {
        previous
            .get(link.older.get())
            .copied()
            .unwrap_or(UnitState::New)
    };
    match predecessors {
        [] => UnitState::New,
        [link] if link.kind == LinkKind::Unchanged => match state(link) {
            UnitState::Reviewed {
                verdict, marked, ..
            } => UnitState::Reviewed {
                verdict,
                marked,
                inherited: true,
            },
            other => other,
        },
        links => {
            // A changed, untrusted or ambiguous link never carries a mark; the newest review
            // on any chain only provides the interdiff.
            let states: Vec<UnitState> = links.iter().map(state).collect();
            if let Some(unseen) = states
                .iter()
                .find(|state| matches!(state, UnitState::Unseen { .. }))
            {
                return *unseen;
            }
            states
                .iter()
                .filter_map(|state| match *state {
                    UnitState::Reviewed {
                        verdict, marked, ..
                    } => Some((marked, verdict)),
                    UnitState::ChangedSinceReviewed { verdict, reviewed } => {
                        Some((reviewed, verdict))
                    }
                    UnitState::New | UnitState::Unseen { .. } => None,
                })
                .max_by_key(|(reviewed, _)| *reviewed)
                .map_or(UnitState::New, |(reviewed, verdict)| {
                    UnitState::ChangedSinceReviewed { verdict, reviewed }
                })
        }
    }
}
