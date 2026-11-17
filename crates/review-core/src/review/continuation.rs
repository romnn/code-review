//! Continuing a pull request or range review in a checkout of its branch.
//!
//! A pull request review records only the pull request's pushed heads, so work in the checkout
//! that is not pushed yet, committed or not, never shows in it. A continuation is a review of
//! the checkout's working tree that starts from the continued review's current revision and its
//! marks, and from there follows the disk as any working-tree review does
//! ([ADR 5](../../../../adr/0005-a-checkout-continues-the-review-of-its-branch.md)).
//!
//! The continued review stays as it is: its history holds only what it fetched or resolved,
//! and pushes to it never enter the continuation, so the two never compete over which revision
//! is current. The continuation is seeded once, all at once, with a copy of the continued
//! revision, whose analysis is the same snapshot pair, so unit indices and evidence digests are
//! the same, and with each unit's review state there; from then on its marks are its own.

use std::path::Path;

use super::{Continues, Detected, Review, ReviewInfo, ReviewMeta, ReviewTarget, memo};
use crate::analysis::hex_digest;
use crate::error::Error;
use crate::git::Repo;
use crate::history::{UnitState, Verdict};
use crate::ids::{ObjectId, ReviewKey, RevisionNumber};
use crate::store::events::{Event, MarkEvent, MarkVerdict, NoteEvent, RevisionEvent, TrustEvent};
use crate::store::log::{self, LockGuard, LogTail};
use crate::store::{self, ReviewDir};
use crate::view::NoteAttachment;

impl Review {
    /// Returns the review to open for this pull request or range review, chosen by `detected`,
    /// in `checkout`: its continuation there when one exists, or when the checkout is on the
    /// reviewed branch and contains its current revision, created and seeded then; otherwise
    /// `self`, saying why the checkout is not followed.
    ///
    /// An existing continuation is reopened whatever the checkout holds now, so its marks are
    /// never stranded by a push to the continued review or a rebase of the checkout.
    /// A review of the working tree, a bare repository and a review with no revision yet are
    /// returned as they are.
    ///
    /// # Errors
    ///
    /// Returns [`Error::KeyConflict`] when the continuation's key holds another review, and an
    /// error when git, the logs or the store fail.
    pub(super) fn continued_in(
        self,
        data_dir: &Path,
        checkout: &Repo,
        detected: Detected,
    ) -> Result<(Self, Detected), Error> {
        let label = match &self.meta.target {
            ReviewTarget::PullRequest { number, .. } => format!("#{number}"),
            ReviewTarget::Range { base, head } => format!("{base}..{head}"),
            ReviewTarget::Worktree { .. } => return Ok((self, detected)),
        };
        let Some(location) = checkout.work_tree() else {
            return Ok((self, detected));
        };
        let key = continuation_key(location, self.key())?;
        let dir = ReviewDir::new(data_dir, &key);
        if let Some(stored) = store::read_json::<ReviewMeta>(&dir.meta())? {
            let Some(continues) = stored
                .continues
                .as_ref()
                .filter(|continues| &continues.review == self.key() && stored.repo == location)
            else {
                return Err(Error::KeyConflict(key.as_str().to_owned()));
            };
            let revision = continues.revision;
            let continuation = Self::load(data_dir, &key)?;
            continuation.seed(&self, revision)?;
            let detected = Detected::Continued {
                label: continues.label.clone(),
                revision,
                created: false,
            };
            return Ok((continuation, detected));
        }
        let history = self.history()?;
        let Some(revision) = history.current() else {
            return Ok((self, detected));
        };
        let record = &history.view(revision)?.record().cloned();
        let Some(record) = record else {
            return Ok((self, detected));
        };
        if let Some(why) = self.unfollowed(checkout, revision, &record.head)? {
            let detected = Detected::Unfollowed {
                rule: Box::new(detected),
                why,
            };
            return Ok((self, detected));
        }
        // The target the continued revision was compared with, unless it is gone, as a target
        // fetched before targets were pinned may be: then its merge base, which is pinned and
        // gives the same base while the checkout contains the reviewed head.
        let target = match &record.target {
            Some(target) if checkout.resolve_commit(target.as_str()).is_ok() => target,
            _ => &record.base,
        };
        let meta = ReviewMeta {
            key,
            repo: location.to_path_buf(),
            target: ReviewTarget::Worktree {
                base: target.as_str().to_owned(),
            },
            continues: Some(Continues {
                review: self.key().clone(),
                revision,
                label: label.clone(),
            }),
        };
        store::create_dir(dir.root())?;
        let lock = LockGuard::acquire(&dir.lock())?;
        // Another first open may have created it meanwhile, maybe from a later revision; its
        // record wins.
        let stored = store::read_json::<ReviewMeta>(&dir.meta())?;
        let meta = match stored {
            Some(stored)
                if stored.repo == meta.repo
                    && stored.continues.as_ref().map(|continues| &continues.review)
                        == Some(self.key()) =>
            {
                stored
            }
            Some(_) => return Err(Error::KeyConflict(meta.key.as_str().to_owned())),
            None => {
                store::write_json_atomic(&dir.meta(), &meta, "review metadata")?;
                meta
            }
        };
        let revision = meta
            .continues
            .as_ref()
            .map_or(revision, |continues| continues.revision);
        let continuation = Self {
            dir,
            repo: checkout.clone(),
            meta,
            memo: memo::Memo::default(),
        };
        continuation.seed_locked(&lock, &self, revision)?;
        drop(lock);
        let detected = Detected::Continued {
            label,
            revision,
            created: true,
        };
        Ok((continuation, detected))
    }

    /// Why `checkout` does not continue this review at `revision`, whose head is `head`, or
    /// `None` when it does: it must be on the reviewed branch, as a local branch, and its
    /// `HEAD` must contain `head`.
    fn unfollowed(
        &self,
        checkout: &Repo,
        revision: RevisionNumber,
        head: &ObjectId,
    ) -> Result<Option<String>, Error> {
        let Some(branch) = checkout.current_branch()? else {
            return Ok(Some("its HEAD is detached".to_owned()));
        };
        let wanted = match &self.meta.target {
            ReviewTarget::PullRequest { .. } => match self.info()? {
                Some(ReviewInfo::PullRequest(pull)) => pull.head_ref,
                _ => {
                    return Ok(Some(
                        "the pull request's branch is not known yet".to_owned(),
                    ));
                }
            },
            ReviewTarget::Range { head: wanted, .. } => {
                let wanted = wanted.strip_prefix("refs/heads/").unwrap_or(wanted);
                if checkout
                    .ref_target(&format!("refs/heads/{wanted}"))?
                    .is_none()
                {
                    return Ok(Some(format!(
                        "the range ends at {wanted}, not a local branch"
                    )));
                }
                wanted.to_owned()
            }
            ReviewTarget::Worktree { .. } => return Ok(None),
        };
        if branch != wanted {
            return Ok(Some(format!("it is on {branch}, not {wanted}")));
        }
        if !checkout.is_ancestor(head, "HEAD")? {
            return Ok(Some(format!(
                "its HEAD does not contain revision {revision}'s head {}",
                head.short()
            )));
        }
        Ok(None)
    }

    /// Seeds this continuation from `revision` of `continued` unless its log already holds
    /// anything, under its lock.
    fn seed(&self, continued: &Self, revision: RevisionNumber) -> Result<(), Error> {
        let lock = LockGuard::acquire(&self.dir.lock())?;
        self.seed_locked(&lock, continued, revision)
    }

    /// Writes this continuation's whole first log at once, unless it already holds anything: a
    /// copy of `revision` of `continued`, the state of each of its units there, the notes on
    /// them, and the continued review's trust, all read from one reading of its log.
    ///
    /// A unit reviewed there is marked with the same verdict; one reset or changed since it was
    /// reviewed is marked unseen, so it stays to review and nothing approves it by policy; a
    /// new unit gets no mark. The snapshots and the target are pinned and the analysis cached
    /// before the log appears, so nothing it names can be missing.
    fn seed_locked(
        &self,
        lock: &LockGuard,
        continued: &Self,
        revision: RevisionNumber,
    ) -> Result<(), Error> {
        if std::fs::metadata(self.dir.events()).is_ok_and(|metadata| metadata.len() > 0) {
            return Ok(());
        }
        let mut tail = LogTail::new(&continued.dir);
        let events = tail.poll()?;
        let trusted = match events.iter().rev().find_map(|event| match event {
            Event::Trust(trust) => Some(trust.trusted),
            _ => None,
        }) {
            Some(trusted) => trusted,
            None => continued.trusted_by_default()?,
        };
        let history = continued.history_of(events, tail.offset(), None)?;
        let view = history.view(revision)?;
        let record = view
            .record()
            .ok_or(Error::UnknownRevision(revision))?
            .clone();
        let analysis = view.analysis().ok_or(Error::UnknownRevision(revision))?;
        let at = store::now_seconds();
        let first = RevisionNumber::FIRST;
        let mut seed = vec![Event::Revision(RevisionEvent {
            number: first,
            base: record.base.clone(),
            head: record.head.clone(),
            target: record.target.clone(),
            merge_bases: record.merge_bases.clone(),
            recorded_at: at,
        })];
        for id in view.unit_ids() {
            let verdict = match view.state(id)? {
                UnitState::Reviewed { verdict, .. } => match verdict {
                    Verdict::Seen => MarkVerdict::Seen,
                    Verdict::Accepted => MarkVerdict::Accepted,
                    Verdict::Flagged => MarkVerdict::Flagged,
                },
                UnitState::Unseen { .. } | UnitState::ChangedSinceReviewed { .. } => {
                    MarkVerdict::Unseen
                }
                UnitState::New => continue,
            };
            seed.push(Event::Mark(MarkEvent {
                revision: first,
                unit: id.index(),
                evidence: view.evidence_digest(id)?,
                verdict,
                at,
            }));
        }
        for note in view.notes() {
            if let NoteAttachment::Unit { unit } = note.attached {
                seed.push(Event::Note(NoteEvent {
                    revision: first,
                    unit: unit.index(),
                    evidence: view.evidence_digest(unit)?,
                    text: note.text,
                    at: note.at,
                }));
            }
        }
        seed.push(Event::Trust(TrustEvent { trusted, at }));
        let ReviewTarget::Worktree { base: target } = &self.meta.target else {
            return Err(Error::InvalidTarget(
                "a continuation reviews a working tree".to_owned(),
            ));
        };
        let target = ObjectId::parse(target)?;
        for pinned in [&record.base, &record.head, &target] {
            self.repo.update_ref(&self.pin_ref(pinned), pinned)?;
        }
        let analysis_path = self.dir.analysis(&record.base, &record.head);
        if !analysis_path.exists() {
            store::write_json_atomic(&analysis_path, analysis, "analysis")?;
        }
        log::publish_initial(&self.dir, lock, &seed)?;
        Ok(())
    }
}

/// The key of `continued`'s continuation in the checkout at `location`: a digest of the two,
/// so it stays short however long the path and the continued key are, under a prefix no key a
/// target forms has. The metadata keeps both, and opening checks them.
fn continuation_key(location: &Path, continued: &ReviewKey) -> Result<ReviewKey, Error> {
    let tuple = format!("{}\0{}", location.display(), continued.as_str());
    let digest = hex_digest(tuple.as_bytes());
    let short = digest.get(..32).unwrap_or(&digest);
    ReviewKey::parse(&format!("continuation:{short}"))
        .map_err(|err| Error::InvalidTarget(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A continuation's key has the same short length however long the checkout's path and
    /// the continued key are, so it fits wherever the continued review's own key does.
    #[test]
    fn continuation_keys_stay_short() {
        let continued = ReviewKey::parse(&format!("{}:main..feature", "a".repeat(400))).unwrap();
        let short = continuation_key(Path::new("/checkout"), &continued).unwrap();
        let long = continuation_key(&Path::new("/").join("b".repeat(400)), &continued).unwrap();
        assert_eq!(short.as_str().len(), long.as_str().len());
        assert!(short.as_str().len() < 64, "{short}");
        assert_ne!(short, long);
    }
}
