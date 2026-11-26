//! What cr keeps, and for how long: every revision a review recorded until the review is
//! dropped, and of everything else only what something still needs.
//!
//! Only an open that holds the review's lease alone removes anything: nothing else has the
//! review open then, so nothing reads or writes what it removes (see [`store::InUse`]).
//! It removes what captures, probes, fetches, sessions and drops left, compacts the review's
//! archive when it is due or the data directory is over its limit, and only then lets others
//! in; an open that finds the review open elsewhere leaves all of that for a later one.
//! What would grow what the data directory retains, a capture, a new revision or a walkthrough,
//! is refused while it retains more than its limit.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::session::LIVE_PINS;
use super::{Review, cache, revision_events};
use crate::error::Error;
use crate::git::PrivateRepo;
use crate::ids::{ObjectId, ReviewKey};
use crate::store::log::LogTail;
use crate::store::{self, Lease, ReviewDir};

/// How many bytes a data directory retains, its analysis caches and transient directories
/// aside, before an addition is refused.
pub(super) const RETAINED_LIMIT: u64 = 10 << 30;

impl Review {
    /// Opens the review in `dir` with `open`, holding its lease shared: when nothing else has
    /// the review open, `open` runs first under the lease held alone, to maintain the review,
    /// and then, the lease let go, the open starts over as one that maintained nothing.
    /// `between` runs where the lease is let go, where a test drops the review meanwhile.
    ///
    /// The analysis caches are kept within their budget either way, and what maintenance
    /// cannot do is left for a later open.
    /// Returns `None` when a drop took the review's directory.
    pub(super) fn under_lease(
        dir: &ReviewDir,
        between: &mut dyn FnMut(),
        open: &dyn Fn() -> Result<Self, Error>,
    ) -> Result<Option<Self>, Error> {
        if let Err(err) = cache::evict(dir.data_dir(), cache::BUDGET) {
            drop(err);
        }
        let Some(mut lease) = Lease::open(dir)? else {
            return Ok(None);
        };
        if lease.try_exclusive()? {
            let maintained = open().and_then(|review| maintain(dir, &review.archive, review.limit));
            if let Err(err) = maintained {
                drop(err);
            }
            drop(lease);
            between();
            let Some(reopened) = Lease::open(dir)? else {
                return Ok(None);
            };
            lease = reopened;
        }
        let in_use = lease.shared()?;
        // A drop that held the lease took the directory with it.
        if !dir.root().exists() {
            return Ok(None);
        }
        let review = open()?;
        Ok(Some(Self {
            archive: review.archive.leased(in_use),
            ..review
        }))
    }
}

/// Maintains the review in `dir`, whose archive is `archive`, while its lease is held alone:
/// removes what nothing can use any more, and compacts the archive when it is due or the data
/// directory retains more than `limit`.
///
/// It removes nothing a symbolic link leads it to, and runs no git where one leads.
fn maintain(dir: &ReviewDir, archive: &PrivateRepo, limit: u64) -> Result<(), Error> {
    let data_dir = dir.data_dir();
    let contained = |path: &Path| store::contained(data_dir, path);
    let left = [
        // The read-only copies of reviewed files that older versions ran language servers in.
        dir.root().join("worktrees"),
        // What captures, probes, fetches and compactions cut short left.
        archive.scratch(),
        // The lock files of the sessions of older versions.
        archive.path().join("sessions"),
    ];
    for path in left.iter().filter(|path| contained(path)) {
        store::remove_read_only(path)?;
    }
    let dropped = data_dir.join("dropped");
    if contained(&dropped) {
        for review in store::entries(&dropped)? {
            // A drop removing its review meanwhile may remove the same files.
            if let Err(err) = store::remove_read_only(&review) {
                drop(err);
            }
        }
    }
    // Compaction writes its pack under `scratch/` before it installs it in `objects/pack`.
    let pack_dir = archive.objects().join("pack");
    if !contained(&pack_dir) || !contained(&archive.scratch()) {
        return Ok(());
    }
    for file in store::entries(&pack_dir)? {
        let half_written = file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".tmp-") || name.starts_with("tmp_"));
        if half_written {
            store::remove_read_only(&file)?;
        }
    }
    let live: Vec<String> = archive
        .repo()
        .refs_under(&format!("{LIVE_PINS}/"))?
        .into_iter()
        .map(|pin| pin.name)
        .collect();
    archive.delete_refs(&live)?;
    unpin_unrecorded(dir, archive)?;
    if archive.compaction_due(retained(data_dir)? > limit)? {
        let mut roots: Vec<ObjectId> = archive
            .repo()
            .refs_under("refs/")?
            .into_iter()
            .map(|pin| pin.target)
            .collect();
        roots.sort();
        roots.dedup();
        archive.compact(&roots, &mut |_| Ok(()))?;
    }
    Ok(())
}

/// Removes the pins of snapshots the log of the review in `dir` does not record, such as those
/// of a recording that died before it appended its record.
fn unpin_unrecorded(dir: &ReviewDir, archive: &PrivateRepo) -> Result<(), Error> {
    let events = LogTail::new(dir).poll()?;
    let recorded: HashSet<ObjectId> = revision_events(&dir.events(), &events, 0)?
        .into_iter()
        .flat_map(|revision| [revision.base, revision.head])
        .collect();
    let unrecorded: Vec<String> = archive
        .repo()
        .refs_under("refs/snap/")?
        .into_iter()
        .filter(|pin| !recorded.contains(&pin.target))
        .map(|pin| pin.name)
        .collect();
    archive.delete_refs(&unrecorded)
}

/// How many bytes the files under `data_dir` hold, the reviews' analysis caches aside, which
/// keep to a budget of their own, and so are their archives' scratch directories, which hold
/// what captures, probes and fetches use only while they run.
fn retained(data_dir: &Path) -> Result<u64, Error> {
    let transient: HashSet<PathBuf> = store::entries(&data_dir.join("reviews"))?
        .into_iter()
        .flat_map(|review| {
            [
                review.join("analysis"),
                review.join("archive").join("scratch"),
            ]
        })
        .collect();
    bytes_under(data_dir, &transient)
}

/// How many bytes the files at or under `path` hold but those under `skipped`, symbolic links
/// counted as themselves.
fn bytes_under(path: &Path, skipped: &HashSet<PathBuf>) -> Result<u64, Error> {
    if skipped.contains(path) {
        return Ok(0);
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        // Removed since it was listed.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(Error::io("read", path, source)),
    };
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0;
    for entry in store::entries(path)? {
        total += bytes_under(&entry, skipped)?;
    }
    Ok(total)
}

impl Review {
    /// Lets an addition to what the data directory retains through while it retains no more
    /// than its limit.
    ///
    /// The check is soft: what is added after it, by this addition or several at once, can take
    /// the data directory past the limit, and the next addition is refused.
    /// Space comes back as an open that has a review to itself compacts its archive, or as
    /// reviews are dropped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StorageFull`] when the data directory retains more than the limit, and
    /// an error when its files cannot be read.
    pub(super) fn admit(&self) -> Result<(), Error> {
        let used = retained(self.dir.data_dir())?;
        if used <= self.limit {
            Ok(())
        } else {
            Err(Error::StorageFull {
                used,
                limit: self.limit,
            })
        }
    }

    /// Drops the review stored under `key` in `data_dir`: its log, archive, caches and view
    /// state, whatever became of its checkout.
    ///
    /// The review's directory is moved aside and then removed, read-only copies older
    /// versions made included, under the review's lease held alone, so nothing has it open
    /// meanwhile; a removal cut short is finished by a later open.
    /// It touches nothing a symbolic link leads to, from `reviews/` on.
    /// Opening it by its key fails from then on, and opening its target makes a new review.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownReview`] when no review is stored under `key`,
    /// [`Error::ReviewInUse`] while anything has it open, such as a window, a command or a
    /// job, and an error when a symbolic link leads it out of the data directory or it cannot
    /// be moved aside.
    pub fn remove(data_dir: &Path, key: &ReviewKey) -> Result<(), Error> {
        let data_dir =
            std::path::absolute(data_dir).map_err(|err| Error::io("resolve", data_dir, err))?;
        let dir = ReviewDir::new(&data_dir, key);
        let dropped = data_dir.join("dropped");
        for path in [dir.root(), &dropped] {
            if !store::contained(&data_dir, path) {
                let leads_out =
                    std::io::Error::other("a symbolic link leads out of the data directory");
                return Err(Error::io("drop", path, leads_out));
            }
        }
        let unknown = || Error::UnknownReview(key.as_str().to_owned());
        if !dir.meta().exists() {
            return Err(unknown());
        }
        let lease = Lease::open(&dir)?.ok_or_else(unknown)?;
        if !lease.try_exclusive()? {
            return Err(Error::ReviewInUse(key.as_str().to_owned()));
        }
        // Another drop may have finished before this one took the lease.
        if !dir.meta().exists() {
            return Err(unknown());
        }
        store::create_dir(&dropped)?;
        let aside = dropped.join(crate::random::hex(12));
        std::fs::rename(dir.root(), &aside).map_err(|err| Error::io("move", dir.root(), err))?;
        if let Err(err) = store::remove_read_only(&aside) {
            drop(err);
        }
        drop(lease);
        Ok(())
    }
}

#[cfg(test)]
impl Review {
    /// The review with `limit` in place of [`RETAINED_LIMIT`], so a test reaches it.
    pub(super) fn with_limit(self, limit: u64) -> Self {
        Self { limit, ..self }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::fmt::Write as _;

    use color_eyre::eyre;
    use indoc::writedoc;
    use tempfile::TempDir;

    use super::*;
    use crate::git::{Repo, Step};
    use crate::history::{History, RevisionRecord};
    use crate::review::{Capture, ReviewTarget};
    use crate::store::events::MarkVerdict;
    use crate::testing::{self, git};

    /// `lib.rs` at revision `revision`: four functions, each returning a value of its own.
    fn library(revision: u32) -> eyre::Result<String> {
        let mut text = String::new();
        for function in 0..4 {
            let value = revision * 10 + function;
            writedoc!(
                text,
                "
                fn f{function}() -> u32 {{
                    {value}
                }}

                "
            )?;
        }
        Ok(text)
    }

    /// A repository whose `feature` is commits past `main`, the review of `main..feature`
    /// with each recorded and accepted, and the directories that hold them.
    pub(in crate::review) struct Recorded {
        pub repo: TempDir,
        pub data: TempDir,
        pub review: Review,
    }

    impl Recorded {
        /// The review with `revisions` revisions recorded.
        pub fn new(revisions: u32) -> eyre::Result<Self> {
            let repo = testing::committed(&[("lib.rs", &library(0)?)])?;
            git(repo.path(), &["checkout", "--quiet", "-b", "feature"])?;
            let data = tempfile::tempdir()?;
            let target = ReviewTarget::Range {
                base: "main".to_owned(),
                head: "feature".to_owned(),
            };
            let review = Review::open(data.path(), Repo::discover(repo.path())?, target)?;
            let recorded = Self { repo, data, review };
            for revision in 1..=revisions {
                recorded.record(revision)?;
            }
            Ok(recorded)
        }

        /// Commits `lib.rs` at `revision`, records it and accepts its units.
        pub fn record(&self, revision: u32) -> eyre::Result<()> {
            std::fs::write(self.repo.path().join("lib.rs"), library(revision)?)?;
            git(
                self.repo.path(),
                &["commit", "--quiet", "--no-verify", "-am", "edit"],
            )?;
            self.review.refresh()?;
            let history = self.review.history()?;
            let view = history.current_view()?;
            self.review
                .mark(&view, &view.unit_ids(), MarkVerdict::Accepted)?;
            Ok(())
        }

        /// The history a review opened anew derives from its archive alone: the repository
        /// lost every object and no analysis is cached.
        pub fn alone(&self) -> eyre::Result<History> {
            for entry in std::fs::read_dir(self.repo.path().join(".git/objects"))? {
                let path = entry?.path();
                if path.file_name().is_some_and(|name| name != "info") {
                    std::fs::remove_dir_all(path)?;
                }
            }
            store::remove_read_only(&self.review.dir.root().join("analysis"))?;
            Ok(Review::load(self.data.path(), self.review.key())?.history()?)
        }

        /// Imports into the archive a commit the log does not record, pinned as a recording
        /// that died before it appended its record leaves it, and returns it.
        fn unrecorded(&self) -> eyre::Result<ObjectId> {
            let tree = git(self.repo.path(), &["rev-parse", "main^{tree}"])?;
            let commit = git(
                self.repo.path(),
                &["commit-tree", &tree, "-m", "unrecorded"],
            )?;
            let commit = ObjectId::parse(&commit)?;
            let archive = &self.review.archive;
            archive.import(&self.review.repo, std::slice::from_ref(&commit))?;
            archive.update_refs(&[(self.review.pin_ref(&commit), commit.clone())])?;
            Ok(commit)
        }

        /// Lets go of the review, and opens it anew by its key, alone.
        fn reopened(self) -> eyre::Result<(TempDir, TempDir, Review)> {
            let Self { repo, data, review } = self;
            let key = review.key().clone();
            drop(review);
            let review = Review::load(data.path(), &key)?;
            Ok((repo, data, review))
        }
    }

    /// The packs in the archive of `review`, by file name.
    fn packs(review: &Review) -> eyre::Result<Vec<String>> {
        let mut packs: Vec<String> = store::entries(&review.archive.objects().join("pack"))?
            .into_iter()
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .filter_map(|path| Some(path.file_name()?.to_str()?.to_owned()))
            .collect();
        packs.sort();
        Ok(packs)
    }

    /// Whether the archive of `review` holds `oid`.
    fn holds(review: &Review, oid: &ObjectId) -> Result<bool, Error> {
        Ok(review
            .archive
            .repo()
            .missing(std::slice::from_ref(oid))?
            .is_empty())
    }

    /// The files under `root`, relative to it, with their sizes.
    fn files(root: &Path) -> eyre::Result<Vec<(PathBuf, u64)>> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                pending.extend(store::entries(&path)?);
            } else {
                found.push((path.strip_prefix(root)?.to_path_buf(), metadata.len()));
            }
        }
        found.sort();
        Ok(found)
    }

    /// The snapshots the log of `review` records.
    fn snapshots(review: &Review) -> eyre::Result<Vec<ObjectId>> {
        let mut snapshots: Vec<ObjectId> = review
            .history()?
            .revisions()
            .iter()
            .flat_map(|revision| [revision.base.clone(), revision.head.clone()])
            .collect();
        snapshots.sort();
        snapshots.dedup();
        Ok(snapshots)
    }

    /// A compaction leaves one pack holding every recorded revision whole, from which alone
    /// the review derives the history it recorded, and drops what no pin reaches, a pin a
    /// recording left before it died included.
    #[test]
    fn a_compaction_keeps_what_the_log_records_and_drops_the_rest() -> eyre::Result<()> {
        let recorded = Recorded::new(3)?;
        let review = &recorded.review;
        let history = review.history()?;
        let unrecorded = recorded.unrecorded()?;
        assert!(packs(review)?.len() > 1);

        maintain(&review.dir, &review.archive, 0)?;
        assert_eq!(packs(review)?.len(), 1);
        assert!(!holds(review, &unrecorded)?);
        assert_eq!(recorded.alone()?, history);
        Ok(())
    }

    /// A compaction cut short where git has written the new pack, where cr installed it, or
    /// where cr removed the packs it replaces but not yet the loose objects leaves an archive
    /// from which alone the review derives the history it recorded, and the next open that
    /// has the review to itself removes what it left.
    #[test]
    fn a_compaction_cut_short_anywhere_loses_nothing() -> eyre::Result<()> {
        for cut in [Step::Staged, Step::Installed, Step::Removed] {
            let recorded = Recorded::new(3)?;
            let review = &recorded.review;
            let history = review.history()?;
            recorded.unrecorded()?;
            let roots = snapshots(review)?;
            // Removing the packs it replaces is cut short once the last one goes.
            let mut left = if cut == Step::Removed {
                packs(review)?.len()
            } else {
                1
            };

            let compacted = review.archive.compact(&roots, &mut |step| {
                if step == cut {
                    left -= 1;
                }
                if left == 0 {
                    Err(Error::Raced("compaction"))
                } else {
                    Ok(())
                }
            });
            assert!(compacted.is_err(), "{cut:?}");
            assert_eq!(recorded.alone()?, history, "{cut:?}");
            let (_repo, _data, review) = recorded.reopened()?;
            assert_eq!(
                store::entries(&review.archive.scratch())?,
                Vec::<PathBuf>::new()
            );
            assert_eq!(review.history()?, history, "{cut:?}");
        }
        Ok(())
    }

    /// A clone configured to split packs still gets one pack, from which alone the review
    /// derives the history it recorded: a compaction takes nothing of the clone's
    /// configuration, so it never installs part of what it checked and removes the rest.
    #[test]
    fn a_compaction_writes_one_pack_whatever_the_clone_says() -> eyre::Result<()> {
        let recorded = Recorded::new(1)?;
        // Three MiB that do not compress, past the smallest limit git takes.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut big = String::new();
        while big.len() < 3 << 20 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            write!(big, "{state:016x}")?;
        }
        std::fs::write(recorded.repo.path().join("big.txt"), big)?;
        git(recorded.repo.path(), &["add", "big.txt"])?;
        recorded.record(2)?;
        git(
            recorded.repo.path(),
            &["config", "pack.packSizeLimit", "1m"],
        )?;
        let review = Review::load(recorded.data.path(), recorded.review.key())?;
        let history = review.history()?;

        maintain(&review.dir, &review.archive, 0)?;
        assert_eq!(packs(&review)?.len(), 1);
        assert_eq!(recorded.alone()?, history);
        Ok(())
    }

    /// A damaged index where the new pack's index goes, as an earlier compaction cut short
    /// could leave, is replaced by the one checked, so the archive alone still derives the
    /// history.
    #[test]
    fn a_damaged_file_where_the_new_pack_goes_is_replaced() -> eyre::Result<()> {
        let recorded = Recorded::new(3)?;
        let review = &recorded.review;
        let history = review.history()?;
        let roots = snapshots(review)?;
        let scratch = review.archive.scratch();
        let pack_dir = review.archive.objects().join("pack");

        review.archive.compact(&roots, &mut |step| {
            if step != Step::Staged {
                return Ok(());
            }
            for staged in store::entries(&scratch)? {
                for file in store::entries(&staged.join("pack"))? {
                    if let Some(name) = file.file_name()
                        && file.extension().is_some_and(|extension| extension == "idx")
                    {
                        let damaged = pack_dir.join(name);
                        std::fs::write(&damaged, b"damaged")
                            .map_err(|source| Error::io("write", damaged, source))?;
                    }
                }
            }
            Ok(())
        })?;
        assert_eq!(packs(review)?.len(), 1);
        assert_eq!(recorded.alone()?, history);
        Ok(())
    }

    /// An open while anything else has the review open, here a blob reader, removes nothing
    /// and compacts nothing; the next open that has the review to itself removes the pins of
    /// sessions that are over and of a recording that died, what captures, fetches and repacks
    /// cut short left, read-only copies older versions made and what a drop could not remove,
    /// and compacts the archive.
    #[test]
    fn only_an_open_that_has_the_review_to_itself_maintains_it() -> eyre::Result<()> {
        let recorded = Recorded::new(2)?;
        let review = &recorded.review;
        let history = review.history()?;
        let unrecorded = recorded.unrecorded()?;
        let head = history
            .revisions()
            .last()
            .map(|revision| revision.head.clone())
            .ok_or_else(|| eyre::eyre!("no revision"))?;
        review
            .archive
            .update_refs(&[("refs/live/over/capture/head".to_owned(), head)])?;
        let fetch = review.archive.scratch().join("fetch");
        std::fs::create_dir_all(fetch.join("objects"))?;
        let half = review.archive.objects().join("pack/.tmp-1-pack-half.pack");
        std::fs::write(&half, b"PACK")?;
        let sessions = review.archive.path().join("sessions");
        std::fs::create_dir_all(&sessions)?;
        std::fs::write(sessions.join("over.lock"), b"")?;
        let copy = review.dir.root().join("worktrees/tree");
        std::fs::create_dir_all(copy.join("src"))?;
        std::fs::write(copy.join("src/lib.rs"), "fn a() {}\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for path in [copy.join("src/lib.rs"), copy.join("src"), copy.clone()] {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555))?;
            }
        }
        let left = review.dir.data_dir().join("dropped/left");
        std::fs::create_dir_all(&left)?;
        std::fs::remove_file(review.archive.path().join("compacted"))?;
        let before = packs(review)?;
        let left_behind = [
            fetch,
            half,
            sessions,
            review.dir.root().join("worktrees"),
            left,
        ];

        let reader = review.snapshots().blob_reader()?;
        let other = Review::load(recorded.data.path(), review.key())?;
        assert_eq!(packs(&other)?, before);
        assert!(holds(&other, &unrecorded)?);
        assert_eq!(other.archive.repo().refs_under("refs/live/")?.len(), 1);
        for path in &left_behind {
            assert!(path.exists(), "{} went", path.display());
        }
        drop((reader, other));

        let (_repo, _data, review) = recorded.reopened()?;
        assert_eq!(packs(&review)?.len(), 1);
        assert!(!holds(&review, &unrecorded)?);
        assert_eq!(review.archive.repo().refs_under("refs/live/")?, []);
        for path in &left_behind {
            assert!(!path.exists(), "{} stays", path.display());
        }
        assert_eq!(review.history()?, history);
        Ok(())
    }

    /// Asking what git ignores and which tree is on disk adds nothing to the archive.
    #[test]
    fn probes_add_nothing_to_the_archive() -> eyre::Result<()> {
        let repo = testing::committed(&[("a.rs", "fn a() {}\n")])?;
        let data = tempfile::tempdir()?;
        let review = Review::open_checkout(data.path(), Repo::discover(repo.path())?)?;
        std::fs::write(repo.path().join("a.rs"), "fn a() { edited(); }\n")?;
        std::fs::write(repo.path().join("b.rs"), "fn b() {}\n")?;
        let objects = review.archive.objects();
        let before = files(&objects)?;
        review.tree_on_disk()?;
        review.ignore_rules().read()?.ignored(["b.rs"])?;
        assert_eq!(files(&objects)?, before);
        Ok(())
    }

    /// Maintenance follows no symbolic link out of the data directory: a `dropped/` that leads
    /// elsewhere keeps what is there, and a compaction due with `scratch/`, where it would
    /// write its pack, leading elsewhere does not run.
    #[cfg(unix)]
    #[test]
    fn maintenance_follows_no_link_out_of_the_data_directory() -> eyre::Result<()> {
        let recorded = Recorded::new(1)?;
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("kept"), "kept")?;
        std::os::unix::fs::symlink(outside.path(), recorded.data.path().join("dropped"))?;
        let scratch = tempfile::tempdir()?;
        std::os::unix::fs::symlink(scratch.path(), recorded.review.archive.scratch())?;
        std::fs::remove_file(recorded.review.archive.path().join("compacted"))?;

        let (_repo, _data, review) = recorded.reopened()?;
        assert!(outside.path().join("kept").exists());
        assert!(!review.archive.path().join("compacted").exists());
        assert_eq!(files(scratch.path())?, []);
        Ok(())
    }

    /// A drop follows no symbolic link out of the data directory: with `reviews/` a link to
    /// elsewhere, it is refused and moves nothing.
    #[cfg(unix)]
    #[test]
    fn a_drop_follows_no_link_out_of_the_data_directory() -> eyre::Result<()> {
        let Recorded { repo, data, review } = Recorded::new(1)?;
        let key = review.key().clone();
        let root = review.dir.root().to_path_buf();
        drop(review);
        let outside = tempfile::tempdir()?;
        std::fs::rename(data.path().join("reviews"), outside.path().join("reviews"))?;
        std::os::unix::fs::symlink(outside.path().join("reviews"), data.path().join("reviews"))?;
        let before = files(outside.path())?;

        assert!(Review::remove(data.path(), &key).is_err());
        assert!(root.exists());
        assert_eq!(files(outside.path())?, before);
        drop(repo);
        Ok(())
    }

    /// An archive that was never compacted, as one a move from an earlier version filled with
    /// a few packs without deltas, is compacted on its next open that has it to itself.
    #[test]
    fn an_archive_never_compacted_is_on_its_next_open() -> eyre::Result<()> {
        let recorded = Recorded::new(3)?;
        let marker = recorded.review.archive.path().join("compacted");
        std::fs::remove_file(&marker)?;
        assert!(packs(&recorded.review)?.len() > 1);

        let (_repo, _data, review) = recorded.reopened()?;
        assert_eq!(packs(&review)?.len(), 1);
        assert!(marker.exists());
        Ok(())
    }

    /// Over the limit, a refresh with a new revision and a walkthrough are refused, leaving the
    /// log as it was, while every recorded revision stays readable and takes marks and notes,
    /// and recording what is recorded already adds nothing to refuse.
    #[test]
    fn an_addition_over_the_limit_is_refused() -> eyre::Result<()> {
        let recorded = Recorded::new(1)?;
        let full = recorded.review.clone().with_limit(0);
        let log = std::fs::read(full.dir.events())?;
        std::fs::write(recorded.repo.path().join("lib.rs"), library(2)?)?;
        git(
            recorded.repo.path(),
            &["commit", "--quiet", "--no-verify", "-am", "edit"],
        )?;
        assert!(matches!(full.refresh(), Err(Error::StorageFull { .. })));
        let history = full.history()?;
        let tour = serde_json::json!({
            "version": 1,
            "review": full.key().clone(),
            "revision": 1,
            "title": "Tour",
            "steps": [{"title": "All", "why": "because", "units": []}],
        });
        assert!(matches!(
            full.import_walkthrough(&history, &tour.to_string()),
            Err(Error::StorageFull { .. })
        ));
        assert_eq!(std::fs::read(full.dir.events())?, log);

        git(
            recorded.repo.path(),
            &["reset", "--quiet", "--hard", "HEAD~1"],
        )?;
        assert!(!full.refresh()?.created);
        let view = history.current_view()?;
        full.mark(&view, &view.unit_ids(), MarkVerdict::Flagged)?;
        full.note(&view, view.unit_ids()[0], "still noted")?;
        let history = full.history()?;
        assert_eq!(
            Review::load(recorded.data.path(), full.key())?.history()?,
            history
        );
        Ok(())
    }

    /// Over the limit, a capture is refused, and so is a mark that would record the capture
    /// on screen, recording nothing, while the capture keeps showing as it did; a mark on a
    /// capture another window recorded meanwhile goes through.
    #[test]
    fn a_refused_live_update_keeps_the_last_view() -> eyre::Result<()> {
        let repo = testing::committed(&[("a.rs", "fn a() {}\n")])?;
        std::fs::write(repo.path().join("a.rs"), "fn a() { edited(); }\n")?;
        let data = tempfile::tempdir()?;
        let review = Review::open_checkout(data.path(), Repo::discover(repo.path())?)?;
        let session = review.start_session();
        let capture = |review: &Review, keep: &[RevisionRecord]| match review.live_history(
            &session,
            keep,
            &[],
        )? {
            Capture::Captured(captured) => Ok(captured.history),
            _ => eyre::bail!("nothing was captured"),
        };
        let shown = capture(&review, &[])?;
        let view = shown.current_view()?;
        assert!(view.is_provisional());
        let keep: Vec<RevisionRecord> = view.record().into_iter().cloned().collect();

        let full = review.clone().with_limit(0);
        assert!(matches!(
            full.live_history(&session, &keep, &[]),
            Err(Error::StorageFull { .. })
        ));
        assert!(matches!(
            full.mark(&view, &view.unit_ids(), MarkVerdict::Accepted),
            Err(Error::StorageFull { .. })
        ));
        assert_eq!(std::fs::read(review.dir.events()).unwrap_or_default(), b"");
        assert_eq!(capture(&review, &keep)?, shown);

        // Another window records the capture; this one's mark then records nothing.
        let other = Review::open_checkout(data.path(), Repo::discover(repo.path())?)?;
        other.mark(&view, &view.unit_ids()[..1], MarkVerdict::Seen)?;
        full.mark(&view, &view.unit_ids(), MarkVerdict::Accepted)?;
        Ok(())
    }

    /// A review is dropped by its key once nothing has it open, a session, an ignore query or
    /// a blob reader that outlived it included, and opening its target then makes a new one;
    /// a symbolic link in it is removed, never followed.
    #[test]
    fn a_review_is_dropped_once_nothing_has_it_open() -> eyre::Result<()> {
        let recorded = Recorded::new(1)?;
        let data = recorded.data.path().to_path_buf();
        let key = recorded.review.key().clone();
        let root = recorded.review.dir.root().to_path_buf();
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("kept"), "kept")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.join("link"))?;
        let in_use = || matches!(Review::remove(&data, &key), Err(Error::ReviewInUse(_)));

        let session = recorded.review.start_session();
        let rules = recorded.review.ignore_rules();
        let reader = recorded.review.snapshots().blob_reader()?;
        assert!(in_use());
        drop(recorded.review);
        assert!(in_use());
        drop(session);
        assert!(in_use());
        drop(rules);
        assert!(in_use());
        drop(reader);
        Review::remove(&data, &key)?;
        assert!(!root.exists());
        assert!(outside.path().join("kept").exists());
        assert_eq!(
            store::entries(&data.join("dropped"))?,
            Vec::<PathBuf>::new()
        );
        assert!(matches!(
            Review::load(&data, &key),
            Err(Error::UnknownReview(_))
        ));
        assert!(matches!(
            Review::remove(&data, &key),
            Err(Error::UnknownReview(_))
        ));
        let target = ReviewTarget::Range {
            base: "main".to_owned(),
            head: "feature".to_owned(),
        };
        let fresh = Review::open(&data, Repo::discover(recorded.repo.path())?, target)?;
        assert_eq!(fresh.history()?.revisions().len(), 0);
        Ok(())
    }

    /// Opens that wait on a drop under way find the review gone once it ends: loading it by its
    /// key fails, unless the open beside it made the review anew first, which it then loads, and
    /// opening its target makes a new one, which a drop that started before it was made finds
    /// in use rather than removing it.
    #[test]
    fn an_open_racing_a_drop_finds_the_review_gone() -> eyre::Result<()> {
        let recorded = Recorded::new(1)?;
        let data = recorded.data.path().to_path_buf();
        let key = recorded.review.key().clone();
        let dir = recorded.review.dir.clone();
        let repo = Repo::discover(recorded.repo.path())?;
        drop(recorded.review);
        let target = ReviewTarget::Range {
            base: "main".to_owned(),
            head: "feature".to_owned(),
        };

        // A drop that holds the lease and has not moved the review aside yet.
        let lease = Lease::open(&dir)?.ok_or_else(|| eyre::eyre!("no review"))?;
        assert!(lease.try_exclusive()?);
        let (loaded, opened) = std::thread::scope(|scope| {
            let loading = scope.spawn(|| {
                Review::load(&data, &key).and_then(|review| Ok(review.history()?.revisions().len()))
            });
            let opening = scope.spawn(|| Review::open(&data, repo.clone(), target.clone()));
            std::thread::sleep(std::time::Duration::from_millis(300));
            std::fs::create_dir_all(data.join("dropped"))?;
            std::fs::rename(dir.root(), data.join("dropped/aside"))?;
            store::remove_read_only(&data.join("dropped/aside"))?;
            drop(lease);
            eyre::Ok((loading.join(), opening.join()))
        })?;
        assert!(matches!(loaded, Ok(Err(Error::UnknownReview(_)) | Ok(0))));
        let Ok(Ok(fresh)) = opened else {
            eyre::bail!("the open failed");
        };
        assert_eq!(fresh.history()?.revisions().len(), 0);
        assert!(matches!(
            Review::remove(&data, &key),
            Err(Error::ReviewInUse(_))
        ));
        assert!(dir.meta().exists());
        Ok(())
    }

    /// A drop between an open letting go of the lease it maintained the review under and
    /// taking it shared is caught as the open starts over: loading by the key fails, and with a
    /// review made anew meanwhile, loading opens that one, as it is.
    #[test]
    fn a_drop_while_an_open_hands_over_its_lease_is_caught() -> eyre::Result<()> {
        let Recorded { repo, data, review } = Recorded::new(1)?;
        let key = review.key().clone();
        drop(review);
        let drop_it = || Review::remove(data.path(), &key).expect("the drop runs");

        let loaded = Review::load_with(data.path(), &key, &mut || drop_it());
        assert!(matches!(loaded, Err(Error::UnknownReview(_))));

        let target = ReviewTarget::Range {
            base: "main".to_owned(),
            head: "feature".to_owned(),
        };
        let open = || Review::open(data.path(), Repo::discover(repo.path())?, target.clone());
        drop(open()?);
        let mut made = None;
        let loaded = Review::load_with(data.path(), &key, &mut || {
            drop_it();
            made = Some(open().expect("the review is made anew"));
        })?;
        assert_eq!(loaded.history()?.revisions().len(), 0);
        assert!(loaded.refresh()?.created);
        let made = made.ok_or_else(|| eyre::eyre!("nothing was made"))?;
        assert_eq!(made.history()?, loaded.history()?);
        Ok(())
    }

    /// A symbolic link to a review's directory takes that review's lease.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_review_shares_its_lease() -> eyre::Result<()> {
        let data = tempfile::tempdir()?;
        let original = data.path().join("reviews/original");
        let alias = data.path().join("reviews/alias");
        store::create_dir(&original)?;
        std::os::unix::fs::symlink(&original, &alias)?;
        let lease = |root: &Path| {
            Lease::open(&ReviewDir::listed(data.path(), root.to_path_buf()))?
                .ok_or_else(|| eyre::eyre!("no directory"))
        };
        let held = lease(&original)?;
        assert!(held.try_exclusive()?);
        assert!(!lease(&alias)?.try_exclusive()?);
        Ok(())
    }

    /// Two spellings of a review's directory that differ in case take one lease, whether the
    /// filesystem takes them for one directory or two.
    #[test]
    fn spellings_that_differ_in_case_share_a_lease() -> eyre::Result<()> {
        let data = tempfile::tempdir()?;
        let lower = data.path().join("reviews/review");
        let upper = data.path().join("reviews/REVIEW");
        store::create_dir(&lower)?;
        store::create_dir(&upper)?;
        let lease = |root: &Path| {
            Lease::open(&ReviewDir::listed(data.path(), root.to_path_buf()))?
                .ok_or_else(|| eyre::eyre!("no directory"))
        };
        let held = lease(&lower)?;
        assert!(held.try_exclusive()?);
        assert!(!lease(&upper)?.try_exclusive()?);
        Ok(())
    }
}
