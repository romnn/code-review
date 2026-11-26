//! Synthetic snapshot commits capturing the working tree, written into a private repository.
//!
//! A capture reads the reviewed repository's working tree and index and writes nothing there:
//! it stages a copy of the index in a [`Scratch`] directory, with the private repository as the
//! only object database, after importing the objects of `HEAD` and of the index it lacks.

use std::collections::BTreeSet;

use super::scratch::Scratch;
use super::{Busy, PrivateRepo, Repo, durable};
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

impl PrivateRepo {
    /// Captures `source`'s working tree, staged and unstaged changes and untracked files that
    /// are not ignored, as a synthetic commit in this repository whose parent is `HEAD`, or as
    /// `HEAD` itself when nothing is beyond it.
    ///
    /// The capture writes nothing to `source`: it works on a copy of the index in a directory
    /// of this repository's, with this repository as its only object
    /// database, and imports the objects of `HEAD` and the index this repository lacks first.
    /// Starting from the copy keeps git's stat cache, so unchanged files are not rehashed, and
    /// keeps sparse-checkout entries that are absent from disk.
    ///
    /// No filter driver runs: the capture records files' bytes as they are on disk, except that
    /// a file with a `filter` attribute keeps its staged entry (see
    /// [`WorktreeCapture::filtered`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::GitBusy`] with [`Busy::Unmerged`] when the index has unmerged paths,
    /// [`Error::MissingObject`] when `source` lacks an object its index or `HEAD` names, an
    /// error for a bare repository, and an error when any git step fails.
    pub(crate) fn capture_worktree(&self, source: &Repo) -> Result<WorktreeCapture, Error> {
        let WorktreeTree {
            tree,
            head,
            filtered,
        } = self.worktree_tree(source)?;
        // A working tree with nothing beyond `HEAD` is `HEAD`, so a capture of committed work
        // is the commit itself and deduplicates with a revision that recorded it.
        if let Some(head) = &head
            && self.repo().tree_of(head)? == tree
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
    pub(crate) fn empty_tree_commit(&self) -> Result<ObjectId, Error> {
        let mut hash_object = self.repo().command();
        durable(&mut hash_object);
        let tree = Repo::run_command_with_input(
            hash_object,
            &["hash-object", "-w", "-t", "tree", "--stdin"],
            Vec::new(),
            &[0],
        )?;
        let tree = ObjectId::parse(String::from_utf8_lossy(&tree.stdout).trim())?;
        self.snapshot_commit(&tree, None, "code-review empty tree")
    }

    /// Writes a commit of `tree` with the fixed identity and date of snapshots, whose parent is
    /// `parent` when there is one.
    fn snapshot_commit(
        &self,
        tree: &ObjectId,
        parent: Option<&ObjectId>,
        message: &str,
    ) -> Result<ObjectId, Error> {
        let mut commit_tree = self.repo().command();
        durable(&mut commit_tree);
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
        let commit = Repo::run_command(commit_tree, args)?;
        Ok(ObjectId::parse(
            String::from_utf8_lossy(&commit.stdout).trim(),
        )?)
    }

    /// Returns the tree a [`capture_worktree`](Self::capture_worktree) of `source` would record
    /// now, writing no commit.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::capture_worktree`].
    pub(crate) fn capture_tree(&self, source: &Repo) -> Result<ObjectId, Error> {
        Ok(self.worktree_tree(source)?.tree)
    }

    /// Writes the tree a capture of `source` records, from a copy of its index updated with
    /// every change in the working tree but those of filtered paths, with every filter driver
    /// off.
    fn worktree_tree(&self, source: &Repo) -> Result<WorktreeTree, Error> {
        self.worktree_tree_with(source, &mut || Ok(()))
    }

    /// See [`Self::worktree_tree`]; `scanned` runs once the changed paths are known and before
    /// they are staged, where a test changes the working tree meanwhile.
    fn worktree_tree_with(
        &self,
        source: &Repo,
        scanned: &mut dyn FnMut() -> Result<(), Error>,
    ) -> Result<WorktreeTree, Error> {
        let scratch = Scratch::new(self, source)?;
        if scratch.unmerged() {
            return Err(Error::GitBusy(Busy::Unmerged));
        }
        let head = scratch.head().cloned();
        // Every command below reads the working tree or the copied index, and refreshing a
        // cached stat could run a clean filter on any tracked file; none can, since the
        // scratch's commands know no filter driver.
        let Changed { paths, deleted } = changed_paths(&scratch)?;
        let filtered = filtered_paths(&scratch, head.as_ref(), &paths)?;
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
            Repo::run_command_with_paths(
                scratch.command(),
                &["update-index", "--force-remove", "-z", "--stdin"],
                removed,
                &[0],
            )?;
        }
        // A path gone since the scan is removed, and one that turned from a file into a
        // directory, or back, replaces what was there.
        let added: Vec<&[u8]> = paths
            .iter()
            .filter(unfiltered)
            .filter(|path| !deleted.contains(*path))
            // An untracked nested repository is listed as a directory, `nested/`, and staged
            // as its commit.
            .map(|path| path.strip_suffix(b"/").unwrap_or(path))
            .collect();
        if !added.is_empty() {
            Repo::run_command_with_paths(
                scratch.command(),
                &[
                    "update-index",
                    "--add",
                    "--remove",
                    "--replace",
                    "-z",
                    "--stdin",
                ],
                added,
                &[0],
            )?;
        }

        // Every object the index names is here, so writing the tree checks that the tree has
        // all it reaches.
        let tree = Repo::run_command(scratch.command(), ["write-tree"])?;
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

/// Lists the paths that differ in the working tree from the index `scratch` holds: modified,
/// deleted and untracked paths that are not ignored.
///
/// A submodule counts only when its commit moved, which is all a capture records of it; git
/// never runs in its checkout to see whether it is dirty, since a filter driver configured
/// only there would run.
fn changed_paths(scratch: &Scratch) -> Result<Changed, Error> {
    let modified = Repo::run_command(
        scratch.command(),
        [
            "diff-files",
            "-z",
            "--name-status",
            "--ignore-submodules=dirty",
        ],
    )?;
    let untracked = Repo::run_command(
        scratch.command(),
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
    scratch: &Scratch,
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
        let output = Repo::run_command_with_paths(
            scratch.command(),
            &args,
            paths.iter().map(Vec::as_slice),
            &[0],
        )?;
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
        let (_data, private) = testing::private(&git)?;
        let staged = testing::git(repo.path(), &["rev-parse", "HEAD:secret"])?;

        let captured = private.worktree_tree_with(&git, &mut || {
            std::fs::write(repo.path().join("secret"), "plaintext\n")
                .map_err(|source| Error::io("write", repo.path().join("secret"), source))
        })?;
        let tree = captured.tree.as_str();
        let blob = |spec: &str| testing::git(private.path(), &["rev-parse", spec]);
        assert_eq!(blob(&format!("{tree}:secret"))?, staged);
        let committed = testing::git(repo.path(), &["rev-parse", "HEAD:a.rs"])?;
        assert_ne!(
            blob(&format!("{tree}:a.rs"))?,
            committed,
            "the edit the scan found is captured"
        );
        Ok(())
    }
}
