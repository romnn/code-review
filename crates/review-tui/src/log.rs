//! The developer's diagnostics, which must never be written over the screen: while the
//! terminal shows the review, they go to a log file and are kept to print on standard error
//! once the terminal is back.
//!
//! The log file is appended to on a thread of its own, the [`LogFile`], so a slow disk never
//! holds the main loop; only the code that runs after the main loop waits for it.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use review_ui::worker::{self, Worker};

/// How long [`LogFile::flush`] waits for the log file's writes, so a stuck disk cannot hang
/// exit.
const FLUSH_DEADLINE: Duration = Duration::from_secs(1);

/// The thread that appends a log's lines to the log file.
#[derive(Debug)]
pub struct LogFile {
    worker: Worker<String>,
    /// Why lines could not be appended, which the thread keeps apart from the lines the main
    /// loop holds.
    failed: Arc<Mutex<Vec<String>>>,
}

impl LogFile {
    /// Starts the thread that appends lines to the file at `path`, creating it and its
    /// directory.
    #[must_use]
    pub fn start(path: PathBuf) -> Self {
        let failed: Arc<Mutex<Vec<String>>> = Arc::default();
        let worker = Worker::start({
            let failed = Arc::clone(&failed);
            move |line: String| {
                if let Err(err) = append(&path, &line) {
                    failed.lock().push(format!(
                        "cr: cannot write the log {}: {err}",
                        path.display()
                    ));
                }
            }
        });
        Self { worker, failed }
    }

    /// Waits until the lines written so far are in the file, for at most [`FLUSH_DEADLINE`].
    pub fn flush(&self) {
        self.worker.flush(FLUSH_DEADLINE);
    }
}

/// Where diagnostics go.
#[derive(Debug, Clone)]
pub struct Log {
    /// Hands lines to the log file's thread; `None` when no state directory is known.
    file: Option<worker::Sender<String>>,
    /// The lines kept to print once the terminal is restored, shared with the panic hook.
    held: Arc<Mutex<Vec<String>>>,
    /// Why the log file's thread could not append lines, printed with the held lines.
    failed: Arc<Mutex<Vec<String>>>,
    /// Whether lines go to standard error at once, as in QA mode, which draws off screen.
    direct: bool,
}

impl Log {
    /// A log that keeps lines for later and has `file` append them to the log file.
    #[must_use]
    pub fn held(file: Option<&LogFile>) -> Self {
        Self {
            file: file.map(|file| file.worker.sender()),
            held: Arc::default(),
            failed: file
                .map(|file| Arc::clone(&file.failed))
                .unwrap_or_default(),
            direct: false,
        }
    }

    /// A log that prints lines on standard error at once.
    #[must_use]
    pub fn direct() -> Self {
        Self {
            file: None,
            held: Arc::default(),
            failed: Arc::default(),
            direct: true,
        }
    }

    /// Records `message`.
    pub fn write(&self, message: &str) {
        let line = format!("cr: {message}");
        if self.direct {
            eprintln!("{line}");
            return;
        }
        if let Some(file) = &self.file {
            file.send(line.clone());
        }
        self.held.lock().push(line);
    }

    /// Prints the lines kept so far on standard error, once, and why any could not be
    /// appended to the log file.
    pub fn replay(&self) {
        // Taken out first, so no lock is held while standard error blocks.
        let held = std::mem::take(&mut *self.held.lock());
        let failed = std::mem::take(&mut *self.failed.lock());
        for line in held.into_iter().chain(failed) {
            eprintln!("{line}");
        }
    }
}

/// Appends `line` to the file at `path`, creating it and its directory.
fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

/// The log file under the state directory: `$XDG_STATE_HOME/code-review/review-tui.log`, or
/// under `~/.local/state` without it.
#[must_use]
pub fn path(xdg_state_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = match xdg_state_home.filter(|dir| dir.is_absolute()) {
        Some(dir) => dir.to_path_buf(),
        None => home?.join(".local").join("state"),
    };
    Some(base.join("code-review").join("review-tui.log"))
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    /// Held lines reach the file in order once flushed, and are printed only once.
    #[test]
    fn held_lines_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state/review-tui.log");
        let log_file = LogFile::start(file.clone());
        let log = Log::held(Some(&log_file));
        log.write("first");
        log.write("second");
        // However long the writes take: `flush` gives up early only so a stuck disk cannot
        // hang exit.
        log_file.worker.flush(Duration::MAX);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            text,
            indoc! {"
                cr: first
                cr: second
            "}
        );
        assert_eq!(log.held.lock().len(), 2);
        log.replay();
        assert!(log.held.lock().is_empty());
    }

    /// The state directory comes from `XDG_STATE_HOME` when it is absolute, else from home.
    #[test]
    fn the_log_lives_in_the_state_directory() {
        // The temporary directory is absolute on every platform, which `/state` is not on
        // Windows.
        let root = std::env::temp_dir();
        let (state, home) = (root.join("state"), root.join("home"));
        assert_eq!(
            path(Some(&state), Some(&home)),
            Some(state.join("code-review").join("review-tui.log"))
        );
        assert_eq!(
            path(Some(Path::new("relative")), Some(&home)),
            Some(
                home.join(".local")
                    .join("state")
                    .join("code-review")
                    .join("review-tui.log")
            )
        );
        assert_eq!(path(None, None), None);
    }
}
