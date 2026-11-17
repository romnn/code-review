//! The selected file's diff: moving between hunks, folding them, following moves, and the
//! eased scrolling the diff and the buffer share.

use std::time::Instant;

use super::{Effect, GAP_STEP, Mode, Model};
use crate::config::{DiffLayout, HunkPosition};
use crate::frame::List;
use crate::rows::{Fold, RowKind};
use crate::scroll;
use crate::state::{self, Progress};

impl Model {
    /// Selects hunk `hunk` of the shown file and scrolls it into view.
    pub(super) fn select_hunk(&mut self, hunk: usize) {
        let Some(last) = self.hunk_rows.len().checked_sub(1) else {
            return;
        };
        self.hunk = hunk.min(last);
        self.selection = None;
        self.reveal_hunk();
    }

    /// Selects hunk `hunk` of a file that just came to show and puts it in view at once, where
    /// [`Self::reveal_hunk`] would ease it: nothing on screen leads from the file before to
    /// this one, so a glide from the file's remembered place to the hunk would only read as a
    /// second jump.
    pub(super) fn select_hunk_at_once(&mut self, hunk: usize) {
        let Some(last) = self.hunk_rows.len().checked_sub(1) else {
            return;
        };
        self.hunk = hunk.min(last);
        self.selection = None;
        if let Some(target) = self.hunk_target(self.diff_scroll.offset()) {
            let viewport = self.grid().viewports.diff;
            self.diff_scroll.stop();
            self.diff_scroll
                .jump(crate::frame::clamp(target, self.rows.rows.len(), viewport));
        }
    }

    /// Whether any row of the selected hunk is in the diff's viewport, where it is or is
    /// heading; before the list is laid out, it counts as shown.
    pub(super) fn hunk_in_view(&self) -> bool {
        let Some(rows) = self.hunk_rows.get(self.hunk) else {
            return false;
        };
        let viewport = self.grid().viewports.diff;
        scroll::shows(rows, self.diff_scroll.target(), viewport)
    }

    /// Scrolls the selected hunk into view where `[diff] hunk_position` puts it: its start a
    /// fifth down unless it already shows clear of the edges, so the code before it, the hunk
    /// and what follows show and a long hunk is read from its beginning; or in the middle,
    /// every time.
    pub(super) fn reveal_hunk(&mut self) {
        if let Some(target) = self.hunk_target(self.diff_scroll.target()) {
            self.ease_diff_to(target);
        }
    }

    /// Where the diff first shows a file: its selected hunk at the reading line, or in the
    /// middle, wherever it starts; `None` before the diff is laid out.
    pub(super) fn hunk_landing(&self) -> Option<f64> {
        let rows = self.hunk_rows.get(self.hunk)?;
        let viewport = self.grid().viewports.diff.filter(|&rows| rows > 0.0)?;
        let (top, bottom) = (scroll::rows(rows.start), scroll::rows(rows.end));
        let target = match self.look().hunk_position {
            HunkPosition::Margin => scroll::reading(viewport, top, scroll::READING_LINE),
            HunkPosition::Center => scroll::centered(viewport, top, bottom, scroll::READING_LINE),
        };
        Some(crate::frame::clamp(
            target,
            self.rows.rows.len(),
            Some(viewport),
        ))
    }

    /// Where the diff goes to show the selected hunk from `offset`, or `None` where it stays.
    fn hunk_target(&self, offset: f64) -> Option<f64> {
        let rows = self.hunk_rows.get(self.hunk)?;
        // A list not laid out yet has no height, and reveals as well as it can.
        let viewport = self.grid().viewports.diff.unwrap_or(0.0);
        let (top, bottom) = (scroll::rows(rows.start), scroll::rows(rows.end));
        match self.look().hunk_position {
            HunkPosition::Margin => {
                scroll::within_margins(offset, viewport, top, bottom, scroll::READING_LINE)
            }
            HunkPosition::Center => Some(scroll::centered(
                viewport,
                top,
                bottom,
                scroll::READING_LINE,
            )),
        }
    }

    /// `z z`: the selected hunk in the middle of the diff once, as vim's `zz` centers a line.
    pub(super) fn center_hunk(&mut self) {
        let Some(rows) = self.hunk_rows.get(self.hunk) else {
            return;
        };
        let viewport = self.grid().viewports.diff.unwrap_or(0.0);
        let target = scroll::centered(
            viewport,
            scroll::rows(rows.start),
            scroll::rows(rows.end),
            scroll::READING_LINE,
        );
        self.ease_diff_to(target);
    }

    /// `j` and `k` in the diff: scrolls `lines` rows, down when positive, and the selected hunk
    /// follows the view.
    pub(super) fn scroll_diff_lines(&mut self, lines: f64) {
        let target = self.diff_scroll.target() + lines;
        self.ease_diff_to(target);
        self.follow_view();
    }

    /// Selects the hunk on screen nearest the reading line a fifth down when the selected one
    /// is neither on screen nor where an eased scroll is taking the diff, so the review keys
    /// act on what shows. It looks only at what is on screen now, never where a scroll is
    /// heading, so no key can act on a hunk before it shows. With no hunk on screen, the
    /// selection stays, and a mark shows it first.
    ///
    /// Each frame's layout runs it, so every way the diff scrolls is covered: keys, the wheel,
    /// the scrollbar's thumb and track, and eased scrolls as they move. Only while the diff has
    /// the keys: a diff previewed from the tree keeps the hunk it opens at, which opening it
    /// reveals.
    pub(super) fn follow_view(&mut self) {
        if self.mode != Mode::Diff {
            return;
        }
        let Some(viewport) = self.grid().viewports.diff.filter(|&rows| rows > 0.0) else {
            return;
        };
        let top = self.diff_scroll.offset();
        let shown = |rows: &std::ops::Range<usize>, offset: f64| {
            scroll::shows(rows, offset, Some(viewport))
        };
        if self
            .hunk_rows
            .get(self.hunk)
            .is_none_or(|rows| shown(rows, top) || shown(rows, self.diff_scroll.target()))
        {
            return;
        }
        let reading = top + viewport * scroll::READING_LINE;
        let distance = |rows: &std::ops::Range<usize>| {
            let (start, end) = (scroll::rows(rows.start), scroll::rows(rows.end));
            if reading < start {
                start - reading
            } else if reading >= end {
                reading - end
            } else {
                0.0
            }
        };
        if let Some((hunk, _)) = self
            .hunk_rows
            .iter()
            .enumerate()
            .filter(|(_, rows)| shown(rows, top))
            .min_by(|(_, first), (_, second)| distance(first).total_cmp(&distance(second)))
        {
            self.hunk = hunk;
        }
    }

    /// `j` and `k`: the next or previous hunk, reviewed or not; past the file's last or first
    /// hunk, with `[diff] cross_files`, the next or previous file in the tree's visible order, at
    /// its first or last hunk. In scroll mode they scroll the diff a line instead.
    pub(super) fn step_hunk(&mut self, forward: bool) {
        if self.scroll_mode {
            self.scroll_diff_lines(if forward { 1.0 } else { -1.0 });
            return;
        }
        let count = self.hunk_rows.len();
        if forward && self.hunk + 1 < count {
            self.select_hunk(self.hunk + 1);
            return;
        }
        if !forward && self.hunk > 0 && count > 0 {
            self.select_hunk(self.hunk - 1);
            return;
        }
        if !self.look().cross_files {
            self.set_status(if forward {
                "This is the file's last hunk."
            } else {
                "This is the file's first hunk."
            });
            return;
        }
        let Some(path) = self
            .adjacent_file(forward)
            .and_then(|position| self.visible_tree().get(position).copied())
            .and_then(|index| self.tree.get(index))
            .map(|node| node.path.clone())
        else {
            self.set_status(if forward {
                "This is the last hunk of the last file."
            } else {
                "This is the first hunk of the first file."
            });
            return;
        };
        self.show_file(&path);
        self.select_hunk_at_once(if forward { 0 } else { usize::MAX });
        self.set_status(format!("On to {path}."));
    }

    /// `]` and `[`: the next or previous hunk that needs the reviewer; past the file's last or
    /// first one, with `[diff] cross_files`, the next or previous file that needs the reviewer,
    /// at its first or last such hunk, around the end of the tree.
    pub(super) fn step_open_hunk(&mut self, forward: bool) {
        let here = if forward {
            self.open_hunk_after(Some(self.hunk))
        } else {
            self.open_hunk_before(self.hunk)
        };
        if let Some(hunk) = here {
            self.select_hunk(hunk);
            return;
        }
        if !self.look().cross_files {
            let others =
                (0..self.rows.hunks.len()).any(|hunk| hunk != self.hunk && self.hunk_open(hunk));
            let status = match (others, self.hunk_open(self.hunk), self.next_open_file()) {
                (true, _, _) if forward => {
                    "No hunk further down needs review; some further up still do.".to_owned()
                }
                (true, _, _) => {
                    "No hunk further up needs review; some further down still do.".to_owned()
                }
                (false, true, Some(path)) => {
                    format!("This is the file's last hunk to review; {path} is next.")
                }
                (false, true, None) => "This is the last hunk left to review.".to_owned(),
                (false, false, Some(path)) => {
                    format!("This file is done; {path} is next to review.")
                }
                (false, false, None) => "Nothing left to review.".to_owned(),
            };
            self.set_status(status);
            return;
        }
        let Some(path) = self.open_file_toward(forward) else {
            self.set_status(if self.open_hunk_after(None).is_some() {
                if forward {
                    "No other file needs review; this one still has hunks to review further up."
                } else {
                    "No other file needs review; this one still has hunks to review further down."
                }
            } else {
                "Nothing left to review."
            });
            return;
        };
        self.show_file(&path);
        let hunk = if forward {
            self.open_hunk_after(None)
        } else {
            self.open_hunk_before(usize::MAX)
        };
        self.select_hunk_at_once(hunk.unwrap_or(0));
        self.set_status(format!("On to {path}, which needs review."));
    }

    /// `s`, or `a` as lazygit switches between hunks and lines: scroll mode on or off, where `j`
    /// and `k` scroll the diff a line instead of moving between hunks.
    pub(super) fn toggle_scroll_mode(&mut self) {
        self.scroll_mode = !self.scroll_mode;
        self.scrolled_from = self
            .scroll_mode
            .then(|| self.hunk_units().first().copied())
            .flatten();
        self.set_status(if self.scroll_mode {
            "Scroll mode: j and k scroll a line; s stays here, Esc goes back to the hunk."
        } else {
            "j and k move between hunks again."
        });
    }

    /// Esc in scroll mode: leaves it and goes back to the hunk selected when it began, or the
    /// selected one when that hunk is gone; the next Esc goes to the files.
    pub(super) fn leave_scroll_mode(&mut self) {
        self.scroll_mode = false;
        let from = self
            .scrolled_from
            .take()
            .and_then(|unit| self.hunk_of(unit));
        self.select_hunk(from.unwrap_or(self.hunk));
        self.set_status("Back at the hunk; j and k move between hunks again.");
    }

    /// Ctrl+D and Ctrl+U: scroll half a page in the diff or the buffer.
    pub(super) fn half_page(&mut self, forward: bool) {
        let viewports = self.grid().viewports;
        if self.mode == Mode::Buffer {
            let rows = scroll::whole(scroll::narrow(viewports.buffer.unwrap_or(0.0)) / 2.0).max(1);
            self.move_buffer_cursor_by(if forward {
                rows.cast_signed()
            } else {
                -rows.cast_signed()
            });
            return;
        }
        let half = viewports.diff.unwrap_or(0.0) / 2.0;
        let target = self.diff_scroll.target() + if forward { half } else { -half };
        self.ease_diff_to(target);
        self.follow_view();
    }

    /// `za`: folds or unfolds the selected hunk. An approved hunk is folded by default, so
    /// unfolding it records that the reviewer opened it.
    pub(super) fn toggle_fold(&mut self) {
        let units = self.hunk_units();
        let Some(view) = self.view() else {
            return;
        };
        let folded = units.iter().all(|&unit| self.folds.is_folded(&view, unit));
        for unit in units {
            if folded {
                self.folds.units.remove(&unit);
                self.folds.opened.insert(unit);
            } else {
                self.folds.opened.remove(&unit);
                self.folds.units.insert(unit);
            }
        }
        self.rebuild();
        self.reveal_hunk();
    }

    /// Enter in the diff: follows the selected hunk's move to its other end, expands a
    /// collapsed unit to show its code, or else folds or unfolds the hunk, so an approved
    /// hunk's code shows without `z a`.
    pub(super) fn diff_activate(&mut self) {
        let Some(rows) = self.hunk_rows.get(self.hunk).cloned() else {
            return;
        };
        let link = self
            .rows
            .rows
            .get(rows.clone())
            .unwrap_or_default()
            .iter()
            .find_map(|row| row.note()?.link);
        if let Some(target) = link {
            self.follow(target);
            return;
        }
        let expand = self
            .rows
            .rows
            .get(rows)
            .unwrap_or_default()
            .iter()
            .find_map(|row| match row.note()?.fold {
                Some(Fold::Expand(unit)) => Some(unit),
                _ => None,
            });
        if let Some(unit) = expand {
            if !self.folds.expanded.remove(&unit) {
                self.folds.expanded.insert(unit);
            }
            self.rebuild();
        } else {
            self.toggle_fold();
        }
    }

    /// Shows the file and hunk holding `unit`, the other end of a move.
    pub(super) fn follow(&mut self, unit: review_core::UnitId) {
        let Some(path) = self.view().and_then(|view| {
            view.file(unit)
                .ok()
                .map(|file| file.display_path().to_owned())
        }) else {
            return;
        };
        self.show_file(&path);
        if let Some(hunk) = self
            .rows
            .hunks
            .iter()
            .position(|units| units.contains(&unit))
        {
            self.select_hunk(hunk);
        }
        self.set_status(format!("Followed the move to {path}."));
    }

    /// Expands the gap at `row` by a step of unchanged lines, as a click on it does.
    pub fn expand_gap(&mut self, row: usize) {
        let Some(Fold::Gap { file, at }) = self.rows.rows.get(row).and_then(|row| row.note()?.fold)
        else {
            return;
        };
        let Some(path) = self.rows.files.get(file).map(|file| file.path.clone()) else {
            return;
        };
        *self.folds.gaps.entry((path, at)).or_default() += GAP_STEP;
        self.rebuild();
    }

    /// `n` and `N`: the next (or previous) file in the visible tree, or the next (or previous)
    /// match while a `/` search is in the diff.
    ///
    /// In the tree the cursor moves there and previews it, as `j` and `k` do; in the diff the
    /// file shows at its first hunk that needs the reviewer. At either end of the tree it
    /// stays, as the tree's cursor does, and says so.
    pub(super) fn step_file(&mut self, forward: bool) {
        if self.mode == Mode::Diff && self.diff_search_active() {
            self.find_step(forward, false);
            return;
        }
        let Some(position) = self.adjacent_file(forward) else {
            self.set_status(if forward {
                "This is the last file."
            } else {
                "This is the first file."
            });
            return;
        };
        if self.mode == Mode::Tree {
            self.tree_move_to(position);
            return;
        }
        let Some(path) = self
            .visible_tree()
            .get(position)
            .and_then(|&index| self.tree.get(index))
            .map(|node| node.path.clone())
        else {
            return;
        };
        self.show_file(&path);
        let hunk = self.open_hunk_after(None).unwrap_or(self.hunk);
        self.select_hunk_at_once(hunk);
    }

    /// Selects the first hunk that still needs the reviewer, or keeps the selection.
    pub(super) fn select_first_open_hunk(&mut self) {
        if let Some(hunk) = self.open_hunk_after(None) {
            self.hunk = hunk;
        }
    }

    /// The first hunk after `after` (or from the start) that is not approved.
    pub(super) fn open_hunk_after(&self, after: Option<usize>) -> Option<usize> {
        let start = after.map_or(0, |hunk| hunk + 1);
        (start..self.rows.hunks.len()).find(|&hunk| self.hunk_open(hunk))
    }

    /// The last hunk before `before` that is not approved.
    pub(super) fn open_hunk_before(&self, before: usize) -> Option<usize> {
        (0..before.min(self.rows.hunks.len()))
            .rev()
            .find(|&hunk| self.hunk_open(hunk))
    }

    /// Whether hunk `hunk` of the shown file still needs the reviewer: some unit of it is not
    /// approved, counting the marks still saving.
    fn hunk_open(&self, hunk: usize) -> bool {
        let Some(view) = self.view() else {
            return false;
        };
        let units = self.rows.hunks.get(hunk).map_or(&[][..], Vec::as_slice);
        !state::badges(&view, units, &self.pending)
            .into_iter()
            .all(state::is_approved)
    }

    /// Whether the shown file still needs the reviewer.
    #[must_use]
    pub fn file_open(&self) -> bool {
        self.selected.as_ref().is_some_and(|path| {
            self.tree
                .iter()
                .find(|node| &node.path == path)
                .is_some_and(|node| Progress::of(&node.counts).needs_attention())
        })
    }

    /// `Y`: copies the selected hunk's first unit ID, which the CLI and notes use.
    pub(super) fn yank_unit_id(&mut self) {
        if let Some(unit) = self.hunk_units().first().copied() {
            self.effect(Effect::Copy(unit.to_string()));
            self.set_status(format!("Copied {unit}."));
        }
    }

    /// Starts or retargets the diff's eased scroll to `target` rows down, or jumps there in a
    /// front end that does not ease its scrolls.
    pub fn ease_diff_to(&mut self, target: f64) {
        let rows = self.list_rows(List::Diff);
        let viewport = self.grid().viewports.diff;
        if self.settings.front_end.capabilities().smooth_scroll {
            self.diff_scroll
                .ease_to(target, rows, viewport, Instant::now());
        } else {
            self.diff_scroll.scroll_to(target, rows, viewport);
        }
    }

    /// Starts or retargets the buffer's eased scroll to `target` rows down, or jumps there in a
    /// front end that does not ease its scrolls.
    pub(super) fn ease_buffer_to(&mut self, target: f64) {
        let rows = self.list_rows(List::Buffer);
        let viewport = self.grid().viewports.buffer;
        if self.settings.front_end.capabilities().smooth_scroll {
            self.buffer_scroll
                .ease_to(target, rows, viewport, Instant::now());
        } else {
            self.buffer_scroll.scroll_to(target, rows, viewport);
        }
    }

    /// The row kinds a click can expand: gaps between hunks.
    #[must_use]
    pub(crate) fn is_gap(&self, row: usize) -> bool {
        self.rows
            .rows
            .get(row)
            .is_some_and(|row| row.kind() == RowKind::Gap)
    }

    /// Selects the hunk at or before the next row of the diff that contains `text`, wrapping
    /// around, as QA mode's `find` does; the row of a gap or note picks the hunk before it.
    ///
    /// # Errors
    ///
    /// Says so when no row contains `text`.
    pub fn select_hunk_containing(&mut self, text: &str) -> Result<(), String> {
        let count = self.rows.rows.len();
        let from = self.hunk_start().unwrap_or(0);
        let found = (1..=count)
            .map(|step| (from + step) % count.max(1))
            .find(|&index| {
                self.rows.rows.get(index).is_some_and(|row| {
                    row.cells().any(|cell| cell.text.contains(text))
                        || row.note().is_some_and(|note| {
                            note.text.contains(text)
                                || note.tag.as_deref().is_some_and(|tag| tag.contains(text))
                        })
                })
            })
            .ok_or_else(|| format!("no row contains {text:?}"))?;
        let hunk = self
            .rows
            .rows
            .get(..=found)
            .unwrap_or_default()
            .iter()
            .rev()
            .find_map(|row| row.hunk)
            .unwrap_or(0);
        self.select_hunk(hunk);
        Ok(())
    }

    /// Whether the diff shows side by side now, rather than only being asked to.
    #[must_use]
    pub fn side_by_side(&self) -> bool {
        self.rows.presentation.layout == DiffLayout::SideBySide
    }
}
