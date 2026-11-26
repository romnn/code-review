//! Repositories cr owns in its data directory: the archive of a review's snapshots, and the
//! transient repositories fetches land in.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use super::Repo;
use crate::error::Error;
use crate::ids::ObjectId;

/// A bare repository cr owns in its data directory, the only kind of repository a git command
/// that writes runs against.
///
/// It takes objects from the reviewed repository only by an explicit list of objects in a pack
/// of their own: never as an alternate, through which git would date the reviewed repository's
/// objects anew whenever it writes one it finds there.
/// Its commands run in the reviewed clone's directory with the clone's configuration as the
/// clone read it when the repository was opened (see [`Repo::clone_context`]), and its
/// `info/attributes`, so they see files and run git's diff as the clone does.
#[derive(Debug, Clone)]
pub(crate) struct PrivateRepo {
    repo: Repo,
    /// The hash algorithm of its objects, the reviewed repository's: `sha1` or `sha256`.
    format: String,
}

impl PrivateRepo {
    /// Opens the private repository at `path`, an absolute path, for the reviewed repository
    /// `source`, creating it as a bare repository with `source`'s object format on first use;
    /// its commands get the variables and prompting `source`'s get.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InsideRepository`] for a `path` inside `source`'s working tree or git
    /// directories, and an error when the repository cannot be created or `source`'s
    /// configuration cannot be read.
    pub(crate) fn open(path: &Path, source: &Repo) -> Result<Self, Error> {
        refuse_inside(path, source)?;
        let private = Self {
            repo: Repo {
                work_tree: None,
                git_dir: path.to_path_buf(),
                common_dir: path.to_path_buf(),
                env: source.env.clone(),
                no_prompts: source.no_prompts,
                clone: Some(source.clone_context()?),
            },
            format: source.object_format()?,
        };
        if !path.join("HEAD").exists() {
            // No template, which could name hooks or alternates of its own.
            let format = format!("--object-format={}", private.format);
            let mut init = private.repo.unbound();
            super::durable(&mut init);
            init.args(["init", "--quiet", "--bare", "--template=", &format])
                .arg(path);
            Repo::run_command(init, [] as [&str; 0])?;
        }
        // Replaced whole, since a window may be reading the archive meanwhile.
        let attributes = source.common_dir().join("info/attributes");
        let copy = path.join("info/attributes");
        match std::fs::read(&attributes) {
            Ok(bytes) if std::fs::read(&copy).ok().as_ref() != Some(&bytes) => {
                crate::store::create_dir(&path.join("info"))?;
                crate::store::write_atomic(&copy, &bytes)?;
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::remove_file(&copy) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(Error::io("remove", copy, err)),
                }
            }
            Err(err) => return Err(Error::io("read", attributes, err)),
        }
        Ok(private)
    }

    /// Returns the repository for reading: its objects, refs and snapshots.
    pub(crate) fn repo(&self) -> &Repo {
        &self.repo
    }

    /// Returns the hash algorithm of the repository's objects, `sha1` or `sha256`.
    pub(crate) fn format(&self) -> &str {
        &self.format
    }

    /// Returns the repository's directory.
    pub(crate) fn path(&self) -> &Path {
        self.repo.git_dir()
    }

    /// The repository's object database.
    pub(crate) fn objects(&self) -> PathBuf {
        self.path().join("objects")
    }

    /// The directory of the stand-ins for the reviewed repository's git directory that
    /// commands reading its index run in, and of transient repositories.
    pub(crate) fn scratch(&self) -> PathBuf {
        self.path().join("scratch")
    }

    /// Makes `roots` and everything they reach but a commit's parents present, copying what
    /// is missing from `from` in one pack.
    ///
    /// A pack becomes visible as a whole, so a commit or a tree here always has everything it
    /// reaches, and a root already here is taken as whole: an import cut short leaves only a
    /// pack that never shows.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingObject`] when `from` lacks an object, as after a garbage
    /// collection or in a partial clone, and an error when git fails.
    pub(crate) fn import(&self, from: &Repo, roots: &[ObjectId]) -> Result<(), Error> {
        self.restore(from, &self.repo.missing(roots)?)
    }

    /// Makes everything `roots` reach but a commit's parents present, as [`Self::import`] does,
    /// whether the roots are here or not: for an archive that lost objects a root it still has
    /// reaches.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::import`].
    pub(crate) fn restore(&self, from: &Repo, roots: &[ObjectId]) -> Result<(), Error> {
        let objects = self.repo.missing(&from.closure(roots)?)?;
        if objects.is_empty() {
            return Ok(());
        }
        copy_pack(from, &self.repo, &[], super::object_lines(&objects))
    }

    /// Checks that `roots` and everything they reach but a commit's parents are here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingObject`] naming an object that is not, and an error when git
    /// fails.
    pub(crate) fn verify(&self, roots: &[ObjectId]) -> Result<(), Error> {
        self.repo.closure(roots).map(drop)
    }
}

/// Packs what `input` lists in `from`, as `git pack-objects` reads it with the options `options`,
/// and indexes the pack into `to`, streaming it from one to the other.
///
/// Reusing the deltas `from` stores keeps the pack small without the time a delta search takes;
/// only objects in the pack serve as bases, so it is self-contained.
pub(super) fn copy_pack(
    from: &Repo,
    to: &Repo,
    options: &[&str],
    input: String,
) -> Result<(), Error> {
    let spawn_error = |args: &str, source| Error::Spawn {
        program: "git",
        args: args.to_owned(),
        source,
    };
    let mut pack = from
        .command()
        .args(["pack-objects", "--stdout", "--quiet", "--window=0"])
        .args(options)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| spawn_error("pack-objects", err))?;
    let packed = pack.stdout.take().map_or_else(Stdio::null, Stdio::from);
    let mut index = to.command();
    super::durable(&mut index);
    let index = index
        .args(["index-pack", "--stdin"])
        .stdin(packed)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| spawn_error("index-pack", err))?;
    let stdin = pack.stdin.take();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(mut stdin) = stdin {
            stdin.write_all(input.as_bytes())?;
        }
        Ok(())
    });
    let packed = pack
        .wait_with_output()
        .map_err(|err| spawn_error("pack-objects", err))?;
    let indexed = index
        .wait_with_output()
        .map_err(|err| spawn_error("index-pack", err))?;
    for (args, output) in [("pack-objects", packed), ("index-pack --stdin", indexed)] {
        if !output.status.success() {
            return Err(Error::CommandFailed {
                program: "git",
                args: args.to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
    }
    match writer.join() {
        Ok(Err(err)) => Err(spawn_error("pack-objects", err)),
        _ => Ok(()),
    }
}

/// Refuses `path`, which need not exist yet, when it is inside `source`'s working tree or git
/// directories, which cr never writes to.
pub(crate) fn refuse_inside(path: &Path, source: &Repo) -> Result<(), Error> {
    let private = resolve(path)?;
    let source_dirs = source
        .work_tree()
        .into_iter()
        .chain([source.git_dir(), source.common_dir()]);
    for dir in source_dirs {
        if private.starts_with(resolve(dir)?) {
            return Err(Error::InsideRepository {
                path: path.to_path_buf(),
                repo: dir.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// Resolves `path` as the filesystem does, symbolic links and all, as far as it exists.
///
/// # Errors
///
/// Returns an error for a `..` after the part of `path` that exists: the directory it climbs out
/// of does not exist yet, so where it leads cannot be checked before the path is made.
fn resolve(path: &Path) -> Result<PathBuf, Error> {
    let absolute = std::path::absolute(path).map_err(|err| Error::io("resolve", path, err))?;
    for existing in absolute.ancestors() {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            let rest = absolute.strip_prefix(existing).unwrap_or(Path::new(""));
            if rest.components().any(|part| part == Component::ParentDir) {
                let climbs = std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "`..` after a directory that does not exist yet",
                );
                return Err(Error::io("resolve", path, climbs));
            }
            return Ok(resolved.join(rest));
        }
    }
    Ok(absolute)
}
