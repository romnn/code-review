//! A private stand-in for a worktree's git directory, so that commands that read the index
//! never touch the real one.
//!
//! Git writes while it reads the index: a split index has git refresh the date of the shared
//! file it is based on, which git looks for in the git directory a command runs in, and a
//! sparse index has git write the trees of the directories it expands into the object
//! database.
//! So a command that reads the index runs in a git directory of cr's own: a detached `HEAD`, a
//! copy of the index and of the shared index it names, copies of the sparse-checkout patterns,
//! `info/exclude` and `info/attributes`, a private repository's object database as its only
//! one, and the clone's configuration as the clone reads it (see [`Repo::clone_context`]).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use indoc::formatdoc;

use super::{PrivateRepo, Repo};
use crate::error::Error;
use crate::ids::ObjectId;

/// Where the commands of a [`Scratch`] keep the objects they import and write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Objects {
    /// In the private repository, for a capture whose snapshot it keeps.
    Private,
    /// In an object database of the directory's own, removed with it, that reads the private
    /// repository's as an alternate, for a probe that keeps nothing.
    Disposable,
}

/// A private git directory standing in for a worktree's own, removed when dropped.
#[derive(Debug)]
pub(crate) struct Scratch {
    /// Runs the directory's commands: bound to no repository, with the clone's configuration.
    repo: Repo,
    work_tree: PathBuf,
    /// The private repository's object database, the directory's only one.
    objects: PathBuf,
    /// The worktree's `HEAD`, or `None` when it is unborn.
    head: Option<ObjectId>,
    /// Whether the index has an unmerged path.
    unmerged: bool,
}

impl Scratch {
    /// Makes a private git directory under `private`'s with a copy of `source`'s index, or a
    /// fresh one from `HEAD` when it has none, and of the other files of its git directories
    /// that commands reading the index read; `HEAD` and every object the index names are where
    /// `objects` says once it returns.
    ///
    /// The objects are listed before any command can expand a sparse index: git drops a
    /// directory it cannot expand without a word, and then a capture would miss it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingObject`] when `source` lacks an object its index or `HEAD`
    /// names, and an error for a bare repository or when git fails.
    pub(crate) fn new(
        private: &PrivateRepo,
        source: &Repo,
        objects: Objects,
    ) -> Result<Self, Error> {
        let work_tree = source.work_tree().ok_or_else(|| {
            Error::InvalidTarget("a bare repository has no working tree".to_owned())
        })?;
        let head = source.resolve_commit("HEAD").ok();
        let clone = source.clone_context()?;
        let dir = private.scratch().join(crate::random::hex(12));
        let mut scratch = Self {
            repo: Repo {
                work_tree: None,
                git_dir: dir.clone(),
                common_dir: dir.clone(),
                env: source.env.clone(),
                no_prompts: source.no_prompts,
                clone: Some(clone),
                lease: private.repo().lease.clone(),
            },
            work_tree: work_tree.to_path_buf(),
            objects: match objects {
                Objects::Private => private.objects(),
                Objects::Disposable => dir.join("objects"),
            },
            head,
            unmerged: false,
        };
        crate::store::create_dir(&dir.join("info"))?;
        crate::store::create_dir(&dir.join("refs"))?;
        if objects == Objects::Disposable {
            crate::store::create_dir(&scratch.objects.join("pack"))?;
            crate::store::create_dir(&scratch.objects.join("info"))?;
            let alternate = format!("{}\n", private.objects().display());
            write(
                &scratch.objects.join("info/alternates"),
                alternate.as_bytes(),
            )?;
        }
        let config = formatdoc! {"
            [core]
            repositoryformatversion = 1
            [extensions]
            objectformat = {format}
        ", format = private.format()};
        write(&dir.join("config"), config.as_bytes())?;
        // An unborn `HEAD` names a branch that does not exist.
        let head = scratch
            .head
            .as_ref()
            .map_or_else(|| "ref: refs/heads/unborn".to_owned(), ToString::to_string);
        write(&dir.join("HEAD"), format!("{head}\n").as_bytes())?;
        let into = match objects {
            Objects::Private => private.repo(),
            Objects::Disposable => &scratch.repo,
        };
        if let Some(head) = &scratch.head {
            super::private::import(into, source, std::slice::from_ref(head))?;
        }
        for (from, name) in [
            (source.git_dir(), "info/sparse-checkout"),
            (source.common_dir(), "info/exclude"),
            (source.common_dir(), "info/attributes"),
        ] {
            let from = from.join(name);
            match std::fs::copy(&from, dir.join(name)) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(Error::io("copy", from, err)),
            }
        }
        if copy_dated(&source.git_dir().join("index"), &scratch.index())? {
            scratch.copy_shared_index(source.git_dir())?;
        } else {
            match &scratch.head {
                Some(head) => Repo::run_command(scratch.command(), ["read-tree", head.as_str()])?,
                None => Repo::run_command(scratch.command(), ["read-tree", "--empty"])?,
            };
        }
        let staged = scratch.index_objects()?;
        let into = match objects {
            Objects::Private => private.repo(),
            Objects::Disposable => &scratch.repo,
        };
        super::private::import(into, source, &staged)?;
        Ok(scratch)
    }

    /// Returns a git command that runs in this directory on the copy of the index, in the
    /// working tree, with the private repository's object database as its only one.
    ///
    /// What it writes there is durable (see [`super::durable`]), whatever it is: expanding a
    /// sparse index writes trees, which a later capture finds and takes as they are.
    pub(crate) fn command(&self) -> Command {
        let mut command = self.repo.unbound();
        super::durable(&mut command);
        command
            .env("GIT_DIR", self.repo.git_dir())
            .env("GIT_WORK_TREE", &self.work_tree)
            .env("GIT_INDEX_FILE", self.index())
            .env("GIT_OBJECT_DIRECTORY", &self.objects);
        command
    }

    /// Returns the worktree's `HEAD`, or `None` when it is unborn.
    pub(crate) fn head(&self) -> Option<&ObjectId> {
        self.head.as_ref()
    }

    /// Whether the index has an unmerged path, whose versions a capture cannot choose between.
    pub(crate) fn unmerged(&self) -> bool {
        self.unmerged
    }

    fn index(&self) -> PathBuf {
        self.repo.git_dir().join("index")
    }

    /// Returns the objects the index names, the trees of a sparse index's directories among
    /// them, without the commits of submodules, which a capture records by name only, and notes
    /// whether a path is unmerged.
    ///
    /// Git expands a sparse directory while it reads the index when files are on disk there,
    /// so the listing tells it to expect them, and fails on any error git reports, such as
    /// `error: invalid object` for a directory whose tree is missing, which git drops and
    /// lists nothing of; a warning, such as for a deprecated setting, counts for nothing.
    fn index_objects(&mut self) -> Result<Vec<ObjectId>, Error> {
        let args = [
            "-c",
            "sparse.expectFilesOutsideOfPatterns=true",
            "ls-files",
            "--stage",
            "--sparse",
            "-z",
        ];
        let listed = Repo::run_command(self.command(), args)?;
        if listed
            .stderr
            .split(|&byte| byte == b'\n')
            .any(|line| line.starts_with(b"error:"))
        {
            return Err(Error::CommandFailed {
                program: "git",
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&listed.stderr).trim().to_owned(),
            });
        }
        let mut objects = Vec::new();
        // `<mode> SP <object> SP <stage> TAB <path> NUL`
        for entry in listed.stdout.split(|&byte| byte == 0) {
            let meta = entry
                .split(|&byte| byte == b'\t')
                .next()
                .unwrap_or_default();
            let meta = String::from_utf8_lossy(meta);
            let mut fields = meta.split(' ');
            let (Some(mode), Some(oid), Some(stage)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            self.unmerged |= stage != "0";
            if mode != "160000" {
                objects.push(ObjectId::parse(oid)?);
            }
        }
        Ok(objects)
    }

    /// Copies the shared index the copied index is split from, if it is: the file
    /// `sharedindex.<id>` of `git_dir` whose id the index holds.
    ///
    /// The index names its shared index by the id's raw bytes, so searching it for them finds
    /// that one; a chance match elsewhere in the index only copies a file git never reads.
    /// A shared index is never rewritten, only dated anew, so any copy of it is the one the
    /// index names.
    fn copy_shared_index(&self, git_dir: &Path) -> Result<(), Error> {
        let index =
            std::fs::read(self.index()).map_err(|err| Error::io("read", self.index(), err))?;
        let entries = std::fs::read_dir(git_dir).map_err(|err| Error::io("read", git_dir, err))?;
        for entry in entries {
            let entry = entry.map_err(|err| Error::io("read", git_dir, err))?;
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_prefix("sharedindex."))
                .and_then(decode_hex)
            else {
                continue;
            };
            if index.windows(id.len()).any(|window| window == id) {
                copy_dated(&entry.path(), &self.repo.git_dir().join(&name))?;
            }
        }
        Ok(())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Every command that ran here has exited, so nothing holds its files open; a directory
        // left behind only takes space.
        if let Err(err) = std::fs::remove_dir_all(self.repo.git_dir()) {
            drop(err);
        }
    }
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::write(path, bytes).map_err(|err| Error::io("write", path, err))
}

/// Copies the file at `from` to `to`, dated as the original, and returns whether there was one.
///
/// Git trusts a cached stat only for files older than the index, and rereads those modified in
/// the same second as it ("racily clean").
/// A copy dated now would make such a file look older, so an edit that kept its size would be
/// missed.
/// The bytes and the date come from one open handle: git replaces the index by renaming a new
/// file over it, and a date read by path afterwards could belong to a newer index than the bytes.
fn copy_dated(from: &Path, to: &Path) -> Result<bool, Error> {
    let mut source = match File::open(from) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(Error::io("open", from, err)),
    };
    let modified = source
        .metadata()
        .and_then(|metadata| metadata.modified())
        .map_err(|err| Error::io("read", from, err))?;
    let mut copy = File::create(to).map_err(|err| Error::io("create", to, err))?;
    std::io::copy(&mut source, &mut copy).map_err(|err| Error::io("copy", from, err))?;
    copy.set_modified(modified)
        .map_err(|err| Error::io("write", to, err))?;
    Ok(true)
}

/// The bytes `hex` spells, or `None` when it is not an even number of hex digits.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect()
}
