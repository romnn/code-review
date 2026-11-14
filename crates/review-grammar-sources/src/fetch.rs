//! Finding an entry in a cache, and fetching it into one: under a lock, staged, checked, then
//! published with one rename.

use std::collections::BTreeSet;
use std::fs::{OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime};

use regex::Regex;

use crate::descriptor::{self, Descriptor};
use crate::git::{self, Repository};
use crate::{Entry, Error, Grammar, Invalid, Pinned, Queries};

/// How long a build waits for another to finish fetching the same entry, longer than the few
/// git commands of a fetch may take.
const LOCK_TIMEOUT: Duration = git::TIMEOUT.saturating_mul(3);

/// The ref a fetch writes in its own repository.
const REFERENCE: &str = "refs/code-review/grammar";

/// A query's `; inherits:` line and the directories it names, as tree-house's `read_query`
/// reads it, which then asks for those directories' query files too.
#[expect(
    clippy::expect_used,
    reason = "the pattern is a valid literal, the one tree-house compiles"
)]
static INHERITS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r";+\s*inherits\s*:?\s*([a-z_,()-]+)\s*").expect("the inherits regex compiles")
});

/// Where an entry is in a cache: a valid one, or the first free name for a new one.
enum Slot {
    Valid(Entry),
    Free(PathBuf),
}

/// The valid entry of `pinned` in `cache`, found without writing anything.
pub(crate) fn find(cache: &Path, pinned: Pinned<'_>) -> Option<Entry> {
    match slot(cache, pinned) {
        Slot::Valid(entry) => Some(entry),
        Slot::Free(_) => None,
    }
}

/// Looks through `pinned`'s entries in `cache`: its name, then that name with `.1`, `.2` and
/// so on, which a fetch takes when the entries before are not valid, since a published entry
/// is never changed.
fn slot(cache: &Path, pinned: Pinned<'_>) -> Slot {
    let name = descriptor::entry_name(pinned);
    let mut generation = 0_u32;
    loop {
        let directory = if generation == 0 {
            cache.join(&name)
        } else {
            cache.join(format!("{name}.{generation}"))
        };
        if std::fs::symlink_metadata(&directory).is_err() {
            return Slot::Free(directory);
        }
        if let Ok(entry) = descriptor::check(&directory, pinned) {
            return Slot::Valid(entry);
        }
        generation += 1;
    }
}

/// Fetches `pinned` into `cache` under its lock, unless a build that held the lock before
/// published a valid entry, which is then returned instead.
///
/// Returns [`Error::NotWritable`] when the cache cannot be written.
pub(crate) fn fetch(cache: &Path, pinned: Pinned<'_>) -> Result<Entry, Error> {
    let lock = lock(cache, pinned)?;
    let entry = produce(cache, pinned);
    drop(lock);
    entry
}

/// Takes `pinned`'s advisory lock in `cache`, which the system releases when the returned file
/// closes, also when the build is killed.
///
/// The lock file stays in place, since another build may hold it.
pub(crate) fn lock(cache: &Path, pinned: Pinned<'_>) -> Result<std::fs::File, Error> {
    let locks = cache.join("_locks");
    std::fs::create_dir_all(&locks).map_err(|source| unwritable("create", &locks, source))?;
    let path = locks.join(format!("{}.lock", pinned.name()));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|source| unwritable("create", &path, source))?;
    let started = Instant::now();
    let mut pause = Duration::from_millis(1);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if started.elapsed() < LOCK_TIMEOUT => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(100));
            }
            Err(TryLockError::WouldBlock) => return Err(Error::LockTimeout { path }),
            Err(TryLockError::Error(source)) => {
                return Err(Error::Io {
                    action: "lock",
                    path,
                    source,
                });
            }
        }
    }
}

/// Publishes a valid entry of `pinned` in `cache`, with its lock held: the one another build
/// published while this one waited, or else a new one.
pub(crate) fn produce(cache: &Path, pinned: Pinned<'_>) -> Result<Entry, Error> {
    let destination = match slot(cache, pinned) {
        Slot::Valid(entry) => return Ok(entry),
        Slot::Free(destination) => destination,
    };
    let Some(name) = destination.file_name() else {
        return Err(Error::Io {
            action: "name an entry in",
            path: cache.to_owned(),
            source: ErrorKind::InvalidInput.into(),
        });
    };
    // The lock makes the staging directory this build's alone, and a build killed while it
    // held the lock may have left it behind.
    let staging = cache.join("_tmp").join(name);
    remove_dir(&staging)?;
    std::fs::create_dir_all(&staging).map_err(|source| unwritable("create", &staging, source))?;
    let published = stage(pinned, &staging).and_then(|staged| {
        std::fs::rename(&staged, &destination)
            .map_err(|source| unwritable("publish", &destination, source))?;
        descriptor::check(&destination, pinned).map_err(|source| Error::InvalidFetch {
            name: pinned.to_string(),
            source,
        })
    });
    let removed = remove_dir(&staging);
    let entry = published?;
    removed?;
    Ok(entry)
}

/// Fetches `pinned`'s commit into a repository under `staging` and writes its entry beside it,
/// returning the entry once it is checked.
///
/// The files are read from git's objects, never checked out, so no filter, hook or line-ending
/// setting can change their bytes.
fn stage(pinned: Pinned<'_>, staging: &Path) -> Result<PathBuf, Error> {
    let repository = Repository::init(&staging.join("repository.git"))?;
    let rev = pinned.rev();
    let refspec = format!("{rev}:{REFERENCE}");
    repository.output(&[
        "fetch",
        "--quiet",
        "--depth",
        "1",
        "--no-tags",
        "--no-write-fetch-head",
        "--no-recurse-submodules",
        "--end-of-options",
        pinned.git(),
        &refspec,
    ])?;
    // A pin names a commit, or an annotated tag of one, as some of Helix's do.
    let fetched = repository.output(&["rev-parse", "--verify", REFERENCE])?;
    let fetched = fetched.trim();
    if fetched != rev {
        return Err(Error::WrongRevision {
            name: pinned.to_string(),
            expected: rev.to_owned(),
            found: fetched.to_owned(),
        });
    }
    let commit =
        repository.output(&["rev-parse", "--verify", &format!("{REFERENCE}^{{commit}}")])?;
    let commit = commit.trim();
    // The files take the commit's time, as an archive of it would, so a build script watching
    // them does not run again only because they were just written.
    let committed = repository.output(&["show", "--no-patch", "--format=%ct", commit])?;
    let committed = committed
        .trim()
        .parse()
        .ok()
        .and_then(|seconds| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let staged = Staged {
        repository: &repository,
        commit,
        entry: staging.join("entry"),
        committed,
    };
    let files = match pinned {
        Pinned::Grammar(grammar) => staged.grammar(grammar)?,
        Pinned::Queries(queries) => staged.queries(queries)?,
    };
    let descriptor = Descriptor::new(pinned, files);
    let text = toml::to_string(&descriptor).map_err(|source| Error::Describe {
        path: staged.entry.clone(),
        source,
    })?;
    write(&staged.entry.join(descriptor::FILE), &text, committed)?;
    descriptor::check(&staged.entry, pinned).map_err(|source| Error::InvalidFetch {
        name: pinned.to_string(),
        source,
    })?;
    Ok(staged.entry)
}

/// A fetched commit, whose files are written to a staged entry.
struct Staged<'a> {
    repository: &'a Repository,
    /// The commit, which a pinned tag names.
    commit: &'a str,
    /// The entry's directory.
    entry: PathBuf,
    /// The commit's time, which every file takes.
    committed: SystemTime,
}

impl Staged<'_> {
    /// Writes `grammar`'s `src/` and license to the entry, returning the files written.
    fn grammar(&self, grammar: &Grammar) -> Result<Vec<descriptor::File>, Error> {
        let src = match &grammar.subpath {
            Some(subpath) => format!("{subpath}/src"),
            None => "src".to_owned(),
        };
        let listing = self.list(&[&src, &grammar.license_file])?;
        let mut files = Vec::new();
        for blob in blobs(&listing) {
            // The license keeps one name in every entry; everything else is the grammar's `src/`.
            let path = if blob.path == grammar.license_file {
                "LICENSE".to_owned()
            } else if let Some(rest) = blob
                .path
                .strip_prefix(&src)
                .and_then(|rest| rest.strip_prefix('/'))
            {
                format!("src/{rest}")
            } else {
                continue;
            };
            if let Some(file) = self.write_blob(&blob, path)? {
                files.push(file);
            }
        }
        Ok(files)
    }

    /// Writes Helix's license and the query files of `queries`' directories to the entry, with
    /// those of every directory their `; inherits:` lines name, returning the files written.
    ///
    /// A directory of neither the manifest nor a query's `; inherits:` line is left out, and so
    /// is a kind of query a directory does not have.
    fn queries(&self, queries: &Queries) -> Result<Vec<descriptor::File>, Error> {
        let mut files = Vec::new();
        let license = self.list(&[Queries::LICENSE_FILE])?;
        for blob in blobs(&license).filter(|blob| blob.path == Queries::LICENSE_FILE) {
            files.extend(self.write_blob(&blob, "LICENSE".to_owned())?);
        }
        let mut pending: Vec<String> = queries.directories.clone();
        let mut seen = BTreeSet::new();
        while let Some(directory) = pending.pop() {
            if !seen.insert(directory.clone()) {
                continue;
            }
            let upstream = format!("{}/{directory}", Queries::UPSTREAM);
            let wanted: Vec<String> = Queries::KINDS
                .iter()
                .map(|kind| format!("{upstream}/{kind}"))
                .collect();
            let wanted: Vec<&str> = wanted.iter().map(String::as_str).collect();
            let listing = self.list(&wanted)?;
            let mut found = false;
            for blob in blobs(&listing) {
                let Some(kind) = blob
                    .path
                    .strip_prefix(&upstream)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .filter(|kind| Queries::KINDS.contains(kind))
                else {
                    continue;
                };
                let path = format!("{}/{directory}/{kind}", Queries::DIRECTORY);
                let Some(file) = self.write_blob(&blob, path)? else {
                    continue;
                };
                found = true;
                let text =
                    std::fs::read_to_string(self.entry.join(&file.path)).map_err(|source| {
                        Error::Io {
                            action: "read",
                            path: self.entry.join(&file.path),
                            source,
                        }
                    })?;
                for inherits in INHERITS.captures_iter(&text) {
                    let Some(names) = inherits.get(1) else {
                        continue;
                    };
                    pending.extend(names.as_str().split(',').map(str::to_owned));
                }
                files.push(file);
            }
            // A directory that is not Helix's is a row the manifest or a query gets wrong.
            if !found {
                return Err(Error::InvalidFetch {
                    name: Pinned::Queries(queries).to_string(),
                    source: Invalid::Missing { path: upstream },
                });
            }
        }
        Ok(files)
    }

    /// The `git ls-tree` listing of `paths` in the commit, relative to its root.
    fn list(&self, paths: &[&str]) -> Result<String, Error> {
        let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", self.commit, "--"];
        args.extend(paths);
        self.repository.output(&args)
    }

    /// Writes `blob` to the entry at `path`, a relative path with `/` between its components,
    /// and returns its descriptor's record, or `None` for a path that could leave the entry.
    fn write_blob(&self, blob: &Blob<'_>, path: String) -> Result<Option<descriptor::File>, Error> {
        let Some(relative) = descriptor::relative(&path) else {
            return Ok(None);
        };
        let size = extract(
            self.repository,
            blob.object,
            &self.entry.join(relative),
            self.committed,
        )?;
        Ok(Some(descriptor::File {
            path,
            size,
            blob: blob.object.to_owned(),
        }))
    }
}

/// One file of `git ls-tree` output.
struct Blob<'a> {
    object: &'a str,
    path: &'a str,
}

/// The regular files `git ls-tree -z` lists, skipping symbolic links and submodules.
fn blobs(listing: &str) -> impl Iterator<Item = Blob<'_>> {
    listing.split('\0').filter_map(|record| {
        let (meta, path) = record.split_once('\t')?;
        let mut fields = meta.split(' ');
        let (Some(mode), Some("blob"), Some(object)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        matches!(mode, "100644" | "100755").then_some(Blob { object, path })
    })
}

/// Writes the blob `object` to `target`, stamped with `time`, and returns its size.
fn extract(
    repository: &Repository,
    object: &str,
    target: &Path,
    time: SystemTime,
) -> Result<u64, Error> {
    let io = |action, path: &Path| {
        let path = path.to_owned();
        move |source| unwritable(action, &path, source)
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(io("create", parent))?;
    }
    let file = std::fs::File::create(target).map_err(io("create", target))?;
    repository.output_to(&["cat-file", "blob", object], file)?;
    stamp(target, time)
}

/// Writes `text` to `path`, stamped with `time`.
fn write(path: &Path, text: &str, time: SystemTime) -> Result<(), Error> {
    std::fs::write(path, text).map_err(|source| unwritable("write", path, source))?;
    stamp(path, time).map(|_| ())
}

/// Sets the modification time of the file at `path`, and returns its size.
fn stamp(path: &Path, time: SystemTime) -> Result<u64, Error> {
    let io = |action| move |source| unwritable(action, path, source);
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io("open"))?;
    file.set_modified(time).map_err(io("stamp"))?;
    Ok(file.metadata().map_err(io("measure"))?.len())
}

/// The error of failing to `action` `path`: [`Error::NotWritable`] when the cache is read-only
/// or holds what this build may not change, such as another user's staging directory, so a
/// lookup falls back to its other cache, and [`Error::Io`] otherwise.
fn unwritable(action: &'static str, path: &Path, source: std::io::Error) -> Error {
    if matches!(
        source.kind(),
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
    ) {
        Error::NotWritable {
            path: path.to_owned(),
            source,
        }
    } else {
        Error::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

/// Removes `path` and everything in it, if it exists.
fn remove_dir(path: &Path) -> Result<(), Error> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(unwritable("remove", path, source)),
    }
}

/// Publishes `entry`'s files as prepared sources of `pinned` at `directory`, which is never
/// changed once it exists, so a build reading it never sees a file change or go.
///
/// A directory that already holds `pinned`'s sources is left as it is, and a new one is staged
/// beside it and renamed into place at once.
/// A directory that holds anything else is refused with [`Error::PreparedMismatch`].
pub(crate) fn prepare(entry: &Entry, directory: &Path, pinned: Pinned<'_>) -> Result<(), Error> {
    let refuse = |directory: &Path| match descriptor::check(directory, pinned) {
        Ok(_) => Ok(()),
        Err(source) => Err(Error::PreparedMismatch {
            path: directory.to_owned(),
            source,
        }),
    };
    if std::fs::symlink_metadata(directory).is_ok() {
        return refuse(directory);
    }
    let (Some(parent), Some(name)) = (directory.parent(), directory.file_name()) else {
        return Err(Error::Io {
            action: "prepare",
            path: directory.to_owned(),
            source: ErrorKind::InvalidInput.into(),
        });
    };
    let staging = parent.join(format!(
        ".{}.staging-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    remove_dir(&staging)?;
    for file in &entry.files {
        let Ok(relative) = file.strip_prefix(&entry.directory) else {
            continue;
        };
        let target = staging.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| unwritable("create", parent, source))?;
        }
        std::fs::copy(file, &target).map_err(|source| Error::Io {
            action: "copy",
            path: file.clone(),
            source,
        })?;
    }
    // A rename onto a directory another run published meanwhile fails, and that directory is
    // then judged as any existing one.
    let published = std::fs::rename(&staging, directory);
    let removed = remove_dir(&staging);
    match published {
        Ok(()) => removed,
        Err(_) if std::fs::symlink_metadata(directory).is_ok() => {
            removed?;
            refuse(directory)
        }
        Err(source) => Err(unwritable("publish", directory, source)),
    }
}
