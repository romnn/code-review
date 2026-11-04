//! Reviews: what is reviewed, recording revisions, and every state transition.
//!
//! All state transitions are serialized by `review.lock` and committed by appending one event to
//! the log.
//! Analyses and correspondences are content-addressed caches that can be recomputed from git at any
//! time.

mod github;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::analysis::{ANALYSIS_FORMAT, Analysis, analyze};
use crate::correspond::{CORRESPONDENCE_FORMAT, Correspondence, correspond};
use crate::error::Error;
use crate::git::Repo;
use crate::history::{History, HistoryInput, RevisionRecord};
use crate::ids::{MoveId, ObjectId, ReviewKey, RevisionNumber, UnitId};
use crate::store::events::{Event, MarkEvent, MarkVerdict, NoteEvent, RevisionEvent, TrustEvent};
use crate::store::log::{LockGuard, LogTail, LogWriter};
use crate::store::{self, ReviewDir};
use crate::view::RevisionView;
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
    /// The working tree, captured as a snapshot, against a ref.
    Worktree {
        /// The target ref.
        base: String,
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
    /// A local review is keyed by the working tree and its refs.
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
}

/// Who writes a freshly computed analysis to the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Publish {
    /// The caller publishes it under the lock, as part of recording a revision.
    UnderLock,
    /// Write it right away.
    Now,
}

/// A review: its storage, its repository and its target.
#[derive(Debug, Clone)]
pub struct Review {
    dir: ReviewDir,
    repo: Repo,
    meta: ReviewMeta,
}

impl Review {
    /// Opens the review of `target` in `repo`, creating its storage on first use.
    ///
    /// # Errors
    ///
    /// Returns an error when the storage cannot be created.
    pub fn open(data_dir: &Path, repo: Repo, target: ReviewTarget) -> Result<Self, Error> {
        let key = target.key(&repo)?;
        let dir = ReviewDir::new(data_dir, &key);
        let meta = ReviewMeta {
            key,
            repo: repo.work_tree().unwrap_or(repo.git_dir()).to_path_buf(),
            target,
        };
        store::create_dir(dir.root())?;
        if store::read_json::<ReviewMeta>(&dir.meta())?.as_ref() != Some(&meta) {
            store::write_json_atomic(&dir.meta(), &meta, "review metadata")?;
        }
        Ok(Self { dir, repo, meta })
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
        Ok(Self { dir, repo, meta })
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
    /// # Errors
    ///
    /// Returns an error when the target cannot be resolved or fetched, or recording fails.
    pub fn refresh(&self) -> Result<Recorded, Error> {
        match &self.meta.target {
            ReviewTarget::Range { base, head } => {
                let target = self.repo.resolve_commit(base)?;
                let head = self.repo.resolve_commit(head)?;
                let remote = self.repo.default_remote()?;
                self.record_snapshots(target, head, remote.as_deref())
            }
            ReviewTarget::Worktree { base } => {
                let target = self.repo.resolve_commit(base)?;
                let head = self.repo.capture_worktree()?;
                let remote = self.repo.default_remote()?;
                self.record_snapshots(target, head, remote.as_deref())
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
                let recorded = self.record_snapshots(
                    fetched.target.clone(),
                    fetched.head.clone(),
                    Some(&fetched.remote),
                );
                drop(fetched);
                recorded
            }
        }
    }

    /// Computes the merge base of `target` and `head` and records the revision.
    fn record_snapshots(
        &self,
        target: ObjectId,
        head: ObjectId,
        deepen_from: Option<&str>,
    ) -> Result<Recorded, Error> {
        let merge_bases = self.repo.merge_bases(&target, &head, deepen_from)?;
        let base = merge_bases
            .first()
            .cloned()
            .ok_or_else(|| Error::NoMergeBase {
                target: target.clone(),
                head: head.clone(),
            })?;
        self.record_revision(RevisionInput {
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
        let analysis = self.analysis(&input.base, &input.head, Publish::UnderLock)?;
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
            });
        }
        let number = match revisions.last() {
            Some(last) => last.number.next().ok_or_else(|| Error::InconsistentLog {
                path: self.dir.events(),
                detail: "revision numbers exhausted".to_owned(),
            })?,
            None => RevisionNumber::FIRST,
        };
        self.repo
            .update_ref(&self.pin_ref(&input.base), &input.base)?;
        self.repo
            .update_ref(&self.pin_ref(&input.head), &input.head)?;
        let analysis_path = self.dir.analysis(&input.base, &input.head);
        if !analysis_path.exists() {
            store::write_json_atomic(&analysis_path, analysis.as_ref(), "analysis")?;
        }
        log.append(Event::Revision(RevisionEvent {
            number,
            base: input.base.clone(),
            head: input.head.clone(),
            target: input.target,
            merge_bases: input.merge_bases.clone(),
            recorded_at: store::now_seconds(),
        }))?;
        Ok(Recorded {
            number,
            created: true,
            base: input.base,
            head: input.head,
            merge_bases: input.merge_bases,
        })
    }

    /// Loads the analysis of `(base, head)` from the cache, or computes it and publishes it
    /// unless the caller publishes it itself.
    fn analysis(
        &self,
        base: &ObjectId,
        head: &ObjectId,
        publish: Publish,
    ) -> Result<Arc<Analysis>, Error> {
        let path = self.dir.analysis(base, head);
        // A cache file that fails to decode is recomputed rather than trusted.
        if let Ok(Some(cached)) = store::read_json::<Analysis>(&path)
            && cached.format == ANALYSIS_FORMAT
            && &cached.base == base
            && &cached.head == head
        {
            return Ok(Arc::new(cached));
        }
        let analysis = analyze(&self.repo, base, head)?;
        if publish == Publish::Now {
            store::write_json_atomic(&path, &analysis, "analysis")?;
        }
        Ok(Arc::new(analysis))
    }

    fn correspondence(&self, older: &Analysis, newer: &Analysis) -> Result<Correspondence, Error> {
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
        store::write_json_atomic(&path, &computed, "correspondence")?;
        Ok(computed)
    }

    /// Reads the log under a shared lock and derives the full history.
    ///
    /// # Errors
    ///
    /// Returns an error when the log is corrupt or an analysis cannot be computed.
    pub fn history(&self) -> Result<History, Error> {
        let events = LogTail::new(&self.dir).poll()?;
        let revisions = revision_events(&self.dir.events(), &events)?;
        let analyses = revisions
            .iter()
            .map(|revision| self.analysis(&revision.base, &revision.head, Publish::Now))
            .collect::<Result<Vec<_>, _>>()?;
        let correspondences = analyses
            .windows(2)
            .map(|pair| match pair {
                [older, newer] => self.correspondence(older, newer),
                _ => Err(Error::NoRevisions),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(History::new(HistoryInput {
            events,
            analyses,
            correspondences,
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
        let current = revision_events(&self.dir.events(), log.events())?
            .last()
            .map(|revision| revision.number);
        if current != Some(view.number()) {
            return Err(Error::NotCurrentRevision {
                revision: view.number(),
                current,
            });
        }
        for event in events {
            log.append(event)?;
        }
        Ok(())
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
        self.append(vec![event])
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

    /// Returns whether tools that can execute code from the reviewed revisions may run.
    ///
    /// Language servers and build tools read the reviewed tree's build configuration, such as a
    /// `.cargo/config.toml` naming a `rustc` wrapper, so running them on untrusted code executes
    /// it.
    /// The reviewer's latest [`set_trusted`](Self::set_trusted) decision applies.
    /// Without one, the working tree and a range whose head is a local branch are trusted, and a
    /// pull request or any other range is not.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be read or git fails.
    pub fn trust(&self) -> Result<Trust, Error> {
        let events = LogTail::new(&self.dir).poll()?;
        let decided = events.iter().rev().find_map(|event| match event {
            Event::Trust(trust) => Some(trust.trusted),
            _ => None,
        });
        Ok(match decided {
            Some(trusted) => Trust {
                trusted,
                decided: true,
            },
            None => Trust {
                trusted: self.trusted_by_default()?,
                decided: false,
            },
        })
    }

    /// Records the reviewer's decision whether tools may execute code from the reviewed
    /// revisions.
    ///
    /// # Errors
    ///
    /// Returns an error when the append fails.
    pub fn set_trusted(&self, trusted: bool) -> Result<(), Error> {
        self.append(vec![Event::Trust(TrustEvent {
            trusted,
            at: store::now_seconds(),
        })])
    }

    fn trusted_by_default(&self) -> Result<bool, Error> {
        match &self.meta.target {
            ReviewTarget::PullRequest { .. } => Ok(false),
            ReviewTarget::Worktree { .. } => Ok(true),
            // A remote-tracking ref or a bare commit may hold someone else's code.
            ReviewTarget::Range { head, .. } => {
                let branch = head.strip_prefix("refs/heads/").unwrap_or(head);
                Ok(self
                    .repo
                    .ref_target(&format!("refs/heads/{branch}"))?
                    .is_some())
            }
        }
    }

    /// Returns a detached worktree at the revision's head, creating it on first use.
    ///
    /// Front ends open it read-only; it is a snapshot of what is reviewed.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot create the worktree.
    pub fn worktree(&self, revision: &RevisionRecord) -> Result<PathBuf, Error> {
        let path = self.dir.worktree(&revision.head);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                store::create_dir(parent)?;
            }
            self.repo.add_worktree(&path, &revision.head)?;
        }
        Ok(path)
    }

    /// Removes the browsing worktree of a revision, if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot remove the worktree.
    pub fn remove_worktree(&self, revision: &RevisionRecord) -> Result<(), Error> {
        let path = self.dir.worktree(&revision.head);
        if path.exists() {
            self.repo.remove_worktree(&path)?;
        }
        Ok(())
    }
}

/// Whether tools that can execute code from the reviewed revisions may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Trust {
    /// Whether such tools may run.
    pub trusted: bool,
    /// Whether the reviewer decided it, rather than the default for the review's target.
    pub decided: bool,
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
