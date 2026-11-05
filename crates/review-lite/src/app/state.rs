//! The review snapshot on screen and the marks waiting for the engine.
//!
//! A snapshot pins one revision of one history; everything shown is derived from it and from
//! caches keyed by blob identity, so replacing the history can never pair new changes with old
//! file text. A mark captures its exact units and the revision they were shown in when the
//! reviewer acts, so a later revision can never widen it.

use std::collections::HashMap;
use std::sync::Arc;

use review_core::{
    Counts, FileStatus, History, MarkVerdict, RevisionNumber, RevisionView, UnitId, UnitState,
    Verdict,
};

use crate::rows::Badge;

/// One revision of a review's history, as shown.
#[derive(Clone)]
pub struct Snapshot {
    /// The history, as last read from the store.
    pub history: Arc<History>,
    /// The revision shown; switching to a newer one is the reviewer's decision.
    pub revision: RevisionNumber,
}

impl Snapshot {
    /// The shown revision's view.
    #[must_use]
    pub fn view(&self) -> Option<RevisionView<'_>> {
        self.history.view(self.revision).ok()
    }

    /// The newest revision in the history, when it is newer than the one shown.
    #[must_use]
    pub fn newer(&self) -> Option<RevisionNumber> {
        self.history
            .current()
            .filter(|&newest| newest > self.revision)
    }
}

/// A mark the reviewer asked for: the exact units, the revision they were shown in, and the
/// history that showed them.
#[derive(Clone)]
pub struct MarkJob {
    /// The units, resolved when the reviewer acted.
    pub units: Vec<UnitId>,
    /// The revision the units were shown in.
    pub revision: RevisionNumber,
    /// The verdict.
    pub verdict: MarkVerdict,
    /// The history the units were resolved in, which the engine checks the mark against.
    pub history: Arc<History>,
}

/// The badge a pending verdict shows until the engine has recorded it.
#[must_use]
pub fn pending_badge(verdict: MarkVerdict) -> Badge {
    match verdict {
        MarkVerdict::Seen => Badge::Seen,
        MarkVerdict::Accepted => Badge::Accepted,
        MarkVerdict::Flagged => Badge::Flagged,
        MarkVerdict::Unseen => Badge::Unreviewed,
    }
}

/// The badges of `units`, with the latest pending verdict of each in place of its recorded
/// state.
#[must_use]
pub fn badges(
    view: &RevisionView<'_>,
    units: &[UnitId],
    pending: &HashMap<UnitId, MarkVerdict>,
) -> Vec<Badge> {
    units
        .iter()
        .map(|unit| match pending.get(unit) {
            Some(&verdict) => pending_badge(verdict),
            None => view.state(*unit).map_or(Badge::Unreviewed, Badge::of),
        })
        .collect()
}

/// Whether a badge counts as approved: seen or accepted, and not changed since.
#[must_use]
pub fn is_approved(badge: Badge) -> bool {
    matches!(badge, Badge::Seen | Badge::Accepted)
}

/// The verdict that toggles `badges` towards `toward`: back to unreviewed when every unit
/// already has it, `toward` otherwise. Approval counts seen units as approved.
#[must_use]
pub fn toggled(badges: &[Badge], toward: MarkVerdict) -> MarkVerdict {
    let all = !badges.is_empty()
        && badges.iter().all(|&badge| match toward {
            MarkVerdict::Accepted | MarkVerdict::Seen => is_approved(badge),
            MarkVerdict::Flagged => badge == Badge::Flagged,
            MarkVerdict::Unseen => badge == Badge::Unreviewed,
        });
    if all { MarkVerdict::Unseen } else { toward }
}

/// The engine's file statuses with `pending` verdicts applied, so the tree reacts to a mark at
/// once.
#[must_use]
pub fn statuses(
    view: &RevisionView<'_>,
    pending: &HashMap<UnitId, MarkVerdict>,
) -> Vec<FileStatus> {
    let mut files = view.status().files;
    if pending.is_empty() {
        return files;
    }
    let index: HashMap<String, usize> = files
        .iter()
        .enumerate()
        .map(|(position, file)| (file.path.clone(), position))
        .collect();
    for (&unit, &verdict) in pending {
        let (Ok(state), Ok(file)) = (view.state(unit), view.file(unit)) else {
            continue;
        };
        let Some(counts) = index
            .get(file.display_path())
            .and_then(|&position| files.get_mut(position))
            .map(|file| &mut file.counts)
        else {
            continue;
        };
        *slot(counts, from_state(state)) = slot(counts, from_state(state)).saturating_sub(1);
        *slot(counts, from_verdict(verdict)) += 1;
    }
    files
}

/// The count a unit's state falls under.
#[derive(Clone, Copy)]
enum Category {
    New,
    Changed,
    Seen,
    Accepted,
    Flagged,
}

fn from_state(state: UnitState) -> Category {
    match state {
        UnitState::New | UnitState::Unseen { .. } => Category::New,
        UnitState::ChangedSinceReviewed { .. } => Category::Changed,
        UnitState::Reviewed { verdict, .. } => match verdict {
            Verdict::Seen => Category::Seen,
            Verdict::Accepted => Category::Accepted,
            Verdict::Flagged => Category::Flagged,
        },
    }
}

fn from_verdict(verdict: MarkVerdict) -> Category {
    match verdict {
        MarkVerdict::Seen => Category::Seen,
        MarkVerdict::Accepted => Category::Accepted,
        MarkVerdict::Flagged => Category::Flagged,
        MarkVerdict::Unseen => Category::New,
    }
}

fn slot(counts: &mut Counts, category: Category) -> &mut usize {
    match category {
        Category::New => &mut counts.new,
        Category::Changed => &mut counts.changed,
        Category::Seen => &mut counts.reviewed,
        Category::Accepted => &mut counts.accepted,
        Category::Flagged => &mut counts.flagged,
    }
}

/// How much of a file or folder still needs the reviewer, for coloring its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Nothing reviewed yet.
    Open,
    /// Some units reviewed.
    Partial,
    /// Every unit approved.
    Done,
    /// Some unit flagged.
    Flagged,
    /// Some unit changed since it was reviewed.
    Changed,
}

impl Progress {
    /// The progress that `counts` describe.
    #[must_use]
    pub fn of(counts: &Counts) -> Self {
        let reviewed = counts.reviewed + counts.accepted;
        if counts.flagged > 0 {
            Self::Flagged
        } else if counts.changed > 0 {
            Self::Changed
        } else if counts.units > 0 && reviewed == counts.units {
            Self::Done
        } else if reviewed > 0 {
            Self::Partial
        } else {
            Self::Open
        }
    }

    /// Whether the reviewer still needs to look at it.
    #[must_use]
    pub fn needs_attention(self) -> bool {
        self != Self::Done
    }
}

#[cfg(test)]
mod tests {
    use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};

    use super::*;

    /// Approving toggles: a hunk that is all approved goes back to unreviewed, anything else
    /// becomes approved; flagging works the same way.
    #[test]
    fn toggles_go_back_only_when_everything_has_the_verdict() {
        let approved = [Badge::Accepted, Badge::Seen];
        assert_eq!(
            toggled(&approved, MarkVerdict::Accepted),
            MarkVerdict::Unseen
        );
        let mixed = [Badge::Accepted, Badge::Unreviewed];
        assert_eq!(
            toggled(&mixed, MarkVerdict::Accepted),
            MarkVerdict::Accepted
        );
        assert_eq!(
            toggled(&[Badge::Changed], MarkVerdict::Accepted),
            MarkVerdict::Accepted
        );
        assert_eq!(
            toggled(&[Badge::Flagged], MarkVerdict::Flagged),
            MarkVerdict::Unseen
        );
        assert_eq!(toggled(&[], MarkVerdict::Accepted), MarkVerdict::Accepted);
    }

    /// A mark keeps the exact units and revision the reviewer saw: once a newer revision is
    /// recorded, the engine refuses it instead of applying it to the new revision's units, and
    /// the snapshot announces the newer revision rather than switching to it.
    #[test]
    fn marks_never_move_to_a_newer_revision() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DemoPaths::under(dir.path());
        let review = prepare(&paths, &fixture_dir(), DemoRequest::Fresh).unwrap();
        let history = Arc::new(review.history().unwrap());
        let revision = history.current().unwrap();
        let view = history.view(revision).unwrap();
        let units = view.units_under(&["src/engine.rs".to_owned()]);
        let job = MarkJob {
            units: units.clone(),
            revision,
            verdict: MarkVerdict::Accepted,
            history: Arc::clone(&history),
        };
        // Another process records revision 2 while the mark waits.
        let second = review_core::RevisionNumber::new(2).unwrap();
        let review = prepare(&paths, &fixture_dir(), DemoRequest::UpTo(second)).unwrap();
        let saved = job
            .history
            .view(job.revision)
            .and_then(|view| review.mark(&view, &job.units, job.verdict));
        assert!(
            saved.is_err(),
            "a mark against revision 1 must not land on revision 2"
        );
        let newer = Snapshot {
            history: Arc::new(review.history().unwrap()),
            revision,
        };
        assert_eq!(newer.newer(), Some(second));
        assert_eq!(
            newer.revision, revision,
            "the shown revision does not change by itself"
        );
    }

    /// A pending verdict moves its unit between the counts of its file before the engine has
    /// recorded it, and the snapshot reports a newer revision only when there is one.
    #[test]
    fn pending_verdicts_show_in_file_statuses() {
        let dir = tempfile::tempdir().unwrap();
        let review = prepare(
            &DemoPaths::under(dir.path()),
            &fixture_dir(),
            DemoRequest::Fresh,
        )
        .unwrap();
        let history = Arc::new(review.history().unwrap());
        let snapshot = Snapshot {
            revision: history.current().unwrap(),
            history: Arc::clone(&history),
        };
        assert!(snapshot.newer().is_none());
        let view = snapshot.view().unwrap();
        let unit = view
            .unit_ids()
            .into_iter()
            .find(|&unit| matches!(view.state(unit), Ok(UnitState::New)))
            .unwrap();
        let path = view.file(unit).unwrap().display_path().to_owned();
        let counts = |files: &[FileStatus]| {
            files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
                .counts
                .clone()
        };
        let before = counts(&statuses(&view, &HashMap::new()));
        let pending = HashMap::from([(unit, MarkVerdict::Accepted)]);
        let after = counts(&statuses(&view, &pending));
        assert_eq!(after.new + 1, before.new);
        assert_eq!(after.accepted, before.accepted + 1);
        assert_eq!(
            badges(&view, &[unit], &pending),
            [Badge::Accepted],
            "the pending verdict shows as the unit's badge"
        );
    }
}
