//! On-disk storage of one review under `<data-dir>/reviews/<encoded-key>/`.
//!
//! - `events.jsonl`: the append-only event log ([`log`]).
//! - `review.json`: what the review targets and in which repository.
//! - `review.lock`: serializes state transitions.
//! - `synced.json`: when the review last read its target's state, written atomically.
//! - `analysis/<base>-<head>.json`: content-addressed analyses, written atomically.
//! - `analysis/<base>-<head>--<base>-<head>.json`: content-addressed correspondences.
//! - `worktrees/<tree>`: read-only copies of a snapshot's files, for browsing it and for the
//!   tools that read it.
//! - `view/<front-end>.json`: each front end's own view state, such as folded folders, written
//!   atomically and never part of the review's state.

pub mod events;
pub mod log;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::Error;
use crate::ids::{ObjectId, ReviewKey};

/// The paths of one review's directory.
#[derive(Debug, Clone)]
pub struct ReviewDir {
    root: PathBuf,
}

impl ReviewDir {
    /// Returns the directory of review `key` under `data_dir`.
    #[must_use]
    pub fn new(data_dir: &Path, key: &ReviewKey) -> Self {
        Self {
            root: data_dir.join("reviews").join(key.encoded()),
        }
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

    /// Returns the path of the read-only copy of the files of `tree`.
    #[must_use]
    pub fn worktree(&self, tree: &ObjectId) -> PathBuf {
        self.root.join("worktrees").join(tree.as_str())
    }
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

/// Takes the write permission away from every file and directory at or below `path`, without
/// following symbolic links, directories last.
pub(crate) fn make_read_only(path: &Path) -> Result<(), Error> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|source| Error::io("read", path, source))?;
    if metadata.is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|source| Error::io("read", path, source))? {
            let entry = entry.map_err(|source| Error::io("read", path, source))?;
            make_read_only(&entry.path())?;
        }
    }
    set_writable(path, &metadata, false)
}

/// Removes the directory at `path` and everything below it, giving back the write permission
/// [`make_read_only`] took first; a missing directory is no error.
pub(crate) fn remove_read_only(path: &Path) -> Result<(), Error> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(Error::io("read", path, source)),
    };
    if metadata.is_dir() && !metadata.is_symlink() {
        set_writable(path, &metadata, true)?;
        for entry in std::fs::read_dir(path).map_err(|source| Error::io("read", path, source))? {
            let entry = entry.map_err(|source| Error::io("read", path, source))?;
            let child = entry.path();
            let kind = entry
                .file_type()
                .map_err(|source| Error::io("read", &child, source))?;
            if kind.is_dir() {
                remove_read_only(&child)?;
            }
        }
        std::fs::remove_dir_all(path).map_err(|source| Error::io("remove", path, source))
    } else {
        std::fs::remove_file(path).map_err(|source| Error::io("remove", path, source))
    }
}

/// Gives the owner of `path` the write permission, or takes it away from everyone.
#[cfg(unix)]
fn set_writable(path: &Path, metadata: &std::fs::Metadata, writable: bool) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = metadata.permissions().mode();
    let mode = if writable {
        mode | 0o200
    } else {
        mode & !0o222
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|source| Error::io("change permissions of", path, source))
}

/// Sets or clears the read-only attribute of `path`.
#[cfg(not(unix))]
fn set_writable(path: &Path, metadata: &std::fs::Metadata, writable: bool) -> Result<(), Error> {
    let mut permissions = metadata.permissions();
    permissions.set_readonly(!writable);
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
