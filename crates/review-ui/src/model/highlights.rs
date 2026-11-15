//! Highlighting the shown revision's files in the background: the selected file at once, in a
//! lane of its own, and the other files on a few workers, nearest the selection first.
//!
//! At most [`Highlights::workers`] background jobs and one job for the selected file run at a
//! time, so highlighting never takes more threads than that.
//! A background job always runs to its end, within the highlighter's budget.
//! The selected file's job is cancelled once what it highlights is no longer what the selected
//! file needs, and its lane starts the next job only once the cancelled one has ended.
//!
//! A file version, a blob under its path, is in flight in at most one job, so no version is
//! highlighted twice at once; a version that a working-tree capture changes is another key, which
//! is not in flight.
//! Every job's completion lands and retires the job, whichever revision shows by then, and its
//! highlights are kept for the versions the shown revision has.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::state::SideKey;

/// A highlight job's number, which its completion carries back.
pub(super) type JobId = u64;

/// The lane a job runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// The selected file's, one job at a time.
    Selected,
    /// Every other file's, up to the workers.
    Background,
}

/// A job that runs.
struct Job {
    lane: Lane,
    sides: Vec<SideKey>,
    /// Set to end the job early; a background job is never cancelled.
    cancel: Arc<AtomicBool>,
}

/// The versions of one file of the shown revision that can be highlighted: regular files only.
#[derive(Debug, Clone, Default)]
pub(super) struct FileSides {
    /// The base version, under its own path.
    pub(super) old: Option<SideKey>,
    /// The head version.
    pub(super) new: Option<SideKey>,
}

impl FileSides {
    fn sides(&self) -> impl Iterator<Item = &SideKey> {
        self.old.iter().chain(&self.new)
    }
}

/// The shown revision's files in review order, built once for each snapshot shown.
struct Index {
    /// The snapshot it was built for, by the model's generation of shown snapshots.
    shown: u64,
    files: Vec<FileSides>,
    /// The files at each path, either version's: a rename's old path names it too.
    by_path: HashMap<String, Vec<usize>>,
    /// The file each version belongs to.
    file_of: HashMap<SideKey, usize>,
}

/// A job for the model to start: highlight `sides`, report back as `id`.
pub(super) struct Start {
    pub(super) id: JobId,
    pub(super) sides: Vec<SideKey>,
    /// The flag that cancels it; only the selected file's lane has one.
    pub(super) cancel: Option<Arc<AtomicBool>>,
}

/// The scheduler of the shown revision's highlighting; see the module documentation.
pub(super) struct Highlights {
    /// How many background jobs may run at once.
    pub(super) workers: usize,
    index: Option<Index>,
    /// The files with versions that need highlighting and are not in flight.
    pending: BTreeSet<usize>,
    /// The job each version in flight belongs to.
    in_flight: HashMap<SideKey, JobId>,
    jobs: HashMap<JobId, Job>,
    /// The selected file's lane, while a job runs in it.
    selected: Option<JobId>,
    next: JobId,
}

impl Highlights {
    /// A scheduler with `workers` background workers, at least one.
    pub(super) fn new(workers: usize) -> Self {
        Self {
            workers: workers.max(1),
            index: None,
            pending: BTreeSet::new(),
            in_flight: HashMap::new(),
            jobs: HashMap::new(),
            selected: None,
            next: 0,
        }
    }

    /// Half the machine's threads, from one to four: highlighting leaves room for the window.
    pub(super) fn machine_workers() -> usize {
        std::thread::available_parallelism()
            .map_or(1, |threads| threads.get() / 2)
            .clamp(1, 4)
    }

    /// Whether any job runs.
    pub(super) fn running(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// The snapshot the index was built for.
    pub(super) fn indexed_for(&self) -> Option<u64> {
        self.index.as_ref().map(|index| index.shown)
    }

    /// Indexes the files of the snapshot `shown`, and finds the ones that need highlighting.
    ///
    /// Jobs in flight run on: what they deliver is kept for the versions this snapshot has.
    pub(super) fn reindex(
        &mut self,
        shown: u64,
        files: Vec<FileSides>,
        needs: impl Fn(&SideKey) -> bool,
    ) {
        let mut by_path: HashMap<String, Vec<usize>> = HashMap::new();
        let mut file_of = HashMap::new();
        for (index, file) in files.iter().enumerate() {
            for side in file.sides() {
                let paths = by_path.entry(side.path.clone()).or_default();
                if !paths.contains(&index) {
                    paths.push(index);
                }
                file_of.insert(side.clone(), index);
            }
        }
        self.index = Some(Index {
            shown,
            files,
            by_path,
            file_of,
        });
        self.pending.clear();
        let count = self.index.as_ref().map_or(0, |index| index.files.len());
        for file in 0..count {
            self.refresh_file(file, &needs);
        }
    }

    /// The versions of the files at `path`: base and head, each under its own path.
    pub(super) fn sides_at(&self, path: &str) -> Vec<SideKey> {
        let Some(index) = &self.index else {
            return Vec::new();
        };
        index
            .by_path
            .get(path)
            .into_iter()
            .flatten()
            .filter_map(|&file| index.files.get(file))
            .flat_map(FileSides::sides)
            .cloned()
            .collect()
    }

    /// The head version of the file at `path`.
    pub(super) fn head_at(&self, path: &str) -> Option<&SideKey> {
        let index = self.index.as_ref()?;
        index
            .by_path
            .get(path)?
            .iter()
            .filter_map(|&file| index.files.get(file))
            .find_map(|file| file.new.as_ref().filter(|side| side.path == path))
    }

    /// Whether the shown revision has the version `side`.
    pub(super) fn shows(&self, side: &SideKey) -> bool {
        self.index
            .as_ref()
            .is_some_and(|index| index.file_of.contains_key(side))
    }

    /// Ends the job `id`, whatever it delivered, and returns the versions it had; its lane and
    /// its versions are free again.
    pub(super) fn retire(&mut self, id: JobId) -> Vec<SideKey> {
        let Some(job) = self.jobs.remove(&id) else {
            return Vec::new();
        };
        if self.selected == Some(id) {
            self.selected = None;
        }
        for side in &job.sides {
            if self.in_flight.get(side) == Some(&id) {
                self.in_flight.remove(side);
            }
        }
        job.sides
    }

    /// Finds again whether the files of `sides` need highlighting, after their versions were
    /// retired or highlighted.
    pub(super) fn refresh(&mut self, sides: &[SideKey], needs: impl Fn(&SideKey) -> bool) {
        let files: Vec<usize> = sides
            .iter()
            .filter_map(|side| self.index.as_ref()?.file_of.get(side).copied())
            .collect();
        for file in files {
            self.refresh_file(file, &needs);
        }
    }

    /// Keeps `file` among the pending files exactly while it has versions that need
    /// highlighting and are not in flight.
    fn refresh_file(&mut self, file: usize, needs: impl Fn(&SideKey) -> bool) {
        let wanted = self
            .index
            .as_ref()
            .and_then(|index| index.files.get(file))
            .is_some_and(|sides| {
                sides
                    .sides()
                    .any(|side| needs(side) && !self.in_flight.contains_key(side))
            });
        if wanted {
            self.pending.insert(file);
        } else {
            self.pending.remove(&file);
        }
    }

    /// Decides what runs now, the selected file at `selected` first, and returns the jobs to
    /// start.
    ///
    /// The selected file's lane takes its versions that need highlighting, except those a
    /// background job already has. A job of that lane that has other versions is cancelled,
    /// and the next one starts once its completion retires it. Background workers then take the
    /// pending files nearest the selected one.
    pub(super) fn pump(
        &mut self,
        selected: Option<&str>,
        needs: impl Fn(&SideKey) -> bool,
    ) -> Vec<Start> {
        let mut starts = Vec::new();
        let target: Vec<SideKey> = selected
            .map(|path| self.sides_at(path))
            .unwrap_or_default()
            .into_iter()
            .filter(|side| needs(side) && !self.in_background(side))
            .collect();
        match self.selected.and_then(|id| self.jobs.get(&id)) {
            Some(job) => {
                if !job.sides.iter().all(|side| target.contains(side)) {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            None if !target.is_empty() => {
                let start = self.launch(Lane::Selected, target, &needs);
                starts.push(start);
            }
            None => {}
        }
        let near = selected
            .and_then(|path| self.index.as_ref()?.by_path.get(path)?.first().copied())
            .unwrap_or(0);
        while self.background_jobs() < self.workers {
            let Some(file) = self.nearest_pending(near) else {
                break;
            };
            self.pending.remove(&file);
            let sides: Vec<SideKey> = self
                .index
                .as_ref()
                .and_then(|index| index.files.get(file))
                .map(|file| {
                    file.sides()
                        .filter(|side| needs(side) && !self.in_flight.contains_key(side))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if !sides.is_empty() {
                starts.push(self.launch(Lane::Background, sides, &needs));
            }
        }
        starts
    }

    /// Registers a job of `lane` for `sides`, which are not in flight.
    fn launch(
        &mut self,
        lane: Lane,
        sides: Vec<SideKey>,
        needs: impl Fn(&SideKey) -> bool,
    ) -> Start {
        let id = self.next;
        self.next += 1;
        for side in &sides {
            self.in_flight.insert(side.clone(), id);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.jobs.insert(
            id,
            Job {
                lane,
                sides: sides.clone(),
                cancel: Arc::clone(&cancel),
            },
        );
        if lane == Lane::Selected {
            self.selected = Some(id);
        }
        self.refresh(&sides, needs);
        Start {
            id,
            sides,
            cancel: (lane == Lane::Selected).then_some(cancel),
        }
    }

    /// Whether a background job has `side`.
    fn in_background(&self, side: &SideKey) -> bool {
        self.in_flight
            .get(side)
            .and_then(|id| self.jobs.get(id))
            .is_some_and(|job| job.lane == Lane::Background)
    }

    fn background_jobs(&self) -> usize {
        self.jobs
            .values()
            .filter(|job| job.lane == Lane::Background)
            .count()
    }

    /// The pending file nearest `file` in review order, the later one on a tie.
    fn nearest_pending(&self, file: usize) -> Option<usize> {
        let after = self.pending.range(file..).next().copied();
        let before = self.pending.range(..file).next_back().copied();
        match (after, before) {
            (Some(after), Some(before)) => Some(if after - file <= file - before {
                after
            } else {
                before
            }),
            (after, before) => after.or(before),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use review_core::ObjectId;

    use super::*;

    /// A version of `path` with a blob named after `blob`.
    fn side(path: &str, blob: u8) -> SideKey {
        SideKey {
            oid: ObjectId::parse(&format!("{blob:02x}").repeat(20)).unwrap(),
            path: path.to_owned(),
        }
    }

    /// A modified file: base blob `base`, head blob `head`.
    fn file(path: &str, base: u8, head: u8) -> FileSides {
        FileSides {
            old: Some(side(path, base)),
            new: Some(side(path, head)),
        }
    }

    /// The files `a.rs` to `e.rs`, every version still to highlight.
    fn five() -> Vec<FileSides> {
        ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]
            .into_iter()
            .zip(0_u8..)
            .map(|(path, at)| file(path, at * 2, at * 2 + 1))
            .collect()
    }

    /// What a test's cache has highlighted.
    #[derive(Default)]
    struct Done(HashSet<SideKey>);

    impl Done {
        fn needs(&self) -> impl Fn(&SideKey) -> bool + '_ {
            |side| !self.0.contains(side)
        }
    }

    fn paths(start: &Start) -> Vec<&str> {
        let mut paths: Vec<&str> = start.sides.iter().map(|side| side.path.as_str()).collect();
        paths.dedup();
        paths
    }

    /// The selected file starts at once in its lane, and the workers take the files nearest it.
    #[test]
    fn the_selected_file_starts_at_once_and_workers_take_its_neighbors() {
        let done = Done::default();
        let mut highlights = Highlights::new(2);
        highlights.reindex(1, five(), done.needs());
        let starts = highlights.pump(Some("c.rs"), done.needs());
        let started: Vec<Vec<&str>> = starts.iter().map(paths).collect();
        assert_eq!(started, [vec!["c.rs"], vec!["d.rs"], vec!["b.rs"]]);
        assert!(
            starts[0].cancel.is_some(),
            "only the selected lane can be cancelled"
        );
        assert!(starts[1..].iter().all(|start| start.cancel.is_none()));
        assert!(
            highlights.pump(Some("c.rs"), done.needs()).is_empty(),
            "the lanes are full"
        );
    }

    /// Moving on while the selected file's job runs cancels it, starts nothing in its lane until
    /// it ends, and then starts the file selected by then: a file passed over never starts.
    #[test]
    fn the_selected_lane_runs_one_job_and_skips_files_passed_over() {
        let done = Done::default();
        let mut highlights = Highlights::new(1);
        highlights.reindex(1, five(), done.needs());
        let starts = highlights.pump(Some("a.rs"), done.needs());
        let a = starts[0].id;
        let a_cancel = starts[0].cancel.clone().unwrap();
        assert_eq!(starts.len(), 2, "a.rs, and b.rs on the worker");

        assert!(highlights.pump(Some("c.rs"), done.needs()).is_empty());
        assert!(a_cancel.load(Ordering::Relaxed), "a.rs's job is cancelled");
        assert!(highlights.pump(Some("d.rs"), done.needs()).is_empty());
        assert_eq!(
            highlights.jobs.len(),
            2,
            "no job starts while the cancelled one runs"
        );

        // a.rs's job ends: its versions are free again, and d.rs starts, never c.rs.
        let freed = highlights.retire(a);
        highlights.refresh(&freed, done.needs());
        let starts = highlights.pump(Some("d.rs"), done.needs());
        assert_eq!(starts.iter().map(paths).collect::<Vec<_>>(), [vec!["d.rs"]]);
        assert!(
            highlights.pending.contains(&0),
            "a.rs waits for a worker again"
        );
    }

    /// A file whose head a capture replaces while its job runs needs only the new head: the base
    /// stays with the job that has it, and the old head is never highlighted twice.
    #[test]
    fn a_replaced_head_starts_while_the_old_job_keeps_its_base() {
        let mut done = Done::default();
        let mut highlights = Highlights::new(1);
        highlights.reindex(1, vec![file("a.rs", 1, 2)], done.needs());
        let first = highlights.pump(Some("a.rs"), done.needs());
        assert_eq!(first.len(), 1);

        // The capture: the same base, a new head, which the free worker starts at once while
        // the job of the old head is cancelled.
        highlights.reindex(2, vec![file("a.rs", 1, 3)], done.needs());
        let head = highlights.pump(Some("a.rs"), done.needs());
        let blobs = |starts: &[Start]| -> Vec<SideKey> {
            starts
                .iter()
                .flat_map(|start| start.sides.clone())
                .collect()
        };
        assert_eq!(blobs(&head), [side("a.rs", 3)]);
        assert!(
            first[0].cancel.as_ref().unwrap().load(Ordering::Relaxed),
            "the job of the old head is no longer what the file needs"
        );

        // Once it ends, the selected lane takes the base again; the old head never returns.
        let freed = highlights.retire(first[0].id);
        highlights.refresh(&freed, done.needs());
        let base = highlights.pump(Some("a.rs"), done.needs());
        assert_eq!(blobs(&base), [side("a.rs", 1)]);

        // A background job that keeps a version the new snapshot still has covers it.
        done.0.insert(side("a.rs", 3));
        let mut highlights = Highlights::new(1);
        highlights.reindex(
            1,
            vec![file("b.rs", 4, 5), file("a.rs", 1, 2)],
            done.needs(),
        );
        let started = highlights.pump(Some("b.rs"), done.needs());
        assert_eq!(
            started.iter().map(paths).collect::<Vec<_>>(),
            [vec!["b.rs"], vec!["a.rs"]]
        );
        highlights.reindex(
            2,
            vec![file("b.rs", 4, 5), file("a.rs", 1, 3)],
            done.needs(),
        );
        assert!(
            highlights.pump(Some("a.rs"), done.needs()).is_empty(),
            "the base is in flight on the worker and the new head is highlighted"
        );
    }

    /// Background jobs land in any order, each freeing its worker for the next file; at no point
    /// do more jobs run than the workers and the selected lane.
    #[test]
    fn background_jobs_land_in_any_order_within_the_bound() {
        let mut done = Done::default();
        let mut highlights = Highlights::new(2);
        highlights.reindex(1, five(), done.needs());
        let mut running: Vec<Start> = highlights.pump(Some("a.rs"), done.needs());
        let mut finished = 0;
        while let Some(start) = running.pop() {
            assert!(highlights.jobs.len() <= 3);
            done.0.extend(start.sides.iter().cloned());
            let freed = highlights.retire(start.id);
            highlights.refresh(&freed, done.needs());
            running.extend(highlights.pump(Some("a.rs"), done.needs()));
            finished += 1;
        }
        assert_eq!(finished, 5, "each file is highlighted once");
        assert!(highlights.pending.is_empty());
        assert!(!highlights.running());
    }
}
