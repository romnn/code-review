//! Following a review of a working tree live: the working tree as captured now, shown as a
//! provisional revision that is recorded only once the reviewer marks it.
//!
//! A capture becomes a revision only when something refers to it, so a working tree an agent
//! edits for an hour leaves one revision per reviewed state, not one per save.
//! Review state carries from the last recorded revision to the provisional one along the
//! correspondence between the two, as it does between any consecutive revisions.

use std::sync::Arc;

use super::checkout::{self, Looks, Observed};
use super::github::CheckoutPullRequest;
use super::{Review, ReviewTarget, RevisionMeta, Subject, revision_events};
use crate::error::Error;
use crate::git::Busy;
use crate::history::{History, RevisionRecord};
use crate::store;
use crate::store::events::RevisionEvent;
use crate::store::log::LogWriter;
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
    /// are, and the reviewed repository is only read.
    /// The capture's objects go into the review's archive, where they stay for as long as
    /// anything holds the review, such as a window that shows the capture or a job that reads
    /// it, although no revision records it: only an open that has the review to itself
    /// compacts the archive.
    /// [`Self::mark`] records a provisional revision before its marks; see
    /// [`History::is_provisional`].
    ///
    /// The history is the recorded one [`Self::history`] reads, with the capture on top.
    /// When neither the log nor what was captured changed since the last capture, it is that
    /// capture's history again, whose provisional revision keeps the time it was captured.
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
    /// Returns [`Error::InvalidTarget`] for a review that is not of a working tree,
    /// [`Error::StorageFull`] while the data directory retains
    /// more than its limit, so nothing is captured, and an error when the capture, git or the
    /// log fails.
    pub fn live_history(&self, pull_requests: &[CheckoutPullRequest]) -> Result<Capture, Error> {
        self.live_history_with(pull_requests, Looks::Once, &mut || Ok(()))
    }

    /// Does what [`Self::live_history`] does, capturing the working tree twice and keeping the
    /// capture only when both saw the same, for a caller that does not watch the working tree.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::live_history`].
    pub fn confirmed_live_history(
        &self,
        pull_requests: &[CheckoutPullRequest],
    ) -> Result<Capture, Error> {
        self.live_history_with(pull_requests, Looks::Twice, &mut || Ok(()))
    }

    /// See [`Self::live_history`]; `captured` runs as [`Self::capture`] says, where a test makes
    /// something happen meanwhile.
    #[tracing::instrument(name = "live_history", skip_all, fields(?looks))]
    fn live_history_with(
        &self,
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
        // The revision the capture follows is the last one recorded before it was taken, read
        // without deriving anything, so the capture waits on nothing the history needs.
        let expected = self.last_revision()?.map(|last| last.number);
        let checkout::Captured {
            input,
            subject,
            filtered,
        } = match self.capture(pull_requests, looks, captured)? {
            Observed::Captured(captured) => *captured,
            Observed::Busy(busy) => return Ok(Capture::Busy(busy)),
            Observed::Moved => return Ok(Capture::Moved),
            Observed::Replaced => return Ok(Capture::Replaced),
        };
        let recorded = self.history()?;
        if recorded.current() != expected {
            return Ok(Capture::Moved);
        }
        let last = recorded.revisions().last().copied();
        let history = if last.is_some_and(|last| last.base == input.base && last.head == input.head)
        {
            recorded
        } else {
            let RevisionMeta {
                branch,
                head_commit,
                pull_request,
            } = RevisionMeta::of(&subject);
            let live = RevisionEvent {
                number: self.next_number(expected)?,
                base: input.base,
                head: input.head,
                target: input.target,
                merge_bases: input.merge_bases,
                recorded_at: store::now_seconds(),
                branch,
                head_commit,
                pull_request,
            };
            self.live_on(&recorded, live)?
        };
        Ok(Capture::Captured(Box::new(LiveCapture {
            history,
            subject,
            filtered,
        })))
    }

    /// Returns `recorded`, a history of the recorded revisions, with the provisional revision
    /// `live` on top: the one the memo keeps when it is of the same log and the same capture,
    /// whenever that was taken.
    fn live_on(&self, recorded: &History, live: RevisionEvent) -> Result<History, Error> {
        if let Some(kept) = self.memo.live()
            && kept.prefix() == recorded.prefix()
            && kept.revisions().last().is_some_and(|&shown| {
                *shown
                    == RevisionRecord {
                        recorded_at: shown.recorded_at,
                        ..RevisionRecord::from(&live)
                    }
            })
        {
            return Ok(History::clone(&kept));
        }
        let history = self.extend_history(
            recorded,
            Vec::new(),
            *recorded.prefix(),
            Some(live),
            &|_| {},
        )?;
        self.memo.keep_live(Arc::new(history.clone()));
        Ok(history)
    }

    /// Makes sure `view`'s revision is recorded before events that refer to it are appended to
    /// `log`, under its lock; with `tip`, it must also be the last revision recorded.
    /// Returns whether it recorded it, so its analysis is published once the lock goes.
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
    ) -> Result<bool, Error> {
        let Some(revision) = self.unsettled(log, view, tip)? else {
            return Ok(false);
        };
        self.append_revision(log, revision)?;
        Ok(true)
    }

    /// Returns the revision event [`Self::settle`] would record for `view` in `log`, under its
    /// lock, without recording it; with `tip`, `view`'s revision must be the last one.
    ///
    /// A recorded revision needs no event.
    /// A provisional one needs one when the log still ends at the revision before it, exactly
    /// as `view`'s history has it, and none when the log already ends at it, as when an
    /// earlier mark recorded it; anything else means the review moved on since `view` was
    /// read.
    /// Nothing is saved for a checkout that is another clone now, nor while the data directory
    /// retains more than its limit, which a revision the log has already never meets.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CloneReplaced`] when the checkout of a review of a checkout is another
    /// clone now, [`Error::NotCurrentRevision`] when the log moved on, and
    /// [`Error::StorageFull`] when the data directory retains more than its limit.
    pub(super) fn unsettled(
        &self,
        log: &LogWriter<'_>,
        view: &RevisionView<'_>,
        tip: bool,
    ) -> Result<Option<RevisionEvent>, Error> {
        if !self.verify_clone()? {
            return Err(Error::CloneReplaced);
        }
        let revisions = revision_events(&self.dir.events(), log.events(), 0)?;
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
        self.admit()?;
        Ok(Some(RevisionEvent {
            recorded_at: store::now_seconds(),
            ..RevisionEvent::from(record)
        }))
    }
}

/// Whether the recorded `event` is the revision `record`: the same number and snapshots.
fn same(event: &RevisionEvent, record: &RevisionRecord) -> bool {
    event.number == record.number && event.base == record.base && event.head == record.head
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use color_eyre::eyre;
    use indoc::writedoc;

    use super::*;
    use crate::git::Repo;
    use crate::ids::{RevisionNumber, UnitId};
    use crate::review::Replay;
    use crate::review::memo::Memo;
    use crate::store::events::MarkVerdict;
    use crate::store::log::LogTail;
    use crate::testing;
    use crate::view::{LineRun, RevisionView, SinceReviewed};

    /// How many functions `a.rs` has; constants follow them.
    const FUNCTIONS: usize = 4;

    /// How many values [`source`] takes: one per function, then one per constant.
    const VALUES: usize = FUNCTIONS + 8;

    /// The text of `a.rs`: functions returning the first [`FUNCTIONS`] of `values`, then a
    /// constant for each of the rest, which together are one unit when the base has none.
    fn source(values: &[u32]) -> eyre::Result<String> {
        let mut text = String::new();
        for (index, value) in values.iter().enumerate() {
            if index < FUNCTIONS {
                writedoc!(
                    text,
                    "
                    fn f{index}() -> u32 {{
                        {value}
                    }}

                    "
                )?;
            } else {
                writeln!(text, "const ITEM_{index}: u32 = {value};")?;
            }
        }
        Ok(text)
    }

    /// A checkout whose `main` has `a.rs` with every function returning 0, and the review of
    /// it.
    struct Checkout {
        repo: tempfile::TempDir,
        data: tempfile::TempDir,
        review: Review,
    }

    impl Checkout {
        fn new() -> eyre::Result<Self> {
            let repo = testing::committed(&[("a.rs", &source(&[0; FUNCTIONS])?)])?;
            let data = tempfile::tempdir()?;
            let review = Review::open_checkout(data.path(), Repo::discover(repo.path())?)?;
            Ok(Self { repo, data, review })
        }

        /// The checkout with `a.rs` holding ones and then twos recorded as revisions 1 and 2.
        fn recorded_twice() -> eyre::Result<Self> {
            let checkout = Self::new()?;
            for value in [1, 2] {
                checkout.write(&[value; VALUES])?;
                checkout.review.refresh()?;
            }
            Ok(checkout)
        }

        /// Writes `a.rs` with `values` in the working tree.
        fn write(&self, values: &[u32]) -> eyre::Result<()> {
            std::fs::write(self.repo.path().join("a.rs"), source(values)?)?;
            Ok(())
        }

        /// Opens the review again, as another window or agent does, with a memo of its own.
        fn another(&self) -> eyre::Result<Review> {
            Ok(Review::open_checkout(
                self.data.path(),
                Repo::discover(self.repo.path())?,
            )?)
        }
    }

    /// Captures the working tree and returns the history.
    fn capture(review: &Review) -> eyre::Result<History> {
        match review.live_history(&[])? {
            Capture::Captured(captured) => Ok(captured.history),
            Capture::Busy(busy) => eyre::bail!("git is busy: {busy}"),
            Capture::Moved => eyre::bail!("the checkout moved during the capture"),
            Capture::Replaced => eyre::bail!("the clone was replaced"),
        }
    }

    /// Derives `history` again from the start, as a review opened anew does: from every
    /// record of the log up to the history's version, with its provisional revision on top
    /// when it has one.
    fn cold(review: &Review, history: &History) -> eyre::Result<History> {
        let fresh = Review {
            memo: Memo::default(),
            ..review.clone()
        };
        let mut tail = LogTail::new(&fresh.dir);
        let events = tail.poll()?;
        eyre::ensure!(
            tail.offset() == history.version(),
            "the log grew since the history was read"
        );
        let live = history
            .current_view()
            .ok()
            .filter(RevisionView::is_provisional)
            .and_then(|view| view.record().map(RevisionEvent::from));
        Ok(fresh.extend_history(&History::empty(), events, tail.prefix(), live, &|_| {})?)
    }

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
        // An earlier capture leaves its history in the memo, which the next one starts from.
        capture(&review)?;
        let captured =
            review.live_history_with(&[], Looks::Once, &mut || review.refresh().map(drop))?;
        assert!(matches!(captured, Capture::Moved));
        Ok(())
    }

    /// A capture taken while the checkout became another clone is not that clone's.
    #[test]
    fn a_capture_is_discarded_when_the_clone_is_replaced_meanwhile() -> eyre::Result<()> {
        let (_repo, _data, review) = edited_checkout()?;
        let captured = review.live_history_with(&[], Looks::Once, &mut || {
            let clones = review.dir.data_dir().join("clones");
            std::fs::remove_dir_all(&clones).map_err(|err| Error::io("remove", clones, err))
        })?;
        assert!(matches!(captured, Capture::Replaced));
        Ok(())
    }

    /// A mark another window saves while a capture is taken shows in the capture: it leaves
    /// the revision the capture follows as it was, and the capture reads the log after it.
    /// Only the states from the marked revision on are derived again; no link is proven again.
    #[test]
    fn a_mark_saved_while_a_capture_is_taken_shows_in_it() -> eyre::Result<()> {
        let checkout = Checkout::recorded_twice()?;
        let review = &checkout.review;
        let other = checkout.another()?;
        checkout.write(&[3; VALUES])?;
        let before = capture(review)?;
        let captured = review.live_history_with(&[], Looks::Once, &mut || {
            let history = other.history()?;
            let view = history.current_view()?;
            other.mark(&view, &view.unit_ids(), MarkVerdict::Accepted)
        })?;
        let Capture::Captured(captured) = captured else {
            eyre::bail!("the capture was discarded");
        };
        let history = captured.history;
        assert!(history.version() > before.version());
        let second = RevisionNumber::new(2).ok_or_else(|| eyre::eyre!("no revision 2"))?;
        let marked = history.view(second)?;
        for unit in marked.unit_ids() {
            assert!(marked.state(unit)?.is_reviewed());
        }
        assert_eq!(history.shared_revisions(&before), [1]);
        assert_eq!(history.shared_links(&before), [2, 3]);
        assert_eq!(history, cold(review, &history)?);
        Ok(())
    }

    /// A capture of what the last one captured, with nothing appended to the log since, is
    /// that capture's history again: nothing is derived or proven anew.
    #[test]
    fn a_capture_of_nothing_new_is_the_last_capture_again() -> eyre::Result<()> {
        let checkout = Checkout::recorded_twice()?;
        let review = &checkout.review;
        checkout.write(&[3; VALUES])?;
        let first = capture(review)?;
        let again = capture(review)?;
        assert_eq!(again.shared_revisions(&first), [1, 2, 3]);
        assert_eq!(again.shared_links(&first), [2, 3]);
        assert!(again.current_view()?.is_provisional());
        Ok(())
    }

    /// A capture of other code shares the recorded revisions and the links between them with
    /// the capture before, and proves only its own link from the last recorded revision.
    #[test]
    fn a_new_capture_proves_only_its_own_link() -> eyre::Result<()> {
        let checkout = Checkout::recorded_twice()?;
        let review = &checkout.review;
        checkout.write(&[3; VALUES])?;
        let first = capture(review)?;
        checkout.write(&[4; VALUES])?;
        let second = capture(review)?;
        assert_eq!(second.shared_revisions(&first), [1, 2]);
        assert_eq!(second.shared_links(&first), [2]);
        assert_eq!(second, cold(review, &second)?);
        Ok(())
    }

    /// Records appended to the log since the history was read derive states again only from
    /// the earliest revision they change, and leave every link between revisions as it was.
    #[test]
    fn records_appended_since_derive_again_from_the_revision_they_change() -> eyre::Result<()> {
        let checkout = Checkout::recorded_twice()?;
        let review = &checkout.review;
        let read = review.history()?;
        let view = read.current_view()?;
        let units = view.unit_ids();
        review.mark(&view, &units[..1], MarkVerdict::Accepted)?;
        let marked = review.history()?;
        // The mark is on revision 2, so revision 1 stays as it was.
        assert_eq!(marked.shared_revisions(&read), [1]);
        assert_eq!(marked.shared_links(&read), [2]);
        assert_eq!(marked, cold(review, &marked)?);

        // A note changes no state.
        review.note(&marked.current_view()?, units[0], "why?")?;
        let noted = review.history()?;
        assert_eq!(noted.shared_revisions(&marked), [1, 2]);
        assert_eq!(noted, cold(review, &noted)?);

        // A new revision adds itself and its link from revision 2.
        checkout.write(&[3; VALUES])?;
        review.refresh()?;
        let recorded = review.history()?;
        assert_eq!(recorded.revisions().len(), 3);
        assert_eq!(recorded.shared_revisions(&noted), [1, 2]);
        assert_eq!(recorded.shared_links(&noted), [2]);
        assert_eq!(recorded, cold(review, &recorded)?);
        Ok(())
    }

    /// A capture of nothing new, and a read of a log with nothing appended, hold the very
    /// notes, walkthroughs and warnings of the history before, not copies.
    #[test]
    fn a_history_read_again_copies_no_text() -> eyre::Result<()> {
        let checkout = Checkout::recorded_twice()?;
        let review = &checkout.review;
        let read = review.history()?;
        let view = read.current_view()?;
        review.note(&view, view.unit_ids()[0], "why?")?;
        let noted = review.history()?;
        assert!(review.history()?.shares_text(&noted));

        checkout.write(&[3; VALUES])?;
        let first = capture(review)?;
        let again = capture(review)?;
        assert!(first.shares_text(&noted));
        assert!(again.shares_text(&first));
        Ok(())
    }

    /// A mark logged before the revision it marks, as in a damaged log, is ignored with a
    /// warning.
    #[test]
    fn a_mark_before_its_revision_is_ignored_with_a_warning() -> eyre::Result<()> {
        let checkout = Checkout::new()?;
        let review = &checkout.review;
        checkout.write(&[1; VALUES])?;
        review.refresh()?;
        let history = review.history()?;
        let view = history.current_view()?;
        review.mark(&view, &view.unit_ids()[..1], MarkVerdict::Accepted)?;
        // The log is the revision and the mark; the mark goes first.
        let log = std::fs::read_to_string(review.dir.events())?;
        let mut reordered = log.lines().rev().collect::<Vec<_>>().join("\n");
        reordered.push('\n');
        std::fs::write(review.dir.events(), reordered)?;

        let history = checkout.another()?.history()?;
        assert_eq!(
            history.warnings(),
            ["ignored a mark on u1-0000: its revision is not recorded before it"]
        );
        assert!(
            !history
                .current_view()?
                .state(view.unit_ids()[0])?
                .is_reviewed()
        );
        Ok(())
    }

    /// The checkout's review with revision 1 recorded, and its first unit accepted, then its
    /// fourth.
    fn marked_twice() -> eyre::Result<Checkout> {
        let checkout = Checkout::new()?;
        let review = &checkout.review;
        checkout.write(&[1; VALUES])?;
        review.refresh()?;
        let history = review.history()?;
        let view = history.current_view()?;
        let units = view.unit_ids();
        for unit in [units[0], units[3]] {
            review.mark(&view, &[unit], MarkVerdict::Accepted)?;
        }
        Ok(checkout)
    }

    /// The text of `review`'s log with the mark of unit `from` of revision 1 moved to unit
    /// `to`, which is as long.
    fn mark_moved(review: &Review, from: usize, to: usize) -> eyre::Result<String> {
        let history = review.history()?;
        let view = history.view(RevisionNumber::FIRST)?;
        let units = view.unit_ids();
        let marked = |unit: UnitId| -> eyre::Result<String> {
            Ok(format!(
                r#""unit":{},"evidence":"{}""#,
                unit.index().get(),
                view.evidence_digest(unit)?
            ))
        };
        let log = std::fs::read_to_string(review.dir.events())?;
        let moved = log.replacen(&marked(units[from])?, &marked(units[to])?, 1);
        assert_eq!(moved.len(), log.len());
        assert_ne!(moved, log);
        Ok(moved)
    }

    /// Whether `result` is the error of a log another took the place of.
    fn replaced<T>(result: &Result<T, Error>) -> bool {
        matches!(result, Err(Error::LogReplaced { .. }))
    }

    /// A log that no longer ends what was read as it did, as after a backup was restored with
    /// `cp`, ends the review for every clone of it, even once the log is as it was again:
    /// nothing read from either log counts any more.
    #[test]
    fn a_log_that_ends_otherwise_ends_the_review() -> eyre::Result<()> {
        let checkout = marked_twice()?;
        let review = &checkout.review;
        let log = std::fs::read_to_string(review.dir.events())?;
        let moved = mark_moved(review, 3, 2)?;
        std::fs::write(review.dir.events(), moved)?;
        assert!(replaced(&review.history()));

        std::fs::write(review.dir.events(), log)?;
        assert!(replaced(&review.clone().history()));
        assert!(replaced(&review.live_history(&[])));
        Ok(())
    }

    /// A log another file took the place of ends the review, even when it is as long and ends
    /// as the one read did.
    #[cfg(unix)]
    #[test]
    fn a_log_another_file_took_the_place_of_ends_the_review() -> eyre::Result<()> {
        let checkout = marked_twice()?;
        let review = &checkout.review;
        let moved = mark_moved(review, 0, 1)?;
        let events = review.dir.events();
        let replacement = events.with_extension("replacement");
        std::fs::write(&replacement, moved)?;
        std::fs::rename(&replacement, &events)?;
        assert!(replaced(&review.history()));
        Ok(())
    }

    /// A log that is shorter than what was read, or gone, ends the review.
    #[test]
    fn a_log_shorter_or_gone_ends_the_review() -> eyre::Result<()> {
        let checkout = marked_twice()?;
        let review = &checkout.review;
        review.history()?;
        let other = checkout.another()?;
        other.history()?;
        let log = std::fs::read_to_string(review.dir.events())?;
        let first = log.lines().next().unwrap_or_default();
        std::fs::write(review.dir.events(), format!("{first}\n"))?;
        assert!(replaced(&review.history()));

        std::fs::remove_file(review.dir.events())?;
        assert!(replaced(&other.history()));
        Ok(())
    }

    /// A mark or a note on what a log showed writes nothing once another log took its place,
    /// even one no reader tells apart, with a record a writer died in that no writer truncates
    /// then, and it ends the review.
    #[test]
    fn nothing_is_written_to_a_log_that_no_longer_starts_with_what_showed() -> eyre::Result<()> {
        let checkout = marked_twice()?;
        let review = &checkout.review;
        let other = checkout.another()?;
        let history = review.history()?;
        let view = history.current_view()?;
        let others = other.history()?;
        let other_view = others.current_view()?;
        let unit = view.unit_ids()[1];
        // The first mark moves, out of reach of a reader's check, and a writer died while it
        // appended the next record.
        let restored = format!("{}{{\"event\":", mark_moved(review, 0, 1)?);
        std::fs::write(review.dir.events(), &restored)?;
        assert!(review.history().is_ok());

        assert!(replaced(&review.mark(
            &view,
            &[unit],
            MarkVerdict::Accepted
        )));
        assert!(replaced(&other.note(&other_view, unit, "why?")));
        assert_eq!(std::fs::read_to_string(review.dir.events())?, restored);
        assert!(replaced(&review.history()));
        Ok(())
    }

    /// Once a review found its log replaced, nothing writes to the log any more: neither a
    /// refresh that would record a revision nor a decision on trust.
    #[test]
    fn nothing_is_written_once_the_log_was_found_replaced() -> eyre::Result<()> {
        let checkout = marked_twice()?;
        let review = &checkout.review;
        review.history()?;
        let log = std::fs::read_to_string(review.dir.events())?;
        let first = log.lines().next().unwrap_or_default();
        let shorter = format!("{first}\n");
        std::fs::write(review.dir.events(), &shorter)?;
        assert!(replaced(&review.history()));

        checkout.write(&[2; VALUES])?;
        assert!(replaced(&review.refresh()));
        assert!(replaced(&review.set_trusted(true)));
        assert_eq!(std::fs::read_to_string(review.dir.events())?, shorter);
        Ok(())
    }

    /// Reading the history again, however the log grew since, gives the history read from
    /// the start, over generated sequences of edits, marks, approvals of changes and notes,
    /// and another window's revisions and marks.
    #[test]
    fn a_history_read_again_is_the_one_read_from_the_start() -> eyre::Result<()> {
        for seed in [1, 2, 3] {
            replay(seed)?;
        }
        Ok(())
    }

    /// A history read with its progress reported counts every revision it replays once, in
    /// order, out of all the log recorded, and is the history read without it; a history read
    /// again replays only what the log recorded since.
    #[test]
    fn a_replay_reports_each_revision_it_replays() -> eyre::Result<()> {
        let checkout = Checkout::recorded_twice()?;
        checkout.write(&[3; VALUES])?;
        checkout.review.refresh()?;
        let fresh = checkout.another()?;
        let reported = std::cell::RefCell::new(Vec::new());
        let history = fresh.history_reporting(&|replay| reported.borrow_mut().push(replay))?;
        // Three revisions, each counted before it is replayed, then all of them.
        let counts = |replayed: &[usize], total| {
            replayed
                .iter()
                .map(|&replayed| Replay { replayed, total })
                .collect::<Vec<_>>()
        };
        assert_eq!(reported.take(), counts(&[0, 1, 2, 3], 3));
        assert_eq!(history, checkout.another()?.history()?);
        // Only the revision recorded since is replayed again.
        checkout.write(&[4; VALUES])?;
        checkout.review.refresh()?;
        fresh.history_reporting(&|replay| reported.borrow_mut().push(replay))?;
        assert_eq!(reported.take(), counts(&[0, 1], 1));
        Ok(())
    }

    /// How many steps a generated sequence takes.
    const STEPS: usize = 30;

    /// Takes the steps the generator seeded with `seed` chooses, each followed by a capture,
    /// and sometimes a read of the recorded history first, as a window does, comparing each
    /// with the history read from the start.
    fn replay(seed: u64) -> eyre::Result<()> {
        let checkout = Checkout::new()?;
        let review = &checkout.review;
        let other = checkout.another()?;
        let mut choices = Choices(seed);
        let mut values = [1; VALUES];
        checkout.write(&values)?;
        let mut shown = capture(review)?;
        for step in 0..STEPS {
            let same = |history: &History| -> eyre::Result<()> {
                let cold = cold(review, history)?;
                assert_eq!(*history, cold, "seed {seed}, step {step}");
                Ok(())
            };
            match choices.below(10) {
                // The working tree changes, sometimes back to what it was.
                0..=2 => {
                    for _ in 0..=choices.below(3) {
                        values[choices.below(VALUES)] = u32::try_from(choices.below(3))?;
                    }
                    checkout.write(&values)?;
                }
                // The window marks the units of the capture it shows, or some of them, mostly
                // accepting them, so that changes since review follow.
                3 | 4 => {
                    let view = shown.current_view()?;
                    let units = match choices.below(2) {
                        0 => view.unit_ids(),
                        _ => choices.some(&view.unit_ids()),
                    };
                    let verdict = [
                        MarkVerdict::Accepted,
                        MarkVerdict::Accepted,
                        MarkVerdict::Seen,
                        MarkVerdict::Flagged,
                        MarkVerdict::Unseen,
                    ][choices.below(5)];
                    if !units.is_empty() {
                        refused_or_saved(review.mark(&view, &units, verdict))?;
                    }
                }
                // The first and the last constant change, which are two changes when the
                // constants were reviewed, and the window approves one change since review its
                // capture shows.
                5..=7 => {
                    for at in [FUNCTIONS, VALUES - 1] {
                        values[at] = (values[at] + 1 + u32::try_from(choices.below(2))?) % 3;
                    }
                    checkout.write(&values)?;
                    shown = capture(review)?;
                    same(&shown)?;
                    let view = shown.current_view()?;
                    let mut runs = Vec::new();
                    for unit in view.unit_ids() {
                        if let Some(SinceReviewed::Delta(delta)) = view.since_reviewed(unit)? {
                            runs.extend(delta.runs().iter().map(LineRun::shown));
                        }
                    }
                    if let Some(run) = runs.get(choices.below(runs.len())) {
                        refused_or_saved(review.approve_run(&view, run))?;
                    }
                }
                // Another window records the working tree.
                8 => {
                    other.refresh()?;
                }
                // Another window marks some units of the last revision it recorded.
                _ if choices.below(2) == 0 => {
                    let history = other.history()?;
                    if let Ok(view) = history.current_view() {
                        let units = choices.some(&view.unit_ids());
                        if !units.is_empty() {
                            other.mark(&view, &units, MarkVerdict::Accepted)?;
                        }
                    }
                }
                // The window notes on a unit of the capture it shows.
                _ => {
                    let view = shown.current_view()?;
                    if let Some(&unit) = view.unit_ids().first() {
                        refused_or_saved(review.note(&view, unit, "why?"))?;
                    }
                }
            }
            if choices.below(2) == 0 {
                same(&review.history()?)?;
            }
            shown = capture(review)?;
            same(&shown)?;
        }
        Ok(())
    }

    /// Takes a refusal of a mark, an approval or a note on a capture the log moved past, or
    /// of a change that changed since, as the window does.
    fn refused_or_saved(result: Result<(), Error>) -> eyre::Result<()> {
        match result {
            Ok(()) | Err(Error::NotCurrentRevision { .. } | Error::ChangedSinceShown { .. }) => {
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Pseudo-random choices, so a generated sequence replays from its seed.
    struct Choices(u64);

    impl Choices {
        /// Returns a choice below `bound`, or 0 when it is 0.
        fn below(&mut self, bound: usize) -> usize {
            // Marsaglia's xorshift64.
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            let bound = u64::try_from(bound).unwrap_or(u64::MAX).max(1);
            usize::try_from(self.0 % bound).unwrap_or_default()
        }

        /// Returns about half of `items`, chosen at random.
        fn some<T: Copy>(&mut self, items: &[T]) -> Vec<T> {
            items
                .iter()
                .filter(|_| self.below(2) == 0)
                .copied()
                .collect()
        }
    }
}
