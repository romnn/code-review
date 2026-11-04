//! Parsing of `git diff-tree -r -z --raw` output.

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::ids::ObjectId;

/// A git tree entry mode, such as `100644`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileMode(u32);

/// What kind of object a tree entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    /// A regular file, executable or not.
    File,
    /// A symbolic link; its blob is the link target.
    Symlink,
    /// A submodule commit.
    Submodule,
    /// Anything else git may report.
    Other,
}

impl FileMode {
    /// Returns the kind of object this mode describes.
    #[must_use]
    pub fn entry_type(self) -> EntryType {
        match self.0 & 0o170_000 {
            0o100_000 => EntryType::File,
            0o120_000 => EntryType::Symlink,
            0o160_000 => EntryType::Submodule,
            _ => EntryType::Other,
        }
    }

    /// Returns the raw octal value.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for FileMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:06o}", self.0)
    }
}

/// How git classified a changed entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ChangeStatus {
    /// The entry is new.
    Added,
    /// The entry was removed.
    Deleted,
    /// Content or mode changed in place.
    Modified,
    /// The entry changed type, for example from a file to a symlink.
    TypeChanged,
    /// The entry moved to a new path, with git's similarity score in percent.
    Renamed {
        /// Git's similarity score, 0 to 100.
        similarity: u8,
    },
}

/// One side of a changed entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSide {
    /// The repository-relative path.
    pub path: String,
    /// The tree entry mode.
    pub mode: FileMode,
    /// The object ID.
    pub oid: ObjectId,
}

/// One changed entry between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChange {
    /// The entry before, absent for an addition.
    pub old: Option<RawSide>,
    /// The entry after, absent for a deletion.
    pub new: Option<RawSide>,
    /// Git's classification.
    pub status: ChangeStatus,
}

pub(super) fn parse(stdout: &[u8]) -> Result<Vec<RawChange>, Error> {
    let unexpected = |detail: &str| Error::UnexpectedOutput {
        program: "git",
        command: "diff-tree".to_owned(),
        detail: detail.to_owned(),
    };
    let mut fields = stdout
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty());
    let mut changes = Vec::new();
    while let Some(header) = fields.next() {
        let header = std::str::from_utf8(header).map_err(|_| unexpected("non-UTF-8 header"))?;
        let header = header.strip_prefix(':').ok_or_else(|| unexpected(header))?;
        let parts: Vec<&str> = header.split(' ').collect();
        let [old_mode, new_mode, old_oid, new_oid, status] = parts.as_slice() else {
            return Err(unexpected(header));
        };
        let mode = |text: &str| {
            u32::from_str_radix(text, 8)
                .map(FileMode)
                .map_err(|_| unexpected(header))
        };
        let mut next_path = || -> Result<String, Error> {
            let path = fields.next().ok_or_else(|| unexpected("missing path"))?;
            String::from_utf8(path.to_vec())
                .map_err(|err| Error::NonUtf8Path(String::from_utf8_lossy(err.as_bytes()).into()))
        };
        let (status_letter, score) = status.split_at(1.min(status.len()));
        let old_path = next_path()?;
        let new_path = if matches!(status_letter, "R" | "C") {
            next_path()?
        } else {
            old_path.clone()
        };
        let old = RawSide {
            path: old_path,
            mode: mode(old_mode)?,
            oid: ObjectId::parse(old_oid)?,
        };
        let new = RawSide {
            path: new_path,
            mode: mode(new_mode)?,
            oid: ObjectId::parse(new_oid)?,
        };
        let change = match status_letter {
            "A" => RawChange {
                old: None,
                new: Some(new),
                status: ChangeStatus::Added,
            },
            "D" => RawChange {
                old: Some(old),
                new: None,
                status: ChangeStatus::Deleted,
            },
            "M" => RawChange {
                old: Some(old),
                new: Some(new),
                status: ChangeStatus::Modified,
            },
            "T" => RawChange {
                old: Some(old),
                new: Some(new),
                status: ChangeStatus::TypeChanged,
            },
            "R" => RawChange {
                old: Some(old),
                new: Some(new),
                status: ChangeStatus::Renamed {
                    similarity: score.parse().map_err(|_| unexpected(header))?,
                },
            },
            _ => return Err(unexpected(header)),
        };
        changes.push(change);
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifications_and_renames() {
        let zero = "0".repeat(40);
        let one = "1".repeat(40);
        let raw = format!(
            ":100644 100755 {one} {one} M\0e\0:100644 100644 {one} {one} R100\0x\0y\0:000000 100644 {zero} {one} A\0n\0"
        );
        let changes = parse(raw.as_bytes()).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[1].status, ChangeStatus::Renamed { similarity: 100 });
        assert_eq!(changes[1].new.as_ref().unwrap().path, "y");
        assert!(changes[2].old.is_none());
        assert_eq!(changes[0].new.as_ref().unwrap().mode.to_string(), "100755");
    }
}
