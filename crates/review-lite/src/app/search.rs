//! `/` in every mode: in the tree it filters the files, in the diff it finds text in the diff's
//! code without leaving it, and in the buffer it finds text in the file. `n` and `N` step
//! through the matches.

use std::sync::Arc;

use gpui::{AppContext as _, Context, Entity, Window};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::scroll::ScrollbarHandle as _;
use review_core::tree::NodeKind;

use super::find::Spot;
use super::text::Column;
use super::{
    ClearFilter, CloseFind, Filter, Find, FindNext, FindPrevious, Mode, OpenFind, ReviewApp, find,
    scroll,
};
use crate::rows::{Body, Row};

/// The code of each line in `row`, with the column it shows in: one across the width, both
/// halves of a side-by-side row, or none for a note.
fn row_code(row: &Row) -> impl Iterator<Item = (Column, &str)> {
    let halves = match &row.body {
        Body::Code(cell) => [(Column::Whole, Some(cell)), (Column::Whole, None)],
        Body::Pair(pair) => [
            (Column::Left, pair.left.as_ref()),
            (Column::Right, pair.right.as_ref()),
        ],
        Body::Note(_) => [(Column::Whole, None), (Column::Whole, None)],
    };
    halves
        .into_iter()
        .filter_map(|(column, cell)| Some((column, cell?.code())))
}

impl ReviewApp {
    /// `/`: filters the tree, or finds in the diff or the buffer, by the mode.
    pub(super) fn open_find(&mut self, _: &OpenFind, window: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Tree => self.open_filter(window, cx),
            mode => self.open_search(mode, window, cx),
        }
    }

    /// Opens the tree's filter bar, keeping the filter in effect.
    fn open_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self
            .filter
            .as_ref()
            .map(|filter| filter.query.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter files by path")
                .default_value(query.clone())
        });
        let subscription = cx.subscribe_in(&input, window, Self::filter_event);
        input.update(cx, |input, cx| input.focus(window, cx));
        self.filter = Some(Filter {
            input,
            query,
            editing: true,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn filter_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value().to_string();
                if let Some(filter) = &mut self.filter {
                    filter.query = query;
                }
                self.apply_filter();
                cx.notify();
            }
            InputEvent::PressEnter { .. } => {
                // The filter stays; the keyboard goes back to the tree.
                if self
                    .filter
                    .as_ref()
                    .is_some_and(|filter| filter.query.is_empty())
                {
                    self.filter = None;
                } else if let Some(filter) = &mut self.filter {
                    filter.editing = false;
                }
                self.focus_mode(window, cx);
                cx.notify();
            }
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    /// The tree's filter query, when one narrows it.
    pub(super) fn filter_query(&self) -> Option<&str> {
        self.filter
            .as_ref()
            .map(|filter| filter.query.as_str())
            .filter(|query| !query.is_empty())
    }

    /// Narrows the tree to the filter, keeping the cursor on its node when it still shows and
    /// otherwise moving it to the first file that matches, whose diff then shows.
    fn apply_filter(&mut self) {
        let cursor = self.cursor_identity();
        self.update_visible_tree();
        let visible = self.visible_tree();
        let still_shown = cursor.as_ref().is_some_and(|(path, kind)| {
            visible.iter().any(|&index| {
                self.tree
                    .get(index)
                    .is_some_and(|node| node.path == *path && node.kind == *kind)
            })
        });
        if still_shown {
            self.restore_cursor(cursor);
            return;
        }
        let first = visible.iter().find_map(|&index| {
            self.tree
                .get(index)
                .filter(|node| node.kind == NodeKind::File)
                .map(|node| node.path.clone())
        });
        match first {
            Some(path) => self.show_file(&path),
            None => self.tree_cursor = 0,
        }
    }

    /// Esc in the tree: clears the filter, keeping the cursor on its node.
    pub(super) fn clear_filter(&mut self, _: &ClearFilter, _: &mut Window, cx: &mut Context<Self>) {
        if self.filter.take().is_none() {
            return;
        }
        let cursor = self.cursor_identity();
        self.update_visible_tree();
        self.restore_cursor(cursor);
        self.reveal_tree_cursor();
        cx.notify();
    }

    /// Opens the find bar over the diff or the buffer.
    fn open_search(&mut self, target: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = if target == Mode::Diff {
            "Find in this diff"
        } else {
            "Find in this file"
        };
        let origin = match target {
            Mode::Buffer => self.buffer.as_ref().map_or(Spot::whole(0, 0), |buffer| {
                Spot::whole(buffer.cursor.row, buffer.cursor.col)
            }),
            _ => Spot::whole(self.hunk_start().unwrap_or(0), 0),
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe_in(&input, window, Self::find_event);
        input.update(cx, |input, cx| input.focus(window, cx));
        self.find = Some(Find {
            target,
            input,
            query: String::new(),
            matches: Arc::new(Vec::new()),
            current: None,
            origin,
            hunk: None,
            editing: true,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn find_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value().to_string();
                let Some(target) = self.find.as_ref().map(|find| find.target) else {
                    return;
                };
                if let Some(find) = &mut self.find {
                    find.query = query;
                }
                self.refresh_find(target);
                self.find_step(true, true, cx);
            }
            InputEvent::PressEnter { .. } => {
                // The search stays for n and N; the keyboard goes back to the mode.
                if self.find.as_ref().is_some_and(|find| find.query.is_empty()) {
                    self.find = None;
                } else if let Some(find) = &mut self.find {
                    find.editing = false;
                }
                self.focus_mode(window, cx);
                cx.notify();
            }
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    /// Finds the search's matches again in `target`'s current text, when the search is there.
    pub(super) fn refresh_find(&mut self, target: Mode) {
        let Some(query) = self
            .find
            .as_ref()
            .filter(|find| find.target == target)
            .map(|find| find.query.clone())
        else {
            return;
        };
        let matches = match target {
            Mode::Buffer => self.buffer.as_ref().map_or_else(Vec::new, |buffer| {
                find::matches(
                    buffer
                        .lines
                        .lines
                        .iter()
                        .enumerate()
                        .map(|(row, line)| (row, Column::Whole, line.as_str())),
                    &query,
                )
            }),
            // Only code is searched, each half of a side-by-side row apart; headers and notes
            // are not the file's text.
            Mode::Diff => find::matches(
                self.rows.rows.iter().enumerate().flat_map(|(index, row)| {
                    row_code(row).map(move |(column, code)| (index, column, code))
                }),
                &query,
            ),
            Mode::Tree => Vec::new(),
        };
        if let Some(find) = &mut self.find {
            find.matches = Arc::new(matches);
            find.current = None;
        }
    }

    /// The search of `mode`, when it has one.
    pub(super) fn search_of(&self, mode: Mode) -> Option<&Find> {
        self.find.as_ref().filter(|find| find.target == mode)
    }

    /// Whether `n` steps through matches in the diff rather than going to the next file.
    pub(super) fn diff_search_active(&self) -> bool {
        self.search_of(Mode::Diff)
            .is_some_and(|find| !find.query.is_empty())
    }

    /// Moves to the next (or previous) match; `here` lets a match at the current place count,
    /// as when the query grows.
    pub(super) fn find_step(&mut self, forward: bool, here: bool, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let target = find.target;
        let origin = find.editing.then_some(find.origin);
        let current = find
            .current
            .and_then(|index| find.matches.get(index))
            .map(find::Match::start);
        let place = match target {
            Mode::Buffer => self
                .buffer
                .as_ref()
                .map(|buffer| Spot::whole(buffer.cursor.row, buffer.cursor.col)),
            // From the current match while its hunk stays selected, else from the selected hunk.
            Mode::Diff => current
                .filter(|_| find.hunk == Some(self.hunk))
                .or_else(|| self.hunk_start().map(|row| Spot::whole(row, 0)))
                .or(current),
            Mode::Tree => None,
        };
        // While the query is typed, every match is found from where the search started.
        let Some(place) = origin.filter(|_| here).or(place) else {
            return;
        };
        let from = if here || !forward {
            place
        } else {
            Spot {
                byte: place.byte + 1,
                ..place
            }
        };
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let Some(index) = find::next_from(&find.matches, from, forward) else {
            if !find.query.is_empty() {
                self.set_status(if target == Mode::Diff {
                    "No matches in this diff."
                } else {
                    "No matches in this file."
                });
            }
            cx.notify();
            return;
        };
        find.current = Some(index);
        let total = find.matches.len();
        let Some(found) = find.matches.get(index).cloned() else {
            return;
        };
        self.set_status(format!("Match {} of {total}", index + 1));
        match target {
            Mode::Buffer => self.set_buffer_cursor(found.row, found.range.start, cx),
            Mode::Diff => {
                // The selected hunk follows the match, so the review keys act on what it
                // found: the hunk that holds it, or for unchanged code the nearest one.
                if let Some(hunk) = find::nearest_hunk(&self.hunk_rows, found.row) {
                    self.hunk = hunk;
                    self.selection = None;
                }
                let hunk = self.hunk;
                if let Some(find) = &mut self.find {
                    find.hunk = Some(hunk);
                }
                self.reveal_match(found.row);
                cx.notify();
            }
            Mode::Tree => {}
        }
    }

    /// Scrolls the diff so `row` has some rows of air.
    /// Scrolls the diff so the match on `row` shows together with the selected hunk when both
    /// fit, else the match alone; Space then shows the hunk before marking it.
    pub(super) fn reveal_match(&mut self, row: usize) {
        let height = self.fonts.row_height();
        let fits = scroll::whole(f32::from(self.list.viewport_bounds().size.height) / height)
            .saturating_sub(8);
        let span = self
            .hunk_rows
            .get(self.hunk)
            .map(|rows| rows.start.min(row)..rows.end.max(row + 1))
            .filter(|span| span.len() <= fits);
        self.reveal_diff_rows(span.unwrap_or(row..row + 1));
    }

    /// Scrolls the diff so `rows` have some rows of air.
    fn reveal_diff_rows(&mut self, rows: std::ops::Range<usize>) {
        let height = self.fonts.row_height();
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let top = scroll::float(rows.start) * height;
        let bottom = scroll::float(rows.end) * height;
        if let Some(target) =
            scroll::reveal(self.list_target(), viewport, top, bottom, height * 4.0)
        {
            self.scroll_list_to(target);
        }
    }

    pub(super) fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.find_step(true, false, cx);
    }

    pub(super) fn find_previous(
        &mut self,
        _: &FindPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find_step(false, false, cx);
    }

    /// Esc in a `/` bar: cancels the search, or the tree's filter.
    pub(super) fn close_find(
        &mut self,
        _: &CloseFind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode == Mode::Tree {
            self.filter = None;
            let cursor = self.cursor_identity();
            self.update_visible_tree();
            self.restore_cursor(cursor);
            self.reveal_tree_cursor();
        } else {
            self.find = None;
        }
        self.focus_mode(window, cx);
        cx.notify();
    }
}
