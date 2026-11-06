//! The file tree: moving through it previews each file's diff, folders fold, and files and
//! folders are approved as a whole.

use std::sync::Arc;
use std::time::Instant;

use review_core::tree::{NodeKind, TreeNode};

use super::effects::{Outcome, Subject};
use super::{Mode, Model, PREVIEW_REST, Place, file_paths};
use crate::frame::Request;
use crate::rows;
use crate::state::{self, Progress};

impl Model {
    /// The indices into the tree of the nodes not hidden by a folded folder, kept until the
    /// tree or its folds change rather than recomputed for every frame.
    #[must_use]
    pub fn visible_tree(&self) -> Arc<Vec<usize>> {
        Arc::clone(&self.tree_visible)
    }

    /// Recomputes the visible tree nodes; call it whenever the tree or its folds change.
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
            if node.kind == NodeKind::Directory && self.tree_collapsed.contains(&node.path) {
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
    pub fn show_file(&mut self, path: &str) {
        self.reveal_in_tree(path);
        if self.selected.as_deref() == Some(path) {
            self.finish_preview();
        } else {
            self.remember_place();
            self.selected = Some(path.to_owned());
            self.selection = None;
            self.show_selected();
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
    fn show_selected(&mut self) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        let place = self.places.get(&path).copied();
        self.hunk = place.map_or(0, |place| place.hunk);
        self.build_selected();
        // A file seen for the first time starts at its first hunk that needs the reviewer.
        if place.is_none() {
            self.select_first_open_hunk();
        }
        self.diff_scroll.stop();
        let offset = super::present::diff_offset(&self.rows, place.unwrap_or_default().top);
        self.diff_scroll.jump(offset);
        // Another file starts at the left edge, whichever layout shows it.
        self.text.reset();
        self.unified_text.reset();
    }

    /// Previews the file the tree cursor lands on. A single press shows its diff at once; while
    /// a held key repeats, only the file's header follows the cursor, and the diff is built
    /// once the cursor rests, so no older file's rows ever show under a newer header.
    fn preview_file(&mut self, path: &str) {
        let now = Instant::now();
        let repeating = self
            .last_move
            .is_some_and(|last| now.duration_since(last) < PREVIEW_REST);
        self.last_move = Some(now);
        if !repeating {
            self.show_file(path);
            return;
        }
        if self.selected.as_deref() == Some(path) {
            return;
        }
        self.remember_place();
        self.selected = Some(path.to_owned());
        self.selection = None;
        self.show_header();
        self.previews += 1;
        let preview = self.previews;
        self.preview = Some(preview);
        // A later move supersedes this preview with its own rest.
        self.after(PREVIEW_REST, Subject::Preview(preview), Outcome::Rested);
    }

    /// Shows the selected file's header alone, while its diff waits for the cursor to rest.
    pub(super) fn show_header(&mut self) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        if let Some(view) = self.view() {
            let units = view.units_of_file(&path);
            let header = rows::header_layout(&view, &units, &self.folds);
            self.rows = Arc::new(rows::present(header, self.presented));
        }
        self.hunk_rows.clear();
        self.hunk = 0;
        self.text.reset();
    }

    /// Builds the diff of a previewed file now, if one is waiting.
    pub(super) fn finish_preview(&mut self) {
        if self.preview.is_some() {
            self.show_selected();
        }
    }

    /// Remembers the selected hunk and the row at the top of the shown file.
    pub(super) fn remember_place(&mut self) {
        // A previewed file was never shown, so it has no place to remember.
        if self.preview.is_some() {
            return;
        }
        if let Some(path) = self.selected.clone() {
            let top = super::present::diff_anchor(&self.rows, self.diff_scroll.offset());
            self.places.insert(
                path,
                Place {
                    hunk: self.hunk,
                    top,
                },
            );
        }
    }

    /// Scrolls the tree so its cursor is visible, at the top when it was out of view, once the
    /// tree is laid out.
    pub(super) fn reveal_tree_cursor(&mut self) {
        self.tree_scroll.request(Request::Top(self.tree_cursor));
    }

    /// Moves the tree cursor by `offset`, previewing the file it lands on.
    pub(super) fn tree_move(&mut self, offset: isize) {
        let count = self.visible_tree().len();
        self.tree_cursor = self
            .tree_cursor
            .saturating_add_signed(offset)
            .min(count.saturating_sub(1));
        if let Some(node) = self.tree_node().filter(|node| node.kind == NodeKind::File) {
            let path = node.path.clone();
            let cursor = self.tree_cursor;
            self.preview_file(&path);
            self.tree_cursor = cursor;
        }
        self.reveal_tree_cursor();
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
        let path = node.path.clone();
        let folded = close.unwrap_or(!self.tree_collapsed.contains(&path));
        if folded {
            self.tree_collapsed.insert(path);
        } else {
            self.tree_collapsed.remove(&path);
        }
        self.update_visible_tree();
        true
    }

    /// `h`: folds an open folder, or moves to the parent folder.
    pub(super) fn tree_collapse(&mut self) {
        let Some(node) = self.tree_node() else {
            return;
        };
        let open_folder =
            node.kind == NodeKind::Directory && !self.tree_collapsed.contains(&node.path);
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
    /// cursor, or of the selected file.
    pub(super) fn open_diff(&mut self) {
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

    /// Esc or `h` in the diff: back to the files, with the shown file under the cursor.
    pub(super) fn focus_tree(&mut self) {
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
        let paths = file_paths(&self.tree);
        let start = self
            .selected
            .as_deref()
            .and_then(|selected| paths.iter().position(|path| *path == selected))
            .map_or(0, |position| position + 1);
        let count = paths.len();
        (0..count)
            .map(|step| (start + step) % count.max(1))
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
