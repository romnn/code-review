//! Remembering which folders of the file tree are folded, and where the reviewer was, per
//! review, across launches.
//!
//! The folded folders and the place are the front ends' view state of the review (see
//! [`Review::view_state`]): loaded with the review, and saved by the front end in the background
//! whenever they change, however they changed. The place is only where the review opens: it
//! never marks anything, and one the shown revision no longer has is ignored.

use std::collections::HashSet;

use review_core::Review;
use review_core::tree::TreeNode;
use serde::{Deserialize, Serialize};

/// The name the front ends keep their view state under: review-gui's name when it was
/// review-lite, which stored it first, kept so the folds it saved keep loading.
pub const FRONT_END: &str = "review-lite";

/// The front ends' view state of one review, as stored.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewState {
    /// The folded folders' paths, sorted.
    #[serde(default)]
    pub collapsed: Vec<String>,
    /// Where the reviewer was when the review was last left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<Place>,
}

/// Where the reviewer was: the file whose diff showed, and the first unit of its selected
/// hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    /// The file's display path.
    pub path: String,
    /// The selected hunk's first unit, as its ID reads, such as `u1-0042`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// Loads the view state last saved for `review`; nothing saved folds nothing and opens where a
/// review opens the first time.
///
/// # Errors
///
/// Returns why, as a message for the developer's log, when the saved state cannot be read; the
/// review then opens as for the first time.
pub fn load(review: &Review) -> Result<ViewState, String> {
    review
        .view_state::<ViewState>(FRONT_END)
        .map(Option::unwrap_or_default)
        .map_err(|err| format!("ignoring the saved view state: {err:#}"))
}

/// The saved folded folders that are still folders of `tree`, on a node of their own or in a
/// chain; a folder the revision no longer changes anything in is dropped.
#[must_use]
pub fn restored(saved: Vec<String>, tree: &[TreeNode]) -> HashSet<String> {
    let folders: HashSet<&str> = tree.iter().flat_map(TreeNode::directories).collect();
    saved
        .into_iter()
        .filter(|path| folders.contains(path.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};
    use review_core::tree::file_tree;

    use super::{FRONT_END, ViewState, load, restored};
    use crate::state;

    /// Saved folders fold again only while the shown revision still has them as folders, on a
    /// node of their own or in a chain, as the demo's `.github` in `.github/workflows`: a
    /// folder that is gone, and a file at a saved path, are dropped.
    #[test]
    fn restores_saved_folders_the_revision_still_has() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DemoPaths::under(dir.path());
        let review = prepare(&paths, &fixture_dir(), DemoRequest::Fresh).unwrap();
        // A review opened for the first time folds nothing.
        assert_eq!(load(&review), Ok(ViewState::default()));

        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let tree = file_tree(&state::statuses(&view, &HashMap::new()));
        let collapsed = ["src/generated", ".github", "gone", "Cargo.toml"]
            .map(str::to_owned)
            .to_vec();
        review
            .save_view_state(
                FRONT_END,
                &ViewState {
                    collapsed,
                    place: None,
                },
            )
            .unwrap();
        assert_eq!(
            restored(load(&review).unwrap().collapsed, &tree),
            HashSet::from(["src/generated".to_owned(), ".github".to_owned()])
        );
    }

    /// A saved state that does not decode, as after a format change, is reported rather than
    /// failing to open the review, which then folds nothing.
    #[test]
    fn an_unreadable_state_folds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DemoPaths::under(dir.path());
        let review = prepare(&paths, &fixture_dir(), DemoRequest::Fresh).unwrap();
        let path = review.dir().view_state(FRONT_END).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        let ignored = load(&review).unwrap_err();
        assert!(
            ignored.starts_with("ignoring the saved view state: "),
            "{ignored}"
        );
    }
}
