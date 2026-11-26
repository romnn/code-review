//! Fetches, which land in a transient private repository rather than the reviewed one.
//!
//! A pull request's commits, and the history a shallow clone lacks to find a merge base, are
//! fetched into a repository of cr's own whose commands run as the reviewed clone's do (see
//! [`Repo::clone_context`]), so it reaches remotes as the reviewed repository does.
//! Only the endpoints a review records are imported from it into the review's archive; the
//! rest of what it fetched goes with it.

use std::path::Path;

use super::{PrivateRepo, Repo};
use crate::error::Error;
use crate::ids::ObjectId;

/// A transient private repository that fetches land in, removed when dropped.
pub(crate) struct FetchRepo {
    private: PrivateRepo,
}

impl FetchRepo {
    /// Makes an empty transient repository under `root` for the reviewed repository `source`.
    pub(crate) fn new(root: &Path, source: &Repo) -> Result<Self, Error> {
        let path = root.join(crate::random::hex(12));
        Ok(Self {
            private: PrivateRepo::open(&path, source)?,
        })
    }

    /// Returns the repository for reading.
    pub(crate) fn repo(&self) -> &Repo {
        self.private.repo()
    }

    /// Fetches exactly `refspecs` from `remote`, a remote of the reviewed repository or a URL:
    /// no tags, no `FETCH_HEAD`, no submodules, and an empty `--refmap`, so only the refspecs'
    /// own destinations move.
    ///
    /// It reaches the remote over the transports the user allows, and as
    /// [`Repo::forbid_prompts`] has it.
    pub(crate) fn fetch(
        &self,
        remote: &str,
        refspecs: &[String],
        options: &[&str],
    ) -> Result<(), Error> {
        let mut args: Vec<String> = [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--refmap=",
        ]
        .into_iter()
        .chain(options.iter().copied())
        .map(str::to_owned)
        .collect();
        args.push(remote.to_owned());
        args.extend(refspecs.iter().cloned());
        let mut command = self.repo().git();
        self.repo().prepare_remote(&mut command);
        Repo::run_command(command, args)?;
        Ok(())
    }

    /// Copies the history of `tips` the shallow `source` holds, with where it stops, so that
    /// the history can be deepened here.
    pub(crate) fn seed(&self, source: &Repo, tips: &[ObjectId]) -> Result<(), Error> {
        super::private::copy_pack(source, self.repo(), &["--revs"], super::object_lines(tips))?;
        let shallow = source.common_dir().join("shallow");
        let target = self.private.path().join("shallow");
        std::fs::copy(&shallow, &target).map_err(|err| Error::io("copy", &shallow, err))?;
        // Refs make the tips known to the remote as commits this repository has.
        let refs: Vec<(String, ObjectId)> = tips
            .iter()
            .enumerate()
            .map(|(index, tip)| (format!("refs/code-review/tip-{index}"), tip.clone()))
            .collect();
        self.private.update_refs(&refs)
    }

    /// Returns every merge base of `target` and `head`, best first, deepening the history from
    /// `remote` while the repository is shallow and has none.
    ///
    /// Deepening never asks the remote for the head: a working-tree snapshot or an unpublished
    /// local commit exists only locally.
    /// It deepens the shallow boundary instead, whose commits all came from the remote, plus
    /// the target.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoMergeBase`] when the histories are unrelated or cannot be deepened far
    /// enough.
    pub(crate) fn merge_bases(
        &self,
        target: &ObjectId,
        head: &ObjectId,
        remote: &str,
    ) -> Result<Vec<ObjectId>, Error> {
        for _ in 0..8 {
            match self.repo().merge_bases(target, head) {
                Err(Error::NoMergeBase { .. }) => {}
                found => return found,
            }
            if !self.repo().is_shallow()? || !self.deepen(remote, target) {
                break;
            }
        }
        Err(Error::NoMergeBase {
            target: target.clone(),
            head: head.clone(),
        })
    }

    /// Deepens the shallow history by 200 commits from the shallow boundary and from `target`.
    ///
    /// When the remote does not know one of the wanted commits (an unpublished target, or a
    /// boundary commit force-pushed away), the commits are fetched one by one and the unknown ones
    /// are dropped.
    /// Returns whether any fetch succeeded.
    fn deepen(&self, remote: &str, target: &ObjectId) -> bool {
        let shallow_file = self.private.path().join("shallow");
        let boundary = std::fs::read_to_string(&shallow_file).unwrap_or_default();
        let mut wants: Vec<String> = vec![target.as_str().to_owned()];
        wants.extend(
            boundary
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned),
        );
        if self.fetch(remote, &wants, &["--deepen=200"]).is_ok() {
            return true;
        }
        let mut deepened = false;
        for want in &wants {
            deepened |= self
                .fetch(remote, std::slice::from_ref(want), &["--deepen=200"])
                .is_ok();
        }
        deepened
    }
}

impl Drop for FetchRepo {
    fn drop(&mut self) {
        // Every command that ran in it has exited; a repository left behind only takes space.
        if let Err(err) = std::fs::remove_dir_all(self.private.path()) {
            drop(err);
        }
    }
}
