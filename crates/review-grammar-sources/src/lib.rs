//! Where review-languages gets the generated C of the grammars it compiles and the Helix queries
//! it embeds: each grammar at the git revision its manifest pins, which are Helix's, and the
//! queries at the manifest's Helix commit, so the grammars match Helix's queries exactly.
//! The manifest itself, `languages.toml`, is parsed and checked here too ([`Manifest`]).
//!
//! Each of these, a grammar or the queries ([`Pinned`]), comes, in order, from:
//!
//! 1. a prepared directory, `$CODE_REVIEW_GRAMMARS_DIR/<name>`, when it exists, for offline
//!    builds;
//! 2. a valid entry of the cache, or of the fallback cache;
//! 3. a fetch of that one commit with `git` into the cache, or into the fallback cache when the
//!    cache cannot be written, as in a read-only checkout.
//!
//! Every entry, prepared or cached, carries a descriptor, `descriptor.toml`, naming what it was
//! made from and the size of every file it holds.
//! An entry is used only when its descriptor matches the manifest and its files match their
//! sizes, every time it is used.
//! A cached entry that does not is left alone and fetched again into a new entry, and a
//! prepared one that does not is an error.
//! Finding valid sources writes nothing, so a read-only checkout with a filled cache builds.
//!
//! Concurrent build scripts may fetch the same sources, so a fetch holds an advisory lock per
//! entry name, stages the entry beside the cache, and publishes it with one rename once it is
//! valid.
//! A published entry is never changed, since compilers may be reading it.

mod descriptor;
mod fetch;
mod git;
mod manifest;

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

pub use descriptor::Invalid;
pub use manifest::{
    Grammar, HELIX_GIT, LOCKABLE, Language, Locked, Manifest, PROOF, Problem, Queries,
    TemplateData, Tier,
};

/// What to run when sources are missing, which every such error names.
const REMEDY: &str = "run `task grammars:fetch` with network access to fill the cache, or set \
                      CODE_REVIEW_GRAMMARS_DIR to a directory of sources prepared with \
                      `task grammars:fetch -- --into <dir>`";

/// What one entry holds, from one pinned commit of one repository.
#[derive(Debug, Clone, Copy)]
pub enum Pinned<'a> {
    /// A grammar's generated C and license.
    Grammar(&'a Grammar),
    /// Helix's queries and license.
    Queries(&'a Queries),
}

impl<'a> Pinned<'a> {
    /// The name of its prepared directory, and of its entries and lock in a cache.
    #[must_use]
    pub fn name(self) -> &'a str {
        match self {
            Self::Grammar(grammar) => &grammar.name,
            Self::Queries(_) => Queries::NAME,
        }
    }

    /// The repository.
    #[must_use]
    pub fn git(self) -> &'a str {
        match self {
            Self::Grammar(grammar) => &grammar.git,
            Self::Queries(queries) => &queries.git,
        }
    }

    /// The commit, as a full object id.
    #[must_use]
    pub fn rev(self) -> &'a str {
        match self {
            Self::Grammar(grammar) => &grammar.rev,
            Self::Queries(queries) => &queries.rev,
        }
    }
}

impl<'a> From<&'a Grammar> for Pinned<'a> {
    fn from(grammar: &'a Grammar) -> Self {
        Self::Grammar(grammar)
    }
}

impl<'a> From<&'a Queries> for Pinned<'a> {
    fn from(queries: &'a Queries) -> Self {
        Self::Queries(queries)
    }
}

/// Names it in errors, such as `grammar bash`.
impl fmt::Display for Pinned<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grammar(grammar) => write!(formatter, "grammar {}", grammar.name),
            Self::Queries(_) => formatter.write_str("Helix's queries"),
        }
    }
}

/// Sources checked against the manifest: a grammar's, or Helix's queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The directory, whose `LICENSE` is the license of what it holds.
    ///
    /// A grammar's `src/` holds its generated C, and the queries' `queries/<directory>/` holds
    /// the query files of one directory of Helix's `runtime/queries/`, such as
    /// `queries/rust/highlights.scm`.
    pub directory: PathBuf,
    /// Every file the entry holds, its descriptor included, which a build script watches with
    /// `rerun-if-changed`.
    pub files: Vec<PathBuf>,
}

/// Why a grammar's sources are not available.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file could not be read or written.
    #[error("failed to {action} {path}")]
    Io {
        /// What was being done, such as `read`.
        action: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The manifest is not valid TOML of the expected shape.
    #[error("{path} is not a valid grammar manifest")]
    Manifest {
        /// The manifest.
        path: PathBuf,
        /// What is wrong with it.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// The manifest's `helix` is not a full commit id, which a fetch needs.
    #[error("`helix = {0:?}` is not a full commit id")]
    HelixRevision(String),
    /// A row of the manifest is not valid.
    #[error("language {name} of the manifest is not valid")]
    Language {
        /// The row's name.
        name: String,
        /// What is wrong with it.
        #[source]
        problem: Problem,
    },
    /// The sources are neither prepared nor cached, and fetching them failed.
    #[error("the sources of {name} are not cached and could not be fetched; {REMEDY}")]
    Unavailable {
        /// What was looked for, such as `grammar bash`.
        name: String,
        /// Why the fetch failed.
        #[source]
        source: Box<Error>,
    },
    /// The sources are neither prepared nor cached, and fetching is forbidden.
    #[error("the sources of {name} are not cached and fetching is off; {REMEDY}")]
    Offline {
        /// What was looked for, such as `grammar bash`.
        name: String,
    },
    /// The cache cannot be written, as in a read-only checkout.
    #[error("the source cache {path} cannot be written")]
    NotWritable {
        /// The file or directory that could not be made.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// `CODE_REVIEW_GRAMMARS_DIR` does not name a directory.
    #[error("CODE_REVIEW_GRAMMARS_DIR names {path}, which is not a directory")]
    PreparedRoot {
        /// What it names.
        path: PathBuf,
    },
    /// Prepared sources are not those the manifest pins.
    #[error(
        "the prepared sources {path} are not those languages.toml pins; prepare them again with \
         `task grammars:fetch -- --into <dir>`"
    )]
    InvalidPrepared {
        /// The prepared directory.
        path: PathBuf,
        /// How they differ.
        #[source]
        source: Invalid,
    },
    /// A directory to prepare sources in already holds other sources, which are never replaced,
    /// since a build may be reading them.
    #[error(
        "{path} already holds other sources than languages.toml pins; prepare them in a fresh \
         directory, or remove it first"
    )]
    PreparedMismatch {
        /// The directory.
        path: PathBuf,
        /// How its sources differ.
        #[source]
        source: Invalid,
    },
    /// Fetched sources are not those the manifest describes, such as when they lack a source
    /// file it names.
    #[error("the fetched sources of {name} are not those languages.toml describes")]
    InvalidFetch {
        /// What was fetched, such as `grammar bash`.
        name: String,
        /// How they differ.
        #[source]
        source: Invalid,
    },
    /// A descriptor could not be written as TOML.
    #[error("failed to describe the entry {path}")]
    Describe {
        /// The entry.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: toml::ser::Error,
    },
    /// A `git` command failed.
    #[error("git {command} failed: {stderr}")]
    Git {
        /// The command's arguments.
        command: String,
        /// What git printed on standard error.
        stderr: String,
    },
    /// A `git` command ran longer than it may, and was killed with its helpers.
    #[error("git {command} did not finish within {} seconds", git::TIMEOUT.as_secs())]
    Timeout {
        /// The command's arguments.
        command: String,
    },
    /// Another build held an entry's lock longer than any fetch takes.
    #[error("timed out waiting for the lock {path}")]
    LockTimeout {
        /// The lock file.
        path: PathBuf,
    },
    /// The fetched commit is not the pinned one.
    #[error("{name} fetched {found}, not the pinned revision {expected}")]
    WrongRevision {
        /// What was fetched, such as `grammar bash`.
        name: String,
        /// The pinned revision.
        expected: String,
        /// What was fetched.
        found: String,
    },
}

/// Where grammars' sources and Helix's queries are looked for, and whether they may be fetched.
#[derive(Debug, Clone)]
pub struct Sources {
    /// The cache, such as `crates/review-languages/_cache`.
    pub cache: PathBuf,
    /// A cache to read, and to fetch into when `cache` cannot be written, such as one under a
    /// build script's `OUT_DIR`.
    pub fallback: Option<PathBuf>,
    /// A directory of prepared sources, one directory per grammar name and one for the queries,
    /// [`Queries::NAME`], which wins over the caches for everything it holds; see
    /// [`prepared_dir`] for the variable that names it.
    pub prepared: Option<PathBuf>,
    /// Whether fetching is forbidden.
    pub offline: bool,
}

impl Sources {
    /// The sources of `pinned`, a grammar or the queries: prepared, cached, or else fetched
    /// into a cache.
    ///
    /// Prepared and cached sources are read without writing anything.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PreparedRoot`] when the prepared directory does not exist,
    /// [`Error::InvalidPrepared`] for prepared sources that are not those the manifest pins,
    /// [`Error::Offline`] when no cache holds the sources and fetching is forbidden, and
    /// [`Error::Unavailable`] when fetching them failed.
    pub fn locate<'a>(&self, pinned: impl Into<Pinned<'a>>) -> Result<Entry, Error> {
        let pinned = pinned.into();
        if let Some(root) = &self.prepared {
            if !root.is_dir() {
                return Err(Error::PreparedRoot { path: root.clone() });
            }
            let directory = root.join(pinned.name());
            if directory.exists() {
                return descriptor::check(&directory, pinned).map_err(|source| {
                    Error::InvalidPrepared {
                        path: directory,
                        source,
                    }
                });
            }
        }
        let mut caches = std::iter::once(&self.cache).chain(&self.fallback);
        if let Some(entry) = caches.find_map(|cache| fetch::find(cache, pinned)) {
            return Ok(entry);
        }
        if self.offline {
            return Err(Error::Offline {
                name: pinned.to_string(),
            });
        }
        let mut fetched = fetch::fetch(&self.cache, pinned);
        if let (Err(Error::NotWritable { .. }), Some(fallback)) = (&fetched, &self.fallback) {
            fetched = fetch::fetch(fallback, pinned);
        }
        fetched.map_err(|source| Error::Unavailable {
            name: pinned.to_string(),
            source: Box::new(source),
        })
    }
}

/// Whether a value of `CARGO_NET_OFFLINE` forbids fetching: it is read as a boolean, so `false`,
/// like an unset variable, allows it.
///
/// Build scripts and `task grammars:fetch` read the variable; `cargo --offline` sets nothing
/// they see.
#[must_use]
pub fn offline(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        )
    })
}

/// The directory a value of `CODE_REVIEW_GRAMMARS_DIR` names: the value itself when it is
/// absolute, and else relative to `package`, review-languages' directory, where Cargo runs its
/// build script and against which it resolves a relative `rerun-if-changed`.
#[must_use]
pub fn prepared_dir(value: &OsStr, package: &Path) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else {
        package.join(path)
    }
}

impl Entry {
    /// Publishes the entry's files at `directory` as prepared sources of `pinned`, the grammar
    /// or queries it holds, for `CODE_REVIEW_GRAMMARS_DIR`.
    ///
    /// Prepared sources never change once published, so a build may read them meanwhile: the
    /// directory appears whole, with one rename, and one that already holds `pinned`'s sources
    /// is left as it is.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PreparedMismatch`] when the directory exists but holds anything else,
    /// [`Error::NotWritable`] when its parent cannot be written, and [`Error::Io`] when a file
    /// cannot be copied.
    pub fn prepare<'a>(
        &self,
        directory: &Path,
        pinned: impl Into<Pinned<'a>>,
    ) -> Result<(), Error> {
        fetch::prepare(self, directory, pinned.into())
    }
}

#[cfg(test)]
mod tests;
