//! Following a review of a working tree live: the working tree as captured now, shown as a
//! provisional revision that is recorded only once the reviewer marks it.
//!
//! A capture becomes a revision only when something refers to it, so a working tree an agent
//! edits for an hour leaves one revision per reviewed state, not one per save.
//! Review state carries from the last recorded revision to the provisional one along the
//! correspondence between the two, as it does between any consecutive revisions.

use super::checkout::{self, Observed};
use super::github::CheckoutPullRequest;
use super::{Review, ReviewTarget, RevisionMeta, Session, Subject, revision_events};
use crate::error::Error;
use crate::git::Busy;
use crate::history::{History, RevisionRecord};
use crate::store;
use crate::store::events::RevisionEvent;
use crate::store::log::{LogTail, LogWriter};
use crate::view::RevisionView;

/// What a capture of the working tree came to; see [`Review::live_history`].
pub enum Capture {
    /// The history, ending in the capture.
    Captured(Box<LiveCapture>),
    /// Git is in the middle of an operation in the working tree, so nothing was captured; the
    /// last capture shown stays.
    Busy(Busy),
    /// `HEAD`, the base or the index changed while the capture was taken, or the review
    /// recorded another revision meanwhile, so it was discarded; capturing again shows what is
    /// there now.
    Moved,
    /// The checkout is another clone now (see [`Review::verify_clone`]), so nothing was
    /// captured; the review has to be opened again.
    Replaced,
}

/// A capture of the working tree, with what it was taken of.
pub struct LiveCapture {
    /// The history, ending in a provisional revision of the capture when it differs from the
    /// last recorded revision.
    pub history: History,
    /// What the capture was taken of and compared with.
    pub subject: Subject,
    /// See [`crate::WorktreeCapture::filtered`].
    pub filtered: Vec<String>,
}

impl Review {
    /// Captures the working tree and returns the history, ending in a provisional revision of
    /// the capture when it differs from the last recorded revision, with what the capture was
    /// taken of; `pull_requests` are what the caller last found for the branches it looked up
    /// (see [`Self::subject`]).
    ///
    /// Nothing is recorded: the log, the review's pinned snapshots and the caches stay as they
    /// are. The capture's base, head and target are pinned under `session` before it is
    /// analyzed, so its objects stay while it shows although no revision records it; first,
    /// `session`'s pins of every capture but those in `keep`, the ones the caller still shows
    /// or holds for later, go. A session captures one at a time, so none of its other pins can
    /// be in use.
    /// [`Self::mark`] records a provisional revision before its marks; see
    /// [`History::is_provisional`].
    ///
    /// The capture is one look at the working tree, which a change made while it is taken
    /// may tear, such as an editor that saved one file of several; a caller that watches the
    /// working tree learns of such a change and captures again, and one that does not takes
    /// [`Self::confirmed_live_history`].
    /// A capture is discarded as [`Capture::Moved`] when the review recorded another revision
    /// while it was taken, since the history would end in a revision it never saw.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTarget`] for a review that is not of a working tree or a
    /// `session` of another review, and an error when the capture, git or the log fails.
    pub fn live_history(
        &self,
        session: &Session,
        keep: &[RevisionRecord],
        pull_requests: &[CheckoutPullRequest],
    ) -> Result<Capture, Error> {
        self.live_history_with(session, keep, pull_requests, Looks::Once, &mut || Ok(()))
    }

    /// Does what [`Self::live_history`] does, capturing the working tree twice and keeping the
    /// capture only when both saw the same, for a caller that does not watch the working tree.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::live_history`].
    pub fn confirmed_live_history(
        &self,
        session: &Session,
        keep: &[RevisionRecord],
        pull_requests: &[CheckoutPullRequest],
    ) -> Result<Capture, Error> {
        self.live_history_with(session, keep, pull_requests, Looks::Twice, &mut || Ok(()))
    }

    /// See [`Self::live_history`]; `captured` runs between the capture and its checks, where a
    /// test makes something happen meanwhile.
    #[tracing::instrument(name = "live_history", skip_all, fields(?looks))]
    fn live_history_with(
        &self,
        session: &Session,
        keep: &[RevisionRecord],
        pull_requests: &[CheckoutPullRequest],
        looks: Looks,
        captured: &mut dyn FnMut() -> Result<(), Error>,
    ) -> Result<Capture, Error> {
        if !matches!(
            self.meta.target,
            ReviewTarget::Worktree { .. } | ReviewTarget::Checkout { .. }
        ) {
            return Err(Error::InvalidTarget(
                "only a review of a working tree follows it".to_owned(),
            ));
        }
        if !session
            .pins()
            .starts_with(&format!("{}/", self.live_pins()))
        {
            return Err(Error::InvalidTarget(
                "the session belongs to another review".to_owned(),
            ));
        }
        if !self.verify_clone()? {
            return Ok(Capture::Replaced);
        }
        session.release(keep)?;
        // The revision the capture follows is the last one recorded before it was taken.
        let expected = self.last_revision()?;
        let observed = match looks {
            Looks::Once => self.capture(pull_requests)?,
            Looks::Twice => self.capture_twice(pull_requests)?,
        };
        captured()?;
        let checkout::Captured {
            input,
            subject,
            filtered,
        } = match observed {
            Observed::Captured(captured) => *captured,
            Observed::Busy(busy) => return Ok(Capture::Busy(busy)),
            Observed::Moved => return Ok(Capture::Moved),
        };
        if !self.verify_clone()? {
            return Ok(Capture::Replaced);
        }
        session.pin(&input)?;
        let mut tail = LogTail::new(&self.dir);
        let events = tail.poll()?;
        let version = tail.offset();
        let revisions = revision_events(&self.dir.events(), &events)?;
        let last = revisions.last();
        if last.map(|last| last.number) != expected {
            return Ok(Capture::Moved);
        }
        let history = if last.is_some_and(|last| last.base == input.base && last.head == input.head)
        {
            self.history_of(events, version, None)?
        } else {
            let RevisionMeta {
                branch,
                head_commit,
                pull_request,
            } = RevisionMeta::of(&subject);
            let live = RevisionEvent {
                number: self.next_number(last)?,
                base: input.base,
                head: input.head,
                target: input.target,
                merge_bases: input.merge_bases,
                recorded_at: store::now_seconds(),
                branch,
                head_commit,
                pull_request,
            };
            self.history_of(events, version, Some(live))?
        };
        Ok(Capture::Captured(Box::new(LiveCapture {
            history,
            subject,
            filtered,
        })))
    }

    /// Makes sure `view`'s revision is recorded before events that refer to it are appended to
    /// `log`, under its lock; with `tip`, it must also be the last revision recorded.
    ///
    /// See [`Self::unsettled`] for what it records.
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
        let Some(revision) = self.unsettled(log, view, tip)? else {
            return Ok(());
        };
        let analysis = view
            .analysis()
            .ok_or(Error::UnknownRevision(view.number()))?;
        self.append_revision(log, revision, analysis)
    }

    /// Returns the revision event [`Self::settle`] would record for `view` in `log`, under its
    /// lock, without recording it; with `tip`, `view`'s revision must be the last one.
    ///
    /// A recorded revision needs no event.
    /// A provisional one needs one when the log still ends at the revision before it, exactly
    /// as `view`'s history has it, and none when the log already ends at it, as when an
    /// earlier mark recorded it; anything else means the review moved on since `view` was
    /// read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotCurrentRevision`] when the log moved on.
    pub(super) fn unsettled(
        &self,
        log: &LogWriter<'_>,
        view: &RevisionView<'_>,
        tip: bool,
    ) -> Result<Option<RevisionEvent>, Error> {
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
            return Ok(None);
        }
        let record = view.record().ok_or(Error::UnknownRevision(view.number()))?;
        if last.is_some_and(|last| same(last, record)) {
            return Ok(None);
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
        Ok(Some(RevisionEvent {
            number: record.number,
            base: record.base.clone(),
            head: record.head.clone(),
            target: record.target.clone(),
            merge_bases: record.merge_bases.clone(),
            recorded_at: store::now_seconds(),
            branch: record.branch.clone(),
            head_commit: record.head_commit.clone(),
            pull_request: record.pull_request,
        }))
    }
}

/// How many times a capture looks at the working tree.
#[derive(Debug, Clone, Copy)]
enum Looks {
    /// Once, for a caller that watches the working tree.
    Once,
    /// Twice, keeping the capture only when both looks saw the same.
    Twice,
}

/// Whether the recorded `event` is the revision `record`: the same number and snapshots.
fn same(event: &RevisionEvent, record: &RevisionRecord) -> bool {
    event.number == record.number && event.base == record.base && event.head == record.head
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::*;
    use crate::git::Repo;
    use crate::testing;

    /// A checkout of one commit on `main` with `a.rs` edited, and the review of it, with the
    /// directories that hold them.
    fn edited_checkout() -> eyre::Result<(tempfile::TempDir, tempfile::TempDir, Review)> {
        let repo = testing::committed(&[("a.rs", "fn a() {}\n")])?;
        let data = tempfile::tempdir()?;
        std::fs::write(repo.path().join("a.rs"), "fn a() { edited(); }\n")?;
        let review = Review::open_checkout(data.path(), Repo::discover(repo.path())?)?;
        Ok((repo, data, review))
    }

    /// A capture taken while another window records a revision is discarded: its history
    /// would end in a revision the capture never saw, and a mark would take it for the
    /// capture's predecessor.
    #[test]
    fn a_capture_is_discarded_when_a_revision_is_recorded_meanwhile() -> eyre::Result<()> {
        let (_repo, _data, review) = edited_checkout()?;
        let session = review.start_session()?;
        let captured = review.live_history_with(&session, &[], &[], Looks::Once, &mut || {
            review.refresh().map(drop)
        })?;
        assert!(matches!(captured, Capture::Moved));
        Ok(())
    }

    /// A capture taken while the checkout became another clone is not that clone's.
    #[test]
    fn a_capture_is_discarded_when_the_clone_is_replaced_meanwhile() -> eyre::Result<()> {
        let (_repo, _data, review) = edited_checkout()?;
        let session = review.start_session()?;
        let captured = review.live_history_with(&session, &[], &[], Looks::Once, &mut || {
            review.repo().delete_ref("refs/code-review/clone-id")
        })?;
        assert!(matches!(captured, Capture::Replaced));
        Ok(())
    }
}
