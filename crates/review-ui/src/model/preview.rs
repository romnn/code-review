//! A folder's diff: while the tree cursor is on a folder, the diff shows every change of the
//! files it holds, each under its path, as lazygit shows a folder's diff.
//!
//! The preview is the tree's alone: the diff and the buffer act on one file, so whatever gives
//! them the keys first opens the file under the preview's reading line, or under a click, at
//! the same place on screen. [`Model::sync_folder_preview`] runs after every update and keeps
//! the preview on the folder under the cursor and on exactly the files the tree shows of it,
//! so a folder's marks always act on the files its diff shows.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use review_core::UnitId;
use review_core::tree::NodeKind;

use super::present::{Anchor, DiffAnchor, RowKey, diff_anchor, diff_offset};
use super::{Mode, Model};
use crate::config::DiffView;
use crate::rows::{self, Layout};
use crate::scroll;
use crate::state;

/// A folder whose diff shows in place of a file's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FolderPreview {
    /// The folder's path in the tree.
    pub(super) path: String,
    /// The files the preview shows, in tree order: the folder's files that the tree's filter
    /// keeps, whether or not a folded folder hides them.
    pub(super) files: Vec<String>,
}

impl Model {
    /// The folder whose diff shows, while one does.
    #[must_use]
    pub fn folder(&self) -> Option<&str> {
        self.folder.as_ref().map(|folder| folder.path.as_str())
    }

    /// The file whose diff shows: the selected file, unless a folder's diff shows instead.
    #[must_use]
    pub fn shown_file(&self) -> Option<&str> {
        self.selected.as_deref().filter(|_| self.folder.is_none())
    }

    /// The files of the folder at `path` that the tree shows, in tree order, or `None` when
    /// the tree has no such folder.
    pub(super) fn folder_members(&self, path: &str) -> Option<Vec<String>> {
        let index = self
            .tree
            .iter()
            .position(|node| node.kind == NodeKind::Directory && node.path == path)?;
        let kept: Option<HashSet<usize>> = self
            .filter_query()
            .map(|query| state::filtered(&self.tree, query).into_iter().collect());
        let depth = self.tree.get(index).map(|node| node.depth)?;
        Some(
            self.tree
                .iter()
                .enumerate()
                .skip(index + 1)
                .take_while(|(_, node)| node.depth > depth)
                .filter(|(position, node)| {
                    node.kind == NodeKind::File
                        && kept.as_ref().is_none_or(|kept| kept.contains(position))
                })
                .map(|(_, node)| node.path.clone())
                .collect(),
        )
    }

    /// The preview the diff should show now: the folder under the tree cursor while the tree
    /// has the keys.
    fn wanted_preview(&self) -> Option<FolderPreview> {
        if self.mode != Mode::Tree {
            return None;
        }
        let node = self
            .tree_node()
            .filter(|node| node.kind == NodeKind::Directory)?;
        let path = node.path.clone();
        let files = self.folder_members(&path)?;
        Some(FolderPreview { path, files })
    }

    /// Keeps the diff on what the tree cursor is on, after every update: the folder under it,
    /// with the files the tree shows of it now, or, once the cursor leaves the folder, the
    /// selected file again. A preview still showing when another mode has the keys opens the
    /// file under its reading line, unless a buffer of a file outside the review took them,
    /// which leaves the diff to the selected file as before the preview.
    pub(super) fn sync_folder_preview(&mut self) {
        if self.folder.is_some() && self.mode != Mode::Tree {
            let elsewhere = self.mode == Mode::Buffer
                && self.buffer.as_ref().is_some_and(|buffer| {
                    !matches!(buffer.origin, super::buffer::Origin::Review(_))
                });
            if elsewhere || !self.open_from_folder(None) {
                self.leave_folder();
            }
            return;
        }
        match (self.wanted_preview(), &self.folder) {
            (Some(wanted), Some(shown)) if wanted == *shown => {}
            (Some(wanted), _) => self.show_folder(wanted),
            (None, Some(_)) => self.leave_folder(),
            (None, None) => {}
        }
    }

    /// Shows `preview`'s diff from its top, keeping where the reviewer was in the file that
    /// showed before.
    fn show_folder(&mut self, preview: FolderPreview) {
        let same_folder = self
            .folder
            .as_ref()
            .is_some_and(|shown| shown.path == preview.path);
        if self.folder.is_none() {
            self.remember_place();
        }
        self.folder = Some(preview);
        self.selection = None;
        self.wheel_momentum.stop();
        if same_folder {
            // The same folder with other files, as when a filter changes: the reviewer keeps
            // their place.
            self.rebuild_folder();
            return;
        }
        self.hunk = 0;
        self.build_selected();
        self.diff_scroll.stop();
        self.diff_scroll.jump(0.0);
        self.text.reset();
        self.unified_text.reset();
        self.pump_highlights();
    }

    /// Ends the preview and shows the selected file again where the reviewer left it, or no
    /// rows without one.
    fn leave_folder(&mut self) {
        self.folder = None;
        self.selection = None;
        self.wheel_momentum.stop();
        if self.selected.is_some() {
            self.show_selected();
        } else {
            self.hunk = 0;
            self.build_selected();
        }
    }

    /// Lays the folder's rows out again, keeping the line at the top where it is: rows of
    /// files before it can grow or shrink as their text arrives or the view changes.
    pub(super) fn rebuild_folder(&mut self) {
        let offset = self.diff_scroll.offset();
        let top = diff_anchor(&self.rows, offset);
        let key = self
            .rows
            .row_on_screen(Anchor::at(offset).row)
            .map(RowKey::of);
        self.build_selected();
        // The same revision's text in the same view, so the line keeps its byte too.
        if let Some(row) = key.and_then(|key| key.find(&self.rows)) {
            let top = DiffAnchor {
                top: Anchor { row, ..top.top },
                ..top
            };
            self.diff_scroll.jump(diff_offset(&self.rows, top));
        }
    }

    /// Lays out the diff of the folder at `path`: every unit of the files the tree shows of
    /// it, as file sections in tree order, in the hunks view whatever the view setting, so a
    /// broad folder lays out fast and reads as its changes alone. `None` when the tree has no
    /// such folder.
    pub(super) fn lay_out_folder(
        &mut self,
        view: &review_core::RevisionView<'_>,
        path: &str,
    ) -> Option<Layout> {
        let files = self.folder_members(path)?;
        if let Some(folder) = &mut self.folder {
            folder.files.clone_from(&files);
        }
        let wanted: HashSet<&str> = files.iter().map(String::as_str).collect();
        self.folds.texts.clear();
        self.folds.syntax.clear();
        self.folds.view = DiffView::Hunks;
        self.folds.layout = self.presented.layout;
        self.folds.single_file = false;
        self.folds.pending.clone_from(&self.pending);
        let mut units: HashMap<&str, Vec<UnitId>> = HashMap::new();
        if let Some(analysis) = view.analysis() {
            for file in &analysis.files {
                let shown = file.display_path();
                let Some(&shown) = wanted.get(shown) else {
                    continue;
                };
                units
                    .entry(shown)
                    .or_default()
                    .extend(file.units.iter().map(|&index| view.id(index)));
                if let Some(text) = file
                    .new
                    .as_ref()
                    .and_then(|side| self.blobs.texts.get(&side.oid))
                {
                    self.folds.texts.insert(shown.to_owned(), Arc::clone(text));
                }
                let syntax = crate::syntax::FileSyntax {
                    head: file
                        .new
                        .as_ref()
                        .and_then(|side| self.blobs.side_syntax(side)),
                    base: file
                        .old
                        .as_ref()
                        .and_then(|side| self.blobs.side_syntax(side)),
                };
                self.folds.syntax.insert(shown.to_owned(), Arc::new(syntax));
            }
        }
        let units: Vec<UnitId> = files
            .iter()
            .filter_map(|path| units.remove(path.as_str()))
            .flatten()
            .collect();
        Some(rows::present(
            rows::build(view, &units, &self.folds),
            rows::Presentation {
                view: DiffView::Hunks,
                ..self.presented
            },
        ))
    }

    /// Opens the file the preview shows at screen row `row`, or else at its reading line, in
    /// its own diff with that line where it was on screen, at its hunk there or the nearest
    /// one of that file. Returns whether a file opened; the caller gives the diff the keys.
    pub(super) fn open_from_folder(&mut self, row: Option<usize>) -> bool {
        if self.folder.is_none() || self.rows.screen.is_empty() {
            return false;
        }
        let offset = self.diff_scroll.offset();
        let row = row.unwrap_or_else(|| {
            let viewport = self.grid().viewports.diff.unwrap_or(0.0);
            scroll::whole_rows(offset + viewport * scroll::READING_LINE)
        });
        let row = row.min(self.rows.screen.len() - 1);
        let Some(target) = self.rows.row_on_screen(row) else {
            return false;
        };
        // Where in its line the screen row starts, when the line wraps.
        let start = diff_anchor(&self.rows, scroll::rows(row)).start;
        let file = target.file;
        let Some(path) = self
            .rows
            .files
            .get(file)
            .map(|section| section.path.clone())
        else {
            return false;
        };
        let key = RowKey::of(target);
        // A row of a change names its unit, which may share a hunk here with others it does
        // not share in the file's own diff.
        let unit = target.hunk.map(|_| target.unit);
        let on_screen = scroll::rows(row) - offset;
        // The preview ends here, so the file's own rows replace it.
        self.show_file(&path);
        let landed = key.find(&self.rows).map(|source| {
            diff_offset(
                &self.rows,
                DiffAnchor {
                    top: Anchor {
                        row: source,
                        fraction: 0.0,
                    },
                    start,
                },
            )
        });
        if let Some(landed) = landed {
            self.diff_scroll.jump((landed - on_screen).max(0.0));
        }
        // The hunk of the unit opened at, or else the one nearest the line opened at.
        let hunk = unit.and_then(|unit| self.hunk_of(unit)).or_else(|| {
            let at = scroll::whole_rows(landed?);
            self.hunk_rows
                .iter()
                .enumerate()
                .min_by_key(|(_, rows)| {
                    if rows.contains(&at) {
                        0
                    } else {
                        rows.start.abs_diff(at).min(rows.end.abs_diff(at + 1))
                    }
                })
                .map(|(hunk, _)| hunk)
        });
        if let Some(hunk) = hunk {
            self.hunk = hunk;
        }
        true
    }
}
