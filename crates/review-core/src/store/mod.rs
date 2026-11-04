//! On-disk storage of one review under `<data-dir>/reviews/<encoded-key>/`.
//!
//! - `events.jsonl`: the append-only event log ([`log`]).
//! - `review.json`: what the review targets and in which repository.
//! - `review.lock`: serializes state transitions.
//! - `analysis/<base>-<head>.json`: content-addressed analyses, written atomically.
//! - `analysis/<base>-<head>--<base>-<head>.json`: content-addressed correspondences.
//! - `worktrees/<head>`: detached worktrees for browsing a revision.

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

    /// Returns the browsing worktree path of a head snapshot.
    #[must_use]
    pub fn worktree(&self, head: &ObjectId) -> PathBuf {
        self.root.join("worktrees").join(head.as_str())
    }
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

/// The file remembering the review last opened for `repo`'s location and current branch.
fn remembered_file(data_dir: &Path, repo: &crate::git::Repo) -> Result<PathBuf, Error> {
    let location = repo
        .work_tree()
        .unwrap_or(repo.git_dir())
        .display()
        .to_string();
    let branch = repo.current_branch()?.unwrap_or_else(|| "HEAD".to_owned());
    let name = encode_component(&format!("{location}@{branch}"));
    Ok(data_dir.join("defaults").join(name))
}

/// Remembers `key` as the review of `repo`'s current branch, for commands that take no review.
///
/// # Errors
///
/// Returns an error when git fails or the file cannot be written.
pub fn remember_review(
    data_dir: &Path,
    repo: &crate::git::Repo,
    key: &ReviewKey,
) -> Result<(), Error> {
    let path = remembered_file(data_dir, repo)?;
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    std::fs::write(&path, key.as_str()).map_err(|source| Error::io("write", &path, source))
}

/// Returns the review last remembered for `repo`'s current branch, if any.
///
/// # Errors
///
/// Returns an error when git fails, the file cannot be read, or it holds an invalid key.
pub fn remembered_review(
    data_dir: &Path,
    repo: &crate::git::Repo,
) -> Result<Option<ReviewKey>, Error> {
    let path = remembered_file(data_dir, repo)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(ReviewKey::parse(text.trim())?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::io("read", path, source)),
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
    use std::io::Write as _;

    let directory = path.parent().unwrap_or(Path::new("."));
    create_dir(directory)?;
    let bytes = serde_json::to_vec(value).map_err(|source| Error::Encode { what, source })?;
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
        file.write_all(&bytes)
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
