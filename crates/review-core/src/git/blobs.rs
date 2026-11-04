//! Blob reads through one long-lived `git cat-file --batch` process.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

use super::Repo;
use crate::error::Error;
use crate::ids::ObjectId;

/// Blobs larger than this are treated as opaque content instead of being diffed.
pub const MAX_TEXT_BLOB_BYTES: u64 = 8 * 1024 * 1024;

/// The content of a blob as far as analysis needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blob {
    /// The full content.
    Content(Vec<u8>),
    /// The blob exceeds [`MAX_TEXT_BLOB_BYTES`]; only its size was read.
    Oversized {
        /// The size in bytes.
        size: u64,
    },
}

/// A `git cat-file --batch` process answering one request at a time.
///
/// Partial clones fetch a missing object on request, so reading every blob analysis needs also
/// makes those objects local.
pub struct BlobReader {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl BlobReader {
    pub(super) fn spawn(repo: &Repo) -> Result<Self, Error> {
        let mut child = repo
            .command()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| Error::Spawn {
                program: "git",
                args: "cat-file --batch".to_owned(),
                source,
            })?;
        let stdin = child.stdin.take();
        let stdout =
            child
                .stdout
                .take()
                .map(BufReader::new)
                .ok_or_else(|| Error::UnexpectedOutput {
                    program: "git",
                    command: "cat-file --batch".to_owned(),
                    detail: "no stdout pipe".to_owned(),
                })?;
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Reads the blob `oid`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingObject`] when the object does not exist, and an error when the
    /// object is not a blob or the process fails.
    pub fn read(&mut self, oid: &ObjectId) -> Result<Blob, Error> {
        let io_error = |source| Error::io("talk to git cat-file about", oid.as_str(), source);
        let stdin = self.stdin.as_mut().ok_or_else(|| Error::UnexpectedOutput {
            program: "git",
            command: "cat-file --batch".to_owned(),
            detail: "stdin closed".to_owned(),
        })?;
        writeln!(stdin, "{oid}").map_err(io_error)?;
        stdin.flush().map_err(io_error)?;

        let mut header = String::new();
        self.stdout.read_line(&mut header).map_err(io_error)?;
        let header = header.trim_end();
        if header.ends_with(" missing") || header.is_empty() {
            return Err(Error::MissingObject(oid.clone()));
        }
        let unexpected = || Error::UnexpectedOutput {
            program: "git",
            command: "cat-file --batch".to_owned(),
            detail: header.to_owned(),
        };
        let mut fields = header.split(' ');
        let (Some(_), Some(kind), Some(size)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(unexpected());
        };
        let size: u64 = size.parse().map_err(|_| unexpected())?;

        // The content is followed by one newline, which is consumed together with it.
        if size > MAX_TEXT_BLOB_BYTES || kind != "blob" {
            std::io::copy(&mut (&mut self.stdout).take(size + 1), &mut std::io::sink())
                .map_err(io_error)?;
            if kind != "blob" {
                return Err(unexpected());
            }
            return Ok(Blob::Oversized { size });
        }
        let length = usize::try_from(size).map_err(|_| unexpected())?;
        let mut content = vec![0; length + 1];
        self.stdout.read_exact(&mut content).map_err(io_error)?;
        content.pop();
        Ok(Blob::Content(content))
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        // Closing stdin makes cat-file exit; reap it so no zombie is left behind.
        drop(self.stdin.take());
        if let Err(err) = self.child.wait() {
            // Nothing can be done about a failed reap during drop.
            drop(err);
        }
    }
}
