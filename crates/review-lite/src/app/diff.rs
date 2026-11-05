//! The selected file's diff: moving between hunks, folding them, following moves, and the
//! eased scrolling the diff and the buffer share.

use std::time::Instant;

use gpui::{ClipboardItem, Context, Window};
use gpui_component::scroll::ScrollbarHandle as _;

use super::state::{self, Progress};
use super::{
    Activate, FirstHunk, GAP_STEP, HalfPageDown, HalfPageUp, LastHunk, Mode, NextFile, NextHunk,
    PreviousHunk, ReviewApp, ToggleFold, YankUnitId, render, scroll,
};
use crate::rows::{Fold, RowKind};
use scroll::Animation;

impl ReviewApp {
    /// Selects hunk `hunk` of the shown file and scrolls it into view.
    pub(super) fn select_hunk(&mut self, hunk: usize, cx: &mut Context<Self>) {
        let Some(last) = self.hunk_rows.len().checked_sub(1) else {
            return;
        };
        self.hunk = hunk.min(last);
        self.selection = None;
        self.reveal_hunk();
        cx.notify();
    }

    /// Scrolls the selected hunk into view, as much of it as fits, starting at its header.
    pub(super) fn reveal_hunk(&mut self) {
        let Some(rows) = self.hunk_rows.get(self.hunk).cloned() else {
            return;
        };
        let height = self.fonts.row_height();
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let fits = scroll::whole(viewport / height).saturating_sub(4).max(1);
        // One row of air above the header, and as much of the hunk as fits.
        let top = rows.start.saturating_sub(1);
        let end = rows.end.min(top + fits);
        let current = self.list_target();
        if let Some(target) = scroll::reveal(
            current,
            viewport,
            scroll::float(top) * height,
            scroll::float(end) * height,
            height * 2.0,
        ) {
            self.scroll_list_to(target);
        }
    }

    pub(super) fn next_hunk(&mut self, _: &NextHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.select_hunk(self.hunk + 1, cx);
    }

    pub(super) fn previous_hunk(
        &mut self,
        _: &PreviousHunk,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_hunk(self.hunk.saturating_sub(1), cx);
    }

    pub(super) fn first_hunk(&mut self, _: &FirstHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.select_hunk(0, cx);
    }

    pub(super) fn last_hunk(&mut self, _: &LastHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.select_hunk(usize::MAX, cx);
    }

    /// Ctrl+D and Ctrl+U: scroll half a page in the diff or the buffer.
    fn half_page(&mut self, forward: bool, cx: &mut Context<Self>) {
        let height = self.fonts.row_height();
        if self.mode == Mode::Buffer {
            let rows = scroll::whole(
                f32::from(self.buffer_list.viewport_bounds().size.height) / height / 2.0,
            )
            .max(1);
            self.move_buffer_cursor_by(
                if forward {
                    rows.cast_signed()
                } else {
                    -rows.cast_signed()
                },
                cx,
            );
            return;
        }
        let half = f32::from(self.list.viewport_bounds().size.height) / 2.0;
        let target = self.list_target() + if forward { half } else { -half };
        self.scroll_list_to(target);
        cx.notify();
    }

    pub(super) fn half_page_down(
        &mut self,
        _: &HalfPageDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.half_page(true, cx);
    }

    pub(super) fn half_page_up(&mut self, _: &HalfPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.half_page(false, cx);
    }

    /// `za`: folds or unfolds the selected hunk. An approved hunk is folded by default, so
    /// unfolding it records that the reviewer opened it.
    pub(super) fn toggle_fold(&mut self, _: &ToggleFold, _: &mut Window, cx: &mut Context<Self>) {
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
        cx.notify();
    }

    /// Enter in the diff: follows the selected hunk's move to its other end, or expands a
    /// collapsed unit to show its code.
    pub(super) fn diff_activate(&mut self, _: &Activate, _: &mut Window, cx: &mut Context<Self>) {
        let Some(rows) = self.hunk_rows.get(self.hunk).cloned() else {
            return;
        };
        let link = self
            .rows
            .rows
            .get(rows.clone())
            .unwrap_or_default()
            .iter()
            .find_map(|row| row.link);
        if let Some(target) = link {
            self.follow(target, cx);
            return;
        }
        let expand = self
            .rows
            .rows
            .get(rows)
            .unwrap_or_default()
            .iter()
            .find_map(|row| match row.fold {
                Some(Fold::Expand(unit)) => Some(unit),
                _ => None,
            });
        if let Some(unit) = expand {
            if !self.folds.expanded.remove(&unit) {
                self.folds.expanded.insert(unit);
            }
            self.rebuild();
            cx.notify();
        }
    }

    /// Shows the file and hunk holding `unit`, the other end of a move.
    pub(super) fn follow(&mut self, unit: review_core::UnitId, cx: &mut Context<Self>) {
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
            self.select_hunk(hunk, cx);
        }
        self.set_status(format!("Followed the move to {path}."));
        cx.notify();
    }

    /// Expands the gap at `row` by a step of unchanged lines.
    pub(super) fn expand_gap(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(Fold::Gap { file, at }) = self.rows.rows.get(row).and_then(|row| row.fold) else {
            return;
        };
        let Some(path) = self.rows.files.get(file).map(|file| file.path.clone()) else {
            return;
        };
        *self.folds.gaps.entry((path, at)).or_default() += GAP_STEP;
        self.rebuild();
        cx.notify();
    }

    /// `n` in the diff: the next file that still needs the reviewer.
    pub(super) fn next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        match self.next_open_file() {
            Some(path) => {
                self.show_file(&path);
                self.select_first_open_hunk();
                self.reveal_hunk();
                self.set_status(format!("Reviewing {path}."));
            }
            None => self.set_status("Every file is reviewed."),
        }
        cx.notify();
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
    pub(super) fn file_open(&self) -> bool {
        self.selected.as_ref().is_some_and(|path| {
            self.tree
                .iter()
                .find(|node| &node.path == path)
                .is_some_and(|node| Progress::of(&node.counts).needs_attention())
        })
    }

    /// `y i`: copies the selected hunk's first unit ID, which the CLI and notes use.
    pub(super) fn yank_unit_id(&mut self, _: &YankUnitId, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(unit) = self.hunk_units().first() {
            cx.write_to_clipboard(ClipboardItem::new_string(unit.to_string()));
            self.set_status(format!("Copied {unit}."));
            cx.notify();
        }
    }

    /// Where the diff is, or is heading.
    pub(super) fn list_target(&self) -> f32 {
        self.list_animation.map_or_else(
            || scroll::offset_of(&self.list),
            |animation| animation.target(),
        )
    }

    /// Starts or retargets the diff's scroll animation. The bound comes from the row count, so
    /// it holds right after a rebuild, before the new rows are laid out.
    pub(super) fn scroll_list_to(&mut self, target: f32) {
        let content = scroll::float(self.rows.rows.len()) * self.fonts.row_height();
        let max = (content - f32::from(self.list.viewport_bounds().size.height)).max(0.0);
        let target = target.clamp(0.0, max);
        let now = Instant::now();
        match &mut self.list_animation {
            Some(animation) => animation.retarget(target, now),
            None => {
                self.list_animation =
                    Some(Animation::new(scroll::offset_of(&self.list), target, now));
            }
        }
    }

    /// Starts or retargets the buffer's scroll animation.
    pub(super) fn scroll_buffer_to(&mut self, target: f32) {
        let lines = self
            .buffer
            .as_ref()
            .map_or(0, |buffer| buffer.lines.lines.len());
        let content = scroll::float(lines) * self.fonts.row_height();
        let max = (content - f32::from(self.buffer_list.viewport_bounds().size.height)).max(0.0);
        let target = target.clamp(0.0, max);
        let now = Instant::now();
        match &mut self.buffer_animation {
            Some(animation) => animation.retarget(target, now),
            None => {
                self.buffer_animation = Some(Animation::new(
                    scroll::offset_of(&self.buffer_list),
                    target,
                    now,
                ));
            }
        }
    }

    /// Advances the scroll animations and track-click pages; returns whether any is running.
    pub(super) fn step_animations(&mut self) -> bool {
        if let Some(target) = self.list_bar.take_page_request() {
            self.scroll_list_to(target);
        }
        if let Some(target) = self.buffer_bar.take_page_request() {
            self.scroll_buffer_to(target);
        }
        let now = Instant::now();
        let mut running = false;
        for (animation, list) in [
            (&mut self.list_animation, &self.list),
            (&mut self.buffer_animation, &self.buffer_list),
        ] {
            if let Some(current) = animation {
                scroll::set_offset_of(list, current.value_at(now));
                if current.done(now) {
                    *animation = None;
                } else {
                    running = true;
                }
            }
        }
        running
            || self
                .failed_at
                .is_some_and(|at| now.saturating_duration_since(at) < render::FLASH)
    }

    /// The row kinds a click can expand: gaps between hunks.
    pub(super) fn is_gap(&self, row: usize) -> bool {
        self.rows
            .rows
            .get(row)
            .is_some_and(|row| row.kind == RowKind::Gap)
    }
}
