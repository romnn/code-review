//! The review of a checkout: whatever its working tree holds, on whatever branch, against the
//! base its subject resolves to on every capture.
//!
//! A checkout's review is one per working tree and clone. What a capture compares with is
//! observed with it, never stored as the review's target: the branch checked out, its `HEAD`,
//! its pull request when the caller found one, and the base that follows from them. A capture
//! reads all of it, and git's state, before and after itself, and is discarded when anything
//! moved in between.

use std::path::Path;

use serde::Serialize;

use super::github::{self, BranchPullRequest, CheckoutPullRequest, PullRequestState};
use super::{Review, ReviewTarget, RevisionInput, Session, clone};
use crate::error::Error;
use crate::git::{Busy, Head, IndexStamp, Repo};
use crate::ids::ObjectId;

/// What a capture of a working tree was taken of, and compared with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Subject {
    /// The branch checked out, or `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// The commit `HEAD` is at, or `None` when its branch is unborn.
    pub head: Option<ObjectId>,
    /// What the base was resolved from.
    pub base: BaseRef,
    /// The branch's pull request, when the caller found one for this branch, whatever its
    /// state; only an open one decides the base.
    pub pull_request: Option<BranchPullRequest>,
    /// The base the open pull request names, such as `origin/develop`, when it is not
    /// available locally, so the base is the default branch instead.
    pub missing_base: Option<String>,
}

impl Subject {
    /// Describes what was captured against what, as a one-line header: `feat/x → origin/main`,
    /// `#12 feat/x → origin/main` while its pull request is open, `detached 1a2b3c4d →
    /// origin/main`, or `main → empty tree` before the first commit.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut head = match (&self.branch, &self.head) {
            (Some(branch), Some(_)) => branch.clone(),
            (Some(branch), None) => format!("{branch} (no commits yet)"),
            (None, Some(commit)) => format!("detached {}", commit.short()),
            (None, None) => "detached".to_owned(),
        };
        if let Some(pull_request) = &self.pull_request {
            head = match pull_request.state {
                PullRequestState::Open => format!("#{} {head}", pull_request.number),
                PullRequestState::Merged => format!("{head} (#{} merged)", pull_request.number),
                PullRequestState::Closed => format!("{head} (#{} closed)", pull_request.number),
            };
        }
        let base = match &self.base {
            BaseRef::Empty {
                unrelated: Some(name),
            } => format!("empty tree (no history shared with {name})"),
            BaseRef::Empty { unrelated: None } => "empty tree".to_owned(),
            BaseRef::Shallow { name } => {
                format!("empty tree (history is shallow; fetch more to compare with {name})")
            }
            base => base.name().unwrap_or_default().to_owned(),
        };
        match &self.missing_base {
            Some(missing) => {
                format!("{head} → {base} (pull request base {missing} not available locally)")
            }
            None => format!("{head} → {base}"),
        }
    }
}

impl Subject {
    /// Says why a capture of this subject has nothing to review, given the revision's base and
    /// head snapshots: `main is level with origin/main. Nothing to review; changes appear here
    /// as you make them.`, or that it is behind its base, has no commits yet, or is the local
    /// default branch that a commit lands on.
    #[must_use]
    pub fn nothing_to_review(&self, snapshot_base: &ObjectId, snapshot_head: &ObjectId) -> String {
        const LATER: &str = "changes appear here as you make them";
        let name = self.branch.as_deref().unwrap_or("HEAD");
        match &self.base {
            BaseRef::LocalDefault { name: default, .. }
                if self.branch.as_ref() == Some(default) =>
            {
                format!(
                    "Nothing to review: {name} is the default branch and there is no remote, so \
                     committing to it lands the work; {LATER}."
                )
            }
            _ if self.head.is_none() => {
                format!("Nothing to review: {name} has no commits yet; {LATER}.")
            }
            base => match (base.name(), base.commit()) {
                (Some(base_name), Some(commit)) if self.head.as_ref() == Some(commit) => {
                    format!("{name} is level with {base_name}. Nothing to review; {LATER}.")
                }
                // The merge base is the capture itself: everything the branch has, the base
                // has.
                (Some(base_name), Some(_)) if snapshot_base == snapshot_head => {
                    format!("{name} is behind {base_name}. Nothing to review; {LATER}.")
                }
                _ => format!("Nothing to review; {LATER}."),
            },
        }
    }
}

/// Says that `count` changed files under a filter driver, such as LFS, were kept at their
/// staged version rather than captured, or `None` when there are none.
#[must_use]
pub fn filtered_note(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 filtered file (such as LFS) not captured until staged".to_owned()),
        count => Some(format!(
            "{count} filtered files (such as LFS) not captured until staged"
        )),
    }
}

/// What a capture's base was resolved from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BaseRef {
    /// The open pull request's base branch on the remote of its base repository.
    PullRequest {
        /// The remote-tracking branch, such as `origin/develop`.
        name: String,
        /// The commit it is at.
        commit: ObjectId,
    },
    /// The remote's default branch.
    RemoteDefault {
        /// The remote-tracking branch, such as `origin/main`.
        name: String,
        /// The commit it is at.
        commit: ObjectId,
    },
    /// The local default branch, when no remote has one; committing to it lands the work.
    LocalDefault {
        /// The branch, such as `main`.
        name: String,
        /// The commit it is at.
        commit: ObjectId,
    },
    /// The ref a review of the working tree against a fixed ref names.
    Given {
        /// The ref as given.
        name: String,
        /// The commit it resolves to.
        commit: ObjectId,
    },
    /// The empty tree: `HEAD` is unborn, there is no default branch, or `HEAD` shares no
    /// history with the branch named.
    Empty {
        /// The branch `HEAD` shares no history with, if that is why.
        unrelated: Option<String>,
    },
    /// The empty tree, because the clone is shallow and its history does not reach a commit
    /// `HEAD` shares with the branch named; fetching more history finds one.
    ///
    /// A capture never fetches, so the comparison waits for the reviewer to.
    Shallow {
        /// The branch, such as `origin/main`.
        name: String,
    },
}

impl BaseRef {
    /// The name of the branch or ref, or `None` for the empty tree.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::PullRequest { name, .. }
            | Self::RemoteDefault { name, .. }
            | Self::LocalDefault { name, .. }
            | Self::Given { name, .. } => Some(name),
            Self::Empty { .. } | Self::Shallow { .. } => None,
        }
    }

    /// The commit the base was resolved to, or `None` for the empty tree.
    #[must_use]
    pub fn commit(&self) -> Option<&ObjectId> {
        match self {
            Self::PullRequest { commit, .. }
            | Self::RemoteDefault { commit, .. }
            | Self::LocalDefault { commit, .. }
            | Self::Given { commit, .. } => Some(commit),
            Self::Empty { .. } | Self::Shallow { .. } => None,
        }
    }
}

/// What a capture of the working tree came to.
pub(super) enum Observed {
    /// The capture, with nothing moved under it.
    Captured(Box<Captured>),
    /// Git is in the middle of an operation in the working tree.
    Busy(Busy),
    /// `HEAD`, the base or the index changed while the capture was taken, or two looks
    /// differed.
    Moved,
    /// The checkout is another clone now (see [`Review::verify_clone`]).
    Replaced,
}

/// How many times a capture looks at the working tree.
#[derive(Debug, Clone, Copy)]
pub(super) enum Looks {
    /// Once, for a caller that watches the working tree.
    Once,
    /// Twice, keeping the capture only when both looks saw the same.
    Twice,
}

/// A capture of the working tree and what it was taken of.
pub(super) struct Captured {
    /// The revision to show or record, whose snapshots [`Review::capture`] pinned.
    pub input: RevisionInput,
    /// What the capture was taken of and compared with.
    pub subject: Subject,
    /// See [`crate::WorktreeCapture::filtered`].
    pub filtered: Vec<String>,
}

/// What a capture reads before and after itself.
#[derive(PartialEq, Eq)]
struct Observation {
    subject: Subject,
    index: Option<IndexStamp>,
}

impl Review {
    /// Opens the review of the checkout `repo` is, creating its storage on first use: whatever
    /// its working tree holds, on whatever branch, keyed by its working tree and its clone (see
    /// [`ReviewTarget::Checkout`]).
    ///
    /// The clone's identity is kept in `data_dir`, bound to its common git directory; every
    /// worktree of the clone shares it, and a clone made anew at the same path gets another,
    /// and so another review, leaving the old one stored under its key.
    /// The first open after an earlier version kept the identity in the clone itself keeps it,
    /// when a review of the clone is stored.
    ///
    /// Opening records nothing, and a checkout with nothing to review opens all the same.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTarget`] for a bare repository, which has no working tree, and
    /// an error when the clone's identity cannot be read or bound, or the review cannot be
    /// opened, as [`Self::open`] says.
    #[tracing::instrument(skip_all)]
    pub fn open_checkout(data_dir: &Path, repo: Repo) -> Result<Self, Error> {
        if repo.work_tree().is_none() {
            return Err(Error::InvalidTarget(
                "a bare repository has no checkout to review".to_owned(),
            ));
        }
        let data_dir =
            std::path::absolute(data_dir).map_err(|err| Error::io("resolve", data_dir, err))?;
        let clone = clone::ensure_clone_id(&data_dir, &repo)?;
        Self::open(&data_dir, repo, ReviewTarget::Checkout { clone })
    }

    /// Returns whether the repository is still the clone the review belongs to, which only a
    /// review of a checkout can stop being: another clone made at the same path has another
    /// identity, and the review's history is not in it.
    ///
    /// # Errors
    ///
    /// Returns an error when the clone's common git directory or its binding cannot be read.
    pub fn verify_clone(&self) -> Result<bool, Error> {
        match &self.meta.target {
            ReviewTarget::Checkout { clone } => {
                Ok(clone::clone_id(self.dir.data_dir(), &self.repo)?.as_ref() == Some(clone))
            }
            _ => Ok(true),
        }
    }

    /// Reads what a capture of the working tree would be taken of now and compared with.
    ///
    /// `pull_requests` are what the caller last found for the branches it looked up (see
    /// [`github::for_checkout`]); the branch's pull request is the one found for the branch
    /// checked out now, only while it pulls from and pushes to where it did then (see
    /// [`crate::BranchIdentity::tracks_the_same`]), and the others do not count.
    /// For a review of a checkout, the base is, in order:
    ///
    /// 1. while the branch's pull request is open, its base branch on the remote whose URL is
    ///    its base repository; when that is not available locally, [`Subject::missing_base`]
    ///    says so and the next rule applies;
    /// 2. the remote's default branch, as [`Repo::default_branch_ref`] finds it;
    /// 3. the local default branch;
    /// 4. the empty tree, when `HEAD` is unborn or there is no default branch.
    ///
    /// A review of the working tree against a fixed ref compares with that ref.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTarget`] for a review that is not of a working tree, and an
    /// error when git fails or a fixed ref does not resolve.
    pub fn subject(&self, pull_requests: &[CheckoutPullRequest]) -> Result<Subject, Error> {
        let Head { branch, commit } = self.repo.head()?;
        let identity = match &branch {
            Some(branch) if !pull_requests.is_empty() => Some(self.repo.branch_identity(branch)?),
            _ => None,
        };
        let pull_request = pull_requests
            .iter()
            .find(|found| {
                identity
                    .as_ref()
                    .is_some_and(|identity| identity.tracks_the_same(&found.branch))
            })
            .and_then(|found| found.pull_request.as_ref());
        let mut missing_base = None;
        let base = match &self.meta.target {
            ReviewTarget::Worktree { base } => BaseRef::Given {
                name: base.clone(),
                commit: self.repo.resolve_commit(base)?,
            },
            ReviewTarget::Checkout { .. } if commit.is_none() => BaseRef::Empty { unrelated: None },
            ReviewTarget::Checkout { .. } => {
                let open = pull_request
                    .filter(|pull_request| pull_request.state == PullRequestState::Open);
                let from_pull_request = match open {
                    Some(open) => match self.pull_request_base(open)? {
                        Ok(base) => Some(base),
                        Err(missing) => {
                            missing_base = Some(missing);
                            None
                        }
                    },
                    None => None,
                };
                match from_pull_request {
                    Some(base) => base,
                    None => self.default_base()?,
                }
            }
            ReviewTarget::PullRequest { .. } | ReviewTarget::Range { .. } => {
                return Err(Error::InvalidTarget(
                    "only a review of a working tree has a subject".to_owned(),
                ));
            }
        };
        Ok(Subject {
            branch,
            head: commit,
            base,
            pull_request: pull_request.cloned(),
            missing_base,
        })
    }

    /// The open `pull_request`'s base branch on the remote of its base repository, or the name
    /// it would have when it is not available locally.
    fn pull_request_base(
        &self,
        pull_request: &BranchPullRequest,
    ) -> Result<Result<BaseRef, String>, Error> {
        let repository = &pull_request.base_repository;
        let branch = &pull_request.base_branch;
        let Some(remote) = github::matching_remote(
            &self.repo,
            &repository.host,
            &repository.owner,
            &repository.name,
        )?
        else {
            return Ok(Err(format!(
                "{}/{}:{branch}",
                repository.owner, repository.name
            )));
        };
        let name = format!("{remote}/{branch}");
        match self
            .repo
            .ref_target(&format!("refs/remotes/{name}^{{commit}}"))?
        {
            Some(commit) => Ok(Ok(BaseRef::PullRequest { name, commit })),
            None => Ok(Err(name)),
        }
    }

    /// The default branch, the remote's before the local one, or the empty tree without one.
    fn default_base(&self) -> Result<BaseRef, Error> {
        let Some(full) = self.repo.default_branch_ref()? else {
            return Ok(BaseRef::Empty { unrelated: None });
        };
        let commit = self.repo.resolve_commit(&full)?;
        Ok(match full.strip_prefix("refs/remotes/") {
            Some(name) => BaseRef::RemoteDefault {
                name: name.to_owned(),
                commit,
            },
            None => BaseRef::LocalDefault {
                name: full.strip_prefix("refs/heads/").unwrap_or(&full).to_owned(),
                commit,
            },
        })
    }

    /// Captures the working tree of the review's own clone, looking `looks` times, and pins
    /// the capture's base and head in the archive under `session`, which holds them until it
    /// releases them or ends; `pull_requests` are as [`Self::subject`] takes them.
    ///
    /// The capture derives no history: the pinned revision and what it was taken of stand on
    /// their own.
    /// Nothing is captured while git is busy in the working tree, when something the capture
    /// depends on moved while it was taken, and when the checkout is not the review's clone
    /// before or after; `captured` runs between the looks and that last check, where a test
    /// makes something happen meanwhile.
    pub(super) fn capture(
        &self,
        session: &Session,
        pull_requests: &[CheckoutPullRequest],
        looks: Looks,
        captured: &mut dyn FnMut() -> Result<(), Error>,
    ) -> Result<Observed, Error> {
        if !self.verify_clone()? {
            return Ok(Observed::Replaced);
        }
        let observed = match looks {
            Looks::Once => self.look(pull_requests)?,
            Looks::Twice => self.look_twice(pull_requests)?,
        };
        captured()?;
        let Observed::Captured(capture) = observed else {
            return Ok(observed);
        };
        if !self.verify_clone()? {
            return Ok(Observed::Replaced);
        }
        session.pin(&capture.input)?;
        Ok(Observed::Captured(capture))
    }

    /// Looks at the working tree once and returns the revision it makes against its subject's
    /// base, unless git is busy in it or something the capture depends on moved while it was
    /// taken.
    ///
    /// Both snapshots are in the archive with everything they reach once it returns.
    #[tracing::instrument(skip_all)]
    fn look(&self, pull_requests: &[CheckoutPullRequest]) -> Result<Observed, Error> {
        let before = match self.observe(pull_requests)? {
            Ok(before) => before,
            Err(busy) => return Ok(Observed::Busy(busy)),
        };
        let capture = match self.archive.capture_worktree(&self.repo) {
            Ok(capture) => capture,
            Err(Error::GitBusy(busy)) => return Ok(Observed::Busy(busy)),
            Err(err) => return Err(err),
        };
        let after = match self.observe(pull_requests)? {
            Ok(after) => after,
            Err(busy) => return Ok(Observed::Busy(busy)),
        };
        if after != before {
            return Ok(Observed::Moved);
        }
        let Observation { mut subject, .. } = after;
        let head = capture.commit;
        let checkout = matches!(self.meta.target, ReviewTarget::Checkout { .. });
        // The checkout's review never fetches while it captures: a fetch has no deadline, and
        // the capture is what the window waits on. A review of the working tree against a
        // fixed ref deepens a shallow clone, as a review of a range does.
        let deepen_from = if checkout {
            None
        } else {
            self.repo.default_remote()?
        };
        let compared = match subject.base.commit() {
            Some(target) => {
                match self.capture_merge_bases(target, &subject, &head, deepen_from.as_deref()) {
                    Ok(merge_bases) => Some((target.clone(), merge_bases)),
                    Err(Error::NoMergeBase { .. }) if checkout => {
                        let name = subject.base.name().map(str::to_owned);
                        subject.base = match name {
                            // The shared history may be beyond what a shallow clone holds.
                            Some(name) if self.repo.is_shallow()? => BaseRef::Shallow { name },
                            // Work that shares no history with the default branch, such as an
                            // orphan branch, is all new.
                            unrelated => BaseRef::Empty { unrelated },
                        };
                        None
                    }
                    Err(err) => return Err(err),
                }
            }
            None => None,
        };
        let input = match compared {
            Some((target, merge_bases)) => RevisionInput {
                base: merge_bases
                    .first()
                    .cloned()
                    .ok_or_else(|| Error::NoMergeBase {
                        target: target.clone(),
                        head: head.clone(),
                    })?,
                head,
                target: Some(target),
                merge_bases,
            },
            None => RevisionInput {
                base: self.archive.empty_tree_commit()?,
                head,
                target: None,
                merge_bases: Vec::new(),
            },
        };
        Ok(Observed::Captured(Box::new(Captured {
            input,
            subject,
            filtered: capture.filtered,
        })))
    }

    /// Every merge base of `target` and `head`, the capture of `subject`, as
    /// [`Self::merge_bases`] finds them.
    ///
    /// A capture's history is its `HEAD`'s, which is its parent, so they are found among the
    /// reviewed repository's commits; a capture on an unborn `HEAD` has none.
    fn capture_merge_bases(
        &self,
        target: &ObjectId,
        subject: &Subject,
        head: &ObjectId,
        deepen_from: Option<&str>,
    ) -> Result<Vec<ObjectId>, Error> {
        match &subject.head {
            Some(ancestry) => self.merge_bases(target, ancestry, deepen_from),
            None => Err(Error::NoMergeBase {
                target: target.clone(),
                head: head.clone(),
            }),
        }
    }

    /// Looks at the working tree twice and returns the second look when both observed the
    /// same: the subject (branch, `HEAD`, pull request and the commit the base resolved to),
    /// the snapshot pair and the filtered paths; when they differ, the capture was
    /// [`Observed::Moved`].
    ///
    /// Each look already discards itself when something moved while it ran; the second one
    /// also catches a change that settled between the reads, such as an editor that saved one
    /// file of several, which a single look cannot tell from a finished state.
    /// It stands in for a watch on the working tree, which tells a caller that follows it
    /// about such a change.
    fn look_twice(&self, pull_requests: &[CheckoutPullRequest]) -> Result<Observed, Error> {
        let first = match self.look(pull_requests)? {
            Observed::Captured(first) => first,
            missed => return Ok(missed),
        };
        let second = match self.look(pull_requests)? {
            Observed::Captured(second) => second,
            missed => return Ok(missed),
        };
        let same = first.input == second.input
            && first.subject == second.subject
            && first.filtered == second.filtered;
        Ok(if same {
            Observed::Captured(second)
        } else {
            Observed::Moved
        })
    }

    /// Reads git's state in the working tree, or what it is busy with.
    fn observe(
        &self,
        pull_requests: &[CheckoutPullRequest],
    ) -> Result<Result<Observation, Busy>, Error> {
        if let Some(busy) = self.repo.busy()? {
            return Ok(Err(busy));
        }
        let index = self.repo.index_stamp()?;
        let subject = self.subject(pull_requests)?;
        Ok(Ok(Observation { subject, index }))
    }
}
