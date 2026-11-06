//! The selected file's diff: moving between hunks, folding them, following moves, and the
//! eased scrolling the diff and the buffer share.

use std::time::Instant;

use super::{Effect, GAP_STEP, Mode, Model};
use crate::config::DiffLayout;
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

    /// Whether any row of the selected hunk is in the diff's viewport, where it is or is
    /// heading; before the list is laid out, it counts as shown.
    pub(super) fn hunk_in_view(&self) -> bool {
        let Some(rows) = self.hunk_rows.get(self.hunk) else {
            return false;
        };
        let viewport = self.grid().viewports.diff;
        scroll::shows(rows, self.diff_scroll.target(), viewport)
    }

    /// Scrolls the selected hunk into view, as much of it as fits, starting at its header.
    pub(super) fn reveal_hunk(&mut self) {
        let Some(rows) = self.hunk_rows.get(self.hunk).cloned() else {
            return;
        };
        // A list not laid out yet has no height, and reveals as well as it can.
        let viewport = self.grid().viewports.diff.unwrap_or(0.0);
        let fits = scroll::whole_rows(viewport).saturating_sub(4).max(1);
        // One row of air above the header, and as much of the hunk as fits.
        let top = rows.start.saturating_sub(1);
        let end = rows.end.min(top + fits);
        if let Some(target) = scroll::reveal(
            self.diff_scroll.target(),
            viewport,
            scroll::rows(top),
            scroll::rows(end),
            2.0,
        ) {
            self.ease_diff_to(target);
        }
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

    /// Enter in the diff: follows the selected hunk's move to its other end, or expands a
    /// collapsed unit to show its code.
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

    /// `n`: the next match while a `/` search is in the diff, else the next file to review.
    pub(super) fn next_file(&mut self) {
        if self.diff_search_active() {
            self.find_step(true, false);
            return;
        }
        match self.next_open_file() {
            Some(path) => {
                self.show_file(&path);
                self.select_first_open_hunk();
                self.reveal_hunk();
                self.set_status(format!("Reviewing {path}."));
            }
            None => self.set_status("Every file is reviewed."),
        }
    }

    /// Selects the first hunk that still needs the reviewer, or keeps the selection.
    pub(super) fn select_first_open_hunk(&mut self) {
        if let Some(hunk) = self.open_hunk_after(None) {
            self.hunk = hunk;
        }
    }

    /// The first hunk after `after` (or from the start) that is not approved.
    pub(super) fn open_hunk_after(&self, after: Option<usize>) -> Option<usize> {
        let view = self.view()?;
        let start = after.map_or(0, |hunk| hunk + 1);
        (start..self.rows.hunks.len()).find(|&hunk| {
            let units = self.rows.hunks.get(hunk).cloned().unwrap_or_default();
            !state::badges(&view, &units, &self.pending)
                .into_iter()
                .all(state::is_approved)
        })
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

    /// `y i`: copies the selected hunk's first unit ID, which the CLI and notes use.
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
