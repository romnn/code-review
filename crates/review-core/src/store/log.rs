//! The append-only event log: one newline-terminated JSON record per event.
//!
//! Writers serialize through `review.lock` and make each append complete or absent: the record and
//! its newline go out in one `write_all` followed by `sync_data` before success is reported.
//! A trailing record without a newline was never acknowledged, so the next writer truncates it.
//! Readers hold a shared lock on `review.lock` while they read, so they never observe a
//! truncation or an append in progress, and they only advance past complete records.
//!
//! The log only grows, so a reader keeps its place and a writer appends after what a reader
//! showed.
//! Another log in its place, such as a backup restored, breaks both: readers notice it where they
//! cheaply can, and a writer given what a reader read refuses to touch a log that no longer starts
//! with it, so a review ends rather than mixing two logs.

use sha2::{Digest as _, Sha256};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use super::ReviewDir;
use super::events::Event;
use crate::error::Error;

/// Holds `review.lock` until dropped: exclusively for writers, shared for readers.
pub struct LockGuard {
    file: File,
}

impl LockGuard {
    fn open(path: &Path) -> Result<File, std::io::Error> {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
    }

    /// Blocks until the lock file at `path` is held exclusively.
    pub(crate) fn acquire(path: &Path) -> Result<Self, Error> {
        let file = Self::open(path).map_err(|source| Error::io("open", path, source))?;
        file.lock()
            .map_err(|source| Error::io("lock", path, source))?;
        Ok(Self { file })
    }

    /// Blocks until the lock file at `path` is held shared, or returns `None` when the review
    /// directory does not exist yet.
    fn acquire_shared(path: &Path) -> Result<Option<Self>, Error> {
        let file = match Self::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::io("open", path, source)),
        };
        file.lock_shared()
            .map_err(|source| Error::io("lock", path, source))?;
        Ok(Some(Self { file }))
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Closing the file releases the lock as well; an explicit unlock only makes it prompt.
        if let Err(err) = self.file.unlock() {
            drop(err);
        }
    }
}

/// How much of a log was read, with a digest of those bytes, which tells whether a log still
/// starts with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Prefix {
    length: u64,
    digest: [u8; 32],
}

impl Prefix {
    /// The prefix that is all of `bytes`.
    pub(crate) fn of(bytes: &[u8]) -> Self {
        #[cfg(test)]
        tests::DIGESTS.set(tests::DIGESTS.get() + 1);
        Self {
            length: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            digest: Sha256::digest(bytes).into(),
        }
    }

    /// The prefix of a log nothing was read from.
    pub(crate) fn empty() -> Self {
        Self::of(&[])
    }

    /// Returns how many bytes were read.
    pub(crate) fn length(&self) -> u64 {
        self.length
    }

    /// Whether `bytes` start with the bytes this prefix was read from.
    fn starts(&self, bytes: &[u8]) -> bool {
        usize::try_from(self.length)
            .ok()
            .and_then(|length| bytes.get(..length))
            .is_some_and(|prefix| Self::of(prefix) == *self)
    }
}

/// The log opened for appending, valid only while the lock is held.
pub(crate) struct LogWriter<'lock> {
    path: PathBuf,
    file: File,
    events: Vec<Event>,
    /// The log's complete records as it was opened.
    records: Vec<u8>,
    _lock: &'lock LockGuard,
}

impl<'lock> LogWriter<'lock> {
    /// Opens the log under `lock`, truncates an unacknowledged trailing record, and reads every
    /// complete record; with `shown`, what a reader read for the caller to act on, only while
    /// the log still starts with it, and otherwise touches nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogReplaced`] when the log no longer starts with `shown`.
    pub(crate) fn open(
        path: &Path,
        lock: &'lock LockGuard,
        shown: Option<&Prefix>,
    ) -> Result<Self, Error> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| Error::io("open", path, source))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| Error::io("read", path, source))?;
        if shown.is_some_and(|shown| !shown.starts(&bytes)) {
            return Err(Error::LogReplaced {
                path: path.to_path_buf(),
            });
        }
        let complete = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1);
        if complete < bytes.len() {
            bytes.truncate(complete);
            file.set_len(u64::try_from(complete).unwrap_or(u64::MAX))
                .map_err(|source| Error::io("truncate", path, source))?;
            file.sync_data()
                .map_err(|source| Error::io("sync", path, source))?;
        }
        let events = parse_records(path, &bytes, 0)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            events,
            records: bytes,
            _lock: lock,
        })
    }

    /// Returns the log's complete records as it was opened, which a
    /// [`History`](crate::History) read from it then was read from; it digests them.
    pub(crate) fn opened(&self) -> Prefix {
        Prefix::of(&self.records)
    }

    /// Returns every event in the log.
    pub(crate) fn events(&self) -> &[Event] {
        &self.events
    }

    /// Appends one event and syncs it to disk before returning.
    pub(crate) fn append(&mut self, event: Event) -> Result<(), Error> {
        let mut record = serde_json::to_vec(&event).map_err(|source| Error::Encode {
            what: "event",
            source,
        })?;
        record.push(b'\n');
        self.file
            .seek(SeekFrom::End(0))
            .map_err(|source| Error::io("seek", &self.path, source))?;
        self.file
            .write_all(&record)
            .map_err(|source| Error::io("append to", &self.path, source))?;
        self.file
            .sync_data()
            .map_err(|source| Error::io("sync", &self.path, source))?;
        self.events.push(event);
        Ok(())
    }
}

/// Reads the log's complete records under `lock`, a writer's, and returns them with their
/// bytes, changing nothing: an unacknowledged trailing record stays for the next writer to
/// remove.
///
/// # Errors
///
/// Returns an error when the log cannot be read or a record is corrupt.
pub(crate) fn read_complete(
    path: &Path,
    _lock: &LockGuard,
) -> Result<(Vec<Event>, Vec<u8>), Error> {
    let mut bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(Error::io("read", path, err)),
    };
    let complete = bytes
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |newline| newline + 1);
    bytes.truncate(complete);
    Ok((parse_records(path, &bytes, 0)?, bytes))
}

/// Parses complete records; `first_line` is the one-based line number of the first record minus
/// one, for error messages.
fn parse_records(path: &Path, bytes: &[u8], first_line: usize) -> Result<Vec<Event>, Error> {
    bytes
        .split_inclusive(|&byte| byte == b'\n')
        .enumerate()
        .map(|(offset, record)| {
            serde_json::from_slice(record).map_err(|source| Error::CorruptLog {
                path: path.to_path_buf(),
                line: first_line + offset + 1,
                source,
            })
        })
        .collect()
}

/// A reader that follows a review's log as it grows.
///
/// Each poll holds a shared lock on `review.lock`, so no writer can truncate or append while it
/// reads.
/// It advances only past complete records and keeps nothing of an incomplete trailing line (left by
/// a writer that died), so a partial record that a later writer truncates and replaces is read
/// correctly once complete.
/// It reads on only while the log is the one it read, as far as it can tell cheaply: the same
/// file where the platform tells files apart, at least as long, and ending what it read with
/// the same bytes.
#[derive(Debug, Clone)]
pub struct LogTail {
    path: PathBuf,
    lock: PathBuf,
    offset: u64,
    lines: usize,
    /// The last [`SUFFIX`] bytes read, which end the log at `offset`.
    suffix: Vec<u8>,
    /// The digest of every byte read.
    digest: Sha256,
    /// The file read, once one was.
    file: Option<FileId>,
}

/// How many of the bytes it read last a [`LogTail`] checks are still there.
const SUFFIX: usize = 64;

/// A file's identity: its device and inode where the platform has them, and else nothing, so
/// that every file is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileId {
    #[cfg(unix)]
    fn of(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    fn of(_metadata: &Metadata) -> Self {
        Self {}
    }
}

impl LogTail {
    /// Starts reading the log of the review in `dir` from the beginning.
    #[must_use]
    pub fn new(dir: &ReviewDir) -> Self {
        Self {
            path: dir.events(),
            lock: dir.lock(),
            offset: 0,
            lines: 0,
            suffix: Vec::new(),
            digest: Sha256::new(),
            file: None,
        }
    }

    /// Returns how much of the log was read, with a digest of it.
    pub(crate) fn prefix(&self) -> Prefix {
        Prefix {
            length: self.offset,
            digest: self.digest.clone().finalize().into(),
        }
    }

    /// Returns the byte offset just past the last complete record read.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Reads the complete records appended since the last call.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptLog`] for a corrupt complete record, and [`Error::LogReplaced`]
    /// when the log is not the one read: gone, shorter, another file, or no longer ending what
    /// was read as it did.
    /// Blocks while a writer holds the lock.
    #[tracing::instrument(name = "read_log", skip_all)]
    pub fn poll(&mut self) -> Result<Vec<Event>, Error> {
        let Some(lock) = LockGuard::acquire_shared(&self.lock)? else {
            return Ok(Vec::new());
        };
        let events = self.read_complete_records();
        drop(lock);
        events
    }

    fn read_complete_records(&mut self) -> Result<Vec<Event>, Error> {
        let replaced = || Error::LogReplaced {
            path: self.path.clone(),
        };
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return if self.offset == 0 {
                    Ok(Vec::new())
                } else {
                    Err(replaced())
                };
            }
            Err(source) => return Err(Error::io("open", &self.path, source)),
        };
        let metadata = file
            .metadata()
            .map_err(|source| Error::io("stat", &self.path, source))?;
        let file_id = Some(FileId::of(&metadata));
        if metadata.len() < self.offset || (self.offset > 0 && file_id != self.file) {
            return Err(replaced());
        }
        // The last bytes read are read again, to check they are still where they were.
        let suffix = u64::try_from(self.suffix.len()).unwrap_or(u64::MAX);
        file.seek(SeekFrom::Start(self.offset.saturating_sub(suffix)))
            .map_err(|source| Error::io("seek", &self.path, source))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| Error::io("read", &self.path, source))?;
        let Some(unread) = bytes.strip_prefix(self.suffix.as_slice()) else {
            return Err(replaced());
        };
        let complete = unread
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1);
        let records = unread.get(..complete).unwrap_or_default();
        let events = parse_records(&self.path, records, self.lines)?;
        self.digest.update(records);
        let read = bytes
            .get(..self.suffix.len() + complete)
            .unwrap_or_default();
        self.suffix = read
            .get(read.len().saturating_sub(SUFFIX)..)
            .unwrap_or_default()
            .to_vec();
        self.file = file_id;
        self.offset += u64::try_from(complete).unwrap_or(u64::MAX);
        self.lines += events.len();
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use color_eyre::eyre;

    use super::*;

    thread_local! {
        /// How many digests of log bytes this thread computed.
        pub(super) static DIGESTS: Cell<usize> = const { Cell::new(0) };
    }

    /// A writer digests the log only to check what a caller shows, once, and to say how the
    /// log was opened when asked.
    #[test]
    fn a_writer_digests_only_what_it_is_asked_for() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "{\"event\":\"trust\",\"trusted\":true,\"at\":1}\n")?;
        let lock = LockGuard::acquire(&dir.path().join("review.lock"))?;
        let digests = || DIGESTS.get();
        let before = digests();
        LogWriter::open(&path, &lock, None)?;
        assert_eq!(digests(), before, "nothing shown, nothing digested");

        let opened = LogWriter::open(&path, &lock, None)?.opened();
        let before = digests();
        let writer = LogWriter::open(&path, &lock, Some(&opened))?;
        assert_eq!(digests(), before + 1, "what shows is digested once");
        writer.opened();
        assert_eq!(digests(), before + 2);
        Ok(())
    }
}
