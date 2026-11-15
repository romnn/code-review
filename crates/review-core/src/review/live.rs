//! Following a working-tree review live: the working tree as captured now, shown as a
//! provisional revision that is recorded only once the reviewer marks it.
//!
//! A capture becomes a revision only when something refers to it, so a working tree an agent
//! edits for an hour leaves one revision per reviewed state, not one per save.
//! Review state carries from the last recorded revision to the provisional one along the
//! correspondence between the two, as it does between any consecutive revisions.

use super::{Review, ReviewTarget, revision_events};
use crate::error::Error;
use crate::history::{History, RevisionRecord};
use crate::ids::ObjectId;
use crate::store;
use crate::store::events::RevisionEvent;
use crate::store::log::{LogTail, LogWriter};
use crate::view::RevisionView;

impl Review {
    /// Captures the working tree and returns the history, ending in a provisional revision of
    /// the capture when it differs from the last recorded revision.
    ///
    /// Nothing is recorded: the log, the pinned snapshots and the caches stay as they are.
    /// A provisional capture is pinned on its own (see [`Self::live_pin`]), so its objects stay
    /// while it shows although no revision records it; first, every live pin of a capture other
    /// than the heads in `keep` goes, those the caller still shows or holds for later.
    /// Callers capture one at a time, so no other capture's pin can be in use.
    /// [`Self::mark`] records a provisional revision before its marks; see
    /// [`History::is_provisional`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTarget`] for a review that is not of the working tree, and an
    /// error when the capture, git or the log fails.
    pub fn live_history(&self, keep: &[ObjectId]) -> Result<History, Error> {
        let ReviewTarget::Worktree { base } = &self.meta.target else {
            return Err(Error::InvalidTarget(
                "only a review of the working tree follows it".to_owned(),
            ));
        };
        for pin in self.repo.refs_under(&self.live_pins())? {
            if !keep.contains(&pin.target) {
                self.repo.delete_ref(&pin.name)?;
            }
        }
        let input = self.worktree_input(base)?;
        let mut tail = LogTail::new(&self.dir);
        let events = tail.poll()?;
        let version = tail.offset();
        let revisions = revision_events(&self.dir.events(), &events)?;
        let last = revisions.last();
        if last.is_some_and(|last| last.base == input.base && last.head == input.head) {
            return self.history_of(events, version, None);
        }
        let number = self.next_number(last)?;
        self.repo
            .update_ref(&self.live_pin(&input.head), &input.head)?;
        let live = RevisionEvent {
            number,
            base: input.base,
            head: input.head,
            target: input.target,
            merge_bases: input.merge_bases,
            recorded_at: store::now_seconds(),
        };
        self.history_of(events, version, Some(live))
    }

    /// Returns the ref that pins the live capture `head` of the working tree while it shows,
    /// which no revision records; see [`Self::live_history`].
    #[must_use]
    pub fn live_pin(&self, head: &ObjectId) -> String {
        format!("{}/{head}", self.live_pins())
    }

    /// The prefix of every live pin of this review.
    fn live_pins(&self) -> String {
        format!("refs/code-review/{}/live", self.meta.key.encoded())
    }

    /// Makes sure `view`'s revision is recorded before events that refer to it are appended to
    /// `log`, under its lock; with `tip`, it must also be the last revision recorded.
    ///
    /// A recorded revision is left as it is.
    /// A provisional one is recorded now when the log still ends at the revision before it,
    /// exactly as `view`'s history has it, and is found when the log already ends at it, as
    /// when an earlier mark recorded it; anything else means the review moved on since `view`
    /// was read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotCurrentRevision`] when the log moved on, and an error when recording
    /// fails.
    pub(super) fn settle(
        &self,
        log: &mut LogWriter<'_>,
        view: &RevisionView<'_>,
        tip: bool,
    ) -> Result<(), Error> {
        let revisions = revision_events(&self.dir.events(), log.events())?;
        let last = revisions.last();
        let moved_on = || Error::NotCurrentRevision {
            revision: view.number(),
            current: last.map(|revision| revision.number),
        };
        if !view.is_provisional() {
            if tip && last.map(|revision| revision.number) != Some(view.number()) {
                return Err(moved_on());
            }
            return Ok(());
        }
        let record = view.record().ok_or(Error::UnknownRevision(view.number()))?;
        if last.is_some_and(|last| same(last, record)) {
            return Ok(());
        }
        let predecessor = view
            .number()
            .previous()
            .and_then(|number| view.history().revision(number))
            .map(|data| &data.record);
        let follows = match (last, predecessor) {
            (Some(last), Some(predecessor)) => same(last, predecessor),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        };
        if !follows {
            return Err(moved_on());
        }
        let analysis = view
            .analysis()
            .ok_or(Error::UnknownRevision(view.number()))?;
        self.append_revision(
            log,
            RevisionEvent {
                number: record.number,
                base: record.base.clone(),
                head: record.head.clone(),
                target: record.target.clone(),
                merge_bases: record.merge_bases.clone(),
                recorded_at: store::now_seconds(),
            },
            analysis,
        )
    }
}

/// Whether the recorded `event` is the revision `record`: the same number and snapshots.
fn same(event: &RevisionEvent, record: &RevisionRecord) -> bool {
    event.number == record.number && event.base == record.base && event.head == record.head
}
