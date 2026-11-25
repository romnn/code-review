//! The full history of a review: every revision with its analysis, the correspondences between
//! consecutive revisions, and the marks, notes and walkthroughs from the event log.
//!
//! Review state is derived here.
//! Correspondences chain across revisions, so each unit knows its last reviewed predecessor even
//! when intermediate revisions were never reviewed.
//! The log's records apply in log order, and a history read again is the one read before
//! extended by the records appended since, so it costs what they changed, not the whole review.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::analysis::Analysis;
use crate::correspond::{Correspondence, Link, LinkKind};
use crate::error::Error;
use crate::ids::{ObjectId, RevisionNumber, UnitId, UnitIndex};
use crate::standing::ReviewPolicy;
use crate::store::events::{Event, MarkVerdict, NoteEvent, RevisionEvent, WalkthroughEvent};
use crate::store::log::Prefix;
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

impl From<&RevisionRecord> for RevisionEvent {
    fn from(record: &RevisionRecord) -> Self {
        Self {
            number: record.number,
            base: record.base.clone(),
            head: record.head.clone(),
            target: record.target.clone(),
            merge_bases: record.merge_bases.clone(),
            recorded_at: record.recorded_at,
            branch: record.branch.clone(),
            head_commit: record.head_commit.clone(),
            pull_request: record.pull_request,
        }
    }
}

/// The links between two consecutive revisions, by unit on either side.
#[derive(Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct Links {
    /// For each unit of the older revision, its links to the newer one.
    successors: Vec<Vec<Link>>,
    /// For each unit of the newer revision, its links to the older one.
    predecessors: Vec<Vec<Link>>,
}

impl Links {
    /// Indexes `correspondence` between a revision of `older` units and one of `newer` units,
    /// dropping each end of a link that names a unit out of range.
    pub(crate) fn new(correspondence: &Correspondence, older: usize, newer: usize) -> Self {
        let mut links = Self {
            successors: vec![Vec::new(); older],
            predecessors: vec![Vec::new(); newer],
        };
        for link in &correspondence.links {
            if let Some(entry) = links.successors.get_mut(link.older.get()) {
                entry.push(*link);
            }
            if let Some(entry) = links.predecessors.get_mut(link.newer.get()) {
                entry.push(*link);
            }
        }
        links
    }
}

/// One revision's data within a [`History`].
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct RevisionData {
    pub record: RevisionRecord,
    pub analysis: Arc<Analysis>,
    /// The latest valid review action on each unit that has one: a later mark replaces a
    /// partial approval and the other way around.
    actions: HashMap<UnitIndex, Action>,
    pub states: Vec<UnitState>,
}

/// Everything known about a review, with review state derived for every revision.
///
/// A history read for a live working-tree review may end in a provisional revision: the
/// working tree as captured just now, numbered after the last recorded revision but not
/// recorded, so it takes part in no other history; see [`Self::is_provisional`].
///
/// A history never changes once read.
/// Histories read one after another share whatever the log's records since left as it was,
/// and cloning one copies no text.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct History {
    revisions: Vec<Arc<RevisionData>>,
    /// The links between each revision and the next.
    links: Vec<Arc<Links>>,
    notes: Arc<Vec<NoteEvent>>,
    walkthroughs: Arc<Vec<WalkthroughEvent>>,
    warnings: Arc<Vec<String>>,
    /// The part of the log this history was read from.
    prefix: Prefix,
    provisional: bool,
}

/// What extends a [`History`]: the records appended to the log since it was read, or a
/// provisional revision, and where to get what each revision among them needs.
pub(crate) struct HistoryInput<'a> {
    /// The records, in log order, followed by the provisional revision's when there is one.
    pub events: Vec<Event>,
    /// The part of the log the history has read once extended.
    pub prefix: Prefix,
    /// Whether the last revision is provisional.
    pub provisional: bool,
    /// Returns the analysis of a revision.
    pub analysis: &'a dyn Fn(&RevisionEvent) -> Result<Arc<Analysis>, Error>,
    /// Returns the links into a revision from the revision before it.
    pub links: &'a LinksInto<'a>,
}

/// Returns the links into a revision from the revision before it, given the revision, the
/// analysis of the one before and its own.
pub(crate) type LinksInto<'a> =
    dyn Fn(&RevisionEvent, &Analysis, &Analysis) -> Result<Arc<Links>, Error> + 'a;

impl History {
    /// Returns the history of a review that recorded nothing.
    pub(crate) fn empty() -> Self {
        Self {
            revisions: Vec::new(),
            links: Vec::new(),
            notes: Arc::default(),
            walkthroughs: Arc::default(),
            warnings: Arc::default(),
            prefix: Prefix::empty(),
            provisional: false,
        }
    }

    /// Returns this history extended by `input`: its records applied in log order, then the
    /// states derived again from the earliest revision they add or act on.
    ///
    /// Every revision before that one stays shared with this history, and so do the links
    /// between them; only a history of recorded revisions is extended, since nothing follows a
    /// provisional revision.
    /// Applying a log's records in one input or in several gives the same history, which is
    /// how a history read again equals one read from the start.
    ///
    /// # Errors
    ///
    /// Returns the errors of `input`'s `analysis` and `links`.
    pub(crate) fn extended(&self, input: HistoryInput<'_>) -> Result<Self, Error> {
        let HistoryInput {
            events,
            prefix,
            provisional,
            analysis,
            links,
        } = input;
        let mut history = Self {
            prefix,
            provisional,
            ..self.clone()
        };
        let mut from = history.revisions.len();
        for event in events {
            let acted_on = match event {
                Event::Revision(revision) => {
                    let analysis = analysis(&revision)?;
                    if let Some(older) = history.revisions.last() {
                        let links = links(&revision, &older.analysis, &analysis)?;
                        history.links.push(links);
                    }
                    history.revisions.push(Arc::new(RevisionData {
                        record: RevisionRecord::from(&revision),
                        analysis,
                        actions: HashMap::new(),
                        states: Vec::new(),
                    }));
                    None
                }
                Event::Mark(mark) => history.act(
                    mark.revision,
                    mark.unit,
                    &mark.evidence,
                    Action::Mark(mark.verdict),
                ),
                Event::PartialApproval(approval) => history.act(
                    approval.revision,
                    approval.unit,
                    &approval.evidence,
                    Action::Partial(approval.text),
                ),
                Event::Note(mut note) => {
                    if let Some(unit) =
                        history.check_evidence(note.revision, note.unit, &note.evidence, "note")
                    {
                        note.unit = unit;
                        Arc::make_mut(&mut history.notes).push(note);
                    }
                    None
                }
                Event::Walkthrough(walkthrough) => {
                    Arc::make_mut(&mut history.walkthroughs).push(walkthrough);
                    None
                }
                // Trust gates tools in the front ends; it does not change review state.
                Event::Trust(_) => None,
            };
            if let Some(index) = acted_on {
                from = from.min(index);
            }
        }
        history.derive_states(from);
        Ok(history)
    }

    /// Applies a review action recorded in the log to unit `unit` of revision `revision`, once
    /// its evidence is checked, and returns the revision's index when it applied.
    fn act(
        &mut self,
        revision: RevisionNumber,
        unit: UnitIndex,
        evidence: &str,
        action: Action,
    ) -> Option<usize> {
        let what = match action {
            Action::Mark(_) => "mark",
            Action::Partial(_) => "partial approval",
        };
        let unit = self.check_evidence(revision, unit, evidence, what)?;
        let data = self.revisions.get_mut(revision.index())?;
        Arc::make_mut(data).actions.insert(unit, action);
        Some(revision.index())
    }

    /// Resolves a recorded unit reference, verifying its evidence digest.
    ///
    /// When the unit at the recorded index has different evidence (an analysis recomputed with a
    /// different numbering), the unit with that exact evidence is used instead; when there is
    /// none, or the revision is not recorded before the reference, it is dropped with a warning.
    fn check_evidence(
        &mut self,
        revision: RevisionNumber,
        unit: UnitIndex,
        evidence: &str,
        what: &str,
    ) -> Option<UnitIndex> {
        let Some(data) = self.revisions.get(revision.index()) else {
            Arc::make_mut(&mut self.warnings).push(format!(
                "ignored a {what} on {}: its revision is not recorded before it",
                UnitId::new(revision, unit)
            ));
            return None;
        };
        if data.analysis.evidence_digest(unit).as_deref() == Some(evidence) {
            return Some(unit);
        }
        let found = data.analysis.find_by_digest(evidence);
        if found.is_none() {
            Arc::make_mut(&mut self.warnings).push(format!(
                "ignored a {what} on {}: the unit's evidence no longer matches",
                UnitId::new(revision, unit)
            ));
        }
        found
    }

    /// Derives the state of every unit of the revisions from index `from` on.
    #[tracing::instrument(name = "standings", skip_all, fields(from))]
    fn derive_states(&mut self, from: usize) {
        for index in from..self.revisions.len() {
            let Some(number) = RevisionNumber::from_index(index) else {
                continue;
            };
            let (earlier, later) = self.revisions.split_at_mut(index);
            let Some(revision) = later.first_mut() else {
                continue;
            };
            let previous = earlier
                .last()
                .map_or(&[][..], |previous| previous.states.as_slice());
            let links = index
                .checked_sub(1)
                .and_then(|previous| self.links.get(previous));
            let revision = Arc::make_mut(revision);
            let states = (0..revision.analysis.units.len())
                .map(|unit| {
                    let unit = UnitIndex::new(unit);
                    let predecessors = links
                        .and_then(|links| links.predecessors.get(unit.get()))
                        .map_or(&[][..], Vec::as_slice);
                    let id = UnitId::new(number, unit);
                    state_of(id, revision.actions.get(&unit), predecessors, previous)
                })
                .collect();
            revision.states = states;
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
        self.prefix.length()
    }

    /// Returns the part of the log this history was read from, which a writer acting on it
    /// checks the log still starts with.
    pub(crate) fn prefix(&self) -> &Prefix {
        &self.prefix
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
        self.revisions.get(number.index()).map(AsRef::as_ref)
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
        match self.revision(unit.revision())?.actions.get(&unit.index())? {
            Action::Partial(text) => Some(text),
            Action::Mark(_) => None,
        }
    }

    /// Returns the predecessor links of a unit.
    pub(crate) fn predecessors(&self, unit: UnitId) -> &[Link] {
        unit.revision()
            .index()
            .checked_sub(1)
            .and_then(|older| self.links.get(older))
            .and_then(|links| links.predecessors.get(unit.index().get()))
            .map_or(&[], Vec::as_slice)
    }

    /// Returns the successor links of a unit.
    pub(crate) fn successors(&self, unit: UnitId) -> &[Link] {
        self.links
            .get(unit.revision().index())
            .and_then(|links| links.successors.get(unit.index().get()))
            .map_or(&[], Vec::as_slice)
    }

    /// Returns the analysis of a revision of `base` against `head`, if the history has one.
    pub(crate) fn analysis_of(&self, base: &ObjectId, head: &ObjectId) -> Option<&Arc<Analysis>> {
        self.revisions
            .iter()
            .map(|revision| &revision.analysis)
            .find(|analysis| &analysis.base == base && &analysis.head == head)
    }

    /// Returns the links between a revision analyzed as `older` and the next one, analyzed as
    /// `newer`, if the history has two such.
    pub(crate) fn links_between(&self, older: &Analysis, newer: &Analysis) -> Option<&Arc<Links>> {
        let same = |revision: &RevisionData, analysis: &Analysis| {
            revision.analysis.base == analysis.base && revision.analysis.head == analysis.head
        };
        self.revisions
            .windows(2)
            .zip(&self.links)
            .find_map(|(window, links)| match window {
                [before, after] if same(before, older) && same(after, newer) => Some(links),
                _ => None,
            })
    }
}

#[cfg(test)]
impl History {
    /// Returns the numbers of the revisions whose data this history shares with `other`,
    /// rather than deriving its own.
    pub(crate) fn shared_revisions(&self, other: &Self) -> Vec<usize> {
        shared(&self.revisions, &other.revisions)
            .map(|index| index + 1)
            .collect()
    }

    /// Returns the numbers of the revisions whose links from the revision before them this
    /// history shares with `other`, rather than proving its own.
    pub(crate) fn shared_links(&self, other: &Self) -> Vec<usize> {
        shared(&self.links, &other.links)
            .map(|index| index + 2)
            .collect()
    }

    /// Whether this history holds the very notes, walkthroughs and warnings `other` holds,
    /// rather than copies of them.
    pub(crate) fn shares_text(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.notes, &other.notes)
            && Arc::ptr_eq(&self.walkthroughs, &other.walkthroughs)
            && Arc::ptr_eq(&self.warnings, &other.warnings)
    }
}

/// The indices at which `mine` and `theirs` hold the same allocation.
#[cfg(test)]
fn shared<'a, T>(mine: &'a [Arc<T>], theirs: &'a [Arc<T>]) -> impl Iterator<Item = usize> + 'a {
    mine.iter()
        .zip(theirs)
        .enumerate()
        .filter(|(_, (mine, theirs))| Arc::ptr_eq(mine, theirs))
        .map(|(index, _)| index)
}

/// What a reviewer did to a unit.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use color_eyre::eyre;

    use super::*;
    use crate::analysis::ANALYSIS_FORMAT;

    /// The revision numbered `number`, of a head of its own.
    fn revision(number: u32) -> eyre::Result<Event> {
        Ok(Event::Revision(RevisionEvent {
            number: RevisionNumber::new(number).ok_or_else(|| eyre::eyre!("revision 0"))?,
            base: ObjectId::parse(&"0".repeat(40))?,
            head: ObjectId::parse(&format!("{number:040}"))?,
            target: None,
            merge_bases: Vec::new(),
            recorded_at: 0,
            branch: None,
            head_commit: None,
            pull_request: None,
        }))
    }

    /// A request for the links into a revision.
    #[derive(Debug, PartialEq)]
    struct Linked {
        into: u32,
        older: ObjectId,
        newer: ObjectId,
    }

    /// Each revision added gets the analysis of its own record, and links only from the
    /// revision right before it, also when its records come in another input than that one's.
    #[test]
    fn each_revision_gets_its_own_analysis_and_links_from_the_one_before() -> eyre::Result<()> {
        let linked = RefCell::new(Vec::new());
        let analysis = |revision: &RevisionEvent| {
            Ok(Arc::new(Analysis {
                format: ANALYSIS_FORMAT,
                base: revision.base.clone(),
                head: revision.head.clone(),
                files: Vec::new(),
                units: Vec::new(),
                moves: Vec::new(),
            }))
        };
        let links = |revision: &RevisionEvent, older: &Analysis, newer: &Analysis| {
            linked.borrow_mut().push(Linked {
                into: revision.number.get(),
                older: older.head.clone(),
                newer: newer.head.clone(),
            });
            Ok(Arc::new(Links::default()))
        };
        let input = |events| HistoryInput {
            events,
            prefix: Prefix::empty(),
            provisional: false,
            analysis: &analysis,
            links: &links,
        };
        let history = History::empty()
            .extended(input(vec![revision(1)?, revision(2)?]))?
            .extended(input(vec![revision(3)?]))?;

        for record in history.revisions() {
            let analysis = history.view(record.number)?.analysis().map(|it| &it.head);
            assert_eq!(analysis, Some(&record.head));
        }
        let head = |number: u32| ObjectId::parse(&format!("{number:040}"));
        assert_eq!(
            linked.into_inner(),
            [
                Linked {
                    into: 2,
                    older: head(1)?,
                    newer: head(2)?,
                },
                Linked {
                    into: 3,
                    older: head(2)?,
                    newer: head(3)?,
                },
            ]
        );
        Ok(())
    }
}
