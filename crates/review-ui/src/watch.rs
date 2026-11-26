//! Watching a working tree for the changes a working-tree capture would record, so a review of
//! the working tree follows the disk.
//!
//! A change counts when it could change what a capture records or compares with: a file
//! created, written, removed, renamed or given other permissions where git does not ignore it;
//! the worktree's `index` or `HEAD` rewritten; git starting or finishing an operation that keeps
//! a capture waiting, such as a merge or a rebase; and a branch or remote-tracking ref moving,
//! made, renamed or removed, loose or in `packed-refs`, which a branch switch, a commit, a push
//! and a fetch all do.
//! A directory's own times and permissions never count, since git records neither.
//! Reads never count, so a capture, which reads the files it hashes and writes only into the
//! review's archive, never wakes the watch.
//! The git directory's other files never count either, such as refs under `refs/code-review/`
//! that earlier versions pinned captures with.
//!
//! On Linux and the BSDs every directory git does not ignore gets a watch of its own, found by
//! walking the tree level by level and asking git about each level's directories: a recursive
//! watch would also watch `target/` and `node_modules/`, a hundred thousand directories in a
//! large repository, and every write a build makes there.
//! On macOS and Windows one watch covers the whole tree at the cost of a handle, so events in
//! ignored directories arrive and git's rules filter them out instead.
//!
//! What a watch can miss (a ref moving without the index changing, a submodule's commit, events
//! dropped by the system) is caught by the periodic capture of whoever waits.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use notify::event::{
    AccessKind, AccessMode, CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode,
};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use parking_lot::Mutex;
use review_core::git::IgnoreRules;

/// How long the working tree must stay quiet before a burst of changes counts as one.
const QUIET: Duration = Duration::from_millis(150);

/// The longest a burst of changes is gathered before it counts, so a tree that never goes
/// quiet still updates.
const GATHER_AT_MOST: Duration = Duration::from_secs(1);

/// Whether this platform watches every directory on its own; see the module documentation.
const PER_DIRECTORY: bool = !cfg!(any(target_os = "macos", windows));

/// Why a working tree cannot be watched.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The repository has no working tree.
    #[error("the repository has no working tree to watch")]
    NoWorkTree,
    /// The working tree or the git directory cannot be resolved.
    #[error("cannot resolve {path}")]
    Resolve {
        /// The directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// The system refused a watch, such as when its limit on watches is reached.
    #[error("cannot watch the working tree")]
    Notify(#[from] notify::Error),
    /// Git could not say which directories it ignores.
    #[error("cannot ask git which directories it ignores")]
    Git(#[from] review_core::Error),
}

/// What [`Waiter::wait`] saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited {
    /// A burst of changes started with something a capture would record: a capture taken
    /// meanwhile may have caught it half-made. [`Self::Changed`] follows once the burst
    /// settles.
    Stirred,
    /// A burst of changes settled, having changed something a capture would record.
    Changed,
    /// The [`Watch`] was dropped, so nothing will change any more.
    Closed,
}

/// The watch on a working tree; dropping it stops watching and ends the [`Waiter`]'s wait.
pub struct Watch {
    shared: Arc<Shared>,
}

/// Waits for the changes a [`Watch`] sees, on a thread that may block for as long as the app
/// runs; it holds the watch only weakly, so it never keeps it alive.
pub struct Waiter {
    events: Receiver<notify::Result<Event>>,
    shared: Weak<Shared>,
    places: Places,
    /// What git ignores in the working tree.
    rules: IgnoreRules,
    /// Why the watch may have missed changes since the last wait, such as a refused watch.
    problem: Option<String>,
    /// The branches and remote-tracking branches `packed-refs` held when last read, so a
    /// rewrite that only packs or drops the refs of `refs/code-review/` does not count.
    packed: Vec<String>,
    /// When the burst that [`Waited::Stirred`] reported started, until it settles.
    stirred: Option<Instant>,
}

/// What the watch and its waiter share.
struct Shared {
    watcher: Mutex<RecommendedWatcher>,
    /// Whether a directory or ref directory could not be watched since the watch started, so
    /// a change there goes unseen.
    incomplete: AtomicBool,
    /// The directories watched on their own, on a per-directory platform.
    watched: Mutex<HashSet<PathBuf>>,
    places: Places,
    /// What git ignores in the working tree.
    rules: IgnoreRules,
}

/// Where the working tree and the git directories are, in the spelling the watches report.
#[derive(Clone)]
struct Places {
    work_tree: PathBuf,
    /// The worktree's own git directory, with its `HEAD` and `index`.
    git_dir: PathBuf,
    /// The git directory every worktree of the clone shares, with the refs; the same as
    /// `git_dir` for a clone's main worktree.
    common_dir: PathBuf,
}

/// The files in a worktree's git directory whose coming or going starts or ends an operation
/// that keeps a capture waiting; see [`review_core::Repo::busy`].
const BUSY_MARKERS: [&str; 6] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
];

/// The directories of refs a capture reads, under the common git directory: branches and
/// remote-tracking branches.
const REF_DIRECTORIES: [&str; 2] = ["refs/heads", "refs/remotes"];

impl Watch {
    /// Starts watching the working tree `rules` judge, and returns the watch with the waiter
    /// that waits on it.
    ///
    /// Changes start counting once this returns, so a capture taken afterwards misses nothing.
    ///
    /// # Errors
    ///
    /// Returns an error for a repository without a working tree, when the system refuses a
    /// watch, and when git cannot say which directories it ignores.
    pub fn start(rules: IgnoreRules) -> Result<(Self, Waiter), WatchError> {
        let repo = rules.repo();
        let work_tree = repo.work_tree().ok_or(WatchError::NoWorkTree)?;
        let places = Places {
            work_tree: spelled(work_tree)?,
            git_dir: spelled(repo.git_dir())?,
            common_dir: spelled(repo.common_dir())?,
        };
        let (sender, events) = channel();
        let watcher = notify::recommended_watcher(forward(sender))?;
        let shared = Arc::new(Shared {
            watcher: Mutex::new(watcher),
            incomplete: AtomicBool::new(false),
            watched: Mutex::new(HashSet::new()),
            places: places.clone(),
            rules: rules.clone(),
        });
        shared.watch_all()?;
        let waiter = Waiter {
            packed: packed_refs(&places.common_dir),
            events,
            shared: Arc::downgrade(&shared),
            places,
            rules,
            problem: None,
            stirred: None,
        };
        Ok((Self { shared }, waiter))
    }

    /// Walks the working tree again and watches the directories git does not ignore now, and
    /// no others, catching what events can miss: a directory made while events were dropped, or
    /// one a changed ignore rule now includes.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot say which directories it ignores or a watch is refused.
    pub fn rewalk(&self) -> Result<(), WatchError> {
        self.shared
            .rewalk()
            .inspect_err(|_| self.shared.incomplete.store(true, Ordering::Relaxed))
    }

    /// Whether every directory the watch should cover has been watched since it started: once
    /// one could not be, as when the system's limit on watches is reached, a change there goes
    /// unseen, and a caller cannot rely on the watch to tell it about every change.
    #[must_use]
    pub fn covers_all(&self) -> bool {
        !self.shared.incomplete.load(Ordering::Relaxed)
    }
}

impl Waiter {
    /// Blocks until something a capture would record may have changed, or until the [`Watch`]
    /// is dropped.
    ///
    /// A burst of changes counts as one, and is reported twice: [`Waited::Stirred`] as soon as
    /// its first change that counts arrives, and [`Waited::Changed`] from the next wait, once
    /// the burst settles. Events are judged as they arrive, each batch already queued at once.
    pub fn wait(&mut self) -> Waited {
        self.wait_until(None).unwrap_or(Waited::Closed)
    }

    /// Does what [`Self::wait`] does, giving up with `None` after `timeout`.
    pub fn wait_for(&mut self, timeout: Duration) -> Option<Waited> {
        self.wait_until(Some(Instant::now() + timeout))
    }

    /// Takes why the watch may have missed changes, which the caller reports once and makes up
    /// for with captures of its own.
    pub fn take_problem(&mut self) -> Option<String> {
        self.problem.take()
    }

    fn wait_until(&mut self, deadline: Option<Instant>) -> Option<Waited> {
        // The rest of a burst that stirred: it counts whatever else it holds, and what that
        // made, such as directories, is watched.
        if let Some(started) = self.stirred.take() {
            let mut rest = Vec::new();
            if !self.gather(started, &mut rest) {
                return Some(Waited::Closed);
            }
            self.counts(rest);
            return Some(Waited::Changed);
        }
        loop {
            let first = match deadline {
                None => match self.events.recv() {
                    Ok(event) => event,
                    Err(_) => return Some(Waited::Closed),
                },
                Some(deadline) => {
                    match self
                        .events
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => return None,
                        Err(RecvTimeoutError::Disconnected) => return Some(Waited::Closed),
                    }
                }
            };
            let started = Instant::now();
            let mut batch = vec![first];
            loop {
                batch.extend(self.events.try_iter());
                if self.counts(std::mem::take(&mut batch)) {
                    self.stirred = Some(started);
                    return Some(Waited::Stirred);
                }
                if !self.gather_one(started, &mut batch) {
                    return Some(Waited::Closed);
                }
                // The burst settled without a change that counts.
                if batch.is_empty() {
                    break;
                }
            }
        }
    }

    /// Gathers the events of the burst that `started` into `events` until it settles: the
    /// working tree quiet for [`QUIET`], or [`GATHER_AT_MOST`] since it started; returns
    /// false once the watch is gone.
    fn gather(&self, started: Instant, events: &mut Vec<notify::Result<Event>>) -> bool {
        loop {
            let before = events.len();
            if !self.gather_one(started, events) {
                return false;
            }
            if events.len() == before {
                return true;
            }
        }
    }

    /// Waits for the next event of the burst that `started`, adding it to `events`, and adds
    /// nothing once the burst settled; returns false once the watch is gone.
    fn gather_one(&self, started: Instant, events: &mut Vec<notify::Result<Event>>) -> bool {
        let left = GATHER_AT_MOST.saturating_sub(started.elapsed()).min(QUIET);
        if left.is_zero() {
            return true;
        }
        match self.events.recv_timeout(left) {
            Ok(event) => events.push(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return false,
        }
        true
    }

    /// Whether a burst of events changed anything a capture records; watches the directories
    /// it made along the way.
    fn counts(&mut self, burst: Vec<notify::Result<Event>>) -> bool {
        let mut counts = false;
        let mut rewalk = false;
        let mut paths: Vec<String> = Vec::new();
        let mut made: Vec<String> = Vec::new();
        let mut gone: Vec<PathBuf> = Vec::new();
        let mut refs_made: Vec<PathBuf> = Vec::new();
        for event in burst {
            let event = match event {
                Ok(event) => event,
                Err(err) => {
                    self.problem = Some(err.to_string());
                    counts = true;
                    rewalk = true;
                    continue;
                }
            };
            if event.need_rescan() || event.paths.is_empty() {
                counts = true;
                rewalk = true;
                continue;
            }
            for path in &event.paths {
                match self.places.classify(path) {
                    Place::WorkTree(relative) => {
                        // A directory removed or moved away loses its watch; one made at its
                        // path later needs a new one.
                        if matches!(
                            event.kind,
                            EventKind::Remove(_)
                                | EventKind::Modify(ModifyKind::Name(
                                    RenameMode::From | RenameMode::Any
                                ))
                        ) {
                            gone.push(path.clone());
                        }
                        // Git records no directory's times or permissions, and an entry made
                        // in or removed from a directory has an event of its own; Windows
                        // reports a directory's new modification time, at times seconds late.
                        if matches!(event.kind, EventKind::Modify(kind) if !matches!(kind, ModifyKind::Name(_)))
                            && is_plain_directory(path)
                        {
                            continue;
                        }
                        if relative.rsplit('/').next() == Some(".gitignore") {
                            counts = true;
                            rewalk = true;
                        }
                        // A directory is asked about as `dir/`, so rules for directories apply.
                        if is_directory(event.kind, path) {
                            made.push(relative.clone());
                            paths.push(format!("{relative}/"));
                        } else {
                            paths.push(relative);
                        }
                    }
                    place => counts |= self.git_counts(&place, event.kind, path, &mut refs_made),
                }
            }
        }
        if !paths.is_empty() {
            let ignored = self
                .rules
                .read()
                .and_then(|copy| copy.ignored(paths.iter().map(String::as_str)));
            match ignored {
                Ok(ignored) => {
                    counts |= paths.iter().any(|path| !ignored.contains(path));
                    made.retain(|directory| !ignored.contains(&format!("{directory}/")));
                }
                // A path git cannot judge, such as one inside a submodule, counts.
                Err(_) => counts = true,
            }
        }
        // A ref directory made since the watch started, such as `refs/remotes` on the first
        // fetch, is watched from now on; one made under a watched one already is.
        if let Some(shared) = self.shared.upgrade()
            && let Err(err) = refs_made
                .iter()
                .try_for_each(|directory| shared.watch_refs_at(directory))
        {
            self.problem = Some(format!("{err}: {}", causes(&err)));
            counts = true;
            if let Some(shared) = self.shared.upgrade() {
                shared.incomplete.store(true, Ordering::Relaxed);
            }
        }
        if PER_DIRECTORY && let Some(shared) = self.shared.upgrade() {
            shared
                .watched
                .lock()
                .retain(|directory| !gone.iter().any(|path| directory.starts_with(path)));
            let watched = if rewalk {
                shared.rewalk()
            } else {
                made.iter()
                    .filter(|directory| {
                        is_plain_directory(&shared.places.work_tree.join(directory))
                    })
                    .try_for_each(|directory| shared.watch_under(directory))
            };
            if let Err(err) = watched {
                self.problem = Some(format!("{err}: {}", causes(&err)));
                counts = true;
                shared.incomplete.store(true, Ordering::Relaxed);
            }
        }
        counts
    }
}

impl Waiter {
    /// Whether an event at `path` in a git directory, at `place`, changed what a capture
    /// records or compares with; a ref directory it made joins `refs_made`.
    fn git_counts(
        &mut self,
        place: &Place,
        kind: EventKind,
        path: &Path,
        refs_made: &mut Vec<PathBuf>,
    ) -> bool {
        match place {
            Place::GitInput => true,
            // Every git command that reads the index takes its lock; only one that goes away
            // can end a wait for it, and one that becomes the index rewrites the index, which
            // counts on its own.
            Place::IndexLock => matches!(
                kind,
                EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
            ),
            Place::Ref => {
                if matches!(kind, EventKind::Create(_)) && is_plain_directory(path) {
                    refs_made.push(path.to_path_buf());
                }
                true
            }
            Place::PackedRefs => {
                let packed = packed_refs(&self.places.common_dir);
                let changed = packed != self.packed;
                self.packed = packed;
                changed
            }
            Place::WorkTree(_) | Place::Elsewhere => false,
        }
    }
}

impl Shared {
    /// Registers the first watches.
    fn watch_all(&self) -> Result<(), WatchError> {
        let mut watcher = self.watcher.lock();
        let places = &self.places;
        if !PER_DIRECTORY {
            watcher.watch(&places.work_tree, RecursiveMode::Recursive)?;
        }
        // Inside the working tree, a recursive watch covers the git directory already.
        if PER_DIRECTORY || !places.git_dir.starts_with(&places.work_tree) {
            watcher.watch(&places.git_dir, RecursiveMode::NonRecursive)?;
        }
        if places.common_dir != places.git_dir
            && (PER_DIRECTORY || !places.common_dir.starts_with(&places.work_tree))
        {
            watcher.watch(&places.common_dir, RecursiveMode::NonRecursive)?;
        }
        // `refs` itself, so a ref directory made later is seen and watched then.
        watcher.watch(&places.common_dir.join("refs"), RecursiveMode::NonRecursive)?;
        drop(watcher);
        for directory in REF_DIRECTORIES {
            let path = places.common_dir.join(directory);
            if path.is_dir() {
                self.watch_refs_at(&path)?;
            }
        }
        if PER_DIRECTORY {
            self.watch_under("")?;
        }
        Ok(())
    }

    /// Watches the ref directory `path` and every directory under it, when it is one of
    /// [`REF_DIRECTORIES`]; the watch follows the directories made in it later.
    fn watch_refs_at(&self, path: &Path) -> Result<(), WatchError> {
        let watched = REF_DIRECTORIES
            .iter()
            .any(|directory| path == self.places.common_dir.join(directory));
        if !watched {
            return Ok(());
        }
        match self.watcher.lock().watch(path, RecursiveMode::Recursive) {
            Ok(()) => Ok(()),
            // Removed again since it was made.
            Err(err) if matches!(err.kind, notify::ErrorKind::PathNotFound) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Watches `directory`, relative to the working tree (empty for its root), and every
    /// directory under it that git does not ignore.
    fn watch_under(&self, directory: &str) -> Result<(), WatchError> {
        let found = walk(&self.rules, &self.places.work_tree, directory)?;
        let mut watcher = self.watcher.lock();
        let mut directories = self.watched.lock();
        for path in found {
            if directories.contains(&path) {
                continue;
            }
            match watcher.watch(&path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    directories.insert(path);
                }
                // A directory removed since the walk has nothing left to watch.
                Err(err) if matches!(err.kind, notify::ErrorKind::PathNotFound) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    /// See [`Watch::rewalk`].
    ///
    /// Every directory found is watched again, not only those missing from the set: one
    /// removed and made again at the same path while its events were lost is a new directory
    /// the old watch does not cover.
    fn rewalk(&self) -> Result<(), WatchError> {
        if !PER_DIRECTORY {
            return Ok(());
        }
        let found: HashSet<PathBuf> = walk(&self.rules, &self.places.work_tree, "")?
            .into_iter()
            .collect();
        let mut watcher = self.watcher.lock();
        let mut directories = self.watched.lock();
        for gone in directories.difference(&found).cloned().collect::<Vec<_>>() {
            // The system dropped the watch of a removed directory already.
            watcher.unwatch(&gone).ok();
            directories.remove(&gone);
        }
        for path in found {
            match watcher.watch(&path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    directories.insert(path);
                }
                Err(err) if matches!(err.kind, notify::ErrorKind::PathNotFound) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }
}

/// Where an event's path is.
enum Place {
    /// The git directory's `index` or `HEAD`, which a capture reads, or a marker of an
    /// operation that keeps a capture waiting.
    GitInput,
    /// The worktree's `index.lock`.
    IndexLock,
    /// A branch or remote-tracking ref, or a directory of them.
    Ref,
    /// The common git directory's `packed-refs`, or its lock.
    PackedRefs,
    /// A path of the working tree, relative to it with `/` between components.
    WorkTree(String),
    /// Anywhere else: the git directory's other files, a nested repository's own git
    /// directory, or outside both.
    Elsewhere,
}

impl Places {
    fn classify(&self, path: &Path) -> Place {
        if let Ok(inside) = path.strip_prefix(&self.common_dir) {
            if REF_DIRECTORIES
                .iter()
                .any(|directory| inside.starts_with(directory))
            {
                return Place::Ref;
            }
            // Any other ref, such as the pins under `refs/code-review`.
            if inside.starts_with("refs") {
                return Place::Elsewhere;
            }
            if inside == Path::new("packed-refs") || inside == Path::new("packed-refs.lock") {
                return Place::PackedRefs;
            }
        }
        if let Ok(inside) = path.strip_prefix(&self.git_dir) {
            if inside == Path::new("index.lock") {
                return Place::IndexLock;
            }
            let input = inside == Path::new("index")
                || inside == Path::new("HEAD")
                || BUSY_MARKERS
                    .iter()
                    .any(|marker| inside == Path::new(marker));
            return if input {
                Place::GitInput
            } else {
                Place::Elsewhere
            };
        }
        if path.starts_with(&self.common_dir) {
            return Place::Elsewhere;
        }
        let Ok(relative) = path.strip_prefix(&self.work_tree) else {
            return Place::Elsewhere;
        };
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) if part == ".git" => return Place::Elsewhere,
                Component::Normal(part) => parts.push(part.to_string_lossy()),
                _ => return Place::Elsewhere,
            }
        }
        if parts.is_empty() {
            return Place::Elsewhere;
        }
        Place::WorkTree(parts.join("/"))
    }
}

/// The branches and remote-tracking branches `packed-refs` in `common_dir` holds, with their
/// targets, as its lines; none when it cannot be read.
fn packed_refs(common_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(common_dir.join("packed-refs")) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| {
            line.split_once(' ').is_some_and(|(_, name)| {
                REF_DIRECTORIES
                    .iter()
                    .any(|directory| name.starts_with(&format!("{directory}/")))
            })
        })
        .map(str::to_owned)
        .collect()
}

/// The event handler: drops what only reads, and forwards the rest to the waiter.
fn forward(sender: Sender<notify::Result<Event>>) -> impl Fn(notify::Result<Event>) + Send {
    move |event| {
        if let Ok(event) = &event
            && matches!(event.kind, EventKind::Access(access) if access != AccessKind::Close(AccessMode::Write))
        {
            return;
        }
        // The waiter is gone once the watch is no longer waited on.
        sender.send(event).ok();
    }
}

/// Whether the event is about a directory that exists now: one made or moved in.
fn is_directory(kind: EventKind, path: &Path) -> bool {
    match kind {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => true,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_)) => is_plain_directory(path),
        _ => false,
    }
}

/// Whether `path` is a directory itself, not a link to one, which git records as the link:
/// changed in place, as Windows can turn a directory into a link, the link counts as a file.
fn is_plain_directory(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.is_dir())
}

/// Every directory at or under `start`, relative to `work_tree` (empty for its root), that git
/// does not ignore, as absolute paths: one `git check-ignore` per level of the tree, all on one
/// reading of the index.
///
/// Symbolic links are not followed, since git records the link itself; nested repositories
/// and submodules are left out, since a capture records only their commit.
fn walk(rules: &IgnoreRules, work_tree: &Path, start: &str) -> Result<Vec<PathBuf>, WatchError> {
    let copy = rules.read()?;
    let mut found = vec![work_tree.join(start)];
    let mut level = vec![start.to_owned()];
    while !level.is_empty() {
        let mut children: Vec<String> = Vec::new();
        for directory in &level {
            // A directory removed or unreadable since it was found has nothing to watch.
            let Ok(entries) = std::fs::read_dir(work_tree.join(directory)) else {
                continue;
            };
            for entry in entries.flatten() {
                let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
                if !is_directory || entry.file_name() == ".git" {
                    continue;
                }
                if entry.path().join(".git").exists() {
                    continue;
                }
                let name = entry.file_name();
                let child = match name.to_str() {
                    Some(name) if directory.is_empty() => name.to_owned(),
                    Some(name) => format!("{directory}/{name}"),
                    // Git cannot be asked about a name that is not UTF-8, so it is watched,
                    // though not what is under it.
                    None => {
                        found.push(entry.path());
                        continue;
                    }
                };
                children.push(child);
            }
        }
        let asked: Vec<String> = children.iter().map(|child| format!("{child}/")).collect();
        let ignored = copy.ignored(asked.iter().map(String::as_str))?;
        level = children
            .into_iter()
            .filter(|child| !ignored.contains(&format!("{child}/")))
            .collect();
        found.extend(level.iter().map(|child| work_tree.join(child)));
    }
    Ok(found)
}

/// `path` as the watches report it: the system's own spelling on macOS, where `/var` is
/// `/private/var`, and as given on Windows, where canonical paths take a `\\?\` prefix the
/// watches do not report.
fn spelled(path: &Path) -> Result<PathBuf, WatchError> {
    if cfg!(windows) {
        return Ok(path.to_path_buf());
    }
    path.canonicalize().map_err(|source| WatchError::Resolve {
        path: path.to_path_buf(),
        source,
    })
}

/// The causes under `err`, joined by `: `.
fn causes(err: &dyn std::error::Error) -> String {
    let mut text = Vec::new();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push(cause.to_string());
        source = cause.source();
    }
    text.join(": ")
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::process::Command;

    use review_core::{Repo, Review};

    use super::*;

    /// Long enough for any platform to deliver an event; a wait that should wake does so well
    /// within it.
    const DELIVERED: Duration = Duration::from_secs(10);

    /// How long a wait that should not wake is given to wake anyway.
    const SILENT: Duration = Duration::from_millis(1500);

    struct Tree {
        dir: tempfile::TempDir,
        /// Where the private repository lives, outside the tree.
        data: tempfile::TempDir,
    }

    impl Tree {
        /// A repository with an ignored `target/` full of directories, and tracked `src/`.
        fn new() -> Self {
            let tree = Self {
                dir: tempfile::tempdir().unwrap(),
                data: tempfile::tempdir().unwrap(),
            };
            tree.git(&["init", "--quiet", "--initial-branch=main"]);
            tree.write(
                ".gitignore",
                indoc::indoc! {"
                    target/
                    *.log
                "},
            );
            tree.write("src/lib.rs", "fn main() {}\n");
            tree.write("src/nested/deep/mod.rs", "\n");
            for index in 0..40 {
                std::fs::create_dir_all(tree.path(&format!("target/debug/build/{index}"))).unwrap();
            }
            tree.git(&["add", "--all"]);
            tree.git(&["commit", "--quiet", "--no-verify", "-m", "start"]);
            tree
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.dir.path().join(relative)
        }

        /// Writes a file through to the disk, as git does below with `core.fsync`: Windows
        /// reports a write's new size and time when its cache is flushed, which can be seconds
        /// later, while a wait expects quiet.
        fn write(&self, relative: &str, text: &str) {
            let path = self.path(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut file = std::fs::File::create(path).unwrap();
            file.write_all(text.as_bytes()).unwrap();
            file.sync_all().unwrap();
        }

        fn git(&self, args: &[&str]) {
            let status = Command::new("git")
                .arg("-C")
                .arg(self.dir.path())
                .args(["-c", "core.fsync=all"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }

        /// A review of the tree's checkout, stored outside the tree.
        fn review(&self) -> Review {
            Review::open_checkout(self.data.path(), Repo::discover(self.dir.path()).unwrap())
                .unwrap()
        }

        fn watch(&self) -> (Watch, Waiter) {
            let (watch, mut waiter) = Watch::start(self.review().ignore_rules()).unwrap();
            settle(&mut waiter);
            (watch, waiter)
        }
    }

    /// Waits out whatever the tree's setup still delivers.
    fn settle(waiter: &mut Waiter) {
        while waiter.wait_for(Duration::from_millis(500)).is_some() {}
    }

    fn wakes(waiter: &mut Waiter, what: &str) {
        assert_eq!(waiter.wait_for(DELIVERED), Some(Waited::Stirred), "{what}");
        assert_eq!(waiter.wait_for(DELIVERED), Some(Waited::Changed), "{what}");
        settle(waiter);
    }

    fn stays_asleep(waiter: &mut Waiter, what: &str) {
        assert_eq!(waiter.wait_for(SILENT), None, "{what}");
    }

    /// Records every event the system reports under the tree, unfiltered, so a wait that woke
    /// when it should not can show what woke it.
    fn record(tree: &Tree) -> (RecommendedWatcher, Arc<Mutex<Vec<String>>>) {
        let reported = Arc::new(Mutex::new(Vec::new()));
        let mut watcher = notify::recommended_watcher({
            let reported = Arc::clone(&reported);
            move |event: notify::Result<Event>| {
                reported.lock().push(match event {
                    Ok(event) => format!("{:?} {:?}", event.kind, event.paths),
                    Err(err) => err.to_string(),
                });
            }
        })
        .unwrap();
        watcher
            .watch(tree.dir.path(), RecursiveMode::Recursive)
            .unwrap();
        (watcher, reported)
    }

    /// On a platform that watches directory by directory, exactly the directories git does not
    /// ignore are watched: none of the build output's.
    #[test]
    fn only_directories_git_keeps_are_watched() {
        if !PER_DIRECTORY {
            return;
        }
        let tree = Tree::new();
        let (watch, _waiter) = tree.watch();
        let root = spelled(tree.dir.path()).unwrap();
        let watched = watch.shared.watched.lock().clone();
        let expected: HashSet<PathBuf> = ["", "src", "src/nested", "src/nested/deep"]
            .into_iter()
            .map(|relative| root.join(relative))
            .collect();
        assert_eq!(watched, expected);
    }

    /// Every change a capture would record wakes the wait, and nothing it would leave out does.
    #[test]
    fn changes_a_capture_records_wake_the_wait() {
        let tree = Tree::new();
        let (_watch, mut waiter) = tree.watch();

        tree.write("target/debug/build/3/out.o", "binary");
        tree.write("build.log", "noise");
        // Git records no directory's time, which Windows reports when an entry is made in it.
        // It is set only where each directory is watched on its own, whose events say just
        // that: FSEvents carries a just-made directory's creation onto its later events, and a
        // directory's creation counts.
        if PER_DIRECTORY {
            std::fs::File::open(tree.path("src/nested"))
                .unwrap()
                .set_modified(std::time::SystemTime::UNIX_EPOCH)
                .unwrap();
        }
        stays_asleep(
            &mut waiter,
            "writes git ignores, and a directory's new time",
        );

        tree.write("src/lib.rs", "fn main() { edited(); }\n");
        wakes(&mut waiter, "an edit to a tracked file");
        tree.write("src/new.rs", "\n");
        wakes(&mut waiter, "a new untracked file");
        tree.write("fresh/inner/one.rs", "\n");
        wakes(&mut waiter, "a new directory");
        tree.write("fresh/inner/one.rs", "edited\n");
        wakes(
            &mut waiter,
            "an edit in a directory made after the watch started",
        );
        std::fs::rename(tree.path("src/new.rs"), tree.path("src/renamed.rs")).unwrap();
        wakes(&mut waiter, "a rename");
        std::fs::remove_file(tree.path("src/renamed.rs")).unwrap();
        wakes(&mut waiter, "a removal");
        tree.write(".gitignore", "target/\n");
        wakes(&mut waiter, "an ignore rule");
        tree.git(&["add", "src/lib.rs"]);
        wakes(&mut waiter, "staging, which rewrites the index");
    }

    /// A directory removed and made again at the same path is watched again, so an edit in it
    /// wakes the wait.
    #[test]
    fn a_directory_made_again_is_watched_again() {
        let tree = Tree::new();
        let (_watch, mut waiter) = tree.watch();
        std::fs::remove_dir_all(tree.path("src/nested")).unwrap();
        std::fs::create_dir_all(tree.path("src/nested/deep")).unwrap();
        wakes(&mut waiter, "the directory made again");
        tree.write("src/nested/deep/mod.rs", "edited\n");
        wakes(&mut waiter, "an edit in the directory made again");
    }

    /// A link to a directory is a file to git: made, it counts and is never walked as a
    /// directory, and changed in place, it counts, though the directory it names is unchanged.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_directory_counts_as_a_file() {
        let tree = Tree::new();
        let (_watch, mut waiter) = tree.watch();
        std::os::unix::fs::symlink("nested", tree.path("src/linked")).unwrap();
        wakes(&mut waiter, "a link made");
        // Walked as a directory, the link would have git asked about the paths beyond it.
        assert_eq!(waiter.take_problem(), None);
        let touched = Command::new("touch")
            .arg("-h")
            .arg(tree.path("src/linked"))
            .status()
            .unwrap();
        assert!(touched.success());
        wakes(&mut waiter, "a link changed in place");
    }

    /// A directory made later that cannot be watched, here one nobody may read, leaves the
    /// watch short of covering the tree, and it says so.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_that_cannot_be_watched_leaves_the_watch_short() {
        use std::os::unix::fs::DirBuilderExt as _;

        let tree = Tree::new();
        let (watch, mut waiter) = tree.watch();
        assert!(watch.covers_all());
        std::fs::DirBuilder::new()
            .mode(0o000)
            .create(tree.path("src/sealed"))
            .unwrap();
        if std::fs::read_dir(tree.path("src/sealed")).is_ok() {
            eprintln!("skipped: permissions do not apply to this user");
            return;
        }
        wakes(&mut waiter, "a directory made");
        assert!(!watch.covers_all());
    }

    /// A capture reads the working tree and the index, and writes only into a private
    /// repository, so it never wakes the wait and never triggers the next.
    #[test]
    fn a_capture_never_wakes_the_wait() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn main() { dirty(); }\n");
        tree.write("src/untracked.rs", "\n");
        let (_watch, mut waiter) = tree.watch();
        let (_recorder, reported) = record(&tree);
        let review = tree.review();
        review.refresh().unwrap();
        review.refresh().unwrap();
        assert_eq!(
            waiter.wait_for(SILENT),
            None,
            "a capture woke the wait; the system reported {:#?}",
            reported.lock()
        );
    }

    /// Branches and remote-tracking branches moving wake the wait, loose or packed, and so do
    /// git starting and finishing an operation; the refs that pin captures never do.
    #[test]
    fn refs_and_operations_wake_the_wait() {
        let tree = Tree::new();
        let (_watch, mut waiter) = tree.watch();

        tree.git(&[
            "update-ref",
            "refs/code-review/key/live/session/head",
            "HEAD",
        ]);
        tree.git(&["update-ref", "-d", "refs/code-review/key/live/session/head"]);
        stays_asleep(&mut waiter, "a pin made and removed");

        tree.git(&["checkout", "--quiet", "-b", "feat/x"]);
        wakes(&mut waiter, "a new branch checked out");
        tree.git(&["branch", "--quiet", "other"]);
        wakes(&mut waiter, "a branch made, in a directory made before");
        tree.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        wakes(&mut waiter, "the first remote-tracking branch");
        tree.git(&[
            "commit",
            "--quiet",
            "--no-verify",
            "--allow-empty",
            "-m",
            "next",
        ]);
        wakes(&mut waiter, "a commit");
        tree.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        wakes(&mut waiter, "a fetch moving a remote-tracking branch");
        tree.git(&["pack-refs", "--all"]);
        wakes(&mut waiter, "packing the refs");
        tree.git(&["update-ref", "refs/remotes/origin/main", "HEAD~1"]);
        wakes(&mut waiter, "a packed remote-tracking branch moved");

        tree.write(
            ".git/MERGE_HEAD",
            "0000000000000000000000000000000000000000\n",
        );
        wakes(&mut waiter, "a merge stopping");
        std::fs::remove_file(tree.path(".git/MERGE_HEAD")).unwrap();
        wakes(&mut waiter, "a merge finishing");
    }

    /// Dropping the watch ends a wait that would otherwise block for good.
    #[test]
    fn dropping_the_watch_ends_the_wait() {
        let tree = Tree::new();
        let (watch, mut waiter) = tree.watch();
        let waiting = std::thread::spawn(move || waiter.wait());
        std::thread::sleep(Duration::from_millis(100));
        drop(watch);
        assert_eq!(waiting.join().unwrap(), Waited::Closed);
    }
}
