//! A clone's identity, kept in cr's data: a random id bound to the clone's common git directory
//! by its path and by what the filesystem tells the directory apart by.
//!
//! Every worktree of a clone shares its common git directory and so its id; another clone has
//! another directory and another id, and so does a clone made anew at the same path, whose
//! directory the filesystem tells apart from the old one's.
//! Nothing is written to the clone: a binding lives under `clones/` in the data directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{ReviewMeta, ReviewTarget};
use crate::error::Error;
use crate::git::Repo;
use crate::ids::ObjectId;
use crate::store;
use crate::store::log::LockGuard;

/// The ref an earlier version named a clone's identity by, in the clone itself.
const LEGACY_CLONE_REF: &str = "refs/code-review/clone-id";

/// A clone's id and the common git directory it is bound to.
#[derive(Debug, Serialize, Deserialize)]
struct Binding {
    /// The common git directory, resolved.
    common_dir: PathBuf,
    /// What the filesystem tells the directory apart by; see [`identity`].
    identity: String,
    id: ObjectId,
}

/// Where the binding of a clone is kept.
struct Located {
    /// The clone's common git directory, resolved.
    common_dir: PathBuf,
    /// The directory of every binding, `clones/` in the data directory.
    clones: PathBuf,
    /// The binding's file, named by a digest of the directory.
    file: PathBuf,
}

/// Returns the id of the clone `repo`'s checkout is in now, or `None` when it has none, as
/// before [`ensure_clone_id`] or once another clone took the place of the one bound.
///
/// The checkout's common git directory is found anew each time, so a worktree path that
/// belongs to another clone now has that clone's id, whatever `repo` found when it was opened.
pub(crate) fn clone_id(data_dir: &Path, repo: &Repo) -> Result<Option<ObjectId>, Error> {
    let Located {
        common_dir, file, ..
    } = locate(data_dir, repo)?;
    let identity = identity(&common_dir)?;
    Ok(store::read_json::<Binding>(&file)?
        .filter(|bound| bound.common_dir == common_dir && bound.identity == identity)
        .map(|bound| bound.id))
}

/// Returns the id of the clone `repo`'s checkout is in, binding one on first use.
///
/// A path has one binding: another directory at a bound path, a clone made anew or restored
/// there, gets a new random id, which replaces the binding.
/// Only the first binding of a path adopts the id an earlier version kept in the clone's ref,
/// and only when a review of a checkout stored in `data_dir` has that id, its checkout is
/// still a worktree of this very clone, and no binding holds the id yet, so a copy of the
/// clone, which carries the ref too, never takes the review: not at another path, whichever is
/// opened first, nor with a worktree at the review's own path after the clone was bound.
/// Bindings are made one at a time, so of two first uses at once, the second takes the first's.
pub(crate) fn ensure_clone_id(data_dir: &Path, repo: &Repo) -> Result<ObjectId, Error> {
    let Located {
        common_dir,
        clones,
        file,
    } = locate(data_dir, repo)?;
    let lock = LockGuard::acquire(&clones.join("bindings.lock"))?;
    let identity = identity(&common_dir)?;
    let bound = store::read_json::<Binding>(&file)?;
    if let Some(bound) = &bound
        && bound.common_dir == common_dir
        && bound.identity == identity
    {
        return Ok(bound.id.clone());
    }
    let adopted = match bound {
        Some(_) => None,
        None => adopted(data_dir, repo, &common_dir)?.filter(|legacy| !held(&clones, legacy)),
    };
    let id = match adopted {
        Some(legacy) => legacy,
        None => ObjectId::parse(&crate::random::hex(40))?,
    };
    let binding = Binding {
        common_dir,
        identity,
        id: id.clone(),
    };
    store::write_json_atomic(&file, &binding, "clone identity")?;
    drop(lock);
    Ok(id)
}

fn locate(data_dir: &Path, repo: &Repo) -> Result<Located, Error> {
    let current = repo.current_common_dir()?;
    let common_dir =
        std::fs::canonicalize(&current).map_err(|err| Error::io("resolve", &current, err))?;
    let mut name = crate::analysis::hex_digest(common_dir.to_string_lossy().as_bytes());
    name.truncate(16);
    let clones = data_dir.join("clones");
    crate::git::refuse_inside(&clones, repo)?;
    store::create_dir(&clones)?;
    Ok(Located {
        common_dir,
        file: clones.join(format!("{name}.json")),
        clones,
    })
}

/// Whether a binding in `clones` holds `id`.
///
/// Only published bindings count: a temporary file a crash left behind before its rename never
/// bound anything.
fn held(clones: &Path, id: &ObjectId) -> bool {
    std::fs::read_dir(clones)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
        .any(|entry| {
            store::read_json::<Binding>(&entry.path())
                .is_ok_and(|bound| bound.is_some_and(|bound| &bound.id == id))
        })
}

/// The id an earlier version named `repo`'s clone by, when a review of a checkout stored in
/// `data_dir` has it and its checkout is a worktree of the clone whose common git directory is
/// `common_dir`, resolved.
fn adopted(data_dir: &Path, repo: &Repo, common_dir: &Path) -> Result<Option<ObjectId>, Error> {
    let Some(legacy) = repo.ref_target(LEGACY_CLONE_REF)? else {
        return Ok(None);
    };
    let reviews = data_dir.join("reviews");
    let entries = match std::fs::read_dir(&reviews) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::io("read", reviews, err)),
    };
    for entry in entries {
        let entry = entry.map_err(|err| Error::io("read", &reviews, err))?;
        // A review that cannot be read, or whose checkout is gone or another clone's, is not
        // this clone's as far as anyone can tell.
        if let Ok(Some(meta)) = store::read_json::<ReviewMeta>(&entry.path().join("review.json"))
            && meta.target
                == (ReviewTarget::Checkout {
                    clone: legacy.clone(),
                })
            && let Ok(checkout) = Repo::discover(&meta.repo)
            && std::fs::canonicalize(checkout.common_dir()).is_ok_and(|its| its == common_dir)
        {
            return Ok(Some(legacy));
        }
    }
    Ok(None)
}

/// What the filesystem tells the directory at `path` apart by: its device and inode, and its
/// creation time where the filesystem keeps one, so an inode reused by a new directory still
/// differs.
#[cfg(unix)]
fn identity(path: &Path) -> Result<String, Error> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(path).map_err(|err| Error::io("read", path, err))?;
    let created = metadata
        .created()
        .ok()
        .and_then(|created| created.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|created| created.as_nanos().to_string())
        .unwrap_or_default();
    Ok(format!("{}:{}:{created}", metadata.dev(), metadata.ino()))
}

/// What the filesystem tells the directory at `path` apart by: its volume's serial number and
/// its file index, whose sequence number changes when NTFS reuses the record.
#[cfg(windows)]
fn identity(path: &Path) -> Result<String, Error> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
    };

    // Opening a directory takes the flag that backup programs open one with.
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(|err| Error::io("open", path, err))?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open for as long as `directory` lives, and `information` is a
    // structure of the type the call writes.
    #[expect(
        unsafe_code,
        reason = "std has no stable way to read a directory's file index"
    )]
    let read =
        unsafe { GetFileInformationByHandle(directory.as_raw_handle(), &raw mut information) };
    if read == 0 {
        return Err(Error::io("read", path, std::io::Error::last_os_error()));
    }
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok(format!("{}:{index}", information.dwVolumeSerialNumber))
}
