//! Saving which folders of the file tree are folded, per review, whenever they change; loading
//! them and their stored form are [`crate::folders`].

use std::collections::HashSet;
use std::sync::Arc;

use crate::folders::{FRONT_END, ViewState, restored};

use super::effects::{Because, Outcome, Subject};
use super::{Effect, Model};

/// The saving of the folded folders.
#[derive(Default)]
pub(super) struct Saver {
    /// Whether the folds of an opened review are saved; nothing is before the review opens.
    active: bool,
    /// The folded folders as last saved, or being saved, or as loaded.
    saved: HashSet<String>,
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
    /// Folds the saved folders of the shown tree and saves every later change of the folds.
    pub(super) fn restore_folders(&mut self, saved: Vec<String>) {
        self.tree_collapsed = restored(saved, &self.tree);
        self.folders.saved = self.tree_collapsed.clone();
        self.folders.active = true;
        self.update_visible_tree();
    }

    /// Saves the folded folders in the background when they differ from those last saved; runs
    /// after every update, however the folds changed.
    pub(super) fn save_folders(&mut self) {
        if !self.folders.active || self.folders.saving || self.tree_collapsed == self.folders.saved
        {
            return;
        }
        let Some(review) = self.review.as_ref().map(Arc::clone) else {
            return;
        };
        self.folders.saved = self.tree_collapsed.clone();
        self.folders.saving = true;
        let mut collapsed: Vec<String> = self.tree_collapsed.iter().cloned().collect();
        collapsed.sort_unstable();
        self.spawn(Subject::Always(Because::Save), move || {
            Outcome::FoldersSaved(review.save_view_state(FRONT_END, &ViewState { collapsed }))
        });
    }

    /// Takes in a save of the folded folders: a failure goes to the log, and to the status
    /// line the first time.
    ///
    /// Changes made while the save ran are saved by the update this completion ends with.
    pub(super) fn folders_saved(&mut self, saved: Result<(), review_core::Error>) {
        if let Err(err) = saved {
            self.effect(Effect::Log(format!(
                "failed to save the folded folders: {err:#}"
            )));
            if !self.folders.failed {
                self.folders.failed = true;
                self.fail(format!("Could not save the folded folders: {err}"));
            }
        }
    }
}
