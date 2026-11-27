//! Keeping a private repository to what its refs reach: every snapshot they pin, whole and
//! without its parents, in one pack with deltas.
//!
//! Only a process that holds the review's lease alone compacts, so nothing else reads or writes
//! the repository meanwhile (see [`crate::store::InUse`]).
//! Git only writes the new pack, into a directory of its own; cr installs it beside the packs it
//! replaces and only then removes those and the loose objects, so a compaction cut short at any
//! point leaves every pinned object readable.

use std::ffi::OsStr;
use std::path::Path;

use super::{PrivateRepo, Repo, durable, object_lines};
use crate::error::Error;
use crate::ids::ObjectId;
use crate::store;

/// Loose objects beyond which a repository is due to be compacted, as git's `gc.auto` has it.
const LOOSE_OBJECTS: u64 = 6700;

/// Packs beyond which a repository is due to be compacted, as git's `gc.autoPackLimit` has it.
const PACKS: u64 = 50;

/// The file whose presence says the repository was compacted once, written once a compaction
/// is done, so a repository a compaction never finished, such as one a move from an earlier
/// version filled with packs without deltas, is due.
const COMPACTED: &str = "compacted";

/// Where a compaction stands, for a test to cut it short there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The new pack is written in its own directory; nothing is installed.
    Staged,
    /// The new pack is installed beside the packs it replaces.
    Installed,
    /// One of the packs it replaces is removed.
    Removed,
}

impl PrivateRepo {
    /// Whether the repository is due to be compacted: when no compaction of it ever finished,
    /// when it holds more loose objects or packs than git's own thresholds for collecting
    /// garbage allow, or, when space is `pressed`, when it holds more than one pack or any
    /// loose object.
    pub(crate) fn compaction_due(&self, pressed: bool) -> Result<bool, Error> {
        if !self.path().join(COMPACTED).exists() {
            return Ok(true);
        }
        let output = Repo::run_command(self.repo().command(), ["count-objects", "-v"])?;
        let count = |name: &str| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.strip_prefix(name)?.trim().parse::<u64>().ok())
                .unwrap_or_default()
        };
        let loose = count("count:");
        let packs = count("packs:");
        Ok(if pressed {
            loose > 0 || packs > 1
        } else {
            loose > LOOSE_OBJECTS || packs > PACKS
        })
    }

    /// Packs every object `roots` reach, without a commit's parents, into one pack with deltas,
    /// and removes every other pack and loose object; the caller holds the review's lease alone.
    ///
    /// Git writes the pack into a directory under [`Self::scratch`], where it is checked to
    /// hold every root whole; cr then installs it and only then removes what it replaces.
    /// `step` runs at each [`Step`], and an error it returns ends the compaction there, as
    /// when cr is killed, leaving the directory for the next open to sweep.
    ///
    /// # Errors
    ///
    /// Returns an error when a root is not whole, or git or the file system fails.
    pub(crate) fn compact(
        &self,
        roots: &[ObjectId],
        step: &mut dyn FnMut(Step) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let pack_dir = self.objects().join("pack");
        let kept = if roots.is_empty() {
            None
        } else {
            // With the path each object is at, by which git pairs the versions of a file to
            // delta one against the other.
            let listed = Repo::run_command_with_input(
                self.repo().command(),
                &["rev-list", "--objects", "--no-walk=unsorted", "--stdin"],
                object_lines(roots).into_bytes(),
                &[0],
            )?;
            let staged = self.scratch().join(crate::random::hex(12));
            let staged_packs = staged.join("pack");
            store::create_dir(&staged_packs)?;
            let mut pack = self.repo().command();
            durable(&mut pack);
            // Nothing of the clone's configuration, and one pack whatever its size.
            pack.env("GIT_CONFIG_COUNT", "0");
            let written = Repo::run_command_with_input(
                pack,
                &[
                    "-c",
                    "pack.packSizeLimit=0",
                    "pack-objects",
                    "--quiet",
                    "--delta-base-offset",
                    &staged_packs.join("pack").to_string_lossy(),
                ],
                listed.stdout,
                &[0],
            )?;
            let name = written_pack(&written.stdout, &staged_packs)?;
            step(Step::Staged)?;
            // The new pack alone holds every root whole.
            let mut alone = self.repo().command();
            alone
                .env("GIT_OBJECT_DIRECTORY", &staged)
                .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
            Repo::run_command_with_input(
                alone,
                &["rev-list", "--objects", "--no-walk=unsorted", "--stdin"],
                object_lines(roots).into_bytes(),
                &[0],
            )?;
            // What was checked replaces whatever has its name, the index last, since git takes
            // a pack for one once its index is there.
            for extension in ["pack", "rev", "idx"] {
                let file = format!("{name}.{extension}");
                let from = staged_packs.join(&file);
                if from.exists() {
                    install(&from, &pack_dir.join(&file))?;
                }
            }
            sync_dir(&pack_dir)?;
            step(Step::Installed)?;
            store::remove_read_only(&staged)?;
            Some(name)
        };
        let files = store::entries(&pack_dir)?;
        let mut replaced: Vec<&OsStr> = files
            .iter()
            .filter(|file| {
                file.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .filter_map(|file| file.file_stem())
            .filter(|name| kept.as_deref().is_none_or(|kept| *name != kept))
            .collect();
        replaced.sort();
        for name in replaced {
            // The index first, so git stops taking the pack for one before it goes.
            store::remove_read_only(&pack_dir.join(name).with_extension("idx"))?;
            for file in files.iter().filter(|file| file.file_stem() == Some(name)) {
                store::remove_read_only(file)?;
            }
            step(Step::Removed)?;
        }
        for directory in store::entries(&self.objects())? {
            let loose = directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.len() == 2 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
            if loose {
                store::remove_read_only(&directory)?;
            }
        }
        store::write_atomic(&self.path().join(COMPACTED), b"")
    }
}

/// The name, `pack-<hash>`, of the one pack `pack-objects` wrote into `staged`, by what it
/// printed, `written`: exactly one hash, whose pack and index are both there.
fn written_pack(written: &[u8], staged: &Path) -> Result<String, Error> {
    let printed = String::from_utf8_lossy(written);
    let mut hashes = printed.split_whitespace();
    let name = match (hashes.next(), hashes.next()) {
        (Some(hash), None) if hash.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            format!("pack-{hash}")
        }
        _ => String::new(),
    };
    let whole = !name.is_empty()
        && ["pack", "idx"]
            .iter()
            .all(|extension| staged.join(format!("{name}.{extension}")).is_file());
    if whole {
        Ok(name)
    } else {
        Err(Error::CommandFailed {
            program: "git",
            args: "pack-objects".to_owned(),
            stderr: format!("wrote other than one whole pack: {}", printed.trim()),
        })
    }
}

/// Moves the file `from`, which git wrote, to `to`, replacing any file there, once its content
/// is on disk; on Windows the move itself is too, and on Unix the caller syncs the directory.
fn install(from: &Path, to: &Path) -> Result<(), Error> {
    // Git writes its packs read-only, which Windows refuses to flush or replace.
    store::make_writable(from)?;
    if to.exists() {
        store::make_writable(to)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(from)
        .and_then(|file| file.sync_all())
        .map_err(|source| Error::io("sync", from, source))?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;

        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };

        // Extended-length, as `std::fs::rename` passes them, so a path past `MAX_PATH` moves.
        let wide = |path: &Path| -> Result<Vec<u16>, Error> {
            let absolute =
                std::path::absolute(path).map_err(|source| Error::io("resolve", path, source))?;
            let absolute: Vec<u16> = absolute.as_os_str().encode_wide().collect();
            let mut extended = extended_length(&absolute);
            extended.push(0);
            Ok(extended)
        };
        let from_wide = wide(from)?;
        let to_wide = wide(to)?;
        // SAFETY: both are NUL-terminated wide strings that live through the call.
        #[expect(
            unsafe_code,
            reason = "std has no way to rename a file through to the disk"
        )]
        let moved = unsafe {
            MoveFileExW(
                from_wide.as_ptr(),
                to_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            return Err(Error::io("move", from, std::io::Error::last_os_error()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    std::fs::rename(from, to).map_err(|source| Error::io("move", from, source))
}

/// `absolute`, an absolute Windows path in UTF-16, as an extended-length one, which Windows
/// takes past `MAX_PATH`, as Rust's own file functions make it: `\\?\` before a drive's path
/// and in place of a device path's `\\.\`, `\\?\UNC\` in place of the `\\` that starts a
/// share's, and one already extended as it is.
#[cfg(any(windows, test))]
fn extended_length(absolute: &[u16]) -> Vec<u16> {
    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
    if absolute.starts_with(&wide(r"\\?\")) {
        return absolute.to_vec();
    }
    if let Some(device) = absolute.strip_prefix(wide(r"\\.\").as_slice()) {
        return [wide(r"\\?\").as_slice(), device].concat();
    }
    match absolute.strip_prefix(wide(r"\\").as_slice()) {
        Some(share) => [wide(r"\\?\UNC\").as_slice(), share].concat(),
        None => [wide(r"\\?\").as_slice(), absolute].concat(),
    }
}

/// Makes the entries of the directory at `path` durable, as renames into it are not by
/// themselves on Unix; Windows makes a rename durable by itself.
fn sync_dir(path: &Path) -> Result<(), Error> {
    if cfg!(unix) {
        std::fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| Error::io("sync", path, source))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::*;

    /// Only output that names exactly one pack, whose pack and index are both there, counts:
    /// two hashes, as a split writes, or a pack without its index, is refused.
    #[test]
    fn only_one_whole_pack_counts() -> eyre::Result<()> {
        let staged = tempfile::tempdir()?;
        for name in ["pack-aa", "pack-bb"] {
            for extension in ["pack", "idx"] {
                std::fs::write(staged.path().join(format!("{name}.{extension}")), b"")?;
            }
        }
        assert_eq!(written_pack(b"aa\n", staged.path())?, "pack-aa");
        assert!(written_pack(b"aa\nbb\n", staged.path()).is_err());
        assert!(written_pack(b"", staged.path()).is_err());
        std::fs::remove_file(staged.path().join("pack-bb.idx"))?;
        assert!(written_pack(b"bb\n", staged.path()).is_err());
        Ok(())
    }

    /// A drive's path, a device's and a share's become extended-length, and one already
    /// extended stays.
    #[test]
    fn windows_paths_become_extended_length() {
        let wide = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
        for (path, extended) in [
            (r"C:\data\pack-aa.idx", r"\\?\C:\data\pack-aa.idx"),
            (
                r"\\server\share\pack-aa.idx",
                r"\\?\UNC\server\share\pack-aa.idx",
            ),
            (r"\\?\C:\data\pack-aa.idx", r"\\?\C:\data\pack-aa.idx"),
            (r"\\.\C:\data\pack-aa.idx", r"\\?\C:\data\pack-aa.idx"),
        ] {
            assert_eq!(extended_length(&wide(path)), wide(extended), "{path}");
        }
    }
}
