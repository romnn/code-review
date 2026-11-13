//! Synthetic snapshot commits capturing the working tree.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::Repo;
use crate::error::Error;
use crate::ids::ObjectId;

/// The fixed identity and date of snapshot commits, so that two captures of identical content on
/// the same `HEAD` produce the same commit and deduplicate.
const SNAPSHOT_ENV: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "code-review"),
    ("GIT_AUTHOR_EMAIL", "code-review@localhost"),
    ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+0000"),
    ("GIT_COMMITTER_NAME", "code-review"),
    ("GIT_COMMITTER_EMAIL", "code-review@localhost"),
    ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+0000"),
];

/// Deletes the temporary index file (and a leftover lock) when dropped.
struct TemporaryIndex {
    path: PathBuf,
}

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        for path in [self.path.clone(), self.path.with_extension("lock")] {
            if let Err(err) = std::fs::remove_file(&path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                // A stale temporary index in the git directory is harmless.
                drop(err);
            }
        }
    }
}

/// Copies the user's index at `from` to a new, writable file at `to`, dated as the original.
///
/// Git trusts a cached stat only for files older than the index, and rereads those modified in
/// the same second as it ("racily clean").
/// A copy dated now would make such a file look older, so an edit that kept its size would be
/// missed.
/// The bytes and the date come from one open handle: git replaces the index by renaming a new
/// file over it, and a date read by path afterwards could belong to a newer index than the bytes.
fn copy_index(from: &Path, to: &Path) -> Result<(), Error> {
    let mut source = File::open(from).map_err(|source| Error::io("open", from, source))?;
    let modified = source
        .metadata()
        .and_then(|metadata| metadata.modified())
        .map_err(|source| Error::io("read", from, source))?;
    let mut copy = File::create(to).map_err(|source| Error::io("create", to, source))?;
    std::io::copy(&mut source, &mut copy).map_err(|source| Error::io("copy", from, source))?;
    copy.set_modified(modified)
        .map_err(|source| Error::io("write", to, source))
}

/// The tree a working-tree capture records, and the `HEAD` it was taken on.
struct WorktreeTree {
    tree: ObjectId,
    /// `None` when `HEAD` is unborn.
    head: Option<ObjectId>,
}

impl Repo {
    /// Captures the working tree, staged and unstaged changes and untracked files that are not
    /// ignored, as a synthetic commit whose parent is `HEAD`.
    ///
    /// The user's index is never touched: the capture works on a copy of it through
    /// `GIT_INDEX_FILE`.
    /// Starting from the copy keeps git's stat cache, so unchanged files are not rehashed, and
    /// keeps sparse-checkout entries that are absent from disk.
    ///
    /// # Errors
    ///
    /// Returns an error for a bare repository or when any git step fails.
    pub fn capture_worktree(&self) -> Result<ObjectId, Error> {
        let WorktreeTree { tree, head } = self.worktree_tree()?;
        let mut commit_tree = self.command();
        commit_tree.envs(SNAPSHOT_ENV);
        let mut args = vec![
            "-c".to_owned(),
            "commit.gpgSign=false".to_owned(),
            "commit-tree".to_owned(),
            tree.as_str().to_owned(),
            "-m".to_owned(),
            "code-review working tree snapshot".to_owned(),
        ];
        if let Some(head) = &head {
            args.extend(["-p".to_owned(), head.as_str().to_owned()]);
        }
        let commit = Self::run_command(commit_tree, args)?;
        Ok(ObjectId::parse(
            String::from_utf8_lossy(&commit.stdout).trim(),
        )?)
    }

    /// Returns whether [`capture_worktree`](Self::capture_worktree) would record anything beyond
    /// `HEAD`: staged or unstaged changes, or untracked files that are not ignored.
    ///
    /// It compares the tree a capture records with `HEAD`'s, so it agrees with the capture by
    /// construction, whatever git's status settings say; changes inside a submodule's checkout,
    /// for example, do not count, because a capture records only the submodule's commit.
    /// Like a capture, it leaves the user's index untouched.
    ///
    /// # Errors
    ///
    /// Returns an error for a bare repository or when any git step fails.
    pub fn has_uncommitted_changes(&self) -> Result<bool, Error> {
        let WorktreeTree { tree, head } = self.worktree_tree()?;
        let committed = match head {
            Some(head) => self.run_text(["rev-parse", &format!("{}^{{tree}}", head.as_str())])?,
            // An unborn `HEAD` has the empty tree, in the repository's hash algorithm.
            None => self.run_text(["hash-object", "-t", "tree", "--stdin"])?,
        };
        Ok(tree.as_str() != committed)
    }

    /// Writes the tree a capture records, from a copy of the user's index updated with every
    /// change in the working tree.
    fn worktree_tree(&self) -> Result<WorktreeTree, Error> {
        if self.work_tree().is_none() {
            return Err(Error::InvalidTarget(
                "--worktree in a bare repository".to_owned(),
            ));
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let index = TemporaryIndex {
            path: self
                .git_dir()
                .join(format!("code-review-index-{}-{nanos}", std::process::id())),
        };
        let head = self.resolve_commit("HEAD").ok();
        let user_index = self.git_dir().join("index");
        if user_index.exists() {
            copy_index(&user_index, &index.path)?;
        } else {
            let mut read_tree = self.command();
            read_tree.env("GIT_INDEX_FILE", &index.path);
            match &head {
                Some(head) => Self::run_command(read_tree, ["read-tree", head.as_str()])?,
                None => Self::run_command(read_tree, ["read-tree", "--empty"])?,
            };
        }

        let mut add = self.command();
        add.env("GIT_INDEX_FILE", &index.path);
        Self::run_command(add, ["add", "--all", "--", ":/"])?;

        let mut write_tree = self.command();
        write_tree.env("GIT_INDEX_FILE", &index.path);
        let tree = Self::run_command(write_tree, ["write-tree"])?;
        let tree = ObjectId::parse(String::from_utf8_lossy(&tree.stdout).trim())?;
        Ok(WorktreeTree { tree, head })
    }
}
