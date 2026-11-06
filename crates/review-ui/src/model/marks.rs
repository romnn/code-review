//! Approving, flagging and resetting, the queue of marks waiting for the engine, and moving to
//! a new revision.
//!
//! A mark resolves its units when the reviewer acts and keeps the revision they were shown in;
//! the engine refuses it if that revision is no longer current. Marks are saved one at a time,
//! and every completion, success or failure, derives the view again from the current state and
//! starts the next mark, so none is ever left waiting.

use std::sync::Arc;

use review_core::{History, MarkVerdict, Recorded, UnitId};

use super::effects::{Because, Outcome, Subject};
use super::{Effect, Mode, Model, QuitKind, Quitting, overview, plural};
use crate::rows;
use crate::state::{self, MarkJob};

impl Model {
    /// The units the current mode's review keys act on, and what to call them, resolved from
    /// what the screen shows, or why nothing there can be marked. Every mark goes through
    /// here, so a key never acts on something other than what the reviewer sees: the tree's
    /// file must be the file whose diff shows, the diff's rows must be that file's, and the
    /// buffer must be that file's.
    fn review_target(&self) -> Result<(Vec<UnitId>, String), String> {
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
                    // A folder acts on the files it shows.
                    let units = state::folder_files(&self.tree, index)
                        .into_iter()
                        .flat_map(|path| view.units_of_file(path))
                        .collect();
                    return Ok((units, node.path.clone()));
                }
                if selected != Some(node.path.as_str()) || shown != selected {
                    return Err("The file under the cursor is not the one shown; move the \
                                cursor to show it first."
                        .to_owned());
                }
                Ok((view.units_of_file(&node.path), node.path.clone()))
            }
            Mode::Diff | Mode::Buffer if shown.is_none() || shown != selected => Err(nothing()),
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
                let units = self.rows.hunks.get(hunk).ok_or_else(nothing)?.clone();
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
        // What the keys act on is what the diff shows, so a preview is completed first.
        self.finish_preview();
        // The selected hunk must be on screen; one scrolled away is shown first, not marked.
        if self.mode == Mode::Diff && !self.hunk_in_view() {
            self.reveal_hunk();
            self.set_status(format!(
                "Hunk {} of {} was off screen; it shows now. Press again to mark it.",
                self.hunk + 1,
                self.rows.hunks.len()
            ));
            return;
        }
        let (units, what) = match self.review_target() {
            Ok(target) => target,
            Err(why) => {
                self.set_status(why);
                return;
            }
        };
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
            self.advance_after_approval();
        }
    }

    /// After approving a hunk, selects the next one that needs the reviewer, or says the file is
    /// done and which file is next.
    fn advance_after_approval(&mut self) {
        if let Some(next) = self.open_hunk_after(Some(self.hunk)) {
            self.select_hunk(next);
            return;
        }
        if let Some(next) = self.open_hunk_after(None) {
            self.select_hunk(next);
            return;
        }
        let next = self.next_open_file();
        self.set_status(match next {
            Some(path) => format!("This file is done. n opens the next file to review: {path}"),
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
            units,
            revision: snapshot.revision,
            verdict,
            history: snapshot.history,
        });
        self.refresh_tree();
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
        self.spawn(Subject::Always(Because::Save), move || {
            let saved = job
                .history
                .view(job.revision)
                .and_then(|view| review.mark(&view, &job.units, job.verdict));
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
            for &unit in &queued.units {
                self.pending.insert(unit, queued.verdict);
            }
        }
        let mut failures = Vec::new();
        let unsaved = match saved {
            Ok(()) => false,
            Err(err) => {
                let units = plural(job.units.len(), "unit");
                // The log keeps it for when the app quits before anyone reads the status line.
                self.effect(Effect::Log(format!(
                    "could not record the mark of {units}: {err:#}"
                )));
                failures.push(format!("Could not record the mark of {units}: {err}"));
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
        self.rebuild();
    }

    /// Installs a freshly read history for the shown revision; a newer revision is announced,
    /// never switched to.
    pub(super) fn take_history(&mut self, history: History) {
        let Some(snapshot) = &mut self.snapshot else {
            return;
        };
        if history.view(snapshot.revision).is_err() {
            return;
        }
        snapshot.history = Arc::new(history);
        self.revisions = overview::summarize(&snapshot.history);
        if let Some(newer) = snapshot.newer() {
            self.set_status(format!(
                "Revision {newer} is available; press R to switch to it."
            ));
        }
    }

    /// `R`: switches to a newer revision when there is one, otherwise fetches and records the
    /// target's latest state in the background.
    pub(super) fn refresh(&mut self) {
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
        self.set_status("Fetching the latest changes…");
        self.spawn(Subject::Always(Because::Refresh), move || {
            let recorded = review.refresh();
            let history = review.history();
            Outcome::Refreshed(Refreshed { recorded, history })
        });
    }

    /// Takes in a fetch of the latest changes: the history it read, and whether it recorded a
    /// new revision.
    pub(super) fn refreshed(&mut self, refreshed: Refreshed) {
        match (refreshed.recorded, refreshed.history) {
            (Ok(recorded), Ok(history)) => {
                self.take_history(history);
                if recorded.created {
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
        self.preview = None;
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
        self.set_status(format!("Showing revision {revision}. {}", self.summary()));
    }

    /// `Ctrl+T`: trusts the review after a confirming second press, or revokes trust and stops
    /// the language servers.
    pub(super) fn toggle_trust(&mut self) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let decision = match (self.trusted, self.trust_pending) {
            (true, _) => false,
            (false, true) => true,
            (false, false) => {
                self.trust_pending = true;
                self.set_status(
                    "Trusting runs a language server in a worktree of the reviewed head, and \
                     through it the review's own build configuration, which can run any \
                     program. Press Ctrl+T again to trust.",
                );
                return;
            }
        };
        self.trust_pending = false;
        match review.set_trusted(decision) {
            Ok(()) => {
                self.trusted = decision;
                // Dropping a server shuts it down; a lookup in flight holds the servers, and
                // its completion stops them once it sees trust revoked.
                if !decision && let Some(mut servers) = self.servers.try_lock() {
                    servers.clear();
                }
                let done = if decision { "Trusted" } else { "Trust revoked" };
                self.set_status(format!("{done} · {}", self.summary()));
            }
            Err(err) => self.fail(format!("Cannot record trust: {err}")),
        }
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
}
