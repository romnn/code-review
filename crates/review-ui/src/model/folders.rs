//! Saving the view state of the review whenever it changes: which folders of the file tree
//! are folded, and where the reviewer is; loading it and its stored form are
//! [`crate::folders`].

use std::sync::Arc;

use crate::folders::{FRONT_END, Place, ViewState, restored};

use super::effects::{Because, Outcome, Subject};
use super::{Effect, Model};

/// The saving of the view state.
#[derive(Default)]
pub(super) struct Saver {
    /// Whether the view state of an opened review is saved; nothing is before the review opens.
    active: bool,
    /// The view state as last saved, or being saved, or as loaded.
    saved: ViewState,
    /// Whether a save is running; changes made meanwhile are saved once it finishes, so saves
    /// never overtake each other.
    pub(super) saving: bool,
    /// Whether a save has failed and said so on the status line; later failures go to the log
    /// alone.
    failed: bool,
}

impl Saver {
    /// Whether a save is running.
    pub(super) fn busy(&self) -> bool {
        self.saving
    }
}

impl Model {
    /// Folds the saved folders of the shown tree, opens at the saved place when the shown
    /// revision still has it, and saves every later change of either.
    pub(super) fn restore_view_state(&mut self, saved: ViewState) {
        self.tree_collapsed = restored(saved.collapsed.clone(), &self.tree);
        self.update_visible_tree();
        self.select_first_file();
        if let Some(place) = &saved.place {
            self.restore_place(place);
        }
        self.folders.saved = saved;
        self.folders.active = true;
    }

    /// Shows the file of `place` at its hunk, when the shown revision still has that file
    /// outside the folded folders, which stay folded; a unit it no longer has leaves the file at
    /// its first hunk that needs the reviewer.
    fn restore_place(&mut self, place: &Place) {
        let shown = self.visible_tree().iter().any(|&index| {
            self.tree.get(index).is_some_and(|node| {
                node.kind == review_core::tree::NodeKind::File && node.path == place.path
            })
        });
        if !shown {
            return;
        }
        self.show_file(&place.path);
        if let Some(hunk) = place
            .unit
            .as_deref()
            .and_then(|unit| unit.parse().ok())
            .and_then(|unit| self.hunk_of(unit))
        {
            self.select_hunk_at_once(hunk);
        }
    }

    /// Where the reviewer is, to save once they quit: the shown file and its selected hunk's
    /// first unit, or the place last saved while the reviewer stays or the shown file's rows
    /// are not laid out yet.
    ///
    /// Only quitting saves it, so moving around writes nothing; a review left without quitting,
    /// as when the app is killed, opens where it last quit.
    fn current_place(&self) -> Option<Place> {
        if !self.quitting.waits() {
            return self.folders.saved.place.clone();
        }
        let path = self.selected.clone()?;
        // While a folder's diff shows, the review opens again where the reviewer left the
        // selected file.
        if self.folder.is_some() {
            let unit = self
                .places
                .get(&path)
                .and_then(|place| place.unit)
                .map(|unit| unit.to_string());
            return Some(Place { path, unit });
        }
        let laid_out = self
            .rows
            .files
            .first()
            .is_some_and(|file| file.path == path);
        if !laid_out {
            return self.folders.saved.place.clone();
        }
        let unit = self.hunk_units().first().map(ToString::to_string);
        Some(Place { path, unit })
    }

    /// Saves the view state in the background when it differs from what was last saved: the
    /// folded folders whenever they change, the place when the reviewer quits. Runs after every
    /// update, however it changed.
    pub(super) fn save_view_state(&mut self) {
        if !self.folders.active || self.folders.saving {
            return;
        }
        let mut collapsed: Vec<String> = self.tree_collapsed.iter().cloned().collect();
        collapsed.sort_unstable();
        let state = ViewState {
            collapsed,
            place: self.current_place(),
        };
        if state == self.folders.saved {
            return;
        }
        let Some(review) = self.review.as_ref().map(Arc::clone) else {
            return;
        };
        self.folders.saved = state.clone();
        self.folders.saving = true;
        self.spawn(Subject::Always(Because::Save), move || {
            Outcome::FoldersSaved(review.save_view_state(FRONT_END, &state))
        });
    }

    /// Takes in a save of the view state: a failure goes to the log, and to the status line
    /// the first time, unless the reviewer is quitting, when the status line keeps what
    /// quitting says, such as a mark that failed.
    ///
    /// Changes made while the save ran are saved by the update this completion ends with.
    pub(super) fn folders_saved(&mut self, saved: Result<(), review_core::Error>) {
        if let Err(err) = saved {
            self.effect(Effect::Log(format!(
                "failed to save the view state: {err:#}"
            )));
            if !self.folders.failed && self.quitting == super::Quitting::No {
                self.folders.failed = true;
                self.fail(format!(
                    "Could not save the folded folders and place: {err}"
                ));
            }
        }
    }
}
