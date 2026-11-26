//! What git is doing in a working tree: its `HEAD`, whether an operation is under way there, and
//! when its index last changed, which a capture reads before and after itself so that nothing
//! moved under it unnoticed.

use std::fmt;
use std::time::SystemTime;

use serde::Serialize;

use super::Repo;
use crate::error::Error;
use crate::ids::ObjectId;

/// What `HEAD` is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Head {
    /// The checked-out branch's name, such as `feat/x`, or `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// The commit `HEAD` is at, or `None` when its branch is unborn.
    pub commit: Option<ObjectId>,
}

/// What decides which pull request a branch has: the branch, where it pulls from and pushes
/// to, and the commit its upstream is at, which a push moves.
///
/// A pull request belongs to the remote branch a local one tracks, which may have another
/// name, such as `topic` for a local `review-topic`, or be a pull request's own ref, as
/// `gh pr checkout` sets up; so two branches are the same only when all of this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchIdentity {
    /// The local branch, such as `feat/x`.
    pub branch: String,
    /// `branch.<name>.remote`: the remote it pulls from, if set.
    pub remote: Option<String>,
    /// `branch.<name>.merge`: the ref it pulls, such as `refs/heads/topic` or
    /// `refs/pull/7/head`, if set.
    pub merge: Option<String>,
    /// The remote it pushes to, when one is set apart from where it pulls from:
    /// `branch.<name>.pushRemote`, or else `remote.pushDefault`.
    pub push_remote: Option<String>,
    /// The commit its upstream is at, or else the default remote's branch of the same name, if
    /// either exists.
    pub upstream: Option<ObjectId>,
}

impl BranchIdentity {
    /// Whether `other` pulls from and pushes to the same as this branch, so it has the same
    /// pull request: equal but for the upstream's commit, which every push and fetch moves,
    /// and for the local name while the branch pulls a ref of its own, so a branch renamed
    /// locally keeps its pull request, and two that pull the same ref share it.
    #[must_use]
    pub fn tracks_the_same(&self, other: &Self) -> bool {
        (self.merge.is_some() || self.branch == other.branch)
            && self.remote == other.remote
            && self.merge == other.merge
            && self.push_remote == other.push_remote
    }
}

/// An operation git is in the middle of in a working tree, during which a capture would record
/// a state nobody meant to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Busy {
    /// Another git command holds `index.lock`.
    IndexLocked,
    /// A merge stopped before committing.
    Merge,
    /// A cherry-pick stopped before committing.
    CherryPick,
    /// A revert stopped before committing.
    Revert,
    /// A rebase is under way.
    Rebase,
    /// A bisect is under way.
    Bisect,
    /// The index has unmerged paths, as after a stash that did not apply cleanly.
    Unmerged,
}

impl fmt::Display for Busy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IndexLocked => "another git command is running",
            Self::Merge => "merge in progress",
            Self::CherryPick => "cherry-pick in progress",
            Self::Revert => "revert in progress",
            Self::Rebase => "rebase in progress",
            Self::Bisect => "bisect in progress",
            Self::Unmerged => "unmerged paths",
        })
    }
}

/// The files and directories in a worktree's git directory whose presence means git is busy.
const BUSY_MARKERS: [(&str, Busy); 7] = [
    ("index.lock", Busy::IndexLocked),
    ("rebase-merge", Busy::Rebase),
    ("rebase-apply", Busy::Rebase),
    ("MERGE_HEAD", Busy::Merge),
    ("CHERRY_PICK_HEAD", Busy::CherryPick),
    ("REVERT_HEAD", Busy::Revert),
    ("BISECT_LOG", Busy::Bisect),
];

/// The real index's modification time and size, which every command that writes the index
/// changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexStamp {
    modified: SystemTime,
    len: u64,
}

impl Repo {
    /// Reads what `HEAD` is: the branch it names, if any, and its commit, if it has one.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run or prints output the engine cannot parse.
    pub fn head(&self) -> Result<Head, Error> {
        let branch = match self.run_text(["symbolic-ref", "--quiet", "HEAD"]) {
            Ok(name) => Some(
                name.strip_prefix("refs/heads/")
                    .map_or_else(|| name.clone(), str::to_owned),
            ),
            // Exit status 1: `HEAD` is detached.
            Err(Error::CommandFailed { .. }) => None,
            Err(err) => return Err(err),
        };
        Ok(Head {
            branch,
            commit: self.ref_target("HEAD")?,
        })
    }

    /// Reads what decides which pull request `branch` has; see [`BranchIdentity`].
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run or prints output the engine cannot parse.
    pub fn branch_identity(&self, branch: &str) -> Result<BranchIdentity, Error> {
        // `git config --get-regexp` exits with 1 when nothing matches.
        let output = Self::run_command_with_paths(
            self.command(),
            &[
                "config",
                "-z",
                "--get-regexp",
                r"^(branch\..*\.(remote|merge|pushremote)|remote\.pushdefault)$",
            ],
            [],
            &[0, 1],
        )?;
        // Records of `key NL value NUL`, keys with the section and the variable in lowercase
        // and the branch as it is; the last value of a key wins, as in git.
        let value = |key: &str| {
            output
                .stdout
                .split(|&byte| byte == 0)
                .filter_map(|record| {
                    let text = std::str::from_utf8(record).ok()?;
                    let (name, value) = text.split_once('\n')?;
                    (name == key).then(|| value.to_owned())
                })
                .next_back()
        };
        let upstream = match self.ref_target(&format!("refs/heads/{branch}@{{upstream}}"))? {
            Some(upstream) => Some(upstream),
            None => match self.default_remote()? {
                Some(remote) => self.ref_target(&format!("refs/remotes/{remote}/{branch}"))?,
                None => None,
            },
        };
        Ok(BranchIdentity {
            branch: branch.to_owned(),
            remote: value(&format!("branch.{branch}.remote")),
            merge: value(&format!("branch.{branch}.merge")),
            push_remote: value(&format!("branch.{branch}.pushremote"))
                .or_else(|| value("remote.pushdefault")),
            upstream,
        })
    }

    /// Returns the operation git is in the middle of in this worktree, or `None` when it is
    /// idle: a lock on the index, a merge, cherry-pick or revert that stopped, or a rebase or
    /// bisect under way.
    ///
    /// Unmerged paths in the index are found by the capture that reads a copy of it, which
    /// fails with [`Error::GitBusy`] and [`Busy::Unmerged`]; reading the index here would
    /// date a split index's shared file anew.
    ///
    /// # Errors
    ///
    /// Returns an error when the git directory cannot be read.
    pub fn busy(&self) -> Result<Option<Busy>, Error> {
        for (marker, busy) in BUSY_MARKERS {
            let path = self.git_dir().join(marker);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => return Ok(Some(busy)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::io("read", path, source)),
            }
        }
        Ok(None)
    }

    /// Returns the real index's stamp, or `None` when there is no index.
    pub(crate) fn index_stamp(&self) -> Result<Option<IndexStamp>, Error> {
        let path = self.git_dir().join("index");
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::io("read", path, source)),
        };
        let modified = metadata
            .modified()
            .map_err(|source| Error::io("read", &path, source))?;
        Ok(Some(IndexStamp {
            modified,
            len: metadata.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A branch named `branch` that pulls `merge` from `origin`.
    fn tracking(branch: &str, merge: Option<&str>) -> BranchIdentity {
        BranchIdentity {
            branch: branch.to_owned(),
            remote: merge.map(|_| "origin".to_owned()),
            merge: merge.map(str::to_owned),
            push_remote: None,
            upstream: None,
        }
    }

    /// The local name counts only for a branch that pulls nothing: one renamed while it pulls
    /// the same ref tracks the same, and so do two that pull it.
    #[test]
    fn the_local_name_counts_only_without_a_merge_ref() {
        let topic = tracking("topic", Some("refs/heads/topic"));
        assert!(topic.tracks_the_same(&tracking("renamed", Some("refs/heads/topic"))));
        assert!(!topic.tracks_the_same(&tracking("topic", Some("refs/heads/other"))));
        assert!(!tracking("a", None).tracks_the_same(&tracking("b", None)));
        assert!(tracking("a", None).tracks_the_same(&tracking("a", None)));
    }
}
