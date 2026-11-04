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
use crate::store::events::{
    Event, MarkEvent, MarkVerdict, NoteEvent, RevisionEvent, WalkthroughEvent,
};
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
    /// A predecessor was reviewed, but the change differs since, or its identity is uncertain.
    ChangedSinceReviewed {
        /// The verdict of the last review.
        verdict: Verdict,
        /// The unit that was last reviewed, for the interdiff.
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
pub struct History {
    revisions: Vec<RevisionData>,
    notes: Vec<NoteEvent>,
    walkthroughs: Vec<WalkthroughEvent>,
    warnings: Vec<String>,
}

/// The inputs to build a [`History`].
pub(crate) struct HistoryInput {
    pub events: Vec<Event>,
    pub analyses: Vec<Arc<Analysis>>,
    pub correspondences: Vec<Correspondence>,
}

impl History {
    /// Builds the history and derives every unit's state.
    pub(crate) fn new(input: HistoryInput) -> Self {
        let HistoryInput {
            events,
            analyses,
            correspondences,
        } = input;
        let mut records = Vec::new();
        let mut marks = Vec::new();
        let mut notes = Vec::new();
        let mut walkthroughs = Vec::new();
        for event in events {
            match event {
                Event::Revision(revision) => records.push(RevisionRecord::from(&revision)),
                Event::Mark(mark) => marks.push(mark),
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
            notes: Vec::new(),
            walkthroughs: Vec::new(),
            warnings: Vec::new(),
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
        let direct = history.direct_marks(&marks);
        history.derive_states(&direct);
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

    /// Returns the latest valid mark per unit, keyed by revision and unit.
    fn direct_marks(&mut self, marks: &[MarkEvent]) -> HashMap<UnitId, MarkVerdict> {
        let mut direct = HashMap::new();
        for mark in marks {
            if let Some(unit) =
                self.check_evidence(mark.revision, mark.unit, &mark.evidence, "mark")
            {
                direct.insert(UnitId::new(mark.revision, unit), mark.verdict);
            }
        }
        direct
    }

    /// Derives every unit's state, revision by revision.
    fn derive_states(&mut self, direct: &HashMap<UnitId, MarkVerdict>) {
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
                    state_of(
                        id,
                        direct.get(&id).copied(),
                        links.unwrap_or_default(),
                        &previous,
                    )
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

    /// Returns warnings about marks, notes or walkthroughs that could not be applied.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Returns a view of revision `number`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownRevision`] when the revision does not exist.
    pub fn view(&self, number: RevisionNumber) -> Result<RevisionView<'_>, Error> {
        if number.index() >= self.revisions.len() {
            return Err(Error::UnknownRevision(number));
        }
        Ok(RevisionView::new(self, number))
    }

    /// Returns a view of the [current](Self::current) revision.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoRevisions`] when nothing was recorded yet.
    pub fn current_view(&self) -> Result<RevisionView<'_>, Error> {
        self.view(self.current().ok_or(Error::NoRevisions)?)
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

/// Derives one unit's state from its own mark and its predecessors' states.
fn state_of(
    unit: UnitId,
    direct: Option<MarkVerdict>,
    predecessors: &[Link],
    previous: &[UnitState],
) -> UnitState {
    let verdict = match direct {
        Some(MarkVerdict::Unseen) => return UnitState::Unseen { marked: unit },
        Some(MarkVerdict::Seen) => Some(Verdict::Seen),
        Some(MarkVerdict::Accepted) => Some(Verdict::Accepted),
        Some(MarkVerdict::Flagged) => Some(Verdict::Flagged),
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
