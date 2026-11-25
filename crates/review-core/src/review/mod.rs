//! Reviews: what is reviewed, recording revisions, and every state transition.
//!
//! All state transitions are serialized by `review.lock` and committed by appending one event to
//! the log.
//! Analyses and correspondences are content-addressed caches that can be recomputed from git at any
//! time.

mod checkout;
pub mod github;
mod info;
mod live;
mod memo;
mod session;
mod trust;

pub use checkout::{BaseRef, Subject, filtered_note};
pub use info::{Commit, PullRequest, ReviewInfo};
pub(crate) use info::{LOG_FORMAT, parse_log};
pub use live::{Capture, LiveCapture};
pub use session::Session;
pub use trust::Trust;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::analysis::{ANALYSIS_FORMAT, Analysis, analyze};
use crate::correspond::{CORRESPONDENCE_FORMAT, Correspondence, correspond};
use crate::error::Error;
use crate::git::Repo;
use crate::history::{History, HistoryInput};
use crate::ids::{MoveId, ObjectId, ReviewKey, RevisionNumber, UnitId};
use crate::store::events::{
    Event, MarkEvent, MarkVerdict, NoteEvent, PartialApprovalEvent, RevisionEvent,
};
use crate::store::log::{LockGuard, LogTail, LogWriter};
use crate::store::{self, ReviewDir};
use crate::view::{RevisionView, ShownRun, SinceReviewed};
use crate::walkthrough;

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
        /// The clone's identity (see [`Repo::ensure_clone_id`]), so that another clone at the same
        /// path never reads this one's history.
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
    /// because its pinned snapshots live in that clone's object store; a second clone starts its
    /// own review rather than reading history it cannot reach.
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
    /// The caller publishes it under the lock, as part of recording a revision; for a
    /// provisional revision, that happens only if it is recorded.
    UnderLock,
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
    repo: Repo,
    meta: ReviewMeta,
    memo: memo::Memo,
}

impl Review {
    /// Opens the review of `target` in `repo`, creating its storage on first use.
    ///
    /// # Errors
    ///
    /// Returns [`Error::KeyConflict`] when the key is stored for another target, such as a range
    /// ending at a branch named `WORKTREE` and a working-tree review against the same base, and
    /// an error when the storage cannot be created.
    pub fn open(data_dir: &Path, repo: Repo, target: ReviewTarget) -> Result<Self, Error> {
        let key = target.key(&repo)?;
        let dir = ReviewDir::new(data_dir, &key);
        let meta = ReviewMeta {
            key,
            repo: repo.work_tree().unwrap_or(repo.git_dir()).to_path_buf(),
            target,
        };
        store::create_dir(dir.root())?;
        // Two first opens of colliding targets must not both find no metadata and both write.
        let lock = LockGuard::acquire(&dir.lock())?;
        let stored = store::read_json::<ReviewMeta>(&dir.meta())?;
        if let Some(stored) = &stored
            && stored.target != meta.target
        {
            return Err(Error::KeyConflict(meta.key.as_str().to_owned()));
        }
        if stored.as_ref() != Some(&meta) {
            store::write_json_atomic(&dir.meta(), &meta, "review metadata")?;
        }
        drop(lock);
        Ok(Self {
            dir,
            repo,
            meta,
            memo: memo::Memo::default(),
        })
    }

    /// Loads the stored review `key`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownReview`] when no such review is stored, and an error when its
    /// repository cannot be opened.
    pub fn load(data_dir: &Path, key: &ReviewKey) -> Result<Self, Error> {
        let dir = ReviewDir::new(data_dir, key);
        let meta: ReviewMeta = store::read_json(&dir.meta())?
            .ok_or_else(|| Error::UnknownReview(key.as_str().to_owned()))?;
        let repo = Repo::discover(&meta.repo)?;
        Ok(Self {
            dir,
            repo,
            meta,
            memo: memo::Memo::default(),
        })
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

    /// Returns the review's repository.
    #[must_use]
    pub fn repo(&self) -> &Repo {
        &self.repo
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

    /// Returns the ref that pins snapshot `oid` for this review.
    #[must_use]
    pub fn pin_ref(&self, oid: &ObjectId) -> String {
        format!("refs/code-review/{}/snap/{oid}", self.meta.key.encoded())
    }

    /// Resolves the target's current snapshots and records a revision if either changed.
    ///
    /// A review of a working tree captures it, against the base its subject resolves to (see
    /// [`Self::subject`]), twice, and records the capture only when both observed the same:
    /// the clone, the subject (branch, `HEAD`, pull request and the commit the base resolved
    /// to) and the snapshot pair.
    /// Nothing else tells a capture caught in the middle of a change, such as an editor saving
    /// several files, from a settled one when no watcher follows the working tree.
    /// The capture is pinned under a session of its own (see [`Self::start_session`]) before
    /// it is analyzed, and recorded only while the log still ends at the revision it ended at
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
                commits: self.repo.commits(base, head)?,
            },
            ReviewTarget::Worktree { base } => ReviewInfo::Commits {
                commits: self.repo.commits(base, "HEAD")?,
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
        let base = match subject.base.commit() {
            Some(base) => base.clone(),
            None => self.repo.empty_tree_commit()?,
        };
        self.repo.commits(base.as_str(), head.as_str())
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

    /// Records the current state of the target as a revision, pinned under a session of its
    /// own until it is, and only while the log still ends at the revision it ended at before.
    fn record_target(
        &self,
        pull_requests: &[github::CheckoutPullRequest],
        progress: &mut dyn FnMut(RefreshStep),
    ) -> Result<Recorded, Error> {
        let observed = self.last_revision()?;
        let session = self.start_session()?;
        let remote = self.repo.default_remote()?;
        let deepen_from = remote.as_deref();
        match &self.meta.target {
            ReviewTarget::Range { base, head } => {
                let target = self.repo.resolve_commit(base)?;
                let head = self.repo.resolve_commit(head)?;
                let input = self.snapshot_input(target, head, deepen_from)?;
                session.pin(&input)?;
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
                } = self.confirmed_capture(pull_requests)?;
                session.pin(&input)?;
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
                let fetched = github::fetch(&self.repo, &parsed, &info.base_ref_name)?;
                let input = self.snapshot_input(
                    fetched.target.clone(),
                    fetched.head.clone(),
                    Some(&fetched.remote),
                )?;
                session.pin(&input)?;
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

    /// The number of the last recorded revision, or `None` before the first.
    fn last_revision(&self) -> Result<Option<RevisionNumber>, Error> {
        let events = LogTail::new(&self.dir).poll()?;
        Ok(revision_events(&self.dir.events(), &events)?
            .last()
            .map(|revision| revision.number))
    }

    /// The revision of `head` against its merge base with `target`.
    fn snapshot_input(
        &self,
        target: ObjectId,
        head: ObjectId,
        deepen_from: Option<&str>,
    ) -> Result<RevisionInput, Error> {
        let merge_bases = self.repo.merge_bases(&target, &head, deepen_from)?;
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

    /// Records the revision `(base, head)` as the new current revision, unless it already is the
    /// current one.
    ///
    /// Revisions form a linear chain in observation order: a snapshot pair seen before, such as
    /// after a force-push back to an earlier head, gets a new number, and its analysis is reused
    /// from the cache.
    /// Review state therefore always flows forward in time.
    ///
    /// The analysis is computed first; it is content-addressed, so it needs no lock.
    /// Then, under `review.lock`, the log is reread and compared with the current `(base, head)`,
    /// both snapshots are pinned, the analysis is published, and the revision event is appended.
    /// The event is the commit point: a crash before it leaves only idempotent pins and a cache
    /// file.
    ///
    /// # Errors
    ///
    /// Returns an error when analysis, pinning or the log append fails.
    pub fn record_revision(&self, input: RevisionInput) -> Result<Recorded, Error> {
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
        let analysis = self.analysis(&input.base, &input.head, Publish::UnderLock, progress)?;
        let lock = LockGuard::acquire(&self.dir.lock())?;
        let mut log = LogWriter::open(&self.dir.events(), &lock)?;
        let revisions = revision_events(&self.dir.events(), log.events())?;
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
        let number = self.next_number(revisions.last())?;
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
            &analysis,
        )?;
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
    fn next_number(&self, last: Option<&RevisionEvent>) -> Result<RevisionNumber, Error> {
        match last {
            Some(last) => last.number.next().ok_or_else(|| Error::InconsistentLog {
                path: self.dir.events(),
                detail: "revision numbers exhausted".to_owned(),
            }),
            None => Ok(RevisionNumber::FIRST),
        }
    }

    /// Appends `revision` to the log under its lock, the commit point of recording it: pins
    /// both snapshots and publishes the analysis first, which are idempotent, so a crash before
    /// the append leaves nothing a later recording does not redo.
    fn append_revision(
        &self,
        log: &mut LogWriter<'_>,
        revision: RevisionEvent,
        analysis: &Analysis,
    ) -> Result<(), Error> {
        // The target may lie beyond the merge base, reachable from neither snapshot.
        let mut pins = vec![
            (self.pin_ref(&revision.base), revision.base.clone()),
            (self.pin_ref(&revision.head), revision.head.clone()),
        ];
        if let Some(target) = &revision.target {
            pins.push((self.pin_ref(target), target.clone()));
        }
        self.repo.update_refs(&pins)?;
        let analysis_path = self.dir.analysis(&revision.base, &revision.head);
        if !analysis_path.exists() {
            store::write_json_atomic(&analysis_path, analysis, "analysis")?;
        }
        log.append(Event::Revision(revision))
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
        if let Some(remembered) = self.memo.analysis(&(base.clone(), head.clone())) {
            return Ok(remembered);
        }
        let path = self.dir.analysis(base, head);
        // A cache file that fails to decode is recomputed rather than trusted.
        if let Ok(Some(cached)) = store::read_json::<Analysis>(&path)
            && cached.format == ANALYSIS_FORMAT
            && &cached.base == base
            && &cached.head == head
        {
            return Ok(Arc::new(cached));
        }
        progress(RefreshStep::Analyzing);
        let analysis = analyze(&self.repo, base, head)?;
        if publish == Publish::Now {
            store::write_json_atomic(&path, &analysis, "analysis")?;
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
        let path = self
            .dir
            .correspondence([&older.base, &older.head], [&newer.base, &newer.head]);
        if let Ok(Some(cached)) = store::read_json::<Correspondence>(&path)
            && cached.format == CORRESPONDENCE_FORMAT
            && cached.analysis_format == ANALYSIS_FORMAT
        {
            return Ok(cached);
        }
        let computed = correspond(&self.repo, older, newer)?;
        if publish == Publish::Now {
            store::write_json_atomic(&path, &computed, "correspondence")?;
        }
        Ok(computed)
    }

    /// Reads the log under a shared lock and derives the full history.
    ///
    /// # Errors
    ///
    /// Returns an error when the log is corrupt or an analysis cannot be computed.
    pub fn history(&self) -> Result<History, Error> {
        let mut tail = LogTail::new(&self.dir);
        let events = tail.poll()?;
        self.history_of(events, tail.offset(), None)
    }

    /// Derives the history from the log's `events`, read when the log was `version` bytes long,
    /// followed by the provisional revision `live` when there is one.
    ///
    /// Analyses and correspondences come from the memo when the last history used them, then
    /// from the cache; a provisional revision's are computed but never written, since nothing
    /// may refer to a revision that is not recorded.
    #[tracing::instrument(name = "history", skip_all)]
    fn history_of(
        &self,
        mut events: Vec<Event>,
        version: u64,
        live: Option<RevisionEvent>,
    ) -> Result<History, Error> {
        let provisional = live.is_some();
        events.extend(live.map(Event::Revision));
        let revisions = revision_events(&self.dir.events(), &events)?;
        let publish = |index: usize| {
            if provisional && index + 1 == revisions.len() {
                Publish::UnderLock
            } else {
                Publish::Now
            }
        };
        let mut analyses = Vec::with_capacity(revisions.len());
        let analysing = tracing::info_span!("analyses", revisions = revisions.len()).entered();
        for (index, revision) in revisions.iter().enumerate() {
            let pair = (revision.base.clone(), revision.head.clone());
            analyses.push((
                pair,
                self.analysis(&revision.base, &revision.head, publish(index), &mut |_| {})?,
            ));
        }
        drop(analysing);
        let mut correspondences = Vec::with_capacity(revisions.len().saturating_sub(1));
        let proving = tracing::info_span!("correspondences").entered();
        for (index, window) in analyses.windows(2).enumerate() {
            let [(older_pair, older), (newer_pair, newer)] = window else {
                return Err(Error::NoRevisions);
            };
            let correspondence = match self.memo.correspondence(older_pair, newer_pair) {
                Some(correspondence) => correspondence,
                None => Arc::new(self.correspondence(older, newer, publish(index + 1))?),
            };
            correspondences.push(correspondence);
        }
        drop(proving);
        self.memo.keep(&analyses, &correspondences);
        Ok(History::new(HistoryInput {
            events,
            analyses: analyses.into_iter().map(|(_, analysis)| analysis).collect(),
            correspondences,
            version,
            provisional,
        }))
    }

    /// Appends events under the lock.
    fn append(&self, events: Vec<Event>) -> Result<(), Error> {
        let lock = LockGuard::acquire(&self.dir.lock())?;
        let mut log = LogWriter::open(&self.dir.events(), &lock)?;
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
    /// Returns [`Error::WrongRevision`] or [`Error::UnknownUnit`] for a unit not in the view and
    /// [`Error::NotCurrentRevision`] when a newer revision exists, checked under the lock before
    /// anything is recorded, and an error when the append fails.
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
        let mut log = LogWriter::open(&self.dir.events(), &lock)?;
        self.settle(&mut log, view, true)?;
        for event in events {
            log.append(event)?;
        }
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
        let mut log = LogWriter::open(&self.dir.events(), &lock)?;
        // The comparison is derived from the log and, for a capture not recorded yet, the
        // capture itself, so a refused approval records nothing, not even the capture.
        let capture = self.unsettled(&log, view, true)?;
        let history = self.history_of(log.events().to_vec(), log.length(), capture.clone())?;
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
        if let Some(capture) = capture {
            let analysis = view
                .analysis()
                .ok_or(Error::UnknownRevision(view.number()))?;
            self.append_revision(&mut log, capture, analysis)?;
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
        log.append(event)
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
    /// and an error when the append fails.
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
        let mut log = LogWriter::open(&self.dir.events(), &lock)?;
        self.settle(&mut log, view, false)?;
        log.append(event)
    }

    /// Validates and stores a walkthrough document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Walkthrough`] when validation fails, and an error when the append fails.
    pub fn import_walkthrough(
        &self,
        history: &History,
        json: &str,
    ) -> Result<RevisionNumber, Error> {
        let event = walkthrough::validate(history, self.key(), json)?;
        let revision = event.revision;
        self.append(vec![Event::Walkthrough(event)])?;
        Ok(revision)
    }
}

/// Extracts the revision events and checks that they are numbered 1, 2, 3, … in log order.
fn revision_events(path: &Path, events: &[Event]) -> Result<Vec<RevisionEvent>, Error> {
    let revisions: Vec<RevisionEvent> = events
        .iter()
        .filter_map(|event| match event {
            Event::Revision(revision) => Some(revision.clone()),
            _ => None,
        })
        .collect();
    for (index, revision) in revisions.iter().enumerate() {
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
