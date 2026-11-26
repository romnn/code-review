//! The review's archive: a private repository holding every snapshot the review recorded, with
//! all it reaches, pinned by its refs, so the reviewed repository is only read.
//!
//! Every snapshot the log records is in the archive and pinned there, which every open checks
//! under the review's lock, importing from the reviewed repository any commit it lacks and
//! pinning any snapshot without its pin.
//! So a review an earlier version kept moves into its archive the first time it opens, from
//! the reviewed repository, where that version's refs still pin every snapshot, and a revision
//! such a version still running records later follows on the next open.
//! The first move counts only once the archive alone derives exactly the history the reviewed
//! repository does with the review's cache, which is what the reviewer saw; until then the
//! review opens as nothing, and the log and the reviewed repository stay as they were, so the
//! move is tried again on the next open.

use std::collections::HashSet;
use std::sync::Arc;

use super::{Review, cached_analysis, cached_correspondence, revision_events};
use crate::analysis::analyze;
use crate::correspond::correspond;
use crate::error::Error;
use crate::git::{PrivateRepo, Repo};
use crate::history::{History, HistoryInput, Links};
use crate::ids::ObjectId;
use crate::store::ReviewDir;
use crate::store::events::Event;
use crate::store::log::{self, LockGuard, Prefix};

/// The file whose presence says the review moved into its archive.
const CUTOVER: &str = "cutover";

impl Review {
    /// Opens the archive of the review in `dir`, under its `lock`, creating it on first use,
    /// importing from `repo` every snapshot the log records whose commit it lacks, and pinning
    /// every one it does not pin.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingObject`] when `repo` lacks an object such a snapshot reaches,
    /// [`Error::ArchiveMismatch`] when the archive derives another history than `repo` on the
    /// first move, and an error when git or the log fails; the log and `repo` are left as they
    /// were.
    #[tracing::instrument(skip_all)]
    pub(super) fn open_archive(
        dir: &ReviewDir,
        repo: &Repo,
        lock: &LockGuard,
    ) -> Result<PrivateRepo, Error> {
        let archive = PrivateRepo::open(&dir.archive(), repo)?;
        let (events, bytes) = log::read_complete(&dir.events(), lock)?;
        let revisions = revision_events(&dir.events(), &events, 0)?;
        let mut snapshots: Vec<ObjectId> = revisions
            .iter()
            .flat_map(|revision| [revision.base.clone(), revision.head.clone()])
            .collect();
        snapshots.sort();
        snapshots.dedup();
        let absent = archive.repo().missing(&snapshots)?;
        let cutover = dir.archive().join(CUTOVER);
        let moved = cutover.exists();
        // The archive reads alone from here on, with no way to the reviewed repository's
        // objects, so these checks see what a review will. A commit here whose tree or blobs
        // are not, as after a crash lost part of the archive, has them copied again.
        let checked = if moved { &absent } else { &snapshots };
        tracing::info_span!("import", snapshots = absent.len()).in_scope(|| {
            archive.import(repo, &absent)?;
            match archive.verify(checked) {
                Err(Error::MissingObject(_)) => {
                    archive.restore(repo, checked)?;
                    archive.verify(checked)
                }
                verified => verified,
            }
        })?;
        // A pin can be lost while its objects stay, as when its publication was cut short.
        let pinned: HashSet<ObjectId> = archive
            .repo()
            .refs_under("refs/snap/")?
            .into_iter()
            .map(|pin| pin.target)
            .collect();
        let pins: Vec<(String, ObjectId)> = snapshots
            .iter()
            .filter(|oid| !pinned.contains(*oid))
            .map(|oid| (pin_ref(oid), oid.clone()))
            .collect();
        archive.update_refs(&pins)?;
        if moved {
            return Ok(archive);
        }
        if !revisions.is_empty() {
            let prefix = Prefix::of(&bytes);
            let seen = tracing::info_span!("replay", from = "repository")
                .in_scope(|| replay(repo, Some(dir), events.clone(), prefix))?;
            let derived = tracing::info_span!("replay", from = "archive")
                .in_scope(|| replay(archive.repo(), None, events, prefix))?;
            if derived != seen {
                return Err(Error::ArchiveMismatch {
                    path: dir.root().to_path_buf(),
                });
            }
        }
        crate::store::write_atomic(&cutover, b"")?;
        Ok(archive)
    }
}

/// The ref of the archive that pins snapshot `oid` for the review's recorded history.
pub(super) fn pin_ref(oid: &ObjectId) -> String {
    format!("refs/snap/{oid}")
}

/// Derives the history of `events`, read up to `prefix`, from `repo`'s objects, with the
/// analyses and correspondences the cache in `cache` holds where there is one, and writing
/// nothing.
fn replay(
    repo: &Repo,
    cache: Option<&ReviewDir>,
    events: Vec<Event>,
    prefix: Prefix,
) -> Result<History, Error> {
    History::empty().extended(HistoryInput {
        events,
        prefix,
        provisional: false,
        analysis: &|revision| {
            let span = tracing::info_span!("analysis", cached = tracing::field::Empty);
            let _entered = span.enter();
            let cached = cache.and_then(|dir| cached_analysis(dir, &revision.base, &revision.head));
            span.record("cached", cached.is_some());
            match cached {
                Some(cached) => Ok(Arc::new(cached)),
                None => Ok(Arc::new(analyze(repo, &revision.base, &revision.head)?)),
            }
        },
        links: &|_, older, newer| {
            let span = tracing::info_span!("links", cached = tracing::field::Empty);
            let _entered = span.enter();
            let cached = cache.and_then(|dir| cached_correspondence(dir, older, newer));
            span.record("cached", cached.is_some());
            let correspondence = match cached {
                Some(cached) => cached,
                None => correspond(repo, older, newer)?,
            };
            Ok(Arc::new(Links::new(
                &correspondence,
                older.units.len(),
                newer.units.len(),
            )))
        },
    })
}
