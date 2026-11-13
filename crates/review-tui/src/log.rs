//! The developer's diagnostics, which must never be written over the screen: while the
//! terminal shows the review, they go to a log file and are kept to print on standard error
//! once the terminal is back.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

/// Where diagnostics go.
#[derive(Debug, Clone)]
pub struct Log {
    /// The log file, appended to; `None` when no state directory is known.
    file: Option<PathBuf>,
    /// The lines kept to print once the terminal is restored, shared with the panic hook.
    held: Arc<Mutex<Vec<String>>>,
    /// Whether lines go to standard error at once, as in QA mode, which draws off screen.
    direct: bool,
}

impl Log {
    /// A log that keeps lines for later and appends them to `file`.
    #[must_use]
    pub fn held(file: Option<PathBuf>) -> Self {
        Self {
            file,
            held: Arc::default(),
            direct: false,
        }
    }

    /// A log that prints lines on standard error at once.
    #[must_use]
    pub fn direct() -> Self {
        Self {
            file: None,
            held: Arc::default(),
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
        if let Some(path) = &self.file
            && let Err(err) = append(path, &line)
        {
            self.held.lock().push(format!(
                "cr: cannot write the log {}: {err}",
                path.display()
            ));
        }
        self.held.lock().push(line);
    }

    /// Prints the lines kept so far on standard error, once.
    pub fn replay(&self) {
        for line in std::mem::take(&mut *self.held.lock()) {
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

    /// Held lines go to the file at once and are printed only once.
    #[test]
    fn held_lines_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state/review-tui.log");
        let log = Log::held(Some(file.clone()));
        log.write("first");
        log.write("second");
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
