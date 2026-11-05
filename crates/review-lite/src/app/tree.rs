//! The file tree: moving through it previews each file's diff, folders fold, and files and
//! folders are approved as a whole.

use gpui::{Context, Window};
use review_core::tree::{NodeKind, TreeNode};

use super::state::Progress;
use super::{
    Activate, CloseAllFolds, FocusDiff, FocusTree, Mode, OpenAllFolds, Place, ReviewApp,
    TreeCollapse, TreeDown, TreeExpand, TreeUp, file_paths, scroll,
};

impl ReviewApp {
    /// The indices into the tree of the nodes not hidden by a folded folder.
    pub(super) fn visible_tree(&self) -> Vec<usize> {
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

    /// The node under the tree cursor.
    pub(super) fn tree_node(&self) -> Option<&TreeNode> {
        let visible = self.visible_tree();
        visible
            .get(self.tree_cursor.min(visible.len().saturating_sub(1)))
            .and_then(|&index| self.tree.get(index))
    }

    /// Selects the first file that needs attention, or the first file, and puts the tree
    /// cursor on it.
    pub(super) fn select_first_file(&mut self) {
        let target = self
            .tree
            .iter()
            .find(|node| {
                node.kind == NodeKind::File && Progress::of(&node.counts).needs_attention()
            })
            .or_else(|| self.tree.iter().find(|node| node.kind == NodeKind::File))
            .map(|node| node.path.clone());
        if let Some(path) = target {
            self.show_file(&path);
        }
    }

    /// Shows `path`'s diff, keeping where the reviewer was in the file before, and moves the
    /// tree cursor to it.
    pub(super) fn show_file(&mut self, path: &str) {
        if self.selected.as_deref() == Some(path) {
            return;
        }
        self.remember_place();
        self.selected = Some(path.to_owned());
        self.selection = None;
        let place = self.places.get(path).copied();
        self.hunk = place.map_or(0, |place| place.hunk);
        self.rebuild();
        // A file seen for the first time starts at its first hunk that needs the reviewer.
        if place.is_none() {
            self.select_first_open_hunk();
        }
        let place = place.unwrap_or_default();
        self.list_animation = None;
        scroll::set_offset_of(&self.list, place.offset);
        if let Some(position) = self
            .visible_tree()
            .iter()
            .position(|&index| self.tree.get(index).is_some_and(|node| node.path == path))
        {
            self.tree_cursor = position;
            self.reveal_tree_cursor();
        }
    }

    /// Remembers the selected hunk and scroll position of the shown file.
    pub(super) fn remember_place(&mut self) {
        if let Some(path) = self.selected.clone() {
            self.places.insert(
                path,
                Place {
                    hunk: self.hunk,
                    offset: scroll::offset_of(&self.list),
                },
            );
        }
    }

    /// Scrolls the tree so its cursor is visible.
    pub(super) fn reveal_tree_cursor(&self) {
        self.tree_list
            .scroll_to_item(self.tree_cursor, gpui::ScrollStrategy::Top);
    }

    /// Moves the tree cursor by `offset`, previewing the file it lands on.
    fn tree_move(&mut self, offset: isize, cx: &mut Context<Self>) {
        let count = self.visible_tree().len();
        self.tree_cursor = self
            .tree_cursor
            .saturating_add_signed(offset)
            .min(count.saturating_sub(1));
        if let Some(node) = self.tree_node().filter(|node| node.kind == NodeKind::File) {
            let path = node.path.clone();
            let cursor = self.tree_cursor;
            self.show_file(&path);
            self.tree_cursor = cursor;
        }
        self.reveal_tree_cursor();
        cx.notify();
    }

    pub(super) fn tree_down(&mut self, _: &TreeDown, _: &mut Window, cx: &mut Context<Self>) {
        self.tree_move(1, cx);
    }

    pub(super) fn tree_up(&mut self, _: &TreeUp, _: &mut Window, cx: &mut Context<Self>) {
        self.tree_move(-1, cx);
    }

    /// Folds the folder under the tree cursor (`Some(true)`), unfolds it (`Some(false)`) or
    /// toggles it; returns whether the node was a folder. The cursor stays on the folder.
    pub(super) fn tree_fold(&mut self, close: Option<bool>, cx: &mut Context<Self>) -> bool {
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
        cx.notify();
        true
    }

    /// `h`: folds an open folder, or moves to the parent folder.
    pub(super) fn tree_collapse(
        &mut self,
        _: &TreeCollapse,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(node) = self.tree_node() else {
            return;
        };
        let open_folder =
            node.kind == NodeKind::Directory && !self.tree_collapsed.contains(&node.path);
        if open_folder {
            self.tree_fold(Some(true), cx);
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
            cx.notify();
        }
    }

    /// `l`: unfolds a folder, or focuses a file's diff.
    pub(super) fn tree_expand(
        &mut self,
        _: &TreeExpand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tree_fold(Some(false), cx) {
            self.open_diff(window, cx);
        }
    }

    /// Enter in the tree: toggles a folder, or focuses a file's diff.
    pub(super) fn tree_activate(
        &mut self,
        _: &Activate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tree_fold(None, cx) {
            self.open_diff(window, cx);
        }
    }

    /// `0`: focuses the selected file's diff.
    pub(super) fn focus_diff(
        &mut self,
        _: &FocusDiff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_diff(window, cx);
    }

    /// Focuses the diff of the file under the tree cursor, or of the selected file.
    fn open_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self
            .tree_node()
            .filter(|node| node.kind == NodeKind::File)
            .map(|node| node.path.clone())
        {
            self.show_file(&path);
        }
        if self.selected.is_some() {
            self.enter_mode(Mode::Diff, window, cx);
            self.reveal_hunk();
        }
    }

    /// Esc or `h` in the diff: back to the files, with the shown file under the cursor.
    pub(super) fn focus_tree(
        &mut self,
        _: &FocusTree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sidebar_shown() {
            self.screen = super::Screen::Split;
            self.layout.sidebar_visible = true;
        }
        self.enter_mode(Mode::Tree, window, cx);
    }

    /// Folds or unfolds every folder, keeping the cursor on its node or the folder that now
    /// holds it.
    pub(super) fn tree_fold_all(&mut self, close: bool, cx: &mut Context<Self>) {
        let selected = self.tree_node().map(|node| node.path.clone());
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
        if let Some(selected) = selected {
            let visible = self.visible_tree();
            let position = visible.iter().rposition(|&index| {
                self.tree
                    .get(index)
                    .is_some_and(|node| review_core::tree::is_under(&selected, &node.path))
            });
            self.tree_cursor = position.unwrap_or(0);
        }
        self.reveal_tree_cursor();
        cx.notify();
    }

    pub(super) fn close_all_folds(
        &mut self,
        _: &CloseAllFolds,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tree_fold_all(true, cx);
    }

    pub(super) fn open_all_folds(
        &mut self,
        _: &OpenAllFolds,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tree_fold_all(false, cx);
    }

    /// The next file after the shown one, wrapping around, that still needs the reviewer.
    pub(super) fn next_open_file(&self) -> Option<String> {
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
