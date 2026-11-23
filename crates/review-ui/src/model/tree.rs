//! The file tree: moving through it shows each file's diff, folders fold, and files and folders
//! are approved as a whole.

use std::sync::Arc;

use review_core::tree::{NodeKind, TreeNode};

use super::{Mode, Model, Place, file_paths};
use crate::frame::Request;
use crate::state::{self, Progress};

impl Model {
    /// The indices into the tree of the nodes not hidden by a folded folder, kept until the
    /// tree or its folds change rather than recomputed for every frame.
    #[must_use]
    pub fn visible_tree(&self) -> Arc<Vec<usize>> {
        Arc::clone(&self.tree_visible)
    }

    /// Recomputes the visible tree nodes; call it whenever the tree or its folds change.
    #[tracing::instrument(skip_all)]
    pub(super) fn update_visible_tree(&mut self) {
        self.tree_visible = Arc::new(self.compute_visible_tree());
    }

    fn compute_visible_tree(&self) -> Vec<usize> {
        if let Some(query) = self.filter_query() {
            return state::filtered(&self.tree, query);
        }
        let mut visible = Vec::new();
        let mut hidden_below: Option<usize> = None;
        for (index, node) in self.tree.iter().enumerate() {
            if let Some(depth) = hidden_below {
                if node.depth > depth {
                    continue;
                }
                hidden_below = None;
            }
            visible.push(index);
            if self.folded(node) {
                hidden_below = Some(node.depth);
            }
        }
        visible
    }

    /// The index into the tree of the node under the tree cursor.
    pub(super) fn tree_index(&self) -> Option<usize> {
        let visible = self.visible_tree();
        visible
            .get(self.tree_cursor.min(visible.len().saturating_sub(1)))
            .copied()
    }

    /// The node under the tree cursor.
    #[must_use]
    pub fn tree_node(&self) -> Option<&TreeNode> {
        let visible = self.visible_tree();
        visible
            .get(self.tree_cursor.min(visible.len().saturating_sub(1)))
            .and_then(|&index| self.tree.get(index))
    }

    /// Selects the first file that needs attention, or the first file, and puts the tree
    /// cursor on it.
    ///
    /// A file the folded folders show comes first, so a folder the reviewer folded stays
    /// folded; only when every file that needs attention is folded away does one unfold.
    pub(super) fn select_first_file(&mut self) {
        let needs_attention = |node: &&TreeNode| {
            node.kind == NodeKind::File && Progress::of(&node.counts).needs_attention()
        };
        let visible = self.visible_tree();
        let target = visible
            .iter()
            .filter_map(|&index| self.tree.get(index))
            .find(needs_attention)
            .or_else(|| self.tree.iter().find(needs_attention))
            .or_else(|| {
                visible
                    .iter()
                    .filter_map(|&index| self.tree.get(index))
                    .find(|node| node.kind == NodeKind::File)
            })
            .or_else(|| self.tree.iter().find(|node| node.kind == NodeKind::File))
            .map(|node| node.path.clone());
        if let Some(path) = target {
            self.show_file(&path);
        }
    }

    /// Shows `path`'s diff, keeping where the reviewer was in the file before, and moves the
    /// tree cursor to it. This is the one way the shown file changes, so the tree cursor always
    /// lands on the file whose diff shows: a filter that hides it is cleared and the folders
    /// that fold it open.
    ///
    /// A folder's diff showing in the selected file's place gives way to `path`'s.
    pub fn show_file(&mut self, path: &str) {
        self.reveal_in_tree(path);
        let previewing = self.folder.take().is_some();
        if previewing || self.selected.as_deref() != Some(path) {
            if !previewing {
                self.remember_place();
            }
            self.wheel_momentum.stop();
            self.selected = Some(path.to_owned());
            self.selection = None;
            self.show_selected();
            // The selected file's highlights start at once, cancelling the previous file's.
            self.pump_highlights();
        }
        if let Some(position) = self.visible_tree().iter().position(|&index| {
            // The file node, never a folder that shares its path.
            self.tree
                .get(index)
                .is_some_and(|node| node.kind == NodeKind::File && node.path == path)
        }) {
            self.tree_cursor = position;
            self.reveal_tree_cursor();
        }
    }

    /// Makes the file node `path` visible: clears a filter that hides it and unfolds the
    /// folders that hold it.
    fn reveal_in_tree(&mut self, path: &str) {
        let is_node = |node: &TreeNode| node.kind == NodeKind::File && node.path == path;
        if !self.tree.iter().any(is_node) {
            return;
        }
        let hidden = !self
            .visible_tree()
            .iter()
            .any(|&index| self.tree.get(index).is_some_and(is_node));
        if !hidden {
            return;
        }
        if self.filter_query().is_some()
            && !state::filtered(&self.tree, self.filter_query().unwrap_or_default())
                .iter()
                .any(|&index| self.tree.get(index).is_some_and(is_node))
        {
            self.filter = None;
        }
        self.tree_collapsed
            .retain(|folder| !review_core::tree::is_under(path, folder));
        self.update_visible_tree();
    }

    /// Builds the selected file's diff at the place the reviewer left it.
    pub(super) fn show_selected(&mut self) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        let place = self.places.get(&path).copied();
        self.hunk = 0;
        self.build_selected();
        if let Some(hunk) = place
            .and_then(|place| place.unit)
            .and_then(|unit| self.hunk_of(unit))
        {
            self.hunk = hunk;
        }
        // A file seen for the first time starts at its first hunk that needs the reviewer.
        if place.is_none() {
            self.select_first_open_hunk();
        }
        self.diff_scroll.stop();
        // A place taken in the other view names a row of other rows: it is found again by
        // what it showed.
        let top = place.map_or_else(super::present::Anchor::default, |place| match place.key {
            Some(key) if place.view != self.presented.view => key
                .find(&self.rows)
                .map_or(place.top, |row| super::present::Anchor { row, ..place.top }),
            _ => place.top,
        });
        // A file seen for the first time shows from its first change that needs the reviewer,
        // not from its first line.
        let offset = match place {
            Some(_) => super::present::diff_offset(&self.rows, top),
            None => self.hunk_landing().unwrap_or(0.0),
        };
        self.diff_scroll.jump(offset);
        // Another file starts at the left edge, whichever layout shows it.
        self.text.reset();
        self.unified_text.reset();
    }

    /// Remembers the selected hunk and the row at the top of the shown file; a folder's diff
    /// is no file's place.
    pub(super) fn remember_place(&mut self) {
        if self.folder.is_some() {
            return;
        }
        if let Some(path) = self.selected.clone() {
            let offset = self.diff_scroll.offset();
            let top = super::present::diff_anchor(&self.rows, offset);
            let key = self
                .rows
                .row_on_screen(super::present::Anchor::at(offset).row)
                .map(super::present::RowKey::of);
            self.places.insert(
                path,
                Place {
                    unit: self.hunk_units().first().copied(),
                    top,
                    key,
                    view: self.presented.view,
                },
            );
        }
    }

    /// Scrolls the tree so its cursor is visible, at the top when it was out of view, once the
    /// tree is laid out.
    pub(super) fn reveal_tree_cursor(&mut self) {
        self.tree_scroll.request(Request::Top(self.tree_cursor));
    }

    /// Moves the tree cursor by `offset`, showing the file it lands on.
    pub(super) fn tree_move(&mut self, offset: isize) {
        let count = self.visible_tree().len();
        let position = self
            .tree_cursor
            .saturating_add_signed(offset)
            .min(count.saturating_sub(1));
        self.tree_move_to(position);
    }

    /// Moves the tree cursor to `position` among the visible nodes, showing the file it lands
    /// on.
    pub(super) fn tree_move_to(&mut self, position: usize) {
        self.tree_cursor = position;
        if let Some(node) = self.tree_node().filter(|node| node.kind == NodeKind::File) {
            let path = node.path.clone();
            self.show_file(&path);
            self.tree_cursor = position;
        }
        self.reveal_tree_cursor();
    }

    /// The position among the visible nodes of the file after (`forward`) or before the one
    /// the keys act on: the tree cursor's node in the tree, elsewhere the shown file, or the
    /// folded folder that holds it.
    ///
    /// Folders are no stops, and a folded folder's files are not visible, so they are skipped
    /// whole. `None` at either end, or when the tree has no other file.
    #[must_use]
    pub fn adjacent_file(&self, forward: bool) -> Option<usize> {
        let visible = self.visible_tree();
        let from = match self.mode {
            Mode::Tree => Some(self.tree_cursor.min(visible.len().saturating_sub(1))),
            Mode::Diff | Mode::Buffer => self.selected.as_deref().and_then(|path| {
                state::cursor_position(&self.tree, &visible, path, NodeKind::File)
            }),
        };
        let is_file = |position: &usize| {
            visible
                .get(*position)
                .and_then(|&index| self.tree.get(index))
                .is_some_and(|node| node.kind == NodeKind::File)
        };
        match (from, forward) {
            (Some(from), true) => (from + 1..visible.len()).find(is_file),
            (Some(from), false) => (0..from).rev().find(is_file),
            (None, _) => (0..visible.len()).find(is_file),
        }
    }

    /// Folds the folder under the tree cursor (`Some(true)`), unfolds it (`Some(false)`) or
    /// toggles it; returns whether the node was a folder. The cursor stays on the folder.
    pub(super) fn tree_fold(&mut self, close: Option<bool>) -> bool {
        let Some(node) = self
            .tree_node()
            .filter(|node| node.kind == NodeKind::Directory)
        else {
            return false;
        };
        let folded = close.unwrap_or(!self.folded(node));
        let path = node.path.clone();
        let folders: Vec<String> = node.directories().map(str::to_owned).collect();
        if folded {
            self.tree_collapsed.insert(path);
        } else {
            // Every folder of a chain unfolds with it, so none folds again when the chain
            // parts.
            for folder in &folders {
                self.tree_collapsed.remove(folder);
            }
        }
        self.update_visible_tree();
        true
    }

    /// Whether `node` is a folded folder: one whose own fold, or a fold of any folder of its
    /// chain, hides what it holds.
    #[must_use]
    pub fn folded(&self, node: &TreeNode) -> bool {
        node.directories()
            .any(|folder| self.tree_collapsed.contains(folder))
    }

    /// `h`: folds an open folder, or moves to the parent folder.
    pub(super) fn tree_collapse(&mut self) {
        let Some(node) = self.tree_node() else {
            return;
        };
        let open_folder = node.kind == NodeKind::Directory && !self.folded(node);
        if open_folder {
            self.tree_fold(Some(true));
            return;
        }
        let depth = node.depth;
        let visible = self.visible_tree();
        let parent = visible.iter().take(self.tree_cursor).rposition(|&index| {
            self.tree
                .get(index)
                .is_some_and(|node| node.depth + 1 == depth)
        });
        if let Some(parent) = parent {
            self.tree_cursor = parent;
            self.reveal_tree_cursor();
        }
    }

    /// `l`: unfolds a folder, or focuses a file's diff.
    pub(super) fn tree_expand(&mut self) {
        if !self.tree_fold(Some(false)) {
            self.open_diff();
        }
    }

    /// Enter in the tree: toggles a folder, or focuses a file's diff.
    pub(super) fn tree_activate(&mut self) {
        if !self.tree_fold(None) {
            self.open_diff();
        }
    }

    /// `0`, and opening a file from the tree: focuses the diff of the file under the tree
    /// cursor, or of the selected file; on a folder, of the file its diff shows at the reading
    /// line.
    pub(super) fn open_diff(&mut self) {
        // A folder's diff opens the file under its reading line, where it shows.
        if self.folder.is_some() {
            if self.open_from_folder(None) {
                self.enter_mode(Mode::Diff);
            }
            return;
        }
        if let Some(path) = self
            .tree_node()
            .filter(|node| node.kind == NodeKind::File)
            .map(|node| node.path.clone())
        {
            self.show_file(&path);
        }
        if self.selected.is_some() {
            self.enter_mode(Mode::Diff);
            self.reveal_hunk();
        }
    }

    /// Esc or `h` in the diff: back to the files, with the shown file under the cursor; with
    /// text selected, first drops the selection, and in scroll mode, first goes out of it and
    /// back to the hunk it began at.
    pub(super) fn focus_tree(&mut self) {
        // A selection goes first, as Esc drops it in the buffer.
        if self.mode == Mode::Diff
            && self
                .selection
                .take()
                .is_some_and(|selection| !selection.is_empty())
        {
            return;
        }
        if self.mode == Mode::Diff && self.scroll_mode {
            self.leave_scroll_mode();
            return;
        }
        if !self.sidebar_shown() {
            self.screen = super::Screen::Split;
            self.layout.sidebar_visible = true;
        }
        self.enter_mode(Mode::Tree);
    }

    /// A click on the tree row at `position`, node `index` of the tree, the `clicks`th of a
    /// quick series: a folder folds or unfolds, a file shows and a double click opens its
    /// diff, and the files take the keys back from another mode.
    pub fn click_tree_row(&mut self, position: usize, index: usize, clicks: usize) {
        if !self.settings.mouse.navigate {
            return;
        }
        self.tree_cursor = position;
        let directory = self
            .tree
            .get(index)
            .is_some_and(|node| node.kind == NodeKind::Directory);
        if directory {
            self.tree_fold(None);
        } else if let Some(path) = self.tree.get(index).map(|node| node.path.clone()) {
            self.show_file(&path);
            self.tree_cursor = position;
            if clicks > 1 {
                self.enter_mode(Mode::Diff);
            }
        }
        if self.mode != Mode::Tree && clicks == 1 {
            self.enter_mode(Mode::Tree);
        }
    }

    /// Folds or unfolds every folder, keeping the cursor on its node or the folder that now
    /// holds it.
    pub(super) fn tree_fold_all(&mut self, close: bool) {
        let cursor = self.cursor_identity();
        if close {
            self.tree_collapsed = self
                .tree
                .iter()
                .filter(|node| node.kind == NodeKind::Directory)
                .map(|node| node.path.clone())
                .collect();
        } else {
            self.tree_collapsed.clear();
        }
        self.update_visible_tree();
        self.restore_cursor(cursor);
        self.reveal_tree_cursor();
    }

    /// What the tree cursor is on: a path and whether it is a file or a folder, since a deleted
    /// file `foo` and a folder `foo/` can both be in the tree.
    pub(super) fn cursor_identity(&self) -> Option<(String, NodeKind)> {
        self.tree_node().map(|node| (node.path.clone(), node.kind))
    }

    /// Puts the tree cursor back on the node `identity` names, or, when a fold hides it, on the
    /// innermost visible folder that holds it; never on a file that only shares a folder's path.
    pub(super) fn restore_cursor(&mut self, identity: Option<(String, NodeKind)>) {
        let Some((path, kind)) = identity else {
            return;
        };
        let visible = self.visible_tree();
        if let Some(position) = state::cursor_position(&self.tree, &visible, &path, kind) {
            self.tree_cursor = position;
        }
    }

    /// The next file after the shown one, wrapping around, that still needs the reviewer.
    #[must_use]
    pub fn next_open_file(&self) -> Option<String> {
        self.open_file_toward(true)
    }

    /// The file after (`forward`) or before the shown one in tree order, wrapping around, that
    /// still needs the reviewer; folded folders' files count too.
    #[must_use]
    pub fn open_file_toward(&self, forward: bool) -> Option<String> {
        let paths = file_paths(&self.tree);
        let count = paths.len();
        let here = self
            .selected
            .as_deref()
            .and_then(|selected| paths.iter().position(|path| *path == selected));
        let start = match (here, forward) {
            (Some(here), true) => here + 1,
            (Some(here), false) => here + count.saturating_sub(1),
            (None, true) => 0,
            (None, false) => count.saturating_sub(1),
        };
        (0..count)
            .map(|step| {
                if forward {
                    (start + step) % count.max(1)
                } else {
                    (start + count - step) % count.max(1)
                }
            })
            .filter_map(|index| paths.get(index))
            .find(|path| {
                Some(**path) != self.selected.as_deref()
                    && self
                        .tree
                        .iter()
                        .find(|node| node.path == **path)
                        .is_some_and(|node| Progress::of(&node.counts).needs_attention())
            })
            .map(|path| (*path).to_owned())
    }
}
