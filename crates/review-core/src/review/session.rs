//! Sessions: a running app, or one recording, that pins the captures it shows until it is done.
//!
//! A capture of a working tree is a synthetic commit no branch reaches, so whoever shows it pins
//! it, under `refs/live/<session>/<capture>/` in the review's archive, where `<capture>` names
//! its head, base and target.
//! A session holds the review open through its archive, so nothing compacts the archive while
//! it runs; the pins of one that died without removing them go with the next open that has the
//! review to itself.

use super::{Review, RevisionInput};
use crate::analysis::hex_digest;
use crate::error::Error;
use crate::git::PrivateRepo;
use crate::history::RevisionRecord;
use crate::ids::ObjectId;

/// A running app, or one recording, that pins the captures it shows under
/// `refs/live/<session>/<capture>/` in the review's archive for as long as it lives.
///
/// Dropping it removes its pins.
#[derive(Debug)]
pub struct Session {
    id: String,
    /// The prefix of its pins, `refs/live/<id>`.
    pins: String,
    /// The review's archive, which holds the review open for as long as the session lives.
    archive: PrivateRepo,
}

impl Session {
    /// Returns the session's identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the prefix of the session's pins; see [`Self::pins_of`].
    #[must_use]
    pub fn pins(&self) -> &str {
        &self.pins
    }

    /// Returns the prefix of the refs that pin the capture `record` is of, under
    /// [`Self::pins`]: they are named for a digest of its head, base and target, so two
    /// captures that share a head but not a base, as when the base moved, each keep their own.
    #[must_use]
    pub fn pins_of(&self, record: &RevisionRecord) -> String {
        format!(
            "{}/{}",
            self.pins,
            capture_name(&record.base, &record.head, record.target.as_ref())
        )
    }

    /// Returns whether the session pins its captures in `archive`.
    pub(super) fn pins_in(&self, archive: &PrivateRepo) -> bool {
        self.archive.path() == archive.path()
    }

    /// Pins the base and the head of `input` until [`Self::release`] or the end of the session.
    pub(super) fn pin(&self, input: &RevisionInput) -> Result<(), Error> {
        let capture = format!(
            "{}/{}",
            self.pins,
            capture_name(&input.base, &input.head, input.target.as_ref())
        );
        self.archive.update_refs(&[
            (format!("{capture}/base"), input.base.clone()),
            (format!("{capture}/head"), input.head.clone()),
        ])
    }

    /// Removes the pins of every capture but those `keep` records.
    pub(super) fn release(&self, keep: &[RevisionRecord]) -> Result<(), Error> {
        let prefix = format!("{}/", self.pins);
        let kept: Vec<String> = keep
            .iter()
            .map(|record| capture_name(&record.base, &record.head, record.target.as_ref()))
            .collect();
        let names: Vec<String> = self
            .archive
            .repo()
            .refs_under(&prefix)?
            .into_iter()
            .map(|pin| pin.name)
            .filter(|name| {
                let capture = name
                    .strip_prefix(&prefix)
                    .and_then(|rest| rest.split('/').next());
                capture.is_none_or(|capture| !kept.iter().any(|kept| kept == capture))
            })
            .collect();
        self.archive.delete_refs(&names)
    }
}

/// The prefix of every session's pins in a review's archive.
pub(super) const LIVE_PINS: &str = "refs/live";

/// The name of a capture's pins: 16 hex digits of a digest of its head, base and target.
///
/// A digest rather than the ids themselves keeps ref paths, below the review's directory, within
/// Windows' path length limit.
fn capture_name(base: &ObjectId, head: &ObjectId, target: Option<&ObjectId>) -> String {
    let joined = match target {
        Some(target) => format!("{head}-{base}-{target}"),
        None => format!("{head}-{base}"),
    };
    let mut name = hex_digest(joined.as_bytes());
    name.truncate(16);
    name
}

impl Drop for Session {
    fn drop(&mut self) {
        // A pin left behind only keeps objects alive, and the next open on its own removes it.
        if let Ok(pins) = self.archive.repo().refs_under(&format!("{}/", self.pins)) {
            let names: Vec<String> = pins.into_iter().map(|pin| pin.name).collect();
            if let Err(err) = self.archive.delete_refs(&names) {
                drop(err);
            }
        }
    }
}

impl Review {
    /// Starts a session that pins the captures it shows (see [`Self::live_history`]) for as
    /// long as it lives.
    #[must_use]
    pub fn start_session(&self) -> Session {
        let id = crate::random::hex(16);
        Session {
            pins: format!("{LIVE_PINS}/{id}"),
            id,
            archive: self.archive.clone(),
        }
    }
}
