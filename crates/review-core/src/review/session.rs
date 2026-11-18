//! Sessions: a running app, or one recording, that pins the captures it shows until it is done.
//!
//! A capture of a working tree is a synthetic commit no branch reaches, so whoever shows it pins
//! it, under `refs/code-review/<key>/live/<session>/<capture>/`, where `<capture>` names its
//! head, base and target. A session holds an exclusive lock on its file
//! `code-review/sessions/<key>/<id>.lock` in the clone's common git directory for as long as it
//! runs, so another session can tell a live one from one that died: it removes only its own
//! pins, and another session's only once that session's file can be locked.
//! The lock files live with the pins they guard, in the clone, so every process that sees the
//! pins sees the locks, whatever data directory it keeps the review in.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use super::{Review, RevisionInput};
use crate::error::Error;
use crate::git::Repo;
use crate::history::RevisionRecord;
use crate::ids::ObjectId;
use crate::store;

/// A running app, or one recording, that pins the captures it shows under
/// `refs/code-review/<key>/live/<session>/<capture>/` for as long as it lives, holding an
/// exclusive lock on its file `code-review/sessions/<key>/<id>.lock` in the clone's common git
/// directory meanwhile.
///
/// Dropping it removes its pins and its lock file. Another session removes them only once that
/// lock is free, which means this one is over.
#[derive(Debug)]
pub struct Session {
    id: String,
    /// The prefix of its pins, `refs/code-review/<key>/live/<id>`.
    pins: String,
    /// Its lock file, held exclusively through `file`.
    path: PathBuf,
    file: File,
    repo: Repo,
}

impl Session {
    /// Returns the session's identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the prefix of the session's pins; see [`Self::pins_of`].
    #[must_use]
    pub fn pins(&self) -> &str {
        &self.pins
    }

    /// Returns the prefix of the refs that pin the capture `record` is of, under
    /// [`Self::pins`]: they are named for its head, base and target, so two captures that
    /// share a head but not a base, as when the base moved, each keep their own.
    #[must_use]
    pub fn pins_of(&self, record: &RevisionRecord) -> String {
        format!(
            "{}/{}",
            self.pins,
            capture_name(&record.base, &record.head, record.target.as_ref())
        )
    }

    /// Returns the lock file the session holds for as long as it runs.
    #[must_use]
    pub fn lock_file(&self) -> &Path {
        &self.path
    }

    /// Pins the base, the head and the target of `input` until [`Self::release`] or the end of
    /// the session.
    pub(super) fn pin(&self, input: &RevisionInput) -> Result<(), Error> {
        let capture = format!(
            "{}/{}",
            self.pins,
            capture_name(&input.base, &input.head, input.target.as_ref())
        );
        let mut pins = vec![
            (format!("{capture}/base"), input.base.clone()),
            (format!("{capture}/head"), input.head.clone()),
        ];
        if let Some(target) = &input.target {
            pins.push((format!("{capture}/target"), target.clone()));
        }
        self.repo.update_refs(&pins)
    }

    /// Removes the pins of every capture but those `keep` records.
    pub(super) fn release(&self, keep: &[RevisionRecord]) -> Result<(), Error> {
        let prefix = format!("{}/", self.pins);
        let kept: Vec<String> = keep
            .iter()
            .map(|record| capture_name(&record.base, &record.head, record.target.as_ref()))
            .collect();
        let names: Vec<String> = self
            .repo
            .refs_under(&prefix)?
            .into_iter()
            .map(|pin| pin.name)
            .filter(|name| {
                let capture = name
                    .strip_prefix(&prefix)
                    .and_then(|rest| rest.split('/').next());
                capture.is_none_or(|capture| !kept.iter().any(|kept| kept == capture))
            })
            .collect();
        self.repo.delete_refs(&names)
    }
}

/// The name of a capture's pins: its head, base and target, joined by `-`.
fn capture_name(base: &ObjectId, head: &ObjectId, target: Option<&ObjectId>) -> String {
    match target {
        Some(target) => format!("{head}-{base}-{target}"),
        None => format!("{head}-{base}"),
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A pin left behind only keeps objects alive, and the next session removes it.
        if let Ok(pins) = self.repo.refs_under(&format!("{}/", self.pins)) {
            let names: Vec<String> = pins.into_iter().map(|pin| pin.name).collect();
            if let Err(err) = self.repo.delete_refs(&names) {
                drop(err);
            }
        }
        if let Err(err) = std::fs::remove_file(&self.path) {
            drop(err);
        }
        if let Err(err) = self.file.unlock() {
            drop(err);
        }
    }
}

impl Review {
    /// Starts a session that pins the captures it shows (see [`Self::live_history`]) for as
    /// long as it lives, and first removes the pins of sessions that ended without doing so
    /// themselves.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file cannot be created or locked, or git fails.
    pub fn start_session(&self) -> Result<Session, Error> {
        let sessions = self.sessions_dir();
        store::create_dir(&sessions)?;
        let session = loop {
            let id = crate::random::hex(16);
            let path = self.session_lock(&id);
            let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(Error::io("create", &path, source)),
            };
            file.lock()
                .map_err(|source| Error::io("lock", &path, source))?;
            // Another session may have found the file unlocked and removed it as a dead
            // session's before the lock was taken; a lock on a removed file guards nothing.
            if path.exists() {
                break Session {
                    pins: format!("{}/{id}", self.live_pins()),
                    id,
                    path,
                    file,
                    repo: self.repo.clone(),
                };
            }
        };
        self.remove_dead_sessions(&session.id)?;
        Ok(session)
    }

    /// The prefix of every live pin of this review.
    pub(super) fn live_pins(&self) -> String {
        format!("refs/code-review/{}/live", self.meta.key.encoded())
    }

    /// The directory of the lock files of this review's sessions, next to the pins they
    /// guard: in the clone's common git directory, which every worktree of the clone shares.
    fn sessions_dir(&self) -> PathBuf {
        self.repo
            .common_dir()
            .join("code-review")
            .join("sessions")
            .join(self.meta.key.encoded())
    }

    /// The lock file of session `id`.
    fn session_lock(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.lock"))
    }

    /// Removes the pins and lock files of every session but `mine` whose lock file is not
    /// held, and pins of the layout before sessions, which no session owns.
    fn remove_dead_sessions(&self, mine: &str) -> Result<(), Error> {
        let prefix = format!("{}/", self.live_pins());
        let mut by_session: Vec<(String, Vec<String>)> = Vec::new();
        let mut unowned = Vec::new();
        for pin in self.repo.refs_under(&prefix)? {
            let Some(rest) = pin.name.strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((session, _)) if session != mine => {
                    match by_session.iter_mut().find(|(id, _)| id == session) {
                        Some((_, names)) => names.push(pin.name.clone()),
                        None => by_session.push((session.to_owned(), vec![pin.name.clone()])),
                    }
                }
                Some(_) => {}
                None => unowned.push(pin.name.clone()),
            }
        }
        self.repo.delete_refs(&unowned)?;
        for (id, names) in by_session {
            if let Some(dead) = self.dead_session(&id)? {
                self.repo.delete_refs(&names)?;
                dead.remove()?;
            }
        }
        // Lock files of sessions that pinned nothing.
        let sessions = self.sessions_dir();
        let entries =
            std::fs::read_dir(&sessions).map_err(|source| Error::io("read", &sessions, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| Error::io("read", &sessions, source))?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".lock")) else {
                continue;
            };
            if id != mine
                && let Some(dead) = self.dead_session(id)?
            {
                dead.remove()?;
            }
        }
        Ok(())
    }

    /// Locks the file of session `id` when no running session holds it, which means the
    /// session is over; a session without a lock file is over too.
    fn dead_session(&self, id: &str) -> Result<Option<DeadSession>, Error> {
        let path = self.session_lock(id);
        let file = match OpenOptions::new().write(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(DeadSession { path, file: None }));
            }
            Err(source) => return Err(Error::io("open", &path, source)),
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(DeadSession {
                path,
                file: Some(file),
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(source)) => Err(Error::io("lock", &path, source)),
        }
    }
}

/// The lock file of a session that is over, held while its pins go.
struct DeadSession {
    path: PathBuf,
    file: Option<File>,
}

impl DeadSession {
    /// Removes the lock file while still holding it, so no session starting meanwhile can take
    /// it for its own.
    fn remove(self) -> Result<(), Error> {
        if self.file.is_some() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::io("remove", &self.path, source)),
            }
        }
        drop(self.file);
        Ok(())
    }
}
