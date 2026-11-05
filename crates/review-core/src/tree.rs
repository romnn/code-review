//! The changed files of a revision as a directory tree, with review counts aggregated per
//! directory, for file-tree sidebars.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::view::{Counts, FileChangeSummary, FileStatus, FileTone};

/// Whether a tree node is a directory or a changed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A directory holding changed files.
    Directory,
    /// A changed file.
    File,
}

/// One node of the tree, in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeNode {
    /// The last path component.
    pub name: String,
    /// The repository-relative path, without a trailing slash.
    pub path: String,
    /// The nesting depth; top-level entries are at 0.
    pub depth: usize,
    /// Whether the node is a directory or a file.
    pub kind: NodeKind,
    /// The counts of the file, or the sum over every file below the directory.
    pub counts: Counts,
    /// For a renamed file, its path at the base.
    pub renamed_from: Option<String>,
    /// The file's own tone, or a directory's most significant tone below it.
    pub tone: FileTone,
    /// How a file changed; `None` for a directory.
    pub change: Option<FileChangeSummary>,
}

#[derive(Default)]
struct Directory {
    directories: BTreeMap<String, Directory>,
    files: BTreeMap<String, FileStatus>,
}

/// Arranges `files` as a tree: directories before files, each sorted by name, depth first.
#[must_use]
pub fn file_tree(files: &[FileStatus]) -> Vec<TreeNode> {
    let mut root = Directory::default();
    for file in files {
        let mut directory = &mut root;
        let mut components: Vec<&str> = file.path.split('/').collect();
        let Some(name) = components.pop() else {
            continue;
        };
        for component in components {
            directory = directory
                .directories
                .entry(component.to_owned())
                .or_default();
        }
        directory.files.insert(name.to_owned(), file.clone());
    }
    let mut nodes = Vec::new();
    flatten(&root, "", 0, &mut nodes);
    nodes
}

/// Appends the nodes below `directory` and returns the sum of their counts and their most
/// significant tone.
fn flatten(
    directory: &Directory,
    prefix: &str,
    depth: usize,
    nodes: &mut Vec<TreeNode>,
) -> (Counts, FileTone) {
    let mut total = Counts::default();
    let mut strongest = FileTone::Generated;
    for (name, child) in &directory.directories {
        let path = format!("{prefix}{name}");
        let index = nodes.len();
        nodes.push(TreeNode {
            name: name.clone(),
            path: path.clone(),
            depth,
            kind: NodeKind::Directory,
            counts: Counts::default(),
            renamed_from: None,
            tone: FileTone::Generated,
            change: None,
        });
        let (counts, tone) = flatten(child, &format!("{path}/"), depth + 1, nodes);
        total.merge(&counts);
        strongest = strongest.max(tone);
        if let Some(node) = nodes.get_mut(index) {
            node.counts = counts;
            node.tone = tone;
        }
    }
    for (name, file) in &directory.files {
        total.merge(&file.counts);
        let tone = file.change.tone();
        strongest = strongest.max(tone);
        nodes.push(TreeNode {
            name: name.clone(),
            path: file.path.clone(),
            depth,
            kind: NodeKind::File,
            counts: file.counts.clone(),
            renamed_from: file.renamed_from.clone(),
            tone,
            change: Some(file.change.clone()),
        });
    }
    (total, strongest)
}

/// Whether `path` is `node` itself or lies below the directory `node`.
#[must_use]
pub fn is_under(path: &str, node: &str) -> bool {
    path == node
        || path
            .strip_prefix(node)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::NoiseCategory;
    use crate::view::ChangeKind;

    fn file(path: &str, reviewed: usize, units: usize) -> FileStatus {
        FileStatus {
            path: path.to_owned(),
            counts: Counts {
                units,
                reviewed,
                new: units - reviewed,
                ..Counts::default()
            },
            renamed_from: None,
            change: FileChangeSummary::modified(),
        }
    }

    /// Directories come first and sum their files' counts at every level.
    #[test]
    fn directories_aggregate_their_files() {
        let nodes = file_tree(&[
            file("src/b.rs", 1, 2),
            file("README.md", 0, 1),
            file("src/app/a.rs", 2, 2),
        ]);
        let shape: Vec<(usize, &str, NodeKind, usize, usize)> = nodes
            .iter()
            .map(|node| {
                (
                    node.depth,
                    node.path.as_str(),
                    node.kind,
                    node.counts.reviewed,
                    node.counts.units,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (0, "src", NodeKind::Directory, 3, 4),
                (1, "src/app", NodeKind::Directory, 2, 2),
                (2, "src/app/a.rs", NodeKind::File, 2, 2),
                (1, "src/b.rs", NodeKind::File, 1, 2),
                (0, "README.md", NodeKind::File, 0, 1),
            ]
        );
    }

    /// A directory takes the most significant tone of the files below it.
    #[test]
    fn directories_take_their_strongest_tone() {
        let mut added = file("src/new.rs", 0, 1);
        added.change.kind = ChangeKind::Added;
        let mut generated = file("gen/schema.rs", 0, 1);
        generated.change.noise = Some(NoiseCategory::Generated);
        let nodes = file_tree(&[added, file("src/old.rs", 0, 1), generated]);
        let tones: Vec<(&str, FileTone)> = nodes
            .iter()
            .map(|node| (node.path.as_str(), node.tone))
            .collect();
        assert_eq!(
            tones,
            [
                ("gen", FileTone::Generated),
                ("gen/schema.rs", FileTone::Generated),
                ("src", FileTone::Added),
                ("src/new.rs", FileTone::Added),
                ("src/old.rs", FileTone::Modified),
            ]
        );
    }

    #[test]
    fn paths_are_under_themselves_and_their_directories() {
        assert!(is_under("src/a.rs", "src"));
        assert!(is_under("src/a.rs", "src/a.rs"));
        assert!(!is_under("srcx/a.rs", "src"));
        assert!(!is_under("src", "src/a.rs"));
    }
}
