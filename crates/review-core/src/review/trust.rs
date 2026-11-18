//! Trust: whether tools that can execute code from the reviewed revisions may run, and the
//! read-only copies of a snapshot's files they run in.
//!
//! Language servers and build tools read the reviewed tree's build configuration, such as a
//! `.cargo/config.toml` naming a `rustc` wrapper, so running them on untrusted code executes it.
//! No review is trusted until the reviewer decides, whatever it is of: a working tree, a local
//! branch or a pull request may hold code from anywhere.
//! For a review of a checkout, a grant covers either exactly one captured snapshot, by its
//! tree, or the whole checkout for as long as it is the same clone.
//! A review of a range or of the working tree against a fixed ref is keyed by the
//! repository's path, which another clone made there later shares, so a grant covers exactly
//! one snapshot, never the whole review.

use std::path::PathBuf;

use serde::Serialize;

use super::{Review, ReviewTarget};
use crate::error::Error;
use crate::history::RevisionRecord;
use crate::ids::ObjectId;
use crate::store;
use crate::store::events::{Event, TrustEvent, TrustScope};
use crate::store::log::LogTail;

/// Whether tools that can execute code from the reviewed revisions may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Trust {
    /// Whether such tools may run.
    pub trusted: bool,
    /// Whether the reviewer decided it, rather than the default, which is untrusted.
    pub decided: bool,
}

/// The latest decision of each scope that applies to one question.
#[derive(Default)]
struct Decisions {
    /// The latest decision without a scope, which a review of a checkout ignores.
    review: Option<bool>,
    /// The latest decision on the snapshot asked about.
    snapshot: Option<bool>,
    /// The latest decision on the whole checkout.
    checkout: Option<bool>,
    /// The latest decision without a scope or on the snapshot asked about, whichever came
    /// last.
    review_or_snapshot: Option<bool>,
}

impl Review {
    /// Returns whether tools may run on the reviewed revisions as a whole.
    ///
    /// For a review of a checkout, that is whether the reviewer trusts the whole checkout,
    /// which holds only while it is the same clone (see [`Self::verify_clone`]); a snapshot's
    /// own grant is asked with [`Self::trust_for`].
    /// A review of a range or of the working tree is trusted one snapshot at a time, so it is
    /// never trusted as a whole.
    /// For a pull request review, the reviewer's latest [`set_trusted`](Self::set_trusted)
    /// decision applies; without one, tools may not run.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be read or git fails.
    pub fn trust(&self) -> Result<Trust, Error> {
        if self.trusted_per_snapshot() {
            return Ok(Trust {
                trusted: false,
                decided: false,
            });
        }
        let decisions = self.decisions(None)?;
        if matches!(self.meta.target, ReviewTarget::Checkout { .. }) {
            return Ok(Trust {
                trusted: decisions.checkout == Some(true) && self.verify_clone()?,
                decided: decisions.checkout.is_some(),
            });
        }
        Ok(match decisions.review {
            Some(trusted) => Trust {
                trusted,
                decided: true,
            },
            None => Trust {
                trusted: false,
                decided: false,
            },
        })
    }

    /// Returns whether tools may run on the snapshot whose tree is `tree`.
    ///
    /// For a review of a checkout, a grant for exactly that snapshot covers it, and so does a
    /// grant for the whole checkout while it is the same clone; a revocation revokes the
    /// latest grant of its own scope.
    /// For a review of a range or of the working tree, only the latest decision on that
    /// snapshot applies.
    /// For a pull request review, the latest decision on that snapshot or on the whole review
    /// applies, as in [`Self::trust`].
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be read or git fails.
    pub fn trust_for(&self, tree: &ObjectId) -> Result<Trust, Error> {
        let decisions = self.decisions(Some(tree))?;
        if matches!(self.meta.target, ReviewTarget::Checkout { .. }) {
            let checkout = decisions.checkout == Some(true) && self.verify_clone()?;
            return Ok(Trust {
                trusted: decisions.snapshot == Some(true) || checkout,
                decided: decisions.snapshot.is_some() || decisions.checkout.is_some(),
            });
        }
        if self.trusted_per_snapshot() {
            return Ok(Trust {
                trusted: decisions.snapshot == Some(true),
                decided: decisions.snapshot.is_some(),
            });
        }
        Ok(match decisions.review_or_snapshot {
            Some(trusted) => Trust {
                trusted,
                decided: true,
            },
            None => Trust {
                trusted: false,
                decided: false,
            },
        })
    }

    /// Records the reviewer's decision whether tools may run on the reviewed revisions as a
    /// whole: for a review of a checkout, a grant or revocation for the whole checkout.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTarget`] for a review of a range or of the working tree, which
    /// is trusted one snapshot at a time with [`Self::set_trust`], and an error when the append
    /// fails.
    pub fn set_trusted(&self, trusted: bool) -> Result<(), Error> {
        if self.trusted_per_snapshot() {
            return Err(Error::InvalidTarget(
                "a review of a range or of the working tree is trusted one snapshot at a time"
                    .to_owned(),
            ));
        }
        let scope = matches!(self.meta.target, ReviewTarget::Checkout { .. })
            .then_some(TrustScope::Checkout);
        self.record_trust(scope, trusted)
    }

    /// Records the reviewer's decision whether tools may run on what `scope` covers: a grant,
    /// or a revocation of the latest grant of that scope.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTarget`] for [`TrustScope::Checkout`] on a review that is not of
    /// a checkout, and an error when the append fails.
    pub fn set_trust(&self, scope: TrustScope, trusted: bool) -> Result<(), Error> {
        if scope == TrustScope::Checkout
            && !matches!(self.meta.target, ReviewTarget::Checkout { .. })
        {
            return Err(Error::InvalidTarget(
                "only a review of a checkout can trust the whole checkout".to_owned(),
            ));
        }
        self.record_trust(Some(scope), trusted)
    }

    /// Whether the review is trusted one snapshot at a time: a review of a range or of the
    /// working tree, whose key another clone at the same path shares.
    #[must_use]
    pub fn trusted_per_snapshot(&self) -> bool {
        matches!(
            self.meta.target,
            ReviewTarget::Range { .. } | ReviewTarget::Worktree { .. }
        )
    }

    fn record_trust(&self, scope: Option<TrustScope>, trusted: bool) -> Result<(), Error> {
        self.append(vec![Event::Trust(TrustEvent {
            trusted,
            at: store::now_seconds(),
            scope,
        })])
    }

    /// Reads the latest decision of each scope; the snapshot's only when `tree` names one.
    fn decisions(&self, tree: Option<&ObjectId>) -> Result<Decisions, Error> {
        let events = LogTail::new(&self.dir).poll()?;
        let mut decisions = Decisions::default();
        for event in events {
            let Event::Trust(TrustEvent { trusted, scope, .. }) = event else {
                continue;
            };
            match scope {
                None => {
                    decisions.review = Some(trusted);
                    decisions.review_or_snapshot = Some(trusted);
                }
                Some(TrustScope::Checkout) => decisions.checkout = Some(trusted),
                Some(TrustScope::Snapshot { tree: decided }) if Some(&decided) == tree => {
                    decisions.snapshot = Some(trusted);
                    decisions.review_or_snapshot = Some(trusted);
                }
                Some(TrustScope::Snapshot { .. }) => {}
            }
        }
        Ok(decisions)
    }

    /// Returns a read-only copy of the files of the revision's head, for browsing it, making it
    /// on first use; see [`Self::tool_worktree`] for one tools may run in.
    ///
    /// It is a snapshot of what is reviewed: no filter driver or hook runs to make it, and
    /// nothing in it may be written, so an editor save cannot change what is being reviewed.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot write the files or they cannot be made read-only.
    pub fn worktree(&self, revision: &RevisionRecord) -> Result<PathBuf, Error> {
        let tree = self.repo.tree_of(&revision.head)?;
        self.materialize(&tree)
    }

    /// Returns the read-only copy of the files of `tree` for tools that can execute code from
    /// it, such as a language server, making it on first use, as [`Self::worktree`] does.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Untrusted`] unless [`Self::trust_for`] says tools may run on `tree`,
    /// and the errors of [`Self::worktree`].
    pub fn tool_worktree(&self, tree: &ObjectId) -> Result<PathBuf, Error> {
        if !self.trust_for(tree)?.trusted {
            return Err(Error::Untrusted(tree.clone()));
        }
        self.materialize(tree)
    }

    /// Removes the read-only copy of the files of the revision's head, if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the copy cannot be removed.
    pub fn remove_worktree(&self, revision: &RevisionRecord) -> Result<(), Error> {
        let tree = self.repo.tree_of(&revision.head)?;
        store::remove_read_only(&self.dir.worktree(&tree))
    }

    /// Returns the read-only copy of `tree`'s files, writing it first unless it exists.
    ///
    /// A copy is written in full into a fresh directory, made read-only and only then renamed
    /// into place, so one in place is always complete. One that is writable again is never
    /// reused: it is removed and written afresh.
    fn materialize(&self, tree: &ObjectId) -> Result<PathBuf, Error> {
        let path = self.dir.worktree(tree);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if metadata.is_dir() && metadata.permissions().readonly() {
                return Ok(path);
            }
            store::remove_read_only(&path)?;
        }
        let parent = self.dir.root().join("worktrees");
        store::create_dir(&parent)?;
        let id = crate::random::hex(16);
        let staging = parent.join(format!(".{tree}-{id}.tmp"));
        let index = parent.join(format!(".{tree}-{id}.index"));
        store::create_dir(&staging)?;
        let written = self
            .repo
            .write_tree_files(tree, &staging, &index)
            .and_then(|()| store::make_read_only(&staging));
        if let Err(err) = written {
            store::remove_read_only(&staging)?;
            return Err(err);
        }
        match std::fs::rename(&staging, &path) {
            Ok(()) => Ok(path),
            // Another caller put its copy in place first.
            Err(_) if path.is_dir() => {
                store::remove_read_only(&staging)?;
                Ok(path)
            }
            Err(source) => {
                let err = Error::io("rename", &path, source);
                store::remove_read_only(&staging)?;
                Err(err)
            }
        }
    }
}
