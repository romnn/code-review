//! Approving, flagging and resetting, the queue of marks waiting for the engine, and moving to
//! a new revision.
//!
//! A mark resolves its units when the reviewer acts and keeps the revision they were shown in;
//! the engine refuses it if that revision is no longer current. Marks are saved one at a time,
//! and every completion, success or failure, derives the view again from the current state and
//! starts the next mark, so none is ever left waiting.

use std::sync::Arc;

use gpui::{Context, Window};
use review_core::{History, MarkVerdict, UnitId};

use super::state::{self, MarkJob};
use super::{
    Mode, Refresh, ResetReview, ReviewApp, ToggleApprove, ToggleFlag, ToggleTrust, overview, plural,
};
use crate::rows;

impl ReviewApp {
    /// The units the current mode's review keys act on, and what to call them.
    fn review_target(&self) -> Option<(Vec<UnitId>, String)> {
        let view = self.view()?;
        match self.mode {
            Mode::Tree => {
                let index = self.tree_index()?;
                let node = self.tree.get(index)?;
                // A folder acts on the files it shows, a file on exactly itself.
                let units = if node.kind == review_core::tree::NodeKind::Directory {
                    state::folder_files(&self.tree, index)
                        .into_iter()
                        .flat_map(|path| view.units_of_file(path))
                        .collect()
                } else {
                    view.units_of_file(&node.path)
                };
                Some((units, node.path.clone()))
            }
            Mode::Diff => {
                let units = self.hunk_units();
                Some((
                    units,
                    format!("hunk {} of {}", self.hunk + 1, self.rows.hunks.len()),
                ))
            }
            Mode::Buffer => {
                let hunk = self.buffer_hunk()?;
                let units = self.rows.hunks.get(hunk)?.clone();
                Some((
                    units,
                    format!("hunk {} of {}", hunk + 1, self.rows.hunks.len()),
                ))
            }
        }
    }

    /// Toggles the current target toward `toward`: everything at `toward` goes back to
    /// unreviewed, anything else takes `toward`.
    fn review(&mut self, toward: MarkVerdict, cx: &mut Context<Self>) {
        let Some((units, what)) = self.review_target() else {
            self.set_status("Nothing here to review.");
            cx.notify();
            return;
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
        if !self.queue_mark(units, verdict, &what, cx) {
            return;
        }
        if self.mode == Mode::Diff && verdict == MarkVerdict::Accepted {
            self.advance_after_approval(cx);
        }
        cx.notify();
    }

    /// After approving a hunk, selects the next one that needs the reviewer, or says the file is
    /// done and which file is next.
    fn advance_after_approval(&mut self, cx: &mut Context<Self>) {
        if let Some(next) = self.open_hunk_after(Some(self.hunk)) {
            self.select_hunk(next, cx);
            return;
        }
        if let Some(next) = self.open_hunk_after(None) {
            self.select_hunk(next, cx);
            return;
        }
        let next = self.next_open_file();
        self.set_status(match next {
            Some(path) => format!("This file is done. n opens the next file to review: {path}"),
            None => "This file is done, and so is every other file.".to_owned(),
        });
    }

    pub(super) fn toggle_approve(
        &mut self,
        _: &ToggleApprove,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.review(MarkVerdict::Accepted, cx);
    }

    pub(super) fn toggle_flag(&mut self, _: &ToggleFlag, _: &mut Window, cx: &mut Context<Self>) {
        self.review(MarkVerdict::Flagged, cx);
    }

    pub(super) fn reset_review(&mut self, _: &ResetReview, _: &mut Window, cx: &mut Context<Self>) {
        self.review(MarkVerdict::Unseen, cx);
    }

    /// Queues `verdict` for exactly `units` of the shown revision and shows it at once; returns
    /// whether it was queued. Marking waits while a newer revision is available, because the
    /// engine records marks only against the current one.
    pub(super) fn queue_mark(
        &mut self,
        units: Vec<UnitId>,
        verdict: MarkVerdict,
        what: &str,
        cx: &mut Context<Self>,
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
        self.next_mark(cx);
        true
    }

    /// Saves the next queued mark in the background, unless one is being saved.
    pub(super) fn next_mark(&mut self, cx: &mut Context<Self>) {
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
        let work = cx.background_executor().spawn(async move {
            let saved = job
                .history
                .view(job.revision)
                .and_then(|view| review.mark(&view, &job.units, job.verdict));
            let history = review.history();
            (job, saved, history)
        });
        cx.spawn(async move |this, cx| {
            let (job, saved, history) = work.await;
            this.update(cx, |this, cx| {
                this.marking = false;
                this.saved(&job, saved, history);
                this.next_mark(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        if let Err(err) = saved {
            self.fail(format!(
                "Could not record the mark of {}: {err}",
                plural(job.units.len(), "unit")
            ));
        }
        match history {
            Ok(history) => self.take_history(history),
            Err(err) => self.fail(format!("Could not reread the review: {err}")),
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
    pub(super) fn refresh(&mut self, _: &Refresh, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(newer) = self.snapshot.as_ref().and_then(state::Snapshot::newer) {
            self.switch_revision(newer, window, cx);
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
        cx.notify();
        let work = cx.background_executor().spawn(async move {
            let recorded = review.refresh();
            let history = review.history();
            (recorded, history)
        });
        cx.spawn(async move |this, cx| {
            let (recorded, history) = work.await;
            this.update(cx, |this, cx| {
                this.refreshing = false;
                match (recorded, history) {
                    (Ok(recorded), Ok(history)) => {
                        this.take_history(history);
                        if recorded.created {
                            this.set_status(format!(
                                "Revision {} recorded; press R to switch to it.",
                                recorded.number
                            ));
                        } else if this
                            .snapshot
                            .as_ref()
                            .and_then(state::Snapshot::newer)
                            .is_none()
                        {
                            this.set_status("Up to date; no new revision.");
                        }
                    }
                    (Err(err), _) | (_, Err(err)) => this.fail(format!("Could not refresh: {err}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Shows `revision` instead of the current one, once no mark is waiting for the old one.
    ///
    /// One transition: everything derived from the old revision (rows, hunks, folds, places,
    /// the buffer and its search) goes first, then the selection is chosen again in the new
    /// revision, and a mode whose subject is gone falls back to one that has it.
    fn switch_revision(
        &mut self,
        revision: review_core::RevisionNumber,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marking || !self.marks.is_empty() {
            self.set_status("Saving marks; press R again in a moment.");
            cx.notify();
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
        self.rows = rows::Layout::default();
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
        self.enter_mode(mode, window, cx);
        self.request_syntax(cx);
        self.set_status(format!("Showing revision {revision}. {}", self.summary()));
        cx.notify();
    }

    /// `Ctrl+T`: trusts the review after a confirming second press, or revokes trust and stops
    /// the language servers.
    pub(super) fn toggle_trust(&mut self, _: &ToggleTrust, _: &mut Window, cx: &mut Context<Self>) {
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
                cx.notify();
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
        cx.notify();
    }
}
