//! The append-only event log: one newline-terminated JSON record per event.
//!
//! Writers serialize through `review.lock` and make each append complete or absent: the record and
//! its newline go out in one `write_all` followed by `sync_data` before success is reported.
//! A trailing record without a newline was never acknowledged, so the next writer truncates it.
//! Readers hold a shared lock on `review.lock` while they read, so they never observe a
//! truncation or an append in progress, and they only advance past complete records.

use std::fs::{File, OpenOptions};
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

/// The log opened for appending, valid only while the lock is held.
pub(crate) struct LogWriter<'lock> {
    path: PathBuf,
    file: File,
    events: Vec<Event>,
    _lock: &'lock LockGuard,
}

impl<'lock> LogWriter<'lock> {
    /// Opens the log under `lock`, truncates an unacknowledged trailing record, and reads every
    /// complete record.
    pub(crate) fn open(path: &Path, lock: &'lock LockGuard) -> Result<Self, Error> {
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
        let complete = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1);
        if complete < bytes.len() {
            let length = u64::try_from(complete).unwrap_or(u64::MAX);
            file.set_len(length)
                .map_err(|source| Error::io("truncate", path, source))?;
            file.sync_data()
                .map_err(|source| Error::io("sync", path, source))?;
        }
        let events = parse_records(path, bytes.get(..complete).unwrap_or_default(), 0)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            events,
            _lock: lock,
        })
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
#[derive(Debug)]
pub struct LogTail {
    path: PathBuf,
    lock: PathBuf,
    offset: u64,
    lines: usize,
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
    /// Returns [`Error::CorruptLog`] for a corrupt complete record, and [`Error::LogShrank`] when
    /// the log is shorter than a complete-record boundary already read.
    /// Blocks while a writer holds the lock.
    pub fn poll(&mut self) -> Result<Vec<Event>, Error> {
        let Some(lock) = LockGuard::acquire_shared(&self.lock)? else {
            return Ok(Vec::new());
        };
        let events = self.read_complete_records();
        drop(lock);
        events
    }

    fn read_complete_records(&mut self) -> Result<Vec<Event>, Error> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && self.offset == 0 => {
                return Ok(Vec::new());
            }
            Err(source) => return Err(Error::io("open", &self.path, source)),
        };
        let length = file
            .metadata()
            .map_err(|source| Error::io("stat", &self.path, source))?
            .len();
        if length < self.offset {
            return Err(Error::LogShrank {
                path: self.path.clone(),
            });
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|source| Error::io("seek", &self.path, source))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| Error::io("read", &self.path, source))?;
        let complete = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1);
        let records = bytes.get(..complete).unwrap_or_default();
        let events = parse_records(&self.path, records, self.lines)?;
        self.offset += u64::try_from(complete).unwrap_or(u64::MAX);
        self.lines += events.len();
        Ok(events)
    }
}
