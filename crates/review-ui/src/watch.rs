//! Watching a working tree for the changes a working-tree capture would record, so a review of
//! the working tree follows the disk.
//!
//! A change counts when it could change what a capture records: a file created, written,
//! removed, renamed or given other permissions where git does not ignore it, and the
//! repository's `index` or `HEAD` rewritten.
//! A directory's own times and permissions never count, since git records neither.
//! Reads never count, so a capture, which reads the files it hashes, never wakes the watch.
//! The git directory's other files, the capture's own temporary index among them, never count
//! either.
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
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use notify::event::{
    AccessKind, AccessMode, CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode,
};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use parking_lot::Mutex;
use review_core::Repo;

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
    /// Something a capture would record may have changed.
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
    repo: Repo,
    /// Why the watch may have missed changes since the last wait, such as a refused watch.
    problem: Option<String>,
}

/// What the watch and its waiter share.
struct Shared {
    watcher: Mutex<RecommendedWatcher>,
    /// The directories watched on their own, on a per-directory platform.
    watched: Mutex<HashSet<PathBuf>>,
    places: Places,
    repo: Repo,
}

/// Where the working tree and the git directory are, in the spelling the watches report.
#[derive(Clone)]
struct Places {
    work_tree: PathBuf,
    git_dir: PathBuf,
}

impl Watch {
    /// Starts watching `repo`'s working tree, and returns the watch with the waiter that waits
    /// on it.
    ///
    /// Changes start counting once this returns, so a capture taken afterwards misses nothing.
    ///
    /// # Errors
    ///
    /// Returns an error for a repository without a working tree, when the system refuses a
    /// watch, and when git cannot say which directories it ignores.
    pub fn start(repo: Repo) -> Result<(Self, Waiter), WatchError> {
        let work_tree = repo.work_tree().ok_or(WatchError::NoWorkTree)?;
        let places = Places {
            work_tree: spelled(work_tree)?,
            git_dir: spelled(repo.git_dir())?,
        };
        let (sender, events) = channel();
        let watcher = notify::recommended_watcher(forward(sender))?;
        let shared = Arc::new(Shared {
            watcher: Mutex::new(watcher),
            watched: Mutex::new(HashSet::new()),
            places: places.clone(),
            repo: repo.clone(),
        });
        shared.watch_all()?;
        let waiter = Waiter {
            events,
            shared: Arc::downgrade(&shared),
            places,
            repo,
            problem: None,
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
        self.shared.rewalk()
    }
}

impl Waiter {
    /// Blocks until something a capture would record may have changed, gathering a burst of
    /// changes into one, or until the [`Watch`] is dropped.
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
            let mut burst = vec![first];
            let started = Instant::now();
            loop {
                let left = GATHER_AT_MOST.saturating_sub(started.elapsed()).min(QUIET);
                if left.is_zero() {
                    break;
                }
                match self.events.recv_timeout(left) {
                    Ok(event) => burst.push(event),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return Some(Waited::Closed),
                }
            }
            if self.counts(burst) {
                return Some(Waited::Changed);
            }
        }
    }

    /// Whether a burst of events changed anything a capture records; watches the directories
    /// it made along the way.
    fn counts(&mut self, burst: Vec<notify::Result<Event>>) -> bool {
        let mut counts = false;
        let mut rewalk = false;
        let mut paths: Vec<String> = Vec::new();
        let mut made: Vec<String> = Vec::new();
        let mut gone: Vec<PathBuf> = Vec::new();
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
                    Place::GitInput => counts = true,
                    Place::Elsewhere => {}
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
                }
            }
        }
        if !paths.is_empty() {
            match self.repo.ignored(paths.iter().map(String::as_str)) {
                Ok(ignored) => {
                    counts |= paths.iter().any(|path| !ignored.contains(path));
                    made.retain(|directory| !ignored.contains(&format!("{directory}/")));
                }
                // A path git cannot judge, such as one inside a submodule, counts.
                Err(_) => counts = true,
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
            }
        }
        counts
    }
}

impl Shared {
    /// Registers the first watches.
    fn watch_all(&self) -> Result<(), WatchError> {
        let mut watcher = self.watcher.lock();
        if !PER_DIRECTORY {
            watcher.watch(&self.places.work_tree, RecursiveMode::Recursive)?;
            if !self.places.git_dir.starts_with(&self.places.work_tree) {
                watcher.watch(&self.places.git_dir, RecursiveMode::NonRecursive)?;
            }
            return Ok(());
        }
        watcher.watch(&self.places.git_dir, RecursiveMode::NonRecursive)?;
        drop(watcher);
        self.watch_under("")
    }

    /// Watches `directory`, relative to the working tree (empty for its root), and every
    /// directory under it that git does not ignore.
    fn watch_under(&self, directory: &str) -> Result<(), WatchError> {
        let found = walk(&self.repo, &self.places.work_tree, directory)?;
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
        let found: HashSet<PathBuf> = walk(&self.repo, &self.places.work_tree, "")?
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
    /// The git directory's `index` or `HEAD`, which a capture reads.
    GitInput,
    /// A path of the working tree, relative to it with `/` between components.
    WorkTree(String),
    /// Anywhere else: the git directory's other files, a nested repository's own git
    /// directory, or outside both.
    Elsewhere,
}

impl Places {
    fn classify(&self, path: &Path) -> Place {
        if let Ok(inside) = path.strip_prefix(&self.git_dir) {
            let input = inside == Path::new("index") || inside == Path::new("HEAD");
            return if input {
                Place::GitInput
            } else {
                Place::Elsewhere
            };
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
/// does not ignore, as absolute paths: one `git check-ignore` per level of the tree.
///
/// Symbolic links are not followed, since git records the link itself; nested repositories
/// and submodules are left out, since a capture records only their commit.
fn walk(repo: &Repo, work_tree: &Path, start: &str) -> Result<Vec<PathBuf>, WatchError> {
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
        let ignored = repo.ignored(asked.iter().map(String::as_str))?;
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

    use super::*;

    /// Long enough for any platform to deliver an event; a wait that should wake does so well
    /// within it.
    const DELIVERED: Duration = Duration::from_secs(10);

    /// How long a wait that should not wake is given to wake anyway.
    const SILENT: Duration = Duration::from_millis(1500);

    struct Tree {
        dir: tempfile::TempDir,
    }

    impl Tree {
        /// A repository with an ignored `target/` full of directories, and tracked `src/`.
        fn new() -> Self {
            let tree = Self {
                dir: tempfile::tempdir().unwrap(),
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

        fn watch(&self) -> (Watch, Waiter) {
            let repo = Repo::discover(self.dir.path()).unwrap();
            let (watch, mut waiter) = Watch::start(repo).unwrap();
            settle(&mut waiter);
            (watch, waiter)
        }
    }

    /// Waits out whatever the tree's setup still delivers.
    fn settle(waiter: &mut Waiter) {
        while waiter.wait_for(Duration::from_millis(500)).is_some() {}
    }

    fn wakes(waiter: &mut Waiter, what: &str) {
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

    /// A capture reads the working tree and writes objects and a temporary index into the git
    /// directory, none of which wakes the wait, so a capture never triggers the next.
    #[test]
    fn a_capture_never_wakes_the_wait() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn main() { dirty(); }\n");
        tree.write("src/untracked.rs", "\n");
        let (_watch, mut waiter) = tree.watch();
        let (_recorder, reported) = record(&tree);
        let repo = Repo::discover(tree.dir.path()).unwrap();
        repo.capture_worktree().unwrap();
        repo.capture_worktree().unwrap();
        assert_eq!(
            waiter.wait_for(SILENT),
            None,
            "a capture woke the wait; the system reported {:#?}",
            reported.lock()
        );
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
