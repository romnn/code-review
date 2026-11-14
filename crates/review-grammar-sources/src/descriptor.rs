//! The descriptor every entry of sources carries, which ties it to the manifest.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Entry, Pinned, Queries};

/// The version of the entries' layout and descriptor.
///
/// It is part of every entry's name and descriptor, so changing either makes new entries
/// instead of reading old ones.
/// Version 2 records each file's git blob id.
pub(crate) const FORMAT: u32 = 2;

/// The descriptor's file name in an entry.
pub(crate) const FILE: &str = "descriptor.toml";

/// What an entry was made from, and the files it holds.
///
/// A grammar's entry has no `directories`, which its descriptor then leaves out, and the
/// queries' entry has no `subpath` or `sources`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct Descriptor {
    pub(crate) format: u32,
    pub(crate) name: String,
    pub(crate) git: String,
    pub(crate) rev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) subpath: Option<String>,
    pub(crate) sources: Vec<String>,
    pub(crate) license_file: String,
    /// The query directories the manifest names, for the queries' entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) directories: Vec<String>,
    #[serde(rename = "file")]
    pub(crate) files: Vec<File>,
}

/// One file of an entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct File {
    /// Its path in the entry, with `/` between components, such as `src/parser.c`.
    pub(crate) path: String,
    /// Its size in bytes.
    pub(crate) size: u64,
    /// Its git blob id in the pinned commit, which its content must hash to.
    pub(crate) blob: String,
}

/// How an entry differs from the sources the manifest pins.
#[derive(Debug, thiserror::Error)]
pub enum Invalid {
    /// The entry has no descriptor that can be read.
    #[error("it has no readable {FILE}")]
    Unreadable(#[source] std::io::Error),
    /// The descriptor is not valid TOML of the expected shape.
    #[error("its {FILE} is not valid")]
    Malformed(#[source] Box<toml::de::Error>),
    /// The descriptor describes other sources than the manifest pins.
    #[error("it holds {field} {found:?}, but languages.toml pins {expected:?}")]
    Mismatch {
        /// The descriptor's field, such as `rev`.
        field: &'static str,
        /// What the manifest pins.
        expected: String,
        /// What the descriptor says.
        found: String,
    },
    /// A file the descriptor or the manifest names is not in the entry.
    #[error("it lacks {path}")]
    Missing {
        /// The file, relative to the entry.
        path: String,
    },
    /// A file is not the size its descriptor records.
    #[error("its {path} has {found} bytes, but its descriptor records {expected}")]
    Size {
        /// The file, relative to the entry.
        path: String,
        /// The size the descriptor records.
        expected: u64,
        /// The file's size.
        found: u64,
    },
    /// A file's content is not the blob of the pinned commit, though its size may be.
    #[error("its {path} is not the blob {expected} of the pinned commit, but {found}")]
    Content {
        /// The file, relative to the entry.
        path: String,
        /// The blob id the descriptor records.
        expected: String,
        /// The blob id of the file's content.
        found: String,
    },
    /// The descriptor names a file outside the entry.
    #[error("its descriptor names {path}, which is not a path inside it")]
    Outside {
        /// The path.
        path: String,
    },
}

impl Descriptor {
    /// The descriptor of an entry of `pinned` that holds `files`.
    pub(crate) fn new(pinned: Pinned<'_>, mut files: Vec<File>) -> Self {
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let (subpath, sources, license_file, directories) = match pinned {
            Pinned::Grammar(grammar) => (
                grammar.subpath.clone(),
                grammar.sources.clone(),
                grammar.license_file.clone(),
                Vec::new(),
            ),
            Pinned::Queries(queries) => (
                None,
                Vec::new(),
                Queries::LICENSE_FILE.to_owned(),
                queries.directories.clone(),
            ),
        };
        Self {
            format: FORMAT,
            name: pinned.name().to_owned(),
            git: pinned.git().to_owned(),
            rev: pinned.rev().to_owned(),
            subpath,
            sources,
            license_file,
            directories,
            files,
        }
    }

    /// Checks that the descriptor describes the sources the manifest pins for `pinned`.
    fn matches(&self, pinned: Pinned<'_>) -> Result<(), Invalid> {
        let expected = Self::new(pinned, Vec::new());
        let fields = [
            (
                "format",
                expected.format.to_string(),
                self.format.to_string(),
            ),
            ("name", expected.name, self.name.clone()),
            ("git", expected.git, self.git.clone()),
            ("rev", expected.rev, self.rev.clone()),
            (
                "subpath",
                expected.subpath.unwrap_or_default(),
                self.subpath.clone().unwrap_or_default(),
            ),
            (
                "sources",
                expected.sources.join(" "),
                self.sources.join(" "),
            ),
            (
                "license-file",
                expected.license_file,
                self.license_file.clone(),
            ),
            (
                "directories",
                expected.directories.join(" "),
                self.directories.join(" "),
            ),
        ];
        for (field, expected, found) in fields {
            if expected != found {
                return Err(Invalid::Mismatch {
                    field,
                    expected,
                    found,
                });
            }
        }
        Ok(())
    }
}

/// Checks that `directory` holds `pinned`'s sources, as its descriptor describes them, without
/// writing anything.
///
/// Every file's content is hashed as git hashes a blob, so a file changed in place, even to
/// the same size, as a restored cache might be, is not taken for the pinned one.
pub(crate) fn check(directory: &Path, pinned: Pinned<'_>) -> Result<Entry, Invalid> {
    let descriptor_path = directory.join(FILE);
    let text = std::fs::read_to_string(&descriptor_path).map_err(Invalid::Unreadable)?;
    let descriptor: Descriptor =
        toml::from_str(&text).map_err(|err| Invalid::Malformed(Box::new(err)))?;
    descriptor.matches(pinned)?;
    let sources: Vec<String> = match pinned {
        Pinned::Grammar(grammar) => grammar
            .sources
            .iter()
            .map(|source| format!("src/{source}"))
            .collect(),
        Pinned::Queries(_) => Vec::new(),
    };
    for path in sources.into_iter().chain(["LICENSE".to_owned()]) {
        if !descriptor.files.iter().any(|file| file.path == path) {
            return Err(Invalid::Missing { path });
        }
    }
    // Each query directory the manifest names holds one query file at least.
    if let Pinned::Queries(queries) = pinned {
        for directory in &queries.directories {
            let path = format!("{}/{directory}/", Queries::DIRECTORY);
            if !descriptor
                .files
                .iter()
                .any(|file| file.path.starts_with(&path))
            {
                return Err(Invalid::Missing { path });
            }
        }
    }
    let mut files = Vec::with_capacity(descriptor.files.len() + 1);
    for file in &descriptor.files {
        let Some(relative) = relative(&file.path) else {
            return Err(Invalid::Outside {
                path: file.path.clone(),
            });
        };
        let path = directory.join(relative);
        let bytes = std::fs::metadata(&path)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .and_then(|_| std::fs::read(&path).ok())
            .ok_or_else(|| Invalid::Missing {
                path: file.path.clone(),
            })?;
        let found = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if found != file.size {
            return Err(Invalid::Size {
                path: file.path.clone(),
                expected: file.size,
                found,
            });
        }
        let found = blob_id(&bytes);
        if found != file.blob {
            return Err(Invalid::Content {
                path: file.path.clone(),
                expected: file.blob.clone(),
                found,
            });
        }
        files.push(path);
    }
    files.push(descriptor_path);
    Ok(Entry {
        directory: directory.to_owned(),
        files,
    })
}

/// git's id of a blob holding `bytes`: the SHA-1 of `blob <length>`, a NUL, and the bytes.
pub(crate) fn blob_id(bytes: &[u8]) -> String {
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hasher.digest().to_string()
}

/// The relative path `path` names with `/` between its components, or `None` when it could
/// reach outside the directory it is relative to.
pub(crate) fn relative(path: &str) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for component in path.split('/') {
        // A separator or drive of another platform would make one component several.
        if component.contains(['\\', ':']) {
            return None;
        }
        let mut components = Path::new(component).components();
        let (Some(Component::Normal(normal)), None) = (components.next(), components.next()) else {
            return None;
        };
        relative.push(normal);
    }
    Some(relative)
}

/// The name of `pinned`'s entry in a cache.
///
/// It carries the name and revision for people, and a fingerprint of everything its descriptor
/// must match, so an entry made for another manifest or format is never looked at.
pub(crate) fn entry_name(pinned: Pinned<'_>) -> String {
    let rev = pinned.rev();
    let short = rev.get(..12).unwrap_or(rev);
    format!("{}-{short}-{:016x}", pinned.name(), fingerprint(pinned))
}

/// The 64-bit FNV-1a hash of the descriptor's fields, each ended by a NUL.
///
/// Unlike std's hasher, it stays the same across Rust releases, so cached entries do too.
/// The query directories come last, and a grammar has none, so they leave the names of
/// grammars' entries as they are.
fn fingerprint(pinned: Pinned<'_>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let format = FORMAT.to_string();
    let descriptor = Descriptor::new(pinned, Vec::new());
    let fields = [
        format.as_str(),
        &descriptor.name,
        &descriptor.git,
        &descriptor.rev,
        descriptor.subpath.as_deref().unwrap_or_default(),
        &descriptor.license_file,
    ]
    .into_iter()
    .chain(descriptor.sources.iter().map(String::as_str))
    .chain(descriptor.directories.iter().map(String::as_str));
    let mut hash = OFFSET;
    for field in fields {
        for byte in field.bytes().chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash
}
