//! Approving, flagging and resetting, the queue of marks waiting for the engine, and moving to
//! a new revision.
//!
//! A mark resolves its units when the reviewer acts and keeps the revision they were shown in;
//! the engine refuses it if that revision is no longer current. Marks are saved one at a time,
//! and every completion, success or failure, derives the view again from the current state and
//! starts the next mark, so none is ever left waiting.

use std::sync::Arc;

use review_core::{History, MarkVerdict, Recorded, Review, ShownRun, TrustScope, UnitId};

use super::effects::{Because, Outcome, Subject};
use super::{
    Effect, Grant, Mode, Model, QuitKind, Quitting, SyncStep, buffer, live, overview, plural,
};
use crate::rows;
use crate::state::{self, Mark, MarkJob};

impl Model {
    /// The units the current mode's review keys act on, and what to call them, resolved from
    /// what the screen shows, or why nothing there can be marked. Every mark goes through
    /// here, so a key never acts on something other than what the reviewer sees: the tree's
    /// file must be the file whose diff shows, the diff's rows must be that file's, and the
    /// buffer must be that file's.
    pub(super) fn review_target(&self) -> Result<(Vec<UnitId>, String), String> {
        let nothing = || "Nothing here to review.".to_owned();
        let view = self.view().ok_or_else(nothing)?;
        let shown = self.rows.files.first().map(|section| section.path.as_str());
        let selected = self.selected.as_deref();
        match self.mode {
            Mode::Tree => {
                let index = self.tree_index().ok_or_else(nothing)?;
                let node = self.tree.get(index).ok_or_else(nothing)?;
                if node.kind == review_core::tree::NodeKind::Directory {
                    // A filtered folder shows only some of its files; marking it would mark
                    // the hidden ones.
                    if self.filter_query().is_some() {
                        return Err("The filter hides some of this folder's files: mark files \
                                    one by one, or clear the filter with Esc to mark the whole \
                                    folder."
                            .to_owned());
                    }
                    // A folder acts on the files it shows, which its diff shows too.
                    let files = state::folder_files(&self.tree, index);
                    let previewed = self.folder.as_ref().is_some_and(|folder| {
                        folder.path == node.path && folder.files.iter().eq(files.iter())
                    });
                    if !previewed {
                        return Err("The folder's diff is not the one shown; move the cursor \
                                    to show it first."
                            .to_owned());
                    }
                    let units = files
                        .into_iter()
                        .flat_map(|path| view.units_of_file(path))
                        .collect();
                    return Ok((units, node.path.clone()));
                }
                if self.folder.is_some()
                    || selected != Some(node.path.as_str())
                    || shown != selected
                {
                    return Err("The file under the cursor is not the one shown; move the \
                                cursor to show it first."
                        .to_owned());
                }
                Ok((view.units_of_file(&node.path), node.path.clone()))
            }
            Mode::Diff | Mode::Buffer
                if self.folder.is_some() || shown.is_none() || shown != selected =>
            {
                Err(nothing())
            }
            Mode::Diff => {
                let units = self.hunk_units();
                if units.is_empty() {
                    return Err(nothing());
                }
                Ok((
                    units,
                    format!("hunk {} of {}", self.hunk + 1, self.rows.hunks.len()),
                ))
            }
            Mode::Buffer => {
                let hunk = self.buffer_hunk().ok_or_else(nothing)?;
                let units = vec![self.rows.hunks.get(hunk).ok_or_else(nothing)?.unit];
                Ok((
                    units,
                    format!("hunk {} of {}", hunk + 1, self.rows.hunks.len()),
                ))
            }
        }
    }

    /// Toggles the current target toward `toward`: everything at `toward` goes back to
    /// unreviewed, anything else takes `toward`.
    pub(super) fn review(&mut self, toward: MarkVerdict) {
        // A mark made while the app waits to quit would be one more to wait for.
        if self.quitting.waits() {
            self.set_status(format!(
                "Not marked while quitting. {}",
                self.quitting_status()
            ));
            return;
        }
        if let Some(why) = self.held_refusal() {
            self.set_status(why);
            return;
        }
        // The selected hunk must be on screen; one scrolled away is shown first, not marked.
        if self.mode == Mode::Diff && !self.hunk_in_view() {
            self.reveal_hunk();
            self.revealed();
            self.set_status(format!(
                "Hunk {} of {} was off screen; it shows now. Press again to mark it.",
                self.hunk + 1,
                self.rows.hunks.len()
            ));
            return;
        }
        if let Some(why) = self.live_refusal() {
            self.set_status(why);
            return;
        }
        let (units, what) = match self.review_target() {
            Ok(target) => target,
            Err(why) => {
                self.set_status(why);
                return;
            }
        };
        if self.refuses_changed(&units, &what) {
            return;
        }
        // One change of a unit that shows what changed since it was reviewed is approved on
        // its own, and Space never approves such a unit whole; flags and resets act on the
        // whole unit.
        if toward == MarkVerdict::Accepted
            && let Some(hunk) = self.target_hunk().cloned()
        {
            match hunk.change {
                Some(change) => {
                    self.approve_change(change, &what);
                    return;
                }
                None if self
                    .view()
                    .is_some_and(|view| rows::shows_changes_apart(&view, hunk.unit)) =>
                {
                    self.unfold_changes(hunk.unit);
                    return;
                }
                None => {}
            }
        }
        let Some(view) = self.view() else {
            return;
        };
        let badges = state::badges(&view, &units, &self.pending);
        let verdict = if toward == MarkVerdict::Unseen {
            MarkVerdict::Unseen
        } else {
            state::toggled(&badges, toward)
        };
        if !self.queue_mark(units, verdict, &what) {
            return;
        }
        if self.mode == Mode::Diff && verdict == MarkVerdict::Accepted {
            self.after_approval();
        }
    }

    /// The display hunk the review keys act on: the selected hunk in the diff, the one under the
    /// cursor in a review buffer, and none in the tree, whose keys act on files and folders.
    pub(super) fn target_hunk(&self) -> Option<&rows::Hunk> {
        let hunk = match self.mode {
            Mode::Diff => Some(self.hunk),
            Mode::Buffer => self.buffer_hunk(),
            Mode::Tree => None,
        }?;
        self.rows.hunks.get(hunk)
    }

    /// Space on a unit the reviewer folded whose changes since review show as hunks of their
    /// own: unfolds it and selects its first change, so Space never approves changes it hides.
    fn unfold_changes(&mut self, unit: UnitId) {
        self.revealed();
        self.folds.units.remove(&unit);
        self.folds.opened.insert(unit);
        self.rebuild();
        if self.mode == Mode::Diff
            && let Some(first) = self.rows.hunk_of(unit, None)
        {
            self.select_hunk(first);
        }
        self.set_status("Unfolded: press Space to approve this change.");
    }

    /// Queues the approval of `change` as the shown revision has it, or in a review buffer,
    /// shows a change that only removes lines in the diff first.
    ///
    /// The change shows approved at once, and the diff moves on to the next change that needs
    /// the reviewer, as after approving a hunk; the unit's other changes stay open, so it
    /// never looks approved as a whole. Once saved, the rows show what is left. A press on a
    /// change already on its way waits for it rather than asking again.
    fn approve_change(&mut self, change: ShownRun, what: &str) {
        if self.mode == Mode::Buffer && change.new.range().is_empty() {
            self.revealed();
            self.leave_buffer();
            self.set_status(
                "This change only removes lines, which only the diff shows; press Space to \
                 approve it.",
            );
            return;
        }
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        if let Some(newer) = snapshot.newer() {
            self.fail(format!(
                "Revision {newer} is available; press R to switch to it before reviewing."
            ));
            return;
        }
        if self.approving_change(&change) {
            self.set_status(format!("Approving {what}…"));
            return;
        }
        if self.quitting == Quitting::Stopped {
            self.quitting = Quitting::No;
        }
        let unit = change.unit;
        self.marks.push_back(MarkJob {
            mark: Mark::Run(change),
            revision: snapshot.revision,
            history: snapshot.history,
        });
        let left = self
            .rows
            .hunks
            .iter()
            .filter_map(|hunk| hunk.change.as_ref())
            .filter(|change| change.unit == unit && !self.approving_change(change))
            .count();
        self.set_status(match left {
            0 => "Approved the last change of this unit.".to_owned(),
            left => format!(
                "Approved the change; {} of this unit left.",
                plural(left, "change")
            ),
        });
        self.next_mark();
        if self.mode == Mode::Diff {
            self.after_approval();
        }
    }

    /// Whether the approval of `change` is saving or waiting to.
    pub(super) fn approving_change(&self, change: &ShownRun) -> bool {
        self.approving.as_ref() == Some(change)
            || self
                .marks
                .iter()
                .any(|job| matches!(&job.mark, Mark::Run(run) if run == change))
    }

    /// After approving a hunk, selects the next one in the file that needs the reviewer, as
    /// lazygit moves on after staging a hunk, coming around from the file's start; or, when the
    /// file has nothing left to review, keeps the hunk and says which file is next. It never
    /// leaves the file by itself, so a held Space never approves code of a file the reviewer
    /// has not seen.
    fn after_approval(&mut self) {
        if let Some(next) = self
            .open_hunk_after(Some(self.hunk))
            .or_else(|| self.open_hunk_after(None))
        {
            self.select_hunk(next);
            return;
        }
        // The approved hunk may have folded under the view; it stays in sight.
        self.reveal_hunk();
        let next = self.next_open_file();
        self.set_status(match next {
            Some(path) if self.look().cross_files => {
                format!("This file is done; ] goes on to {path}.")
            }
            Some(path) => format!("This file is done; {path} is next to review."),
            None => "This file is done, and so is every other file.".to_owned(),
        });
    }

    /// Queues `verdict` for exactly `units` of the shown revision and shows it at once; returns
    /// whether it was queued. Marking waits while a newer revision is available, because the
    /// engine records marks only against the current one.
    pub(super) fn queue_mark(
        &mut self,
        units: Vec<UnitId>,
        verdict: MarkVerdict,
        what: &str,
    ) -> bool {
        let Some(snapshot) = self.snapshot.clone() else {
            return false;
        };
        if units.is_empty() {
            self.set_status(format!("{what} has nothing to review."));
            return false;
        }
        if let Some(newer) = snapshot.newer() {
            self.fail(format!(
                "Revision {newer} is available; press R to switch to it before reviewing."
            ));
            return false;
        }
        for &unit in &units {
            self.pending.insert(unit, verdict);
        }
        // A new mark is the reviewer reviewing again, so the next quit is an ordinary one,
        // which a failure stops.
        if self.quitting == Quitting::Stopped {
            self.quitting = Quitting::No;
        }
        let count = units.len();
        self.marks.push_back(MarkJob {
            mark: Mark::Units { units, verdict },
            revision: snapshot.revision,
            history: snapshot.history,
        });
        self.refresh_tree();
        // The mark shows at once: in the hunks view an approved hunk folds now, while the
        // reviewer acts, so the save landing later moves no row.
        self.rebuild();
        let done = match verdict {
            MarkVerdict::Accepted | MarkVerdict::Seen => "Approved",
            MarkVerdict::Flagged => "Flagged",
            MarkVerdict::Unseen => "Reset",
        };
        self.set_status(format!("{done} {what} ({}).", plural(count, "unit")));
        self.next_mark();
        true
    }

    /// Saves the next queued mark in the background, unless one is being saved.
    pub(super) fn next_mark(&mut self) {
        if self.marking {
            return;
        }
        let Some(review) = self.review.clone() else {
            return;
        };
        let Some(job) = self.marks.pop_front() else {
            return;
        };
        self.marking = true;
        self.approving = match &job.mark {
            Mark::Run(run) => Some(run.clone()),
            Mark::Units { .. } => None,
        };
        self.spawn(Subject::Always(Because::Save), move || {
            let saved = job.save(&review);
            let history = review.history();
            Outcome::Marked(Marked {
                job,
                saved,
                history,
            })
        });
    }

    /// Takes in a saved (or refused) mark and starts the next one.
    pub(super) fn marked(&mut self, marked: Marked) {
        let Marked {
            job,
            saved,
            history,
        } = marked;
        self.saved(&job, saved, history);
        self.next_mark();
    }

    /// Takes in a saved (or refused) mark: the new history, the remaining pending verdicts,
    /// and the view derived again from the current selection.
    fn saved(
        &mut self,
        job: &MarkJob,
        saved: Result<(), review_core::Error>,
        history: Result<History, review_core::Error>,
    ) {
        // Pending verdicts are exactly those of the marks still queued.
        self.pending.clear();
        for queued in &self.marks {
            if let Mark::Units { units, verdict } = &queued.mark {
                for &unit in units {
                    self.pending.insert(unit, *verdict);
                }
            }
        }
        self.approving = None;
        let mut failures = Vec::new();
        let unsaved = match (saved, &job.mark) {
            (Ok(()), _) => false,
            // The rows are read again below, so the reviewer sees what the change is now; the
            // message says where it is, since Space has moved on from it.
            (Err(err @ review_core::Error::ChangedSinceShown { .. }), Mark::Run(run)) => {
                self.effect(Effect::Log(format!("did not approve a change: {err:#}")));
                let path = self
                    .view()
                    .and_then(|view| Some(view.file(run.unit).ok()?.display_path().to_owned()))
                    .unwrap_or_else(|| run.unit.to_string());
                failures.push(format!(
                    "Nothing approved at {path}:{}: the change was approved or changed since \
                     it showed, and shows as it is now",
                    run.new.start + 1
                ));
                true
            }
            (Err(err), mark) => {
                let what = match mark {
                    Mark::Units { units, .. } => {
                        format!("the mark of {}", plural(units.len(), "unit"))
                    }
                    Mark::Run(run) => format!("the approval of a change of {}", run.unit),
                };
                // The log keeps it for when the app quits before anyone reads the status line.
                self.effect(Effect::Log(format!("could not record {what}: {err:#}")));
                failures.push(format!("Could not record {what}: {err}"));
                true
            }
        };
        match history {
            Ok(history) => self.take_history(history),
            Err(err) => failures.push(format!("Could not reread the review: {err}")),
        }
        // Failures go last, so a newer revision's notice never hides them; quitting now would
        // take them off screen with the app, unless nobody can see them or the reviewer saw one
        // and chose to leave anyway.
        if unsaved {
            match self.quitting {
                Quitting::Waiting(QuitKind::Interactive) => {
                    self.quitting = Quitting::Stopped;
                    failures.push(self.stopped_hint().to_owned());
                }
                Quitting::Stopped => failures.push(self.stopped_hint().to_owned()),
                Quitting::No | Quitting::Waiting(QuitKind::Unattended) | Quitting::Done => {}
            }
        }
        if !failures.is_empty() {
            self.fail(failures.join(". "));
        }
        self.refresh_tree();
        self.rebuild_in_place();
    }

    /// Installs a freshly read history that has the snapshot on screen, the same revision of
    /// the same base and head; a newer revision is announced, never switched to, or, for a live
    /// review, captured and shown with the reviewer's place kept.
    ///
    /// A history read before the one installed knows less, such as a mark saved since, and one
    /// without the snapshot on screen would change what its rows mean, so neither is installed.
    pub(super) fn take_history(&mut self, history: History) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        if history.version() < self.version {
            return;
        }
        let shown = live::same_snapshot(
            &snapshot.history,
            &history,
            snapshot.revision,
            snapshot.revision,
        );
        if !shown {
            if self.following() {
                self.capture(false);
            }
            return;
        }
        self.version = history.version();
        let Some(snapshot) = &mut self.snapshot else {
            return;
        };
        snapshot.history = Arc::new(history);
        self.revisions = overview::summarize(&snapshot.history, snapshot.policy);
        let newer = snapshot.newer();
        match newer {
            Some(_) if self.following() => self.capture(false),
            Some(newer) => self.set_status(format!(
                "Revision {newer} is available; press R to switch to it."
            )),
            None => {}
        }
    }

    /// `R`: switches to a newer revision when there is one, otherwise fetches and records the
    /// target's latest state in the background.
    pub(super) fn refresh(&mut self) {
        // A live review shows the newest snapshot as it captures it, keeping the place, and
        // records nothing until something is marked.
        if self.following() {
            self.recapture();
            return;
        }
        if let Some(newer) = self.snapshot.as_ref().and_then(state::Snapshot::newer) {
            self.switch_revision(newer);
            return;
        }
        let Some(review) = self.review.clone() else {
            return;
        };
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.sync.failed_at = None;
        self.set_status("Fetching the latest changes…");
        let step = Arc::clone(&self.sync.step);
        *step.lock() = Some(SyncStep::Refresh(review_core::RefreshStep::Fetching));
        self.spawn(Subject::Always(Because::Refresh), move || {
            let recorded =
                review.refresh_reporting(&mut |done| *step.lock() = Some(SyncStep::Refresh(done)));
            *step.lock() = Some(SyncStep::Carrying);
            let history = review.history();
            let synced = review.synced();
            Outcome::Refreshed(Refreshed {
                recorded,
                history,
                synced,
            })
        });
        self.spin();
    }

    /// Takes in a fetch of the latest changes: the history it read, and whether it recorded a
    /// new revision.
    pub(super) fn refreshed(&mut self, refreshed: Refreshed) {
        match refreshed.synced {
            Ok(Some(synced)) => self.sync.synced = Some(synced),
            Ok(None) => {}
            Err(err) => self.effect(Effect::Log(format!(
                "could not read when the review synced: {err:#}"
            ))),
        }
        match (refreshed.recorded, refreshed.history) {
            (Ok(recorded), Ok(history)) => {
                self.take_history(history);
                // The history may carry marks saved elsewhere since, which what shows derives
                // from.
                self.refresh_tree();
                self.rebuild();
                if let Some(err) = &recorded.sync_error {
                    self.effect(Effect::Log(format!(
                        "could not store when the review synced: {err}"
                    )));
                }
                if recorded.created && self.following() {
                    self.set_status(format!("Revision {} recorded.", recorded.number));
                } else if recorded.created {
                    self.set_status(format!(
                        "Revision {} recorded; press R to switch to it.",
                        recorded.number
                    ));
                } else if self
                    .snapshot
                    .as_ref()
                    .and_then(state::Snapshot::newer)
                    .is_none()
                {
                    self.set_status("Up to date; no new revision.");
                }
            }
            (Err(err), _) | (_, Err(err)) => {
                self.fail(format!("Could not refresh: {err}"));
                self.sync_failed();
            }
        }
    }

    /// Shows `revision` instead of the current one, once no mark is waiting for the old one.
    ///
    /// One transition: everything derived from the old revision (rows, hunks, folds, places,
    /// the buffer and its search) goes first, then the selection is chosen again in the new
    /// revision, and a mode whose subject is gone falls back to one that has it.
    fn switch_revision(&mut self, revision: review_core::RevisionNumber) {
        if self.marking || !self.marks.is_empty() {
            self.set_status("Saving marks; press R again in a moment.");
            return;
        }
        let Some(snapshot) = &mut self.snapshot else {
            return;
        };
        snapshot.revision = revision;
        self.shown += 1;
        if let Some(view) = snapshot.view() {
            self.blobs.retain_for(&view);
        }
        // Unit IDs and hunks belong to a revision; the reviewer's folds and places do not
        // carry over.
        self.folds.units.clear();
        self.folds.opened.clear();
        self.folds.expanded.clear();
        self.folds.gaps.clear();
        self.places.clear();
        self.set_buffer(None);
        self.rows = std::sync::Arc::new(rows::Layout::default());
        self.hunk_rows.clear();
        self.hunk = 0;
        let previous = self.selected.take();
        self.refresh_tree();
        match previous.filter(|path| state::is_file(&self.tree, path)) {
            Some(path) => self.show_file(&path),
            None => self.select_first_file(),
        }
        let mode = match self.mode {
            _ if self.selected.is_none() => Mode::Tree,
            Mode::Buffer => Mode::Diff,
            mode => mode,
        };
        self.enter_mode(mode);
        self.request_syntax();
        self.read_snapshot_trust();
        self.set_status(format!("Showing revision {revision}. {}", self.summary()));
    }

    /// `Ctrl+T`: trusts the review after a confirming second press, or revokes trust and stops
    /// the language servers; for a review of the checkout, asks which grant to make, or
    /// revokes the one that applies.
    pub(super) fn toggle_trust(&mut self) {
        let Some(review) = self.review.clone() else {
            return;
        };
        if self.is_checkout() {
            match self.grant {
                Some(grant) => self.revoke(&review, grant),
                None => self.ask_trust(false),
            }
            return;
        }
        // The confirmation holds only for the snapshot it was asked on.
        let shown = self
            .view()
            .and_then(|view| view.record().map(|record| record.head.clone()));
        let decision = match (self.trusted, self.trust_pending.take()) {
            (true, _) => false,
            (false, Some(asked)) if shown.as_ref() == Some(&asked) => true,
            (false, _) => {
                self.trust_pending = shown;
                self.set_status(
                    "Trusting runs a language server in a worktree of the reviewed head, and \
                     through it the review's own build configuration, which can run any \
                     program. Press Ctrl+T again to trust.",
                );
                return;
            }
        };
        // A review whose key another clone at the same path shares is trusted one snapshot at
        // a time: the one on screen.
        let tree = review.trusted_per_snapshot().then(|| {
            self.view()
                .and_then(|view| view.record().map(|record| record.head.clone()))
                .ok_or_else(|| review_core::Error::InvalidTarget("no revision shows".to_owned()))
                .and_then(|head| review.repo().tree_of(&head))
        });
        let record = || match tree {
            Some(tree) => {
                tree.and_then(|tree| review.set_trust(TrustScope::Snapshot { tree }, decision))
            }
            None => review.set_trusted(decision),
        };
        // A revocation stops the language servers as it records itself.
        let recorded = if decision {
            record()
        } else {
            buffer::revoke_servers(&self.servers, || record().map(|()| true))
        };
        match recorded {
            Ok(()) => {
                self.trusted = decision;
                let done = match (decision, review.trusted_per_snapshot()) {
                    (true, true) => "Trusted this snapshot",
                    (true, false) => "Trusted",
                    (false, _) => "Trust revoked",
                };
                self.set_status(format!("{done} · {}", self.summary()));
            }
            Err(err) => self.fail(format!("Cannot record trust: {err}")),
        }
    }
}

impl Model {
    /// Asks whether to trust the checkout's code, with a grant for this snapshot or the whole
    /// checkout; with `resume`, go to definition runs once trusted.
    pub(super) fn ask_trust(&mut self, resume: bool) {
        if self.live.as_ref().and_then(|live| live.tree()).is_none() {
            self.set_status("Nothing to trust yet: the working tree has not been captured.");
            return;
        }
        self.trust_resumes = resume;
        self.overlay = Some(super::Overlay::Trust);
    }

    /// `s` or `c` in the trust question: records the grant, for this snapshot or the whole
    /// checkout, and goes on to what asked for it.
    pub(super) fn grant_trust(&mut self, grant: Grant) {
        if self.overlay != Some(super::Overlay::Trust) {
            return;
        }
        let (Some(review), Some(tree)) = (
            self.review.clone(),
            self.live.as_ref().and_then(|live| live.tree()).cloned(),
        ) else {
            return;
        };
        self.overlay = None;
        let scope = match grant {
            Grant::Snapshot => TrustScope::Snapshot { tree },
            Grant::Checkout => TrustScope::Checkout,
        };
        self.trust_decisions += 1;
        if let Err(err) = review.set_trust(scope, true) {
            self.fail(format!("Cannot record trust: {err}"));
            return;
        }
        self.set_grant(Some(grant));
        self.set_status(match grant {
            Grant::Snapshot => {
                "Trusted this snapshot: tools may run on it until the code changes. Ctrl+T \
                 revokes."
            }
            Grant::Checkout => {
                "Trusted this checkout: tools may run on whatever it holds until Ctrl+T \
                 revokes it."
            }
        });
        if std::mem::take(&mut self.trust_resumes) {
            self.go_to_definition();
        }
    }

    /// Revokes the checkout's `grant`, stopping the language servers, and says whether another
    /// grant still applies.
    fn revoke(&mut self, review: &Review, grant: Grant) {
        let Some(tree) = self.live.as_ref().and_then(|live| live.tree()).cloned() else {
            return;
        };
        let scope = match grant {
            Grant::Snapshot => TrustScope::Snapshot { tree: tree.clone() },
            Grant::Checkout => TrustScope::Checkout,
        };
        self.trust_decisions += 1;
        let decisions = self.trust_decisions;
        let mut still = None;
        // The language servers stop as the revocation records itself, unless another grant
        // still applies.
        let revoked = buffer::revoke_servers(&self.servers, || {
            review.set_trust(scope, false)?;
            still = live::granted(review, &tree, decisions).grant;
            Ok::<_, review_core::Error>(still.is_none())
        });
        if let Err(err) = revoked {
            self.fail(format!("Cannot record trust: {err}"));
            return;
        }
        self.set_grant(still);
        let revoked = match grant {
            Grant::Snapshot => "Trust in this snapshot revoked",
            Grant::Checkout => "Trust in this checkout revoked",
        };
        self.set_status(match still {
            Some(Grant::Snapshot) => {
                format!("{revoked}; this snapshot stays trusted, which Ctrl+T revokes.")
            }
            Some(Grant::Checkout) | None => format!("{revoked}. {}", self.summary()),
        });
    }
}

/// A mark saved or refused, with the review's history read after it.
pub(super) struct Marked {
    /// The mark.
    pub(super) job: MarkJob,
    /// Whether the engine recorded it.
    pub(super) saved: Result<(), review_core::Error>,
    /// The history read after it.
    pub(super) history: Result<History, review_core::Error>,
}

/// A fetch of the latest changes, with the review's history read after it.
pub(super) struct Refreshed {
    /// The revision the fetch recorded or found current.
    pub(super) recorded: Result<Recorded, review_core::Error>,
    /// The history read after it.
    pub(super) history: Result<History, review_core::Error>,
    /// When the review last synced, read after it.
    pub(super) synced: Result<Option<review_core::Synced>, review_core::Error>,
}
