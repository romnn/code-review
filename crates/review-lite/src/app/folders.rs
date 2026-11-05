//! Remembering which folders of the file tree are folded, per review, across launches.
//!
//! The folded folders are this front end's view state of the review (see
//! [`Review::view_state`]): loaded with the review, and saved in the background whenever they
//! change, however they changed.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{Context, Subscription};
use review_core::Review;
use review_core::tree::{NodeKind, TreeNode};
use serde::{Deserialize, Serialize};

use super::ReviewApp;

/// The name review-lite keeps its view state under.
const FRONT_END: &str = "review-lite";

/// review-lite's view state of one review, as stored.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ViewState {
    /// The folded folders' paths, sorted.
    #[serde(default)]
    collapsed: Vec<String>,
}

/// Loads the folded folders last saved for `review`; nothing saved, or a state that cannot be
/// read, folds nothing.
pub(super) fn load(review: &Review) -> Vec<String> {
    match review.view_state::<ViewState>(FRONT_END) {
        Ok(state) => state.unwrap_or_default().collapsed,
        Err(err) => {
            eprintln!("review-lite: ignoring the saved folded folders: {err:#}");
            Vec::new()
        }
    }
}

/// The saved folded folders that are still folders of `tree`; a folder the revision no longer
/// changes anything in is dropped.
pub(super) fn restored(saved: Vec<String>, tree: &[TreeNode]) -> HashSet<String> {
    let folders: HashSet<&str> = tree
        .iter()
        .filter(|node| node.kind == NodeKind::Directory)
        .map(|node| node.path.as_str())
        .collect();
    saved
        .into_iter()
        .filter(|path| folders.contains(path.as_str()))
        .collect()
}

/// The saving of the folded folders.
#[derive(Default)]
pub(super) struct Saver {
    /// The folded folders as last saved, or being saved, or as loaded.
    saved: HashSet<String>,
    /// Whether a save is running; changes made meanwhile are saved once it finishes, so saves
    /// never overtake each other.
    saving: bool,
    /// Whether a save has failed and said so on the status line; later failures go to stderr.
    failed: bool,
    /// Saves after every change of the window's state that changed the folds.
    observer: Option<Subscription>,
}

impl Saver {
    /// Whether a save is running.
    pub(super) fn busy(&self) -> bool {
        self.saving
    }
}

impl ReviewApp {
    /// Folds the saved folders of the shown tree and saves every later change of the folds.
    pub(super) fn restore_folders(&mut self, saved: Vec<String>, cx: &mut Context<Self>) {
        self.tree_collapsed = restored(saved, &self.tree);
        self.folders.saved = self.tree_collapsed.clone();
        self.update_visible_tree();
        let entity = cx.entity();
        self.folders.observer = Some(cx.observe(&entity, |this, _, cx| this.save_folders(cx)));
    }

    /// Saves the folded folders in the background when they differ from those last saved.
    fn save_folders(&mut self, cx: &mut Context<Self>) {
        if self.folders.saving || self.tree_collapsed == self.folders.saved {
            return;
        }
        let Some(review) = self.review.as_ref().map(Arc::clone) else {
            return;
        };
        self.folders.saved = self.tree_collapsed.clone();
        self.folders.saving = true;
        let mut collapsed: Vec<String> = self.tree_collapsed.iter().cloned().collect();
        collapsed.sort_unstable();
        let work = cx
            .background_executor()
            .spawn(async move { review.save_view_state(FRONT_END, &ViewState { collapsed }) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| {
                this.folders.saving = false;
                if let Err(err) = result {
                    eprintln!("review-lite: failed to save the folded folders: {err:#}");
                    if !this.folders.failed {
                        this.folders.failed = true;
                        this.fail(format!("Could not save the folded folders: {err}"));
                        cx.notify();
                    }
                }
                this.save_folders(cx);
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};
    use review_core::tree::file_tree;

    use super::super::state;
    use super::{FRONT_END, ViewState, load, restored};

    /// Saved folders fold again only while the shown revision still has them as folders: a
    /// folder that is gone, and a file at a saved path, are dropped.
    #[test]
    fn restores_saved_folders_the_revision_still_has() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DemoPaths::under(dir.path());
        let review = prepare(&paths, &fixture_dir(), DemoRequest::Fresh).unwrap();
        // A review opened for the first time folds nothing.
        assert_eq!(load(&review), Vec::<String>::new());

        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let tree = file_tree(&state::statuses(&view, &HashMap::new()));
        let collapsed = ["src/generated", "gone", "Cargo.toml"]
            .map(str::to_owned)
            .to_vec();
        review
            .save_view_state(FRONT_END, &ViewState { collapsed })
            .unwrap();
        assert_eq!(
            restored(load(&review), &tree),
            HashSet::from(["src/generated".to_owned()])
        );
    }

    /// A saved state that does not decode, as after a format change, folds nothing rather than
    /// failing to open the review.
    #[test]
    fn an_unreadable_state_folds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DemoPaths::under(dir.path());
        let review = prepare(&paths, &fixture_dir(), DemoRequest::Fresh).unwrap();
        let path = review.dir().view_state(FRONT_END).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&review), Vec::<String>::new());
    }
}
