//! The analysis cache: each review's analyses and correspondences under its `analysis/`,
//! recomputable from its archive at any time, which every review of a data directory keeps
//! within one budget by removing the entries used least recently.
//!
//! An entry's modification time is when it was last used: written, or read and usable.
//! Writing an entry and keeping the caches within the budget are one step under the data
//! directory's `cache.lock`, so no entry written meanwhile escapes the count.
//! A failure to read, write or evict an entry only costs computing it again, so none fails
//! what it was for.
//! The cache of a review whose move into its archive is unfinished is what the move compares
//! with, so nothing removes or rewrites it.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::archive::CUTOVER;
use crate::error::Error;
use crate::store::{self, ReviewDir};

/// How many bytes the analysis caches of one data directory hold at most.
pub(super) const BUDGET: u64 = 256 << 20;

/// Reads the entry at `path` of the cache of the review in `dir` when it decodes and `usable`
/// accepts it, and marks it used.
///
/// An entry this version cannot use is removed, so the next one computed takes its place,
/// unless the review's move into its archive is unfinished.
pub(super) fn read<T: DeserializeOwned>(
    dir: &ReviewDir,
    path: &Path,
    usable: impl FnOnce(&T) -> bool,
) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<T>(&bytes) {
        Ok(entry) if usable(&entry) => {
            touch(path);
            Some(entry)
        }
        _ => {
            if moved(dir.archive().as_path())
                && let Err(err) = std::fs::remove_file(path)
            {
                drop(err);
            }
            None
        }
    }
}

/// Writes `entry` to `path` in the cache of the review in `dir`, unless it alone exceeds the
/// budget, and keeps the data directory's caches within the budget.
pub(super) fn install<T: Serialize>(dir: &ReviewDir, path: &Path, entry: &T) {
    let Ok(bytes) = serde_json::to_vec(entry) else {
        return;
    };
    if u64::try_from(bytes.len()).is_ok_and(|size| size > BUDGET) {
        return;
    }
    let installed = locked(dir.data_dir()).and_then(|lock| {
        store::write_atomic(path, &bytes)?;
        evict_locked(dir.data_dir(), BUDGET)?;
        drop(lock);
        Ok(())
    });
    if let Err(err) = installed {
        drop(err);
    }
}

/// Removes the entries of the reviews' caches in `data_dir` used least recently until they
/// hold `budget` bytes at most.
pub(super) fn evict(data_dir: &Path, budget: u64) -> Result<(), Error> {
    let lock = locked(data_dir)?;
    evict_locked(data_dir, budget)?;
    drop(lock);
    Ok(())
}

/// Blocks until `cache.lock` in `data_dir` is held.
fn locked(data_dir: &Path) -> Result<File, Error> {
    let path = data_dir.join("cache.lock");
    let lock = store::open_lock(&path)?;
    lock.lock()
        .map_err(|source| Error::io("lock", path, source))?;
    Ok(lock)
}

/// Whether the review whose archive is at `archive` moved into it; see [`CUTOVER`].
fn moved(archive: &Path) -> bool {
    archive.join(CUTOVER).exists()
}

/// Marks the entry at `path` used now.
fn touch(path: &Path) {
    let touched = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()));
    if let Err(err) = touched {
        drop(err);
    }
}

/// An entry of a review's cache, with when it was last used.
struct Entry {
    path: PathBuf,
    used: SystemTime,
    bytes: u64,
}

/// Does what [`evict`] does, under `cache.lock`, which the caller holds; a cache a symbolic
/// link leads out of the data directory to is left alone.
fn evict_locked(data_dir: &Path, budget: u64) -> Result<(), Error> {
    let mut entries = Vec::new();
    for review in store::entries(&data_dir.join("reviews"))? {
        let cache = review.join("analysis");
        if !moved(&review.join("archive")) || !store::contained(data_dir, &cache) {
            continue;
        }
        for file in store::entries(&cache)? {
            // An entry removed meanwhile holds nothing.
            let Ok(metadata) = std::fs::symlink_metadata(&file) else {
                continue;
            };
            if metadata.is_file() {
                entries.push(Entry {
                    used: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    bytes: metadata.len(),
                    path: file,
                });
            }
        }
    }
    entries.sort_by_key(|entry| entry.used);
    let mut total: u64 = entries.iter().map(|entry| entry.bytes).sum();
    for entry in entries {
        if total <= budget {
            break;
        }
        match std::fs::remove_file(&entry.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::io("remove", entry.path, source)),
        }
        total = total.saturating_sub(entry.bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use color_eyre::eyre;

    use super::*;
    use crate::analysis::Analysis;
    use crate::review::Review;
    use crate::review::retention::tests::Recorded;
    use crate::testing::git;

    /// Eviction removes the entries used least recently until the caches fit their budget,
    /// across reviews, a read counting as a use, and keeps the cache of a review whose move
    /// into its archive is unfinished.
    #[test]
    fn eviction_removes_what_was_used_least_recently() -> eyre::Result<()> {
        let data = tempfile::tempdir()?;
        let moved = data.path().join("reviews/moved");
        std::fs::create_dir_all(moved.join("archive"))?;
        std::fs::write(moved.join("archive").join(CUTOVER), b"")?;
        let review = ReviewDir::listed(data.path(), moved.clone());
        // A review whose move is unfinished keeps even an entry this version cannot read.
        let unmoved = data.path().join("reviews/unmoved/analysis/kept.json");
        std::fs::create_dir_all(data.path().join("reviews/unmoved/analysis"))?;
        std::fs::write(&unmoved, format!("{:<100}", "{"))?;
        let baseline = ReviewDir::listed(data.path(), data.path().join("reviews/unmoved"));
        assert_eq!(read::<u32>(&baseline, &unmoved, |_| true), None);
        assert!(unmoved.exists());
        let entries: Vec<PathBuf> = ["first", "second", "third"]
            .iter()
            .map(|name| moved.join("analysis").join(format!("{name}.json")))
            .collect();
        std::fs::create_dir_all(moved.join("analysis"))?;
        let then = SystemTime::now() - Duration::from_secs(3600);
        for (age, entry) in (1..).zip(&entries) {
            std::fs::write(entry, format!("{:<100}", 0))?;
            let file = std::fs::OpenOptions::new().write(true).open(entry)?;
            file.set_modified(then + Duration::from_secs(age))?;
        }
        // The first entry is read, which makes the second the one used least recently.
        assert_eq!(read::<u32>(&review, &entries[0], |_| true), Some(0));

        evict(data.path(), 200)?;
        assert!(entries[0].exists());
        assert!(!entries[1].exists());
        assert!(entries[2].exists());
        assert!(unmoved.exists());
        Ok(())
    }

    /// Marks, notes and revisions recorded while another process evicts every cached analysis
    /// and correspondence as fast as it can leave a log whose history read from the start is
    /// the one read as they were saved.
    #[test]
    fn records_racing_eviction_replay_the_same() -> eyre::Result<()> {
        let recorded = Recorded::new(1)?;
        let data = recorded.data.path();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            // Evicts until told to stop, for a minute at most, and stops at its first error.
            let evicting = scope.spawn(|| {
                let started = std::time::Instant::now();
                while !stop.load(Ordering::Relaxed) && started.elapsed() < Duration::from_secs(60) {
                    evict(data, 0)?;
                }
                Ok::<_, Error>(())
            });
            let recording = (2..5).try_for_each(|revision| -> eyre::Result<()> {
                recorded.record(revision)?;
                let history = recorded.review.history()?;
                let view = history.current_view()?;
                recorded.review.note(&view, view.unit_ids()[0], "why?")?;
                Ok(())
            });
            stop.store(true, Ordering::Relaxed);
            let evicted = evicting
                .join()
                .map_err(|_| eyre::eyre!("eviction panicked"))?;
            recording?;
            eyre::Ok(evicted?)
        })?;
        let history = recorded.review.history()?;
        assert_eq!(
            Review::load(data, recorded.review.key())?.history()?,
            history
        );
        Ok(())
    }

    /// A cached correspondence is not used with an analysis computed again whose units are
    /// numbered otherwise, as after its cache entry was evicted: marks carry by the code the
    /// units hold, never by a stale link.
    #[test]
    fn a_link_never_outlives_the_analyses_it_indexes() -> eyre::Result<()> {
        let recorded = Recorded::new(1)?;
        let repo = recorded.repo.path();
        // Revision 2 changes only the first function again, so the marks of the others carry.
        let library = std::fs::read_to_string(repo.join("lib.rs"))?;
        std::fs::write(
            repo.join("lib.rs"),
            library.replacen("    10\n", "    99\n", 1),
        )?;
        git(repo, &["commit", "--quiet", "--no-verify", "-am", "again"])?;
        recorded.review.refresh()?;
        let history = recorded.review.history()?;
        let revision = history
            .revisions()
            .last()
            .map(|&revision| revision.clone())
            .ok_or_else(|| eyre::eyre!("no revision"))?;
        let dir = &recorded.review.dir;
        let path = dir.analysis(&revision.base, &revision.head);

        // The analysis computed again numbers its first two units the other way round.
        let mut analysis: Analysis = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(analysis.units.len(), 4);
        analysis.units.swap(0, 1);
        analysis.files[0].units.swap(0, 1);
        std::fs::write(&path, serde_json::to_vec(&analysis.sealed()?)?)?;

        let history = Review::load(recorded.data.path(), recorded.review.key())?.history()?;
        let view = history.current_view()?;
        let analysis = view.analysis().ok_or_else(|| eyre::eyre!("no analysis"))?;
        for unit in view.unit_ids() {
            let changed_again = analysis
                .unit(unit.index())
                .and_then(|unit| unit.chunk())
                .is_some_and(|chunk| chunk.added.contains("99"));
            assert_eq!(view.state(unit)?.is_reviewed(), !changed_again, "{unit}");
        }
        Ok(())
    }
}
