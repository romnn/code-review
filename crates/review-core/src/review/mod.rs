//! Reviews: what is reviewed, recording revisions, and every state transition.
//!
//! All state transitions are serialized by `review.lock` and committed by appending one event to
//! the log.
//! Analyses and correspondences are content-addressed caches that can be recomputed from the
//! review's archive at any time.
//!
//! The reviewed repository is only read: snapshots and their pins live in the review's archive,
//! a private repository in its data directory, and the clone's identity in the data directory
//! too.

mod archive;
mod cache;
mod checkout;
mod clone;
pub mod github;
mod info;
mod live;
mod memo;
mod retention;
mod trust;

pub use checkout::{BaseRef, Subject, filtered_note};
pub use info::{Commit, PullRequest, ReviewInfo};
pub(crate) use info::{LOG_FORMAT, parse_log};
pub use live::{Capture, LiveCapture};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::analysis::{ANALYSIS_FORMAT, Analysis, analyze};
use crate::correspond::{CORRESPONDENCE_FORMAT, Correspondence, correspond};
use crate::error::Error;
use crate::git::fetch::FetchRepo;
use crate::git::{IgnoreRules, PrivateRepo, Repo};
use crate::history::{History, HistoryInput, Links, RevisionRecord};
use crate::ids::{MoveId, ObjectId, ReviewKey, RevisionNumber, UnitId};
use crate::store::events::{
    Event, MarkEvent, MarkVerdict, NoteEvent, PartialApprovalEvent, RevisionEvent,
};
use crate::store::log::{LockGuard, LogTail, LogWriter, Prefix};
use crate::store::{self, ReviewDir};
use crate::view::{FileStatus, RevisionView, ShownRun, SinceReviewed};
use crate::walkthrough;
use checkout::{Looks, Observed};
use memo::{Read, Replayed};

/// What a review covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewTarget {
    /// A GitHub pull request.
    PullRequest {
        /// The forge host, such as `github.com`.
        host: String,
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The pull request number.
        number: u64,
    },
    /// A local range: the merge base of `base` and `head`, against `head`.
    Range {
        /// The target ref.
        base: String,
        /// The head ref.
        head: String,
    },
    /// The working tree, captured as a snapshot, against a fixed ref.
    Worktree {
        /// The target ref.
        base: String,
    },
    /// The checkout: whatever its working tree holds, on whatever branch, against the base its
    /// subject resolves to on every capture (see [`Subject`]).
    Checkout {
        /// The clone's identity, which cr keeps in its data directory (see
        /// [`Review::open_checkout`]), so that another clone at the same path never reads this
        /// one's history.
        clone: ObjectId,
    },
}

impl ReviewTarget {
    /// Parses a pull request number, a pull request URL, or a `base..head` range.
    ///
    /// A bare number is looked up with `gh pr view` in `repo`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTarget`] for anything else, and an error when `gh` fails.
    pub fn parse(spec: &str, repo: &Repo) -> Result<Self, Error> {
        if let Some((base, head)) = spec.split_once("..") {
            if base.is_empty() || head.is_empty() || head.starts_with('.') {
                return Err(Error::InvalidTarget(spec.to_owned()));
            }
            return Ok(Self::Range {
                base: base.to_owned(),
                head: head.to_owned(),
            });
        }
        let url = if spec.bytes().all(|byte| byte.is_ascii_digit()) && !spec.is_empty() {
            github::view(repo, spec)?.url
        } else {
            spec.to_owned()
        };
        let parsed =
            github::parse_url(&url).ok_or_else(|| Error::InvalidTarget(spec.to_owned()))?;
        Ok(Self::PullRequest {
            host: parsed.host,
            owner: parsed.owner,
            repo: parsed.repo,
            number: parsed.number,
        })
    }

    /// Builds the target from command-line style arguments: `worktree` (the base ref of a
    /// working-tree review) wins; otherwise `spec` is parsed as in [`Self::parse`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTarget`] when neither is given, and the errors of [`Self::parse`].
    pub fn from_arguments(
        spec: Option<&str>,
        worktree: Option<&str>,
        repo: &Repo,
    ) -> Result<Self, Error> {
        match (spec, worktree) {
            (_, Some(base)) => Ok(Self::Worktree {
                base: base.to_owned(),
            }),
            (Some(spec), None) => Self::parse(spec, repo),
            (None, None) => Err(Error::InvalidTarget(
                "pass a pull request, a base..head range, or --worktree".to_owned(),
            )),
        }
    }

    /// Returns the review key for this target in `repo_dir`.
    ///
    /// A pull request review is keyed per clone, `<host>/<owner>/<repo>#<number>@<common-dir>`,
    /// so a second clone starts its own review.
    /// A local review is keyed by the working tree and its refs, and the review of a checkout by
    /// the working tree and the clone, `<working tree>:WORKTREE@<first 16 digits of the clone>`.
    ///
    /// # Errors
    ///
    /// Returns an error when the resulting key is invalid.
    pub fn key(&self, repo_dir: &Repo) -> Result<ReviewKey, Error> {
        let location = repo_dir
            .work_tree()
            .unwrap_or(repo_dir.git_dir())
            .display()
            .to_string();
        let key = match self {
            Self::PullRequest {
                host,
                owner,
                repo,
                number,
            } => format!(
                "{host}/{owner}/{repo}#{number}@{}",
                repo_dir.common_dir().display()
            ),
            Self::Range { base, head } => format!("{location}:{base}..{head}"),
            Self::Worktree { base } => format!("{location}:{base}..WORKTREE"),
            Self::Checkout { clone } => {
                // Sixteen digits tell clones apart; the stored target keeps all of them, so a
                // clash is refused as a key conflict.
                let short = clone.as_str().get(..16).unwrap_or(clone.as_str());
                format!("{location}:WORKTREE@{short}")
            }
        };
        Ok(ReviewKey::parse(&key)?)
    }
}

/// The stored description of a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewMeta {
    /// The review's key.
    pub key: ReviewKey,
    /// The repository's working tree or git directory.
    pub repo: PathBuf,
    /// What is reviewed.
    pub target: ReviewTarget,
}

/// The snapshot pair of a revision to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionInput {
    /// The base snapshot.
    pub base: ObjectId,
    /// The head snapshot.
    pub head: ObjectId,
    /// The target commit the base was computed from.
    pub target: Option<ObjectId>,
    /// Every merge base git reported.
    pub merge_bases: Vec<ObjectId>,
}

/// The outcome of recording a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recorded {
    /// The revision's number.
    pub number: RevisionNumber,
    /// Whether this call created it, rather than finding it to be the current revision already.
    pub created: bool,
    /// The base snapshot.
    pub base: ObjectId,
    /// The head snapshot.
    pub head: ObjectId,
    /// Every merge base; more than one is worth telling the user about.
    pub merge_bases: Vec<ObjectId>,
    /// Why the pull request's metadata or the range's commits could not be refreshed, if they
    /// could not; the revision is recorded regardless.
    pub info_error: Option<String>,
    /// Why when the review synced could not be stored for [`Review::synced`], if it could not;
    /// the revision is recorded regardless.
    pub sync_error: Option<String>,
    /// What a review of a working tree observed when it captured it: the branch, `HEAD` and
    /// the base it compared with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<Box<Subject>>,
    /// The paths a capture of a working tree kept at their staged entry because a filter
    /// driver would have to run on them; see [`crate::WorktreeCapture::filtered`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filtered: Vec<String>,
}

/// When a review last read its target's state and recorded it, which [`Review::refresh`]
/// writes each time it succeeds, unless storing it fails ([`Recorded::sync_error`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Synced {
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// The revision that was current then: the one recorded, or the current one found
    /// unchanged.
    pub revision: RevisionNumber,
}

/// How far [`Review::history_reporting`] has got: how many of the revisions it reads it has
/// replayed, each with its analysis and its links to the revision before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Replay {
    /// The revisions replayed.
    pub replayed: usize,
    /// The revisions to replay: those the log recorded since the review last read its history.
    pub total: usize,
}

/// A step of [`Review::refresh_reporting`], for a front end to show while it waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshStep {
    /// Reading the target's state, which fetches a pull request.
    Fetching,
    /// Analyzing the changes of a snapshot pair the cache does not hold yet.
    Analyzing,
    /// Fetching the pull request's metadata or reading the range's commits.
    Describing,
}

/// Who writes a freshly computed analysis or correspondence to the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Publish {
    /// The caller publishes it once it recorded the revision, after the log's lock; for a
    /// provisional revision, that happens only if it is recorded.
    Recorded,
    /// Write it right away.
    Now,
}

/// How many times [`Review::refresh`] reads its target when recording keeps racing.
const RECORD_ATTEMPTS: u32 = 3;

/// How many times [`Review::refresh`] captures a working tree, twice each, when the captures
/// keep disagreeing or recording keeps racing.
const CAPTURE_ATTEMPTS: u32 = 5;

/// Which last revision recording a revision requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    /// Any: the caller gave the revision, rather than reading it from the target.
    Anything,
    /// The one the log ended at before the target was read, or none; a revision read before
    /// another one was recorded would otherwise be recorded after it, as if it were newer.
    Revision(Option<RevisionNumber>),
}

/// What describes a revision besides its snapshots; see [`RevisionEvent::branch`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RevisionMeta {
    branch: Option<String>,
    head_commit: Option<ObjectId>,
    pull_request: Option<u64>,
}

impl RevisionMeta {
    /// What `subject` says about a capture of the working tree.
    fn of(subject: &Subject) -> Self {
        Self {
            branch: subject.branch.clone(),
            head_commit: subject.head.clone(),
            pull_request: subject
                .pull_request
                .as_ref()
                .map(|pull_request| pull_request.number),
        }
    }
}

/// A review: its storage, its repository and its target.
#[derive(Debug, Clone)]
pub struct Review {
    dir: ReviewDir,
    /// The reviewed repository, which the review only reads.
    repo: Repo,
    /// The snapshots the review recorded or shows, and their pins.
    archive: PrivateRepo,
    meta: ReviewMeta,
    memo: memo::Memo,
    /// How many bytes the data directory may retain before additions are refused; see
    /// [`Self::admit`].
    limit: u64,
}

impl Review {
    /// Opens the review of `target` in `repo`, creating its storage on first use.
    ///
    /// A review an earlier version stored moves its snapshots into its archive first (see
    /// [`Self::snapshots`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InsideRepository`] for review storage inside `repo`,
    /// [`Error::KeyConflict`] when the key is stored for another target, such as a range ending
    /// at a branch named `WORKTREE` and a working-tree review against the same base, the errors
    /// of moving a stored review into its archive, such as [`Error::MissingObject`], and an
    /// error when the storage cannot be created.
    pub fn open(data_dir: &Path, repo: Repo, target: ReviewTarget) -> Result<Self, Error> {
        Self::open_with(data_dir, repo, target, &mut || {})
    }

    /// See [`Self::open`]; `between` runs where an open that maintained the review lets go of
    /// its lease and before it takes it shared, where a test drops the review meanwhile.
    fn open_with(
        data_dir: &Path,
        repo: Repo,
        target: ReviewTarget,
        between: &mut dyn FnMut(),
    ) -> Result<Self, Error> {
        let key = target.key(&repo)?;
        let dir = storage(data_dir, &key, &repo)?;
        let stored = dir.clone();
        let meta = ReviewMeta {
            key,
            repo: repo.work_tree().unwrap_or(repo.git_dir()).to_path_buf(),
            target,
        };
        let open = move || {
            // Two first opens of colliding targets must not both find no metadata and both
            // write.
            let lock =
                tracing::info_span!("review_lock").in_scope(|| LockGuard::acquire(&dir.lock()))?;
            let stored = store::read_json::<ReviewMeta>(&dir.meta())?;
            if let Some(stored) = &stored
                && stored.target != meta.target
            {
                return Err(Error::KeyConflict(meta.key.as_str().to_owned()));
            }
            if stored.as_ref() != Some(&meta) {
                store::write_json_atomic(&dir.meta(), &meta, "review metadata")?;
            }
            let archive = Self::open_archive(&dir, &repo, &lock)?;
            drop(lock);
            Ok(Self {
                dir: dir.clone(),
                repo: repo.clone(),
                archive,
                meta: meta.clone(),
                memo: memo::Memo::default(),
                limit: retention::RETAINED_LIMIT,
            })
        };
        // A review a drop takes meanwhile is made anew.
        loop {
            store::create_dir(stored.root())?;
            if let Some(review) = Self::under_lease(&stored, between, &open)? {
                return Ok(review);
            }
        }
    }

    /// Loads the stored review `key`.
    ///
    /// The review of a checkout binds its clone's identity as [`Self::open_checkout`] does, so
    /// a review an earlier version stored stays the clone's.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownReview`] when no such review is stored, the errors of
    /// [`Self::open`], and an error when its repository cannot be opened.
    pub fn load(data_dir: &Path, key: &ReviewKey) -> Result<Self, Error> {
        Self::load_with(data_dir, key, &mut || {})
    }

    /// See [`Self::load`] and, for `between`, [`Self::open_with`].
    fn load_with(
        data_dir: &Path,
        key: &ReviewKey,
        between: &mut dyn FnMut(),
    ) -> Result<Self, Error> {
        let unknown = || Error::UnknownReview(key.as_str().to_owned());
        let stored = ReviewDir::new(
            &std::path::absolute(data_dir).map_err(|err| Error::io("resolve", data_dir, err))?,
            key,
        );
        let load = || {
            let meta: ReviewMeta = store::read_json(&stored.meta())?.ok_or_else(unknown)?;
            let repo = Repo::discover(&meta.repo)?;
            let dir = storage(data_dir, key, &repo)?;
            if matches!(meta.target, ReviewTarget::Checkout { .. }) {
                clone::ensure_clone_id(dir.data_dir(), &repo)?;
            }
            let lock = LockGuard::acquire(&dir.lock())?;
            let archive = Self::open_archive(&dir, &repo, &lock)?;
            drop(lock);
            Ok(Self {
                dir,
                repo,
                archive,
                meta,
                memo: memo::Memo::default(),
                limit: retention::RETAINED_LIMIT,
            })
        };
        if !stored.meta().exists() {
            return Err(unknown());
        }
        Self::under_lease(&stored, between, &load)?.ok_or_else(unknown)
    }

    /// Returns the review's key.
    #[must_use]
    pub fn key(&self) -> &ReviewKey {
        &self.meta.key
    }

    /// Returns what the review covers.
    #[must_use]
    pub fn target(&self) -> &ReviewTarget {
        &self.meta.target
    }

    /// Returns the reviewed repository, which the review only reads: its working tree, branches
    /// and remotes.
    #[must_use]
    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// Returns the review's archive for reading: a private repository with every snapshot
    /// the review recorded or shows, with all they reach, where reads of a snapshot's objects
    /// go.
    #[must_use]
    pub fn snapshots(&self) -> &Repo {
        self.archive.repo()
    }

    /// Returns what git ignores in the checkout the review follows, judged as its captures
    /// judge it.
    #[must_use]
    pub fn ignore_rules(&self) -> IgnoreRules {
        IgnoreRules::new(self.repo.clone(), self.archive.clone())
    }

    /// Keeps every `git` and `gh` command the review runs from now on from prompting; see
    /// [`Repo::forbid_prompts`].
    pub fn forbid_prompts(&mut self) {
        self.repo.forbid_prompts();
    }

    /// Returns the review's storage paths.
    #[must_use]
    pub fn dir(&self) -> &ReviewDir {
        &self.dir
    }

    /// Returns the ref of the archive that pins snapshot `oid` for this review.
    #[must_use]
    pub fn pin_ref(&self, oid: &ObjectId) -> String {
        archive::pin_ref(oid)
    }

    /// Resolves the target's current snapshots and records a revision if either changed.
    ///
    /// A review of a working tree captures it, against the base its subject resolves to (see
    /// [`Self::subject`]), twice, and records the capture only when both observed the same:
    /// the clone, the subject (branch, `HEAD`, pull request and the commit the base resolved
    /// to) and the snapshot pair.
    /// Nothing else tells a capture caught in the middle of a change, such as an editor saving
    /// several files, from a settled one when no watcher follows the working tree.
    /// The capture is recorded only while the log still ends at the revision it ended at
    /// before the capture, so a capture never lands after a newer one.
    /// A capture that something moved under, that its confirming capture disagreed with, or
    /// that raced another recording, is taken again, a few times at most.
    ///
    /// # Errors
    ///
    /// Returns [`Error::GitBusy`] while git is in the middle of an operation in the working
    /// tree, [`Error::CloneReplaced`] when the checkout of a review of a checkout is another
    /// clone now, [`Error::Raced`] when the repository or the review kept changing, and an
    /// error when the target cannot be resolved or fetched, or recording fails.
    pub fn refresh(&self) -> Result<Recorded, Error> {
        self.refresh_reporting(&mut |_| {})
    }

    /// Does what [`Self::refresh`] does, calling `progress` on the calling thread as each
    /// [`RefreshStep`] starts; a step that is not needed, such as analyzing a snapshot pair the
    /// cache holds, is not reported.
    ///
    /// Once the target's state is recorded, when the review last synced is stored for
    /// [`Self::synced`].
    /// That record only informs the reader, so failing to store it never fails the refresh:
    /// [`Recorded::sync_error`] says why instead, as [`Recorded::info_error`] does for the
    /// metadata.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::refresh`].
    pub fn refresh_reporting(
        &self,
        progress: &mut dyn FnMut(RefreshStep),
    ) -> Result<Recorded, Error> {
        self.refresh_with(None, progress)
    }

    /// Does what [`Self::refresh_reporting`] does, with `pull_request` as what the caller found
    /// for the branch checked out (see [`github::for_checkout`]), for a review of a checkout to
    /// resolve its base from and record with the revision while the branch is still as it was
    /// then; other reviews ignore it.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::refresh`].
    pub fn refresh_with(
        &self,
        pull_request: Option<&github::CheckoutPullRequest>,
        progress: &mut dyn FnMut(RefreshStep),
    ) -> Result<Recorded, Error> {
        progress(RefreshStep::Fetching);
        let pull_requests: Vec<github::CheckoutPullRequest> =
            pull_request.into_iter().cloned().collect();
        let limit = if self.follows_working_tree() {
            CAPTURE_ATTEMPTS
        } else {
            RECORD_ATTEMPTS
        };
        let mut attempts = 0;
        let mut recorded = loop {
            attempts += 1;
            match self.record_target(&pull_requests, progress) {
                Err(Error::Raced(_)) if attempts < limit => {}
                result => break result?,
            }
        };
        recorded.sync_error = store::write_json_atomic(
            &self.dir.synced(),
            &Synced {
                at: store::now_seconds(),
                revision: recorded.number,
            },
            "sync record",
        )
        .err()
        .map(|err| with_causes(&err));
        progress(RefreshStep::Describing);
        recorded.info_error = self
            .describe(recorded.subject.as_deref())
            .err()
            .map(|err| err.to_string());
        Ok(recorded)
    }

    /// Whether the review captures a working tree rather than reading commits.
    fn follows_working_tree(&self) -> bool {
        matches!(
            self.meta.target,
            ReviewTarget::Worktree { .. } | ReviewTarget::Checkout { .. }
        )
    }

    /// Returns when the review last synced with its target, or `None` before its first
    /// [`refresh`](Self::refresh).
    ///
    /// # Errors
    ///
    /// Returns an error when the stored record cannot be read.
    pub fn synced(&self) -> Result<Option<Synced>, Error> {
        store::read_json(&self.dir.synced())
    }

    /// Returns what the review is about, as last fetched, or `None` before the first refresh.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored record cannot be read.
    pub fn info(&self) -> Result<Option<ReviewInfo>, Error> {
        store::read_json(&self.dir.info())
    }

    /// Fetches the pull request's metadata with `gh`, or the range's commits with `git log`,
    /// and stores them atomically with the review.
    ///
    /// # Errors
    ///
    /// Returns an error when `gh` or `git` fails or the record cannot be written.
    pub fn refresh_info(&self) -> Result<ReviewInfo, Error> {
        self.describe(None)
    }

    /// Does what [`Self::refresh_info`] does, describing a review of a checkout by `subject`
    /// when the caller just observed it, rather than observing it again without its pull
    /// request.
    fn describe(&self, subject: Option<&Subject>) -> Result<ReviewInfo, Error> {
        let info = match &self.meta.target {
            ReviewTarget::PullRequest {
                host,
                owner,
                repo,
                number,
            } => {
                let url = format!("https://{host}/{owner}/{repo}/pull/{number}");
                ReviewInfo::PullRequest(Box::new(github::details(&self.repo, &url)?.into()))
            }
            ReviewTarget::Range { base, head } => ReviewInfo::Commits {
                commits: self.repo.commits(Some(base), head)?,
            },
            ReviewTarget::Worktree { base } => ReviewInfo::Commits {
                commits: self.repo.commits(Some(base), "HEAD")?,
            },
            ReviewTarget::Checkout { .. } => {
                let observed;
                let subject = if let Some(subject) = subject {
                    subject
                } else {
                    observed = self.subject(&[])?;
                    &observed
                };
                ReviewInfo::Commits {
                    commits: self.subject_commits(subject)?,
                }
            }
        };
        self.store_info(&info)?;
        Ok(info)
    }

    /// Returns the commits a capture of `subject` holds beyond its base, newest first: those
    /// from its base to its `HEAD`, every commit of `HEAD` against the empty tree, or none
    /// before the first commit.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails.
    pub fn subject_commits(&self, subject: &Subject) -> Result<Vec<Commit>, Error> {
        let Some(head) = &subject.head else {
            return Ok(Vec::new());
        };
        let base = subject.base.commit().map(ObjectId::as_str);
        self.repo.commits(base, head.as_str())
    }

    /// Returns the view state `front_end` last saved with
    /// [`save_view_state`](Self::save_view_state), or `None` when it has saved none.
    ///
    /// View state is how a front end last showed the review, such as which folders were
    /// folded; it is not review state, so it takes no lock and never enters the event log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidFrontEnd`] for a name that is not plain, and an error when the
    /// file cannot be read or does not decode as `T`, as after the front end changed its format.
    pub fn view_state<T: DeserializeOwned>(&self, front_end: &str) -> Result<Option<T>, Error> {
        store::read_json(&self.dir.view_state(front_end)?)
    }

    /// Saves `state` as `front_end`'s view state of this review, replacing the previous one.
    ///
    /// The file is replaced atomically, so a reader sees the old state or the new one whole;
    /// of two concurrent saves, the later rename wins.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidFrontEnd`] for a name that is not plain, and an error when the
    /// state cannot be encoded or written.
    pub fn save_view_state<T: Serialize>(&self, front_end: &str, state: &T) -> Result<(), Error> {
        store::write_json_atomic(&self.dir.view_state(front_end)?, state, "view state")
    }

    /// Stores `info` as what the review is about.
    pub(crate) fn store_info(&self, info: &ReviewInfo) -> Result<(), Error> {
        store::write_json_atomic(&self.dir.info(), info, "review info")
    }

    /// Records the current state of the target as a revision, only while the log still ends at
    /// the revision it ended at before.
    fn record_target(
        &self,
        pull_requests: &[github::CheckoutPullRequest],
        progress: &mut dyn FnMut(RefreshStep),
    ) -> Result<Recorded, Error> {
        let last = self.last_revision()?;
        let observed = last.as_ref().map(|last| last.number);
        // Importing the target's snapshots adds to the archive, unless they are the last
        // revision's.
        let admit = |base: &ObjectId, head: &ObjectId| {
            if last
                .as_ref()
                .is_none_or(|last| &last.base != base || &last.head != head)
            {
                self.admit()?;
            }
            Ok(())
        };
        let remote = self.repo.default_remote()?;
        let deepen_from = remote.as_deref();
        match &self.meta.target {
            ReviewTarget::Range { base, head } => {
                let target = self.repo.resolve_commit(base)?;
                let head = self.repo.resolve_commit(head)?;
                let merge_bases =
                    self.merge_bases(&target, &head, deepen_from, &|base| admit(base, &head))?;
                self.archive
                    .import(&self.repo, std::slice::from_ref(&head))?;
                let input = snapshot_input(target, head, merge_bases)?;
                self.record_revision_reporting(
                    input,
                    RevisionMeta::default(),
                    After::Revision(observed),
                    progress,
                )
            }
            ReviewTarget::Worktree { .. } | ReviewTarget::Checkout { .. } => {
                let checkout::Captured {
                    input,
                    subject,
                    filtered,
                } = match self.capture(pull_requests, Looks::Twice, &mut || Ok(()))? {
                    Observed::Captured(captured) => *captured,
                    Observed::Busy(busy) => return Err(Error::GitBusy(busy)),
                    Observed::Moved => return Err(Error::Raced("working tree")),
                    Observed::Replaced => return Err(Error::CloneReplaced),
                };
                let meta = RevisionMeta::of(&subject);
                let mut recorded = self.record_revision_reporting(
                    input,
                    meta,
                    After::Revision(observed),
                    progress,
                )?;
                recorded.subject = Some(Box::new(subject));
                recorded.filtered = filtered;
                Ok(recorded)
            }
            ReviewTarget::PullRequest {
                host,
                owner,
                repo,
                number,
            } => {
                let url = format!("https://{host}/{owner}/{repo}/pull/{number}");
                let info = github::view(&self.repo, &url)?;
                let parsed = github::parse_url(&info.url)
                    .ok_or_else(|| Error::InvalidTarget(info.url.clone()))?;
                // The fetch lands in a repository of its own with the whole history of both
                // ends, which only the snapshots outlive.
                let github::FetchedPullRequest {
                    head,
                    target,
                    repo: fetched,
                } = github::fetch(
                    &self.repo,
                    &self.archive.scratch(),
                    &parsed,
                    &info.base_ref_name,
                )?;
                let merge_bases = fetched.repo().merge_bases(&target, &head)?;
                let input = snapshot_input(target, head, merge_bases)?;
                admit(&input.base, &input.head)?;
                self.archive
                    .import(fetched.repo(), &[input.base.clone(), input.head.clone()])?;
                drop(fetched);
                self.record_revision_reporting(
                    input,
                    RevisionMeta::default(),
                    After::Revision(observed),
                    progress,
                )
            }
        }
    }

    /// The last recorded revision, or `None` before the first, read from the records appended
    /// since the history the review last read, which derives nothing.
    fn last_revision(&self) -> Result<Option<RevisionRecord>, Error> {
        if self.memo.replaced() {
            return Err(self.log_replaced());
        }
        let (mut tail, read) = match (self.memo.replayed(), self.memo.read()) {
            (Some(replayed), _) => (
                replayed.tail.clone(),
                replayed
                    .history
                    .revisions()
                    .last()
                    .map(|&last| last.clone()),
            ),
            (None, Some(read)) => (read.tail.clone(), last_record(&read.events)),
            (None, None) => (LogTail::new(&self.dir), None),
        };
        let events = self.memo.note_replaced(tail.poll())?;
        Ok(last_record(&events).or(read))
    }

    /// Returns the last revision the log records, read without deriving anything, or `None`
    /// before the first.
    ///
    /// What it reads, the review keeps: the history it reads next starts from there, taking in
    /// only the records appended since, and finds the log replaced since, as a history read
    /// again does.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogReplaced`] once this review or a clone of it found its log is not
    /// the one it read, and an error when the log cannot be read or is corrupt.
    #[tracing::instrument(skip_all)]
    pub fn last_recorded(&self) -> Result<Option<RevisionRecord>, Error> {
        if self.memo.replayed().is_some() || self.memo.read().is_some() {
            return self.last_revision();
        }
        if self.memo.replaced() {
            return Err(self.log_replaced());
        }
        let mut tail = LogTail::new(&self.dir);
        let events = self.memo.note_replaced(tail.poll())?;
        revision_events(&self.dir.events(), &events, 0)?;
        // The tip comes from the read the history goes on from, which nothing appended since
        // can be newer than.
        let last = last_record(&events);
        self.memo.keep_read(Arc::new(Read { events, tail }));
        Ok(last)
    }

    /// Returns the analysis of the recorded revision `record` when the cache holds it in this
    /// version's format, computing nothing.
    ///
    /// The review keeps it for the history it reads next.
    #[must_use]
    #[tracing::instrument(skip_all)]
    pub fn cached_analysis(&self, record: &RevisionRecord) -> Option<Arc<Analysis>> {
        let analysis = self
            .memo
            .analysis(&record.base, &record.head)
            .or_else(|| cached_analysis(&self.dir, &record.base, &record.head).map(Arc::new))?;
        self.memo.keep_analysis(Arc::clone(&analysis));
        Some(analysis)
    }

    /// Returns the analysis of the recorded revision `record` from the cache, or computes it
    /// and writes it there.
    ///
    /// The review keeps it for the history it reads next, which then neither reads nor
    /// computes it again.
    ///
    /// # Errors
    ///
    /// Returns an error when the analysis cannot be computed.
    #[tracing::instrument(skip_all)]
    pub fn analyze_recorded(&self, record: &RevisionRecord) -> Result<Arc<Analysis>, Error> {
        let analysis = self.analysis(&record.base, &record.head, Publish::Now, &mut |_| {})?;
        self.memo.keep_analysis(Arc::clone(&analysis));
        Ok(analysis)
    }

    /// Returns the files the recorded revision `record` changes, as one comparison of its trees
    /// in the archive finds them, without renames and with nothing counted: what shows of it
    /// before its changes are analyzed.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails, as when a snapshot is not in the archive.
    #[tracing::instrument(skip_all)]
    pub fn changed_files(&self, record: &RevisionRecord) -> Result<Vec<FileStatus>, Error> {
        Ok(self
            .archive
            .repo()
            .diff_tree_unrenamed(&record.base, &record.head)?
            .into_iter()
            .filter_map(FileStatus::unanalyzed)
            .collect())
    }

    /// Every merge base of `target` and `ancestry`, the commit whose history the head has, best
    /// first, with the best imported into the archive once `admit` lets it through.
    ///
    /// A shallow clone that holds none is deepened from `deepen_from`, when given, in a
    /// transient repository that starts with the history the clone holds.
    fn merge_bases(
        &self,
        target: &ObjectId,
        ancestry: &ObjectId,
        deepen_from: Option<&str>,
        admit: &dyn Fn(&ObjectId) -> Result<(), Error>,
    ) -> Result<Vec<ObjectId>, Error> {
        let found = self.repo.merge_bases(target, ancestry);
        let deepen = match &found {
            Err(Error::NoMergeBase { .. }) => deepen_from,
            _ => None,
        };
        let (merge_bases, fetched) = match deepen {
            Some(remote) if self.repo.is_shallow()? => {
                let fetched = FetchRepo::new(&self.archive.scratch(), &self.repo)?;
                fetched.seed(&self.repo, &[target.clone(), ancestry.clone()])?;
                (
                    fetched.merge_bases(target, ancestry, remote)?,
                    Some(fetched),
                )
            }
            _ => (found?, None),
        };
        let from = fetched.as_ref().map_or(&self.repo, FetchRepo::repo);
        if let Some(best) = merge_bases.first() {
            admit(best)?;
            self.archive.import(from, std::slice::from_ref(best))?;
        }
        Ok(merge_bases)
    }

    /// Records the revision `(base, head)` as the new current revision, unless it already is the
    /// current one.
    ///
    /// Revisions form a linear chain in observation order: a snapshot pair seen before, such as
    /// after a force-push back to an earlier head, gets a new number, and its analysis is reused
    /// from the cache.
    /// Review state therefore always flows forward in time.
    ///
    /// A revision that is not the current one already is an addition, which the data
    /// directory's limit may refuse; see [`Error::StorageFull`].
    ///
    /// The analysis is computed first; it is content-addressed, so it needs no lock.
    /// Then, under `review.lock`, the log is reread and compared with the current `(base, head)`,
    /// both snapshots are pinned, the revision event is appended, and the analysis is published
    /// to the cache.
    /// The event is the commit point: a crash before it leaves only idempotent pins, which the
    /// archive's next compaction removes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StorageFull`] when the data directory retains more than its limit, and
    /// an error when analysis, pinning or the log append fails.
    pub fn record_revision(&self, input: RevisionInput) -> Result<Recorded, Error> {
        // Recording the current revision again adds nothing.
        if self
            .last_revision()?
            .is_none_or(|last| last.base != input.base || last.head != input.head)
        {
            self.admit()?;
        }
        self.record_revision_reporting(input, RevisionMeta::default(), After::Anything, &mut |_| {})
    }

    /// Records `input`, described by `meta`, while the log's last revision is the one `after`
    /// asks for, unless the log already ends at `input`; reports [`RefreshStep::Analyzing`] to
    /// `progress` when the snapshot pair has to be analyzed.
    fn record_revision_reporting(
        &self,
        input: RevisionInput,
        meta: RevisionMeta,
        after: After,
        progress: &mut dyn FnMut(RefreshStep),
    ) -> Result<Recorded, Error> {
        // A capture or a fetch already put both snapshots in the archive; a revision given by
        // the caller has them in the reviewed repository.
        self.archive
            .import(&self.repo, &[input.base.clone(), input.head.clone()])?;
        let analysis = self.analysis(&input.base, &input.head, Publish::Recorded, progress)?;
        let lock = LockGuard::acquire(&self.dir.lock())?;
        let mut log = self.open_log(&lock, None)?;
        let revisions = revision_events(&self.dir.events(), log.events(), 0)?;
        if let Some(existing) = revisions
            .last()
            .filter(|revision| revision.base == input.base && revision.head == input.head)
        {
            return Ok(Recorded {
                number: existing.number,
                created: false,
                base: existing.base.clone(),
                head: existing.head.clone(),
                merge_bases: existing.merge_bases.clone(),
                info_error: None,
                sync_error: None,
                subject: None,
                filtered: Vec::new(),
            });
        }
        if let After::Revision(observed) = after
            && observed != revisions.last().map(|last| last.number)
        {
            return Err(Error::Raced("review"));
        }
        let number = self.next_number(revisions.last().map(|last| last.number))?;
        self.append_revision(
            &mut log,
            RevisionEvent {
                number,
                base: input.base.clone(),
                head: input.head.clone(),
                target: input.target,
                merge_bases: input.merge_bases.clone(),
                recorded_at: store::now_seconds(),
                branch: meta.branch,
                head_commit: meta.head_commit,
                pull_request: meta.pull_request,
            },
        )?;
        drop(log);
        drop(lock);
        self.publish(&analysis);
        Ok(Recorded {
            number,
            created: true,
            base: input.base,
            head: input.head,
            merge_bases: input.merge_bases,
            info_error: None,
            sync_error: None,
            subject: None,
            filtered: Vec::new(),
        })
    }

    /// The number of the revision after `last`, or the first one.
    fn next_number(&self, last: Option<RevisionNumber>) -> Result<RevisionNumber, Error> {
        match last {
            Some(last) => last.next().ok_or_else(|| Error::InconsistentLog {
                path: self.dir.events(),
                detail: "revision numbers exhausted".to_owned(),
            }),
            None => Ok(RevisionNumber::FIRST),
        }
    }

    /// Appends `revision` to the log under its lock, the commit point of recording it: pins
    /// both snapshots in the archive first, which is idempotent, so a crash before the append
    /// leaves nothing a later recording does not redo.
    ///
    /// The caller publishes the revision's analysis once it let go of the log's lock (see
    /// [`Self::publish`]).
    /// The target is only a label of the revision: nothing reads its objects.
    fn append_revision(
        &self,
        log: &mut LogWriter<'_>,
        revision: RevisionEvent,
    ) -> Result<(), Error> {
        // A revision of no change has one snapshot, which one ref pins.
        let mut snapshots = vec![&revision.base];
        if revision.head != revision.base {
            snapshots.push(&revision.head);
        }
        let pins: Vec<(String, ObjectId)> = snapshots
            .into_iter()
            .map(|oid| (self.pin_ref(oid), oid.clone()))
            .collect();
        self.archive.update_refs(&pins)?;
        log.append(Event::Revision(revision))
    }

    /// Publishes `analysis`, of a revision just recorded, to the cache unless it holds one,
    /// which only saves computing it again.
    fn publish(&self, analysis: &Analysis) {
        let path = self.dir.analysis(&analysis.base, &analysis.head);
        if !path.exists() {
            cache::install(&self.dir, &path, analysis);
        }
    }

    /// Loads the analysis of `(base, head)` from the memo or the cache, or computes it,
    /// reporting [`RefreshStep::Analyzing`] to `progress` first, and publishes it unless the
    /// caller publishes it itself.
    fn analysis(
        &self,
        base: &ObjectId,
        head: &ObjectId,
        publish: Publish,
        progress: &mut dyn FnMut(RefreshStep),
    ) -> Result<Arc<Analysis>, Error> {
        if let Some(remembered) = self.memo.analysis(base, head) {
            return Ok(remembered);
        }
        if let Some(cached) = cached_analysis(&self.dir, base, head) {
            return Ok(Arc::new(cached));
        }
        progress(RefreshStep::Analyzing);
        let analysis = analyze(self.archive.repo(), base, head)?;
        if publish == Publish::Now {
            cache::install(&self.dir, &self.dir.analysis(base, head), &analysis);
        }
        Ok(Arc::new(analysis))
    }

    /// Loads the correspondence between `older` and `newer` from the cache, or computes it, and
    /// writes it to the cache unless the caller publishes the newer revision itself.
    fn correspondence(
        &self,
        older: &Analysis,
        newer: &Analysis,
        publish: Publish,
    ) -> Result<Correspondence, Error> {
        if let Some(cached) = cached_correspondence(&self.dir, older, newer) {
            return Ok(cached);
        }
        let computed = correspond(self.archive.repo(), older, newer)?;
        if publish == Publish::Now {
            let path = self
                .dir
                .correspondence([&older.base, &older.head], [&newer.base, &newer.head]);
            cache::install(&self.dir, &path, &computed);
        }
        Ok(computed)
    }

    /// Reads the log under a shared lock and derives the full history.
    ///
    /// A review reads only the records appended since it last read its history, and derives
    /// again only what they change; its clones share what it read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogReplaced`] once this review or a clone of it found its log is not
    /// the one it read, and an error when the log is corrupt or an analysis cannot be computed.
    pub fn history(&self) -> Result<History, Error> {
        self.history_reporting(&|_| {})
    }

    /// Does what [`Self::history`] does, calling `progress` on the calling thread with how far
    /// it has got: before each revision it replays, and once every one is.
    ///
    /// A history that reads nothing new from the log replays nothing and calls nothing.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::history`].
    pub fn history_reporting(&self, progress: &dyn Fn(Replay)) -> Result<History, Error> {
        if self.memo.replaced() {
            return Err(self.log_replaced());
        }
        // A review that read its log before any history starts from what it read.
        let (mut replayed, mut events) = match (self.memo.replayed(), self.memo.read()) {
            (Some(kept), _) => (Replayed::clone(&kept), Vec::new()),
            (None, Some(read)) => (
                Replayed {
                    history: History::empty(),
                    tail: read.tail.clone(),
                },
                read.events.clone(),
            ),
            (None, None) => (Replayed::start(&self.dir), Vec::new()),
        };
        events.extend(self.memo.note_replaced(replayed.tail.poll())?);
        if events.is_empty() {
            return Ok(replayed.history);
        }
        replayed.history = self.extend_history(
            &replayed.history,
            events,
            replayed.tail.prefix(),
            None,
            progress,
        )?;
        let history = replayed.history.clone();
        self.memo.keep_replayed(Arc::new(replayed));
        Ok(history)
    }

    /// Opens the log under `lock` to append to it, unless this review or a clone of it found
    /// the log replaced; with `shown`, the history what is appended was made on, only while
    /// the log still starts with what `shown` was read from, so nothing lands in another log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogReplaced`] when the log was found replaced or does not start with
    /// what `shown` was read from, and an error when it cannot be read.
    fn open_log<'lock>(
        &self,
        lock: &'lock LockGuard,
        shown: Option<&History>,
    ) -> Result<LogWriter<'lock>, Error> {
        self.guard_log(|path| LogWriter::open(path, lock, shown.map(History::prefix)))
    }

    /// Opens the log at its path with `open`, a [`LogWriter`] constructor, unless this review
    /// or a clone of it found the log replaced, and notes a replacement `open` finds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogReplaced`] when the log was found replaced, and the errors of `open`.
    fn guard_log<T>(&self, open: impl FnOnce(&Path) -> Result<T, Error>) -> Result<T, Error> {
        if self.memo.replaced() {
            return Err(self.log_replaced());
        }
        self.memo.note_replaced(open(&self.dir.events()))
    }

    fn log_replaced(&self) -> Error {
        Error::LogReplaced {
            path: self.dir.events(),
        }
    }

    /// Extends `history` by the log's `events`, read since it was until the log was `prefix`,
    /// followed by the provisional revision `live` when there is one, telling `progress` how
    /// many of their revisions it replayed as [`Self::history_reporting`] says.
    ///
    /// Analyses and the links between consecutive revisions come from the memo when a history
    /// it keeps has them, then from the cache; a provisional revision's are computed but never
    /// written, since nothing may refer to a revision that is not recorded.
    #[tracing::instrument(name = "history", skip_all, fields(records = events.len()))]
    fn extend_history(
        &self,
        history: &History,
        mut events: Vec<Event>,
        prefix: Prefix,
        live: Option<RevisionEvent>,
        progress: &dyn Fn(Replay),
    ) -> Result<History, Error> {
        let provisional = live.as_ref().map(|live| live.number);
        events.extend(live.map(Event::Revision));
        let total = revision_events(&self.dir.events(), &events, history.revisions().len())?.len();
        let publish = |revision: &RevisionEvent| {
            if Some(revision.number) == provisional {
                Publish::Recorded
            } else {
                Publish::Now
            }
        };
        // A revision's analysis is asked for once the revision before it is linked, so each
        // ask counts every revision before it as replayed.
        let replayed = std::cell::Cell::new(0);
        let extended = history.extended(HistoryInput {
            events,
            prefix,
            provisional: provisional.is_some(),
            analysis: &|revision| {
                progress(Replay {
                    replayed: replayed.get(),
                    total,
                });
                replayed.set(replayed.get() + 1);
                tracing::info_span!("analysis").in_scope(|| {
                    self.analysis(
                        &revision.base,
                        &revision.head,
                        publish(revision),
                        &mut |_| {},
                    )
                })
            },
            links: &|revision, older, newer| {
                tracing::info_span!("links")
                    .in_scope(|| self.links(older, newer, publish(revision)))
            },
        })?;
        progress(Replay {
            replayed: total,
            total,
        });
        Ok(extended)
    }

    /// Returns the links between consecutive revisions analyzed as `older` and `newer`, from
    /// the memo, or else from their correspondence.
    fn links(
        &self,
        older: &Analysis,
        newer: &Analysis,
        publish: Publish,
    ) -> Result<Arc<Links>, Error> {
        if let Some(links) = self.memo.links(older, newer) {
            return Ok(links);
        }
        let correspondence = self.correspondence(older, newer, publish)?;
        Ok(Arc::new(Links::new(
            &correspondence.links,
            older.units.len(),
            newer.units.len(),
        )))
    }

    /// Appends events under the lock.
    fn append(&self, events: Vec<Event>) -> Result<(), Error> {
        let lock = LockGuard::acquire(&self.dir.lock())?;
        let mut log = self.open_log(&lock, None)?;
        for event in events {
            log.append(event)?;
        }
        Ok(())
    }

    /// Marks units of `view`'s revision, which must be the current revision.
    ///
    /// Marks are only made on the tip of the revision chain, so a verdict can never be overtaken
    /// by an older one inherited along a later link.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongRevision`] or [`Error::UnknownUnit`] for a unit not in the view,
    /// [`Error::NotCurrentRevision`] when a newer revision exists and [`Error::CloneReplaced`]
    /// when the checkout of a review of a checkout is another clone now, checked under the lock
    /// before anything is recorded, [`Error::StorageFull`] when a provisional revision cannot
    /// be recorded for want of space, and an error when the append fails.
    pub fn mark(
        &self,
        view: &RevisionView<'_>,
        units: &[UnitId],
        verdict: MarkVerdict,
    ) -> Result<(), Error> {
        let at = store::now_seconds();
        let events = units
            .iter()
            .map(|&unit| {
                Ok(Event::Mark(MarkEvent {
                    revision: view.number(),
                    unit: unit.index(),
                    evidence: view.evidence_digest(unit)?,
                    verdict,
                    at,
                }))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let lock = LockGuard::acquire(&self.dir.lock())?;
        let mut log = self.open_log(&lock, Some(view.history()))?;
        let recorded = self.settle(&mut log, view, true)?;
        for event in events {
            log.append(event)?;
        }
        drop(log);
        drop(lock);
        self.published(view, recorded);
        Ok(())
    }

    /// Approves `shown`, a change since review of one unit as `view` showed it.
    ///
    /// The approval records the reviewed text with that change's current lines in place of its
    /// reviewed ones, so the unit's other changes still show; when that text is the whole
    /// current change, it records the unit accepted instead.
    ///
    /// Under the lock, the unit's comparison is derived again from the log as it is then, with
    /// `view`'s capture when it is a provisional one the log does not have yet, and the change
    /// is approved only when that comparison still has it: the same current lines at the same
    /// place, replacing the same reviewed lines.
    /// Only then is the capture recorded, as for [`Self::mark`], so a refused approval records
    /// nothing.
    /// An approval of another change of the unit since the view was read leaves it as it was,
    /// apart from where its reviewed lines are in the reviewed text, so it is still approved;
    /// one already approved, or changed since, is refused.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ChangedSinceShown`] when the log's comparison no longer has the change,
    /// the errors of [`Self::mark`] for a revision that is not current, and an error when the
    /// append fails.
    pub fn approve_run(&self, view: &RevisionView<'_>, shown: &ShownRun) -> Result<(), Error> {
        let unit = shown.unit;
        let changed = || Error::ChangedSinceShown { unit };
        let evidence = view.evidence_digest(unit)?;
        let lock = LockGuard::acquire(&self.dir.lock())?;
        let (mut log, opened) = self
            .guard_log(|path| LogWriter::open_with_prefix(path, &lock, view.history().prefix()))?;
        // The comparison is derived from the log and, for a capture not recorded yet, the
        // capture itself, so a refused approval records nothing, not even the capture.
        let capture = self.unsettled(&log, view, true)?;
        let history = self.extend_history(
            &History::empty(),
            log.events().to_vec(),
            opened,
            capture.clone(),
            &|_| {},
        )?;
        let now = history.view(view.number())?;
        if now.evidence_digest(unit)? != evidence {
            return Err(changed());
        }
        let Some(SinceReviewed::Delta(delta)) = now.since_reviewed(unit)? else {
            return Err(changed());
        };
        let run = delta
            .runs()
            .into_iter()
            .find(|run| run.is(shown))
            .ok_or_else(changed)?;
        let approved = delta.approving(&run);
        let recorded = capture.is_some();
        if let Some(capture) = capture {
            self.append_revision(&mut log, capture)?;
        }
        let at = store::now_seconds();
        let event = if approved == delta.after.added {
            Event::Mark(MarkEvent {
                revision: view.number(),
                unit: unit.index(),
                evidence,
                verdict: MarkVerdict::Accepted,
                at,
            })
        } else {
            Event::PartialApproval(PartialApprovalEvent {
                revision: view.number(),
                unit: unit.index(),
                evidence,
                at,
                text: approved,
            })
        };
        log.append(event)?;
        drop(log);
        drop(lock);
        self.published(view, recorded);
        Ok(())
    }

    /// Marks every unit of the files at `paths` (or below them, for directories) in `view`'s
    /// revision, which must be the current revision, and returns them.
    ///
    /// This is a convenience over [`mark`](Self::mark): the units are fixed from `view`, so units
    /// that appear in later revisions are never covered, and they are recorded under one lock.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoChangedFiles`] when no unit matches, and the errors of
    /// [`mark`](Self::mark).
    pub fn mark_files(
        &self,
        view: &RevisionView<'_>,
        paths: &[String],
        verdict: MarkVerdict,
    ) -> Result<Vec<UnitId>, Error> {
        let units = view.units_under(paths);
        if units.is_empty() {
            return Err(Error::NoChangedFiles(paths.to_vec()));
        }
        self.mark(view, &units, verdict)?;
        Ok(units)
    }

    /// Acknowledges a plain move: marks `seen` exactly the units it covers completely, on both
    /// sides, and returns them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownMove`] or [`Error::MoveHasEdits`] for a move that cannot be
    /// acknowledged, and an error when the append fails.
    pub fn acknowledge_move(
        &self,
        view: &RevisionView<'_>,
        id: MoveId,
    ) -> Result<Vec<UnitId>, Error> {
        let units = view.units_covered_by_move(id)?;
        self.mark(view, &units, MarkVerdict::Seen)?;
        Ok(units)
    }

    /// Attaches a note to a unit of `view`'s revision.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EmptyNote`] for blank text, the unit errors of [`RevisionView::unit`],
    /// [`Error::StorageFull`] when the provisional revision it would record cannot be for want
    /// of space, and an error when the append fails.
    pub fn note(&self, view: &RevisionView<'_>, unit: UnitId, text: &str) -> Result<(), Error> {
        if text.trim().is_empty() {
            return Err(Error::EmptyNote);
        }
        let event = Event::Note(NoteEvent {
            revision: view.number(),
            unit: unit.index(),
            evidence: view.evidence_digest(unit)?,
            text: text.to_owned(),
            at: store::now_seconds(),
        });
        let lock = LockGuard::acquire(&self.dir.lock())?;
        let mut log = self.open_log(&lock, Some(view.history()))?;
        let recorded = self.settle(&mut log, view, false)?;
        log.append(event)?;
        drop(log);
        drop(lock);
        self.published(view, recorded);
        Ok(())
    }

    /// Publishes the analysis of `view`'s revision to the cache when it was `recorded` just now,
    /// as a capture a mark, an approval or a note recorded; see [`Self::publish`].
    fn published(&self, view: &RevisionView<'_>, recorded: bool) {
        if recorded {
            self.publish(view.analysis());
        }
    }

    /// Validates and stores a walkthrough document.
    ///
    /// A walkthrough adds text of any length to the log, which the data directory's limit may
    /// refuse.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Walkthrough`] when validation fails, [`Error::StorageFull`] when the
    /// data directory retains more than its limit, and an error when the append fails.
    pub fn import_walkthrough(
        &self,
        history: &History,
        json: &str,
    ) -> Result<RevisionNumber, Error> {
        let event = walkthrough::validate(history, self.key(), json)?;
        self.admit()?;
        let revision = event.revision;
        self.append(vec![Event::Walkthrough(event)])?;
        Ok(revision)
    }
}

/// Returns the storage of the review `key` in `data_dir`, as an absolute path, so that git,
/// which runs elsewhere, finds it too.
///
/// # Errors
///
/// Returns [`Error::InsideRepository`] when the storage or its archive, followed through any
/// symbolic link, is inside `repo`, which cr never writes to.
fn storage(data_dir: &Path, key: &ReviewKey, repo: &Repo) -> Result<ReviewDir, Error> {
    let data_dir =
        std::path::absolute(data_dir).map_err(|err| Error::io("resolve", data_dir, err))?;
    let dir = ReviewDir::new(&data_dir, key);
    crate::git::refuse_inside(dir.root(), repo)?;
    crate::git::refuse_inside(&dir.archive(), repo)?;
    Ok(dir)
}

/// The analysis of `(base, head)` the cache in `dir` holds, if it holds one this version reads;
/// a cache file that fails to decode is computed again rather than trusted.
///
/// One an earlier version wrote without its digest gets it now.
fn cached_analysis(dir: &ReviewDir, base: &ObjectId, head: &ObjectId) -> Option<Analysis> {
    let cached = cache::read(dir, &dir.analysis(base, head), |cached: &Analysis| {
        cached.format == ANALYSIS_FORMAT && &cached.base == base && &cached.head == head
    })?;
    if cached.digest.is_empty() {
        cached.sealed().ok()
    } else {
        Some(cached)
    }
}

/// The last revision `events` record, if they record one.
fn last_record(events: &[Event]) -> Option<RevisionRecord> {
    events.iter().rev().find_map(|event| match event {
        Event::Revision(revision) => Some(RevisionRecord::from(revision)),
        _ => None,
    })
}

/// The correspondence between `older` and `newer` the cache in `dir` holds, if it holds one
/// this version reads that was computed between these very analyses, so its links index their
/// units even after one was computed again.
fn cached_correspondence(
    dir: &ReviewDir,
    older: &Analysis,
    newer: &Analysis,
) -> Option<Correspondence> {
    let path = dir.correspondence([&older.base, &older.head], [&newer.base, &newer.head]);
    cache::read(dir, &path, |cached: &Correspondence| {
        cached.format == CORRESPONDENCE_FORMAT
            && cached.older == older.digest
            && cached.newer == newer.digest
    })
}

/// The revision of `head` against the first of its `merge_bases` with `target`.
fn snapshot_input(
    target: ObjectId,
    head: ObjectId,
    merge_bases: Vec<ObjectId>,
) -> Result<RevisionInput, Error> {
    let base = merge_bases
        .first()
        .cloned()
        .ok_or_else(|| Error::NoMergeBase {
            target: target.clone(),
            head: head.clone(),
        })?;
    Ok(RevisionInput {
        base,
        head,
        target: Some(target),
        merge_bases,
    })
}

/// Extracts the revision events and checks that they are numbered in log order after the
/// `recorded` revisions before them: 1, 2, 3, … from the start of the log.
fn revision_events(
    path: &Path,
    events: &[Event],
    recorded: usize,
) -> Result<Vec<RevisionEvent>, Error> {
    let revisions: Vec<RevisionEvent> = events
        .iter()
        .filter_map(|event| match event {
            Event::Revision(revision) => Some(revision.clone()),
            _ => None,
        })
        .collect();
    for (index, revision) in (recorded..).zip(&revisions) {
        if Some(revision.number) != RevisionNumber::from_index(index) {
            return Err(Error::InconsistentLog {
                path: path.to_path_buf(),
                detail: format!(
                    "revision {} recorded at position {}",
                    revision.number,
                    index + 1
                ),
            });
        }
    }
    Ok(revisions)
}

/// Renders `err` with its causes, `failed to rename …: Is a directory`, for a note that is not
/// an error value of its own.
fn with_causes(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
