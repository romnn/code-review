//! Synthetic snapshot commits capturing the working tree.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    /// See [`WorktreeCapture::filtered`].
    filtered: Vec<String>,
}

/// A capture of the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCapture {
    /// The snapshot: a synthetic commit whose parent is `HEAD`, or `HEAD` itself when nothing is
    /// beyond it.
    pub commit: ObjectId,
    /// The paths, sorted, that differ in the working tree from their entry in the index but whose
    /// `filter` attribute is set, so the capture kept their staged entry rather than their bytes
    /// on disk; they are captured once staged.
    ///
    /// A path counts as filtered when the attribute is set in the working tree's, the index's or
    /// `HEAD`'s attributes.
    /// A path whose file only looks changed to git, such as one touched without being edited,
    /// may be listed too.
    pub filtered: Vec<String>,
}

impl Repo {
    /// Captures the working tree, staged and unstaged changes and untracked files that are not
    /// ignored, as a synthetic commit whose parent is `HEAD`, or as `HEAD` itself when nothing
    /// is beyond it.
    ///
    /// The user's index is never touched: the capture works on a copy of it through
    /// `GIT_INDEX_FILE`.
    /// Starting from the copy keeps git's stat cache, so unchanged files are not rehashed, and
    /// keeps sparse-checkout entries that are absent from disk.
    ///
    /// No filter driver runs: the capture records files' bytes as they are on disk, except that
    /// a file with a `filter` attribute keeps its staged entry (see
    /// [`WorktreeCapture::filtered`]).
    ///
    /// # Errors
    ///
    /// Returns an error for a bare repository or when any git step fails.
    pub fn capture_worktree(&self) -> Result<WorktreeCapture, Error> {
        let WorktreeTree {
            tree,
            head,
            filtered,
        } = self.worktree_tree()?;
        // A working tree with nothing beyond `HEAD` is `HEAD`, so a capture of committed work
        // is the commit itself and deduplicates with a revision that recorded it.
        if let Some(head) = &head
            && self.run_text(["rev-parse", &format!("{}^{{tree}}", head.as_str())])?
                == tree.as_str()
        {
            return Ok(WorktreeCapture {
                commit: head.clone(),
                filtered,
            });
        }
        Ok(WorktreeCapture {
            commit: self.snapshot_commit(
                &tree,
                head.as_ref(),
                "code-review working tree snapshot",
            )?,
            filtered,
        })
    }

    /// Returns the commit of the empty tree that a review of a working tree with nothing to
    /// compare it with uses as its base: a commit without parents, of the fixed identity and
    /// date, so it is the same commit every time.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot write the tree or the commit.
    pub fn empty_tree_commit(&self) -> Result<ObjectId, Error> {
        let tree = Self::run_command_with_input(
            self.command(),
            &["hash-object", "-w", "-t", "tree", "--stdin"],
            Vec::new(),
            &[0],
        )?;
        let tree = ObjectId::parse(String::from_utf8_lossy(&tree.stdout).trim())?;
        self.snapshot_commit(&tree, None, "code-review empty tree")
    }

    /// Returns the tree of `commit`.
    ///
    /// # Errors
    ///
    /// Returns an error when `commit` is not a commit in the repository.
    pub fn tree_of(&self, commit: &ObjectId) -> Result<ObjectId, Error> {
        let text = self.run_text([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{commit}^{{tree}}"),
        ])?;
        Ok(ObjectId::parse(&text)?)
    }

    /// Writes the files of `tree` into the directory `dir`, which must be empty or absent, as
    /// they are stored: no filter driver runs on them. `index` is a path for a temporary index,
    /// which is deleted afterwards.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot read the tree or write its files.
    pub(crate) fn write_tree_files(
        &self,
        tree: &ObjectId,
        dir: &Path,
        index: &Path,
    ) -> Result<(), Error> {
        let index = TemporaryIndex {
            path: index.to_path_buf(),
        };
        let filters = self.filters_off()?;
        let command = || {
            let mut command = filters.command(self);
            command
                .env("GIT_INDEX_FILE", &index.path)
                .env("GIT_WORK_TREE", dir);
            command
        };
        Self::run_command(command(), ["read-tree", tree.as_str()])?;
        Self::run_command(command(), ["checkout-index", "--all", "--force"])?;
        Ok(())
    }

    /// Writes a commit of `tree` with the fixed identity and date of snapshots, whose parent is
    /// `parent` when there is one.
    fn snapshot_commit(
        &self,
        tree: &ObjectId,
        parent: Option<&ObjectId>,
        message: &str,
    ) -> Result<ObjectId, Error> {
        let mut commit_tree = self.command();
        commit_tree.envs(SNAPSHOT_ENV);
        let mut args = vec![
            "-c".to_owned(),
            "commit.gpgSign=false".to_owned(),
            "commit-tree".to_owned(),
            tree.as_str().to_owned(),
            "-m".to_owned(),
            message.to_owned(),
        ];
        if let Some(parent) = parent {
            args.extend(["-p".to_owned(), parent.as_str().to_owned()]);
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
        let WorktreeTree { tree, head, .. } = self.worktree_tree()?;
        let committed = match head {
            Some(head) => self.run_text(["rev-parse", &format!("{}^{{tree}}", head.as_str())])?,
            // An unborn `HEAD` has the empty tree, in the repository's hash algorithm.
            None => self.run_text(["hash-object", "-t", "tree", "--stdin"])?,
        };
        Ok(tree.as_str() != committed)
    }

    /// Writes the tree a capture records, from a copy of the user's index updated with every
    /// change in the working tree but those of filtered paths, with every filter driver off.
    fn worktree_tree(&self) -> Result<WorktreeTree, Error> {
        self.worktree_tree_with(&mut || Ok(()))
    }

    /// See [`Self::worktree_tree`]; `scanned` runs once the changed paths are known and before
    /// they are staged, where a test changes the working tree meanwhile.
    fn worktree_tree_with(
        &self,
        scanned: &mut dyn FnMut() -> Result<(), Error>,
    ) -> Result<WorktreeTree, Error> {
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
        let filters = self.filters_off()?;
        // Every command below reads the working tree or the copied index, and refreshing a
        // cached stat could run a clean filter on any tracked file, so all run without filters.
        let command = || {
            let mut command = filters.command(self);
            command.env("GIT_INDEX_FILE", &index.path);
            command
        };
        let user_index = self.git_dir().join("index");
        if user_index.exists() {
            copy_index(&user_index, &index.path)?;
        } else {
            match &head {
                Some(head) => Self::run_command(command(), ["read-tree", head.as_str()])?,
                None => Self::run_command(command(), ["read-tree", "--empty"])?,
            };
        }

        let Changed { paths, deleted } = changed_paths(&command)?;
        let filtered = filtered_paths(&command, head.as_ref(), &paths)?;
        scanned()?;
        // Only the paths the scan found changed and unfiltered are staged, by name: a path that
        // changed since, maybe one under a filter driver, waits for the next capture rather
        // than going in as bytes no one classified.
        let unfiltered = |path: &&Vec<u8>| !filtered.contains(*path);
        // Deletions go first, by name alone: git reads nothing of a path it removes, so one
        // beyond a symbolic link that replaced its directory goes too, which `--remove` refuses.
        let removed: Vec<&[u8]> = deleted
            .iter()
            .filter(unfiltered)
            .map(Vec::as_slice)
            .collect();
        if !removed.is_empty() {
            Self::run_command_with_paths(
                command(),
                &["update-index", "--force-remove", "-z", "--stdin"],
                removed,
                &[0],
            )?;
        }
        // A path gone since the scan is removed, and one that turned from a file into a
        // directory, or back, replaces what was there.
        let staged: Vec<&[u8]> = paths
            .iter()
            .filter(unfiltered)
            .filter(|path| !deleted.contains(*path))
            // An untracked nested repository is listed as a directory, `nested/`, and staged
            // as its commit.
            .map(|path| path.strip_suffix(b"/").unwrap_or(path))
            .collect();
        if !staged.is_empty() {
            Self::run_command_with_paths(
                command(),
                &[
                    "update-index",
                    "--add",
                    "--remove",
                    "--replace",
                    "-z",
                    "--stdin",
                ],
                staged,
                &[0],
            )?;
        }

        let tree = Self::run_command(command(), ["write-tree"])?;
        let tree = ObjectId::parse(String::from_utf8_lossy(&tree.stdout).trim())?;
        Ok(WorktreeTree {
            tree,
            head,
            filtered: filtered
                .iter()
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .collect(),
        })
    }
}

/// Lists the paths that differ in the working tree from the index `command` uses: modified,
/// deleted and untracked paths that are not ignored.
///
/// A submodule counts only when its commit moved, which is all a capture records of it; git
/// never runs in its checkout to see whether it is dirty, since a filter driver configured
/// only there would run.
fn changed_paths(command: &dyn Fn() -> Command) -> Result<Changed, Error> {
    let modified = Repo::run_command(
        command(),
        [
            "diff-files",
            "-z",
            "--name-status",
            "--ignore-submodules=dirty",
        ],
    )?;
    let untracked = Repo::run_command(
        command(),
        [
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--full-name",
            ":/",
        ],
    )?;
    // `diff-files -z --name-status` prints a status, then its path, each ending in NUL.
    let fields: Vec<&[u8]> = modified
        .stdout
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let (records, _) = fields.as_chunks::<2>();
    let mut deleted = BTreeSet::new();
    let mut paths: Vec<Vec<u8>> = Vec::new();
    for [status, path] in records {
        if status.starts_with(b"D") {
            deleted.insert(path.to_vec());
        }
        paths.push(path.to_vec());
    }
    paths.extend(
        untracked
            .stdout
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(<[u8]>::to_vec),
    );
    paths.sort();
    paths.dedup();
    Ok(Changed { paths, deleted })
}

/// The paths that differ in the working tree from the index.
struct Changed {
    /// Every one, sorted.
    paths: Vec<Vec<u8>>,
    /// Those git finds gone from the working tree, deleted or beyond a symbolic link.
    deleted: BTreeSet<Vec<u8>>,
}

/// Returns those of `paths` whose `filter` attribute is set in the working tree's, the
/// index's or `HEAD`'s attributes, sorted.
fn filtered_paths(
    command: &dyn Fn() -> Command,
    head: Option<&ObjectId>,
    paths: &[Vec<u8>],
) -> Result<BTreeSet<Vec<u8>>, Error> {
    let mut filtered = BTreeSet::new();
    if paths.is_empty() {
        return Ok(filtered);
    }
    let mut sources: Vec<Vec<&str>> = vec![Vec::new(), vec!["--cached"]];
    if let Some(head) = head {
        sources.push(vec!["--source", head.as_str()]);
    }
    for source in sources {
        let args: Vec<&str> = ["check-attr", "-z", "--stdin"]
            .into_iter()
            .chain(source)
            .chain(["filter"])
            .collect();
        let output =
            Repo::run_command_with_paths(command(), &args, paths.iter().map(Vec::as_slice), &[0])?;
        let fields: Vec<&[u8]> = output.stdout.split(|&byte| byte == 0).collect();
        let (records, _) = fields.as_chunks::<3>();
        for [path, _, value] in records {
            if !matches!(*value, b"unspecified" | b"unset") {
                filtered.insert(path.to_vec());
            }
        }
    }
    Ok(filtered)
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::*;
    use crate::testing;

    /// A file under a filter driver that the scan found clean and that changes before the
    /// capture stages what changed keeps its staged entry: its bytes on disk, which no filter
    /// cleaned, never go in.
    #[test]
    fn a_filtered_file_changed_after_the_scan_is_not_captured() -> eyre::Result<()> {
        let repo = testing::committed(&[
            (".gitattributes", "secret filter=crypt\n"),
            ("secret", "ciphertext\n"),
            ("a.rs", "fn a() {}\n"),
        ])?;
        testing::git(repo.path(), &["config", "filter.crypt.clean", "cat"])?;
        std::fs::write(repo.path().join("a.rs"), "fn a() { edited(); }\n")?;
        let git = Repo::discover(repo.path())?;
        let staged = testing::git(repo.path(), &["rev-parse", "HEAD:secret"])?;

        let captured = git.worktree_tree_with(&mut || {
            std::fs::write(repo.path().join("secret"), "plaintext\n")
                .map_err(|source| Error::io("write", repo.path().join("secret"), source))
        })?;
        let tree = captured.tree.as_str();
        expect_blob(repo.path(), &format!("{tree}:secret"), &staged)?;
        let edited = testing::git(repo.path(), &["rev-parse", &format!("{tree}:a.rs")])?;
        let committed = testing::git(repo.path(), &["rev-parse", "HEAD:a.rs"])?;
        assert_ne!(edited, committed, "the edit the scan found is captured");
        Ok(())
    }

    /// Asserts that `spec` names the blob `expected`.
    fn expect_blob(dir: &Path, spec: &str, expected: &str) -> eyre::Result<()> {
        assert_eq!(testing::git(dir, &["rev-parse", spec])?, expected, "{spec}");
        Ok(())
    }
}
