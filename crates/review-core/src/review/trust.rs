//! Trust: whether tools that run the code under review, such as language servers, may run in
//! the checkout.
//!
//! A language server runs in the checkout itself with its builds on, so build scripts, proc
//! macros and the workspace's configuration, such as a `.cargo/config.toml` naming a `rustc`
//! wrapper, can run any program and write in the checkout.
//! No checkout is trusted until the reviewer decides, whatever it holds: a working tree or a
//! local branch may hold code from anywhere.
//! The review of a checkout holds the decision, one per working tree and clone, which covers
//! later edits and branch switches until it is revoked.
//! A review of the working tree against a fixed ref shares it ([`Review::checkout_review`]); a
//! review of a range or a pull request has no code on disk to trust.

use super::{Review, ReviewTarget};
use crate::error::Error;
use crate::ids::ObjectId;
use crate::store;
use crate::store::events::{Event, TrustEvent, TrustScope};
use crate::store::log::LogTail;

impl Review {
    /// Returns whether tools may run in the checkout: the reviewer's latest decision on it,
    /// while it is the same clone (see [`Self::verify_clone`]).
    ///
    /// Only the review of a checkout is ever trusted.
    /// Decisions of other scopes, read from older logs, grant nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be read or git fails.
    pub fn trusted(&self) -> Result<bool, Error> {
        if !matches!(self.meta.target, ReviewTarget::Checkout { .. }) {
            return Ok(false);
        }
        let mut trusted = false;
        for event in LogTail::new(&self.dir).poll()? {
            if let Event::Trust(TrustEvent {
                trusted: decided,
                scope: Some(TrustScope::InCheckout),
                ..
            }) = event
            {
                trusted = decided;
            }
        }
        Ok(trusted && self.verify_clone()?)
    }

    /// Records the reviewer's decision whether tools may run in the checkout: a grant, or its
    /// revocation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTarget`] for a review that is not of a checkout, which holds no
    /// trust, and an error when the append fails.
    pub fn set_trusted(&self, trusted: bool) -> Result<(), Error> {
        if !matches!(self.meta.target, ReviewTarget::Checkout { .. }) {
            return Err(Error::InvalidTarget(
                "only the review of a checkout holds trust".to_owned(),
            ));
        }
        self.append(vec![Event::Trust(TrustEvent {
            trusted,
            at: store::now_seconds(),
            scope: Some(TrustScope::InCheckout),
        })])
    }

    /// Returns the review that holds the trust in this review's checkout: this review for a
    /// checkout's, and for a review of the working tree against a fixed ref, the review of the
    /// checkout of the clone there now, in the same data directory.
    ///
    /// Returns `None` for a review of a range or a pull request, whose code is not on disk.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkout's review cannot be opened, as
    /// [`Self::open_checkout`] says.
    pub fn checkout_review(&self) -> Result<Option<Self>, Error> {
        match &self.meta.target {
            ReviewTarget::Checkout { .. } => Ok(Some(self.clone())),
            ReviewTarget::Worktree { .. } => {
                Self::open_checkout(self.dir.data_dir(), self.repo.clone()).map(Some)
            }
            ReviewTarget::PullRequest { .. } | ReviewTarget::Range { .. } => Ok(None),
        }
    }

    /// Returns the tree the checkout's working tree holds now, as a capture would record it,
    /// with every filter driver off, recording and pinning nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CloneReplaced`] when the checkout of a review of a checkout is another
    /// clone now, [`Error::GitBusy`] while git is busy in it, and an error when git fails.
    pub fn tree_on_disk(&self) -> Result<ObjectId, Error> {
        if !self.verify_clone()? {
            return Err(Error::CloneReplaced);
        }
        if let Some(busy) = self.repo.busy()? {
            return Err(Error::GitBusy(busy));
        }
        self.repo.capture_tree()
    }
}
