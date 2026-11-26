//! On-disk storage of one review under `<data-dir>/reviews/<encoded-key>/`.
//!
//! - `events.jsonl`: the append-only event log ([`log`]).
//! - `review.json`: what the review targets and in which repository.
//! - `review.lock`: serializes state transitions.
//! - `synced.json`: when the review last read its target's state, written atomically.
//! - `analysis/<base>-<head>.json`: content-addressed analyses, written atomically.
//! - `analysis/<base>-<head>--<base>-<head>.json`: content-addressed correspondences.
//! - `view/<front-end>.json`: each front end's own view state, such as folded folders, written
//!   atomically and never part of the review's state.
//! - `archive/`: a private bare git repository with every snapshot the review recorded or shows,
//!   pinned by its refs; see [`crate::Review::snapshots`].
//!
//! Beside the reviews, `clones/<digest>.json` binds a clone's identity to its common git
//! directory, `locks/<digest>.lock` is each review's lease, which everything that has it open
//! holds shared ([`InUse`]), `dropped/` holds what a dropped review left when removing it was
//! cut short, and `cache.lock` serializes writing to the analysis caches and keeping them within
//! their budget.

pub mod events;
pub mod log;

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::Error;
use crate::ids::{ObjectId, ReviewKey};

/// The paths of one review's directory.
#[derive(Debug, Clone)]
pub struct ReviewDir {
    /// The data directory it is in.
    data: PathBuf,
    root: PathBuf,
}

impl ReviewDir {
    /// Returns the directory of review `key` under `data_dir`.
    #[must_use]
    pub fn new(data_dir: &Path, key: &ReviewKey) -> Self {
        Self {
            data: data_dir.to_path_buf(),
            root: data_dir.join("reviews").join(key.encoded()),
        }
    }

    /// Returns the directory `root` of a review under `data_dir`, as listed there.
    #[cfg(test)]
    pub(crate) fn listed(data_dir: &Path, root: PathBuf) -> Self {
        Self {
            data: data_dir.to_path_buf(),
            root,
        }
    }

    /// Returns the data directory the review is in, which holds other reviews too.
    #[must_use]
    pub(crate) fn data_dir(&self) -> &Path {
        &self.data
    }

    /// Returns the review's directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the event log's path.
    #[must_use]
    pub fn events(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }

    /// Returns the lock file's path.
    #[must_use]
    pub fn lock(&self) -> PathBuf {
        self.root.join("review.lock")
    }

    /// Returns the path of what the review is about: the pull request or the commits.
    #[must_use]
    pub fn info(&self) -> PathBuf {
        self.root.join("info.json")
    }

    /// Returns the path of when the review last synced with its target.
    #[must_use]
    pub fn synced(&self) -> PathBuf {
        self.root.join("synced.json")
    }

    /// Returns the path of the review's archive of snapshots.
    #[must_use]
    pub fn archive(&self) -> PathBuf {
        self.root.join("archive")
    }

    /// Returns the metadata file's path.
    #[must_use]
    pub fn meta(&self) -> PathBuf {
        self.root.join("review.json")
    }

    /// Returns the analysis cache path of a snapshot pair.
    #[must_use]
    pub fn analysis(&self, base: &ObjectId, head: &ObjectId) -> PathBuf {
        self.root
            .join("analysis")
            .join(format!("{base}-{head}.json"))
    }

    /// Returns the correspondence cache path between two snapshot pairs.
    #[must_use]
    pub fn correspondence(&self, older: [&ObjectId; 2], newer: [&ObjectId; 2]) -> PathBuf {
        let [older_base, older_head] = older;
        let [newer_base, newer_head] = newer;
        self.root.join("analysis").join(format!(
            "{older_base}-{older_head}--{newer_base}-{newer_head}.json"
        ))
    }

    /// Returns the path of `front_end`'s view state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidFrontEnd`] unless `front_end` is 1 to 64 lowercase ASCII
    /// letters, digits, `-` and `_`, so it is always one plain file name.
    pub fn view_state(&self, front_end: &str) -> Result<PathBuf, Error> {
        let plain = (1..=64).contains(&front_end.len())
            && front_end.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            });
        if !plain {
            return Err(Error::InvalidFrontEnd(front_end.to_owned()));
        }
        Ok(self.root.join("view").join(format!("{front_end}.json")))
    }
}

/// A review held open: a shared lock on its lease, a file in `locks/` of the data directory that
/// is never moved or removed, which every clone of the review, its sessions, probes and its
/// archive's readers hold until the last one goes.
///
/// Only a holder of the lease alone, exclusively, changes what others may be using: an open
/// maintaining the review's storage, and [`crate::Review::remove`].
#[derive(Debug, Clone)]
pub(crate) struct InUse {
    #[expect(
        dead_code,
        reason = "the open file holds the lock for as long as anything holds the review open"
    )]
    lock: Arc<File>,
}

/// A review's lease file, open and not locked yet; see [`InUse`].
pub(crate) struct Lease {
    file: File,
    path: PathBuf,
}

impl Lease {
    /// Opens the lease file of the review in `dir`, creating it, or returns `None` when the
    /// review's directory does not exist.
    ///
    /// The file is `locks/<digest>.lock` in the data directory, named for a digest of the name
    /// of the directory a symbolic link leads to, if one does, which is the key encoded in
    /// ASCII, in lowercase, so a link to a review and two spellings of a key that a filesystem
    /// ignoring case takes for one directory share one lease.
    /// On a filesystem that tells case apart, two reviews whose keys differ only in case share
    /// one too, which only keeps a drop or maintenance of one waiting for the other.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be resolved, a symbolic link leads the lease
    /// out of the data directory, or the file cannot be opened.
    pub(crate) fn open(dir: &ReviewDir) -> Result<Option<Self>, Error> {
        let resolved = match std::fs::canonicalize(&dir.root) {
            Ok(resolved) => resolved,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::io("resolve", &dir.root, source)),
        };
        let name = resolved.file_name().unwrap_or_default().to_string_lossy();
        let digest = crate::analysis::hex_digest(name.to_ascii_lowercase().as_bytes());
        let path = dir.data.join("locks").join(format!("{digest}.lock"));
        if !contained(&dir.data, &path) {
            let leads_out =
                std::io::Error::other("a symbolic link leads out of the data directory");
            return Err(Error::io("open", path, leads_out));
        }
        if let Some(locks) = path.parent() {
            create_dir(locks)?;
        }
        let file = open_lock(&path)?;
        Ok(Some(Self { file, path }))
    }

    /// Takes the lease exclusively unless anything holds it, and returns whether it did.
    pub(crate) fn try_exclusive(&self) -> Result<bool, Error> {
        match self.file.try_lock() {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(source)) => Err(Error::io("lock", &self.path, source)),
        }
    }

    /// Holds the review open with the lease, once a shared lock on it is taken, waiting while
    /// an exclusive holder has it.
    pub(crate) fn shared(self) -> Result<InUse, Error> {
        self.file
            .lock_shared()
            .map_err(|source| Error::io("lock", &self.path, source))?;
        Ok(InUse {
            lock: Arc::new(self.file),
        })
    }
}

/// Opens the lock file at `path`, creating it.
pub(crate) fn open_lock(path: &Path) -> Result<File, Error> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|source| Error::io("open", path, source))
}

/// Returns the default configuration file the front ends and `cr` share,
/// `$XDG_CONFIG_HOME/code-review/config.toml` or `$HOME/.config/code-review/config.toml`, from
/// those variables' values, which the caller reads at its process boundary.
///
/// A relative `$XDG_CONFIG_HOME` is ignored, as the XDG base directory specification asks.
/// Returns `None` when neither variable gives a directory.
#[must_use]
pub fn default_config_file(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = match xdg_config_home.filter(|dir| dir.is_absolute()) {
        Some(dir) => dir.to_path_buf(),
        None => home?.join(".config"),
    };
    Some(base.join("code-review").join("config.toml"))
}

/// Returns the default data directory, `$XDG_DATA_HOME/code-review` or
/// `$HOME/.local/share/code-review`, from those variables' values, which the caller reads at its
/// process boundary.
#[must_use]
pub fn default_data_dir(xdg_data_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    match (xdg_data_home, home) {
        (Some(xdg), _) if !xdg.as_os_str().is_empty() => Some(xdg.join("code-review")),
        (_, Some(home)) => Some(home.join(".local/share/code-review")),
        _ => None,
    }
}

/// Percent-encodes `text` into one safe path component: every byte outside `[A-Za-z0-9_-]`
/// becomes `%XX`, so distinct texts never collide.
#[must_use]
pub fn encode_component(text: &str) -> String {
    crate::ids::encode_component(text)
}

/// Whether `path`, under `data_dir`, stays in it: no part of it below `data_dir` that exists is
/// a symbolic link, which could lead maintenance to remove or rewrite what is not cr's.
pub(crate) fn contained(data_dir: &Path, path: &Path) -> bool {
    let Ok(rest) = path.strip_prefix(data_dir) else {
        return false;
    };
    let mut at = data_dir.to_path_buf();
    for part in rest.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return false;
        }
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(metadata) if metadata.is_symlink() => return false,
            Ok(_) => {}
            // What does not exist yet leads nowhere.
            Err(_) => return true,
        }
    }
    true
}

/// The entries of the directory at `path`, or none when it does not exist.
pub(crate) fn entries(path: &Path) -> Result<Vec<PathBuf>, Error> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::io("read", path, source)),
    };
    entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|source| Error::io("read", path, source))
        })
        .collect()
}

/// Creates `path` and its parents.
pub(crate) fn create_dir(path: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(path).map_err(|source| Error::io("create directory", path, source))
}

/// Writes `value` as JSON to `path` atomically: a temporary file in the same directory, synced
/// and then renamed over the target, so readers see the old file or the complete new one.
pub(crate) fn write_json_atomic<T: Serialize>(
    path: &Path,
    value: &T,
    what: &'static str,
) -> Result<(), Error> {
    let bytes = serde_json::to_vec(value).map_err(|source| Error::Encode { what, source })?;
    write_atomic(path, &bytes)
}

/// Writes `bytes` to `path` through a synced temporary file in the same directory renamed over
/// it, so readers see the old file or the complete new one.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write as _;

    let directory = path.parent().unwrap_or(Path::new("."));
    create_dir(directory)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = directory.join(format!(".{file_name}.{}-{nanos}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)
            .map_err(|source| Error::io("create", &temporary, source))?;
        file.write_all(bytes)
            .map_err(|source| Error::io("write", &temporary, source))?;
        file.sync_data()
            .map_err(|source| Error::io("sync", &temporary, source))?;
        std::fs::rename(&temporary, path).map_err(|source| Error::io("rename", path, source))
    })();
    if result.is_err() {
        // Leave no temporary behind; a failure to delete it only wastes space.
        if let Err(err) = std::fs::remove_file(&temporary) {
            drop(err);
        }
    }
    result
}

/// Removes the file or the directory at `path`, and everything below it, giving the owner the
/// write permission first wherever it lacks it; a symbolic link is removed, never followed, and
/// a missing path is no error.
///
/// Older versions wrote read-only copies of the reviewed files into a review's data, and git
/// writes its packs read-only, which a plain removal cannot remove: on Unix their directories
/// refuse to lose entries, and on Windows a read-only file refuses to be deleted.
pub(crate) fn remove_read_only(path: &Path) -> Result<(), Error> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(Error::io("read", path, source)),
    };
    make_writable(path)?;
    let removed = if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    removed.map_err(|source| Error::io("remove", path, source))
}

/// Gives the owner the write permission on `path` and everything below it, without following
/// symbolic links.
pub(crate) fn make_writable(path: &Path) -> Result<(), Error> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|source| Error::io("read", path, source))?;
    if metadata.is_symlink() {
        return Ok(());
    }
    if metadata.permissions().readonly() {
        set_writable(path, &metadata)?;
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|source| Error::io("read", path, source))? {
            let entry = entry.map_err(|source| Error::io("read", path, source))?;
            make_writable(&entry.path())?;
        }
    }
    Ok(())
}

/// Gives the owner of `path`, whose metadata is `metadata`, the write permission.
#[cfg(unix)]
fn set_writable(path: &Path, metadata: &std::fs::Metadata) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = metadata.permissions().mode() | 0o200;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|source| Error::io("change permissions of", path, source))
}

/// Clears the read-only attribute of `path`, whose metadata is `metadata`.
#[cfg(not(unix))]
fn set_writable(path: &Path, metadata: &std::fs::Metadata) -> Result<(), Error> {
    let mut permissions = metadata.permissions();
    #[expect(
        clippy::permissions_set_readonly_false,
        reason = "Windows has no mode bits to make world-writable: clearing the attribute only \
                  lets the file be deleted"
    )]
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions)
        .map_err(|source| Error::io("change permissions of", path, source))
}

/// Reads JSON from `path`, or `None` when it does not exist.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::io("read", path, source)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| Error::Decode {
            path: path.to_path_buf(),
            source,
        })
}

/// Returns the current time in seconds since the Unix epoch.
pub(crate) fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}
