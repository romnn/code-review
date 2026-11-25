//! review-tui on a pseudo-terminal, as a terminal runs it: what it writes to the terminal, and
//! how it ends.
#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use color_eyre::eyre;
use parking_lot::Mutex;

/// How long the app gets for what a test waits on, however loaded the machine.
const DEADLINE: Duration = Duration::from_secs(60);

/// What a terminal that speaks the kitty keyboard protocol answers its query with: its flags.
const FLAGS_ANSWER: &[u8] = b"\x1b[?0u";

/// The query of the kitty keyboard protocol's flags.
const FLAGS_QUERY: &[u8] = b"\x1b[?u";

/// The protocol's push of the disambiguation flag, and its pop.
const PUSH: &[u8] = b"\x1b[>1u";
const POP: &[u8] = b"\x1b[<1u";

/// The app on a pseudo-terminal of 160×50 cells, its controlling terminal, as a shell runs it.
struct Session {
    app: Child,
    /// The terminal's side of the pseudo-terminal, which types what a test sends.
    terminal: File,
    /// Everything the app wrote so far.
    written: Arc<Mutex<Vec<u8>>>,
    /// Says when the app's side of the terminal closed, so all it wrote is in `written`.
    closed: Receiver<()>,
}

/// How the app ended, with everything it wrote.
struct Ended {
    status: ExitStatus,
    written: Vec<u8>,
}

impl Session {
    /// Starts the app with `args` in `cwd`, with its data, settings and log under `dir`, none
    /// of the user's git configuration, and the variables `env` on top.
    fn start(dir: &Path, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> eyre::Result<Self> {
        let size = nix::pty::Winsize {
            ws_row: 50,
            ws_col: 160,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = nix::pty::openpty(&size, None)?;
        let terminal = File::from(pty.master);
        let app_side = File::from(pty.slave);
        // util-linux's `setsid` makes the pseudo-terminal, the app's standard input, its
        // controlling terminal before it runs the app in its place.
        let app = Command::new("setsid")
            .arg("--ctty")
            .arg(env!("CARGO_BIN_EXE_review-tui"))
            .args(args)
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env("CODE_REVIEW_CONFIG", dir.join("config.toml"))
            .env("CODE_REVIEW_DEMO_DIR", dir.join("demo"))
            .env("CR_DATA_DIR", dir.join("data"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", dir)
            .envs(env.iter().copied())
            .stdin(app_side.try_clone()?)
            .stdout(app_side.try_clone()?)
            .stderr(app_side)
            .spawn()?;
        let written: Arc<Mutex<Vec<u8>>> = Arc::default();
        let (close, closed) = mpsc::channel();
        let mut reader = terminal.try_clone()?;
        std::thread::spawn({
            let written = Arc::clone(&written);
            move || {
                let mut chunk = [0; 4096];
                // Reading fails once the app is gone, and with it every copy of its side.
                while let Ok(read @ 1..) = reader.read(&mut chunk) {
                    written
                        .lock()
                        .extend_from_slice(chunk.get(..read).unwrap_or_default());
                }
                // Nobody may wait for this any more.
                close.send(()).ok();
            }
        });
        Ok(Self {
            app,
            terminal,
            written,
            closed,
        })
    }

    /// Waits until the app wrote `text`.
    fn wait_for(&self, text: &str) -> eyre::Result<()> {
        let deadline = Instant::now() + DEADLINE;
        while !contains(&self.written.lock(), text.as_bytes()) {
            eyre::ensure!(Instant::now() < deadline, "the app never wrote {text:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    /// Sends `bytes` to the app, as the terminal does what is typed and its answers.
    fn send(&mut self, bytes: &[u8]) -> eyre::Result<()> {
        Ok(self.terminal.write_all(bytes)?)
    }

    /// Waits for the app to end.
    fn end(&mut self) -> eyre::Result<Ended> {
        let deadline = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = self.app.try_wait()? {
                break status;
            }
            eyre::ensure!(Instant::now() < deadline, "the app never ended");
            std::thread::sleep(Duration::from_millis(10));
        };
        self.closed.recv_timeout(DEADLINE)?;
        Ok(Ended {
            status,
            written: std::mem::take(&mut *self.written.lock()),
        })
    }
}

impl Drop for Session {
    /// Ends an app a failed test left running.
    fn drop(&mut self) {
        if matches!(self.app.try_wait(), Ok(None)) {
            self.app.kill().ok();
            self.app.wait().ok();
        }
    }
}

/// Whether `haystack` holds `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// How often `haystack` holds `needle`.
fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// Runs git in `repo` with `args`, as its own user, without the user's configuration.
fn git(repo: &Path, args: &[&str]) -> eyre::Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()?;
    eyre::ensure!(output.status.success(), "git {args:?}: {output:?}");
    Ok(())
}

/// A terminal that answers a question about the keyboard only in part, with its flags but
/// without its device attributes, still has `q` quit at once: the app asks the terminal
/// nothing and waits for no answer, pushing the keyboard protocol's flags as it takes the
/// terminal over and popping them as it gives it back.
#[test]
fn q_quits_whatever_the_terminal_answers() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut session = Session::start(dir.path(), dir.path(), &["--demo"], &[])?;
    session.wait_for("Opening the review")?;
    session.send(FLAGS_ANSWER)?;
    session.send(b"q")?;
    let Ended { status, written } = session.end()?;
    assert!(status.success(), "{status}");
    assert!(
        !contains(&written, FLAGS_QUERY),
        "the app asked about the keyboard"
    );
    assert_eq!(count(&written, PUSH), 1, "the flags are pushed once");
    assert_eq!(count(&written, POP), 1, "and popped once");
    Ok(())
}

/// A review that cannot be opened shows why, and once the reviewer quits, the app gives the
/// terminal back and ends in failure.
#[test]
fn a_review_that_cannot_be_opened_ends_in_failure() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    // The directory is no repository.
    let mut session = Session::start(dir.path(), dir.path(), &[], &[])?;
    session.wait_for("Couldn't open this review")?;
    session.send(b"q")?;
    let Ended { status, written } = session.end()?;
    assert!(!status.success(), "{status}");
    assert!(
        contains(&written, b"\x1b[?1049l"),
        "the alternate screen is left"
    );
    Ok(())
}

/// Quitting while the review still opens, here while git is busy in a checkout with nothing
/// recorded yet to show meanwhile, ends the app at once, and successfully: the reviewer left,
/// and nothing failed.
#[test]
fn quitting_while_the_review_opens_succeeds() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo)?;
    git(&repo, &["init", "--quiet", "--initial-branch=main"])?;
    std::fs::write(repo.join("a.rs"), "fn a() {}\n")?;
    git(&repo, &["add", "--all"])?;
    git(&repo, &["commit", "--quiet", "-m", "base"])?;
    std::fs::write(repo.join(".git/index.lock"), "")?;
    let mut session = Session::start(dir.path(), &repo, &[], &[])?;
    session.wait_for("Opening the review")?;
    session.send(b"q")?;
    let Ended { status, .. } = session.end()?;
    assert!(status.success(), "{status}");
    Ok(())
}

/// The Linux console, whose older kernels print what follows `>` or `<` in a sequence they do
/// not know, is asked for no keyboard protocol.
#[test]
fn the_linux_console_gets_no_keyboard_protocol() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut session = Session::start(dir.path(), dir.path(), &["--demo"], &[("TERM", "linux")])?;
    session.wait_for("Opening the review")?;
    session.send(b"q")?;
    let Ended { status, written } = session.end()?;
    assert!(status.success(), "{status}");
    assert!(!contains(&written, PUSH), "the flags are pushed");
    assert!(!contains(&written, POP), "the flags are popped");
    Ok(())
}

/// No fetch can prompt on the terminal, whatever command runs ssh: here a shell wrapper, `exec`
/// before a script that asks on `/dev/tty` as ssh asks for a passphrase, for the fetch that
/// deepens a shallow clone. The script runs, cannot open the terminal, and the review shows
/// why it could not be opened.
#[test]
fn no_fetch_prompts_on_the_terminal() -> eyre::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir()?;
    let origin = dir.path().join("origin");
    std::fs::create_dir(&origin)?;
    git(&origin, &["init", "--quiet", "--initial-branch=main"])?;
    std::fs::write(origin.join("a.rs"), "fn a() {}\n")?;
    git(&origin, &["add", "--all"])?;
    git(&origin, &["commit", "--quiet", "-m", "base"])?;
    git(&origin, &["checkout", "--quiet", "-b", "feature"])?;
    std::fs::write(origin.join("a.rs"), "fn a() { 1 }\n")?;
    git(&origin, &["commit", "--quiet", "--all", "-m", "feature"])?;
    git(&origin, &["checkout", "--quiet", "main"])?;
    std::fs::write(origin.join("b.rs"), "fn b() {}\n")?;
    git(&origin, &["add", "--all"])?;
    git(&origin, &["commit", "--quiet", "-m", "main"])?;
    // Each branch without its parent, so the range has no merge base until a fetch deepens it.
    let url = format!("file://{}", origin.display());
    git(
        dir.path(),
        &[
            "clone",
            "--quiet",
            "--depth=1",
            "--no-single-branch",
            &url,
            "clone",
        ],
    )?;
    let clone = dir.path().join("clone");
    git(
        &clone,
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://example.invalid/origin",
        ],
    )?;
    let asked = dir.path().join("asked");
    let ssh = dir.path().join("ssh");
    let script = indoc::formatdoc! {"
        #!/bin/sh
        if {{ printf 'Enter passphrase: ' >/dev/tty; }} 2>/dev/null; then
            echo prompted >'{asked}'
            read -r answer </dev/tty
        else
            echo 'no terminal' >'{asked}'
        fi
        exit 1
    ", asked = asked.display()};
    std::fs::write(&ssh, script)?;
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755))?;
    let command = format!("exec '{}'", ssh.display());

    let mut session = Session::start(
        dir.path(),
        &clone,
        &["origin/main..origin/feature"],
        &[("GIT_SSH_COMMAND", &command), ("GIT_SSH_VARIANT", "ssh")],
    )?;
    session.wait_for("Couldn't open this review")?;
    session.send(b"q")?;
    let Ended { status, written } = session.end()?;
    assert!(!status.success(), "{status}");
    assert_eq!(std::fs::read_to_string(&asked)?, "no terminal\n");
    assert!(!contains(&written, b"passphrase"), "the script prompted");
    Ok(())
}
