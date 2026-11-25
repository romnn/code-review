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

/// What a terminal is asked when an app wants to know what it can show, each of which would
/// wait for an answer the input thread reads: the kitty keyboard flags, device attributes,
/// the terminal's version, its cell and window sizes, its background, its graphics, and the
/// kitty graphics protocol's query.
const QUERIES: &[&[u8]] = &[
    FLAGS_QUERY,
    b"\x1b[c",
    b"\x1b[0c",
    b"\x1b[>c",
    b"\x1b[>q",
    b"\x1b[14t",
    b"\x1b[16t",
    b"\x1b[18t",
    b"\x1b]11;?",
    b"\x1b[?1;1;0S",
    b"\x1b[?2;1;0S",
    b"_Ga=q",
    b",a=q",
];

/// The start of a kitty graphics transmission with a virtual placement, as it reaches the
/// terminal, and the same inside tmux's passthrough.
const TRANSMIT: &[u8] = b"\x1b_Gq=2,i=";
const TMUX_TRANSMIT: &[u8] = b"\x1bPtmux;\x1b\x1b_Gq=2,i=";

/// The command that drops an image's data.
const DROP: &[u8] = b"_Ga=d,d=I,i=";

/// The start of an iTerm2 inline image, as WezTerm shows it.
const INLINE: &[u8] = b"\x1b]1337;File=inline=1;";

/// The app on a pseudo-terminal of 160×50 cells of 10×20 pixels, its controlling terminal, as
/// a shell runs it.
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
            ws_xpixel: 1600,
            ws_ypixel: 1000,
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
            // Not inside the tmux the tests may run in, unless a test says so.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TERM_PROGRAM")
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

/// A repository under `dir` whose `feature` branch changes one PNG, `a.png`, which the review
/// of `main..feature` shows first.
fn image_repo(dir: &Path) -> eyre::Result<std::path::PathBuf> {
    let repo = dir.join("repo");
    std::fs::create_dir(&repo)?;
    git(&repo, &["init", "--quiet", "--initial-branch=main"])?;
    let mut image = image::RgbaImage::from_pixel(64, 48, image::Rgba([20, 40, 60, 255]));
    image.save(repo.join("a.png"))?;
    git(&repo, &["add", "--all"])?;
    git(&repo, &["commit", "--quiet", "-m", "base"])?;
    git(&repo, &["checkout", "--quiet", "-b", "feature"])?;
    image.put_pixel(3, 4, image::Rgba([200, 40, 60, 255]));
    image.save(repo.join("a.png"))?;
    git(&repo, &["commit", "--quiet", "--all", "-m", "feature"])?;
    Ok(repo)
}

/// Whether `written` holds any question a terminal answers.
fn asks(written: &[u8]) -> Option<String> {
    QUERIES
        .iter()
        .find(|query| contains(written, query))
        .map(|query| String::from_utf8_lossy(query).into_owned())
}

/// In a terminal that names itself as one that shows images, a changed image's versions go to
/// the terminal without a question asked of it, `q` still quits at once with nothing answered,
/// and the terminal is told to drop the images as the app quits.
#[test]
fn images_show_without_asking_the_terminal() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = image_repo(dir.path())?;
    let mut session = Session::start(
        dir.path(),
        &repo,
        &["main..feature"],
        &[("TERM", "xterm-ghostty")],
    )?;
    session.wait_for("pixels changed")?;
    session.wait_for(std::str::from_utf8(TRANSMIT)?)?;
    session.send(b"q")?;
    let Ended { status, written } = session.end()?;
    assert!(status.success(), "{status}");
    assert_eq!(asks(&written), None, "the app asked the terminal");
    assert_eq!(count(&written, TRANSMIT), 2, "both versions go out once");
    assert_eq!(count(&written, DROP), 2, "and both are dropped");
    Ok(())
}

/// WezTerm, known by `TERM_PROGRAM` under the usual `TERM`, gets both versions as iTerm2
/// inline images without a question asked of it, and never a kitty image or a drop, since an
/// inline image goes with the cells drawn over it.
#[test]
fn wezterm_gets_inline_images_without_asking() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = image_repo(dir.path())?;
    let mut session = Session::start(
        dir.path(),
        &repo,
        &["main..feature"],
        &[("TERM", "xterm-256color"), ("TERM_PROGRAM", "WezTerm")],
    )?;
    session.wait_for("pixels changed")?;
    let deadline = Instant::now() + DEADLINE;
    while count(&session.written.lock(), INLINE) < 2 {
        eyre::ensure!(Instant::now() < deadline, "both versions never went out");
        std::thread::sleep(Duration::from_millis(10));
    }
    session.send(b"q")?;
    let Ended { status, written } = session.end()?;
    assert!(status.success(), "{status}");
    assert_eq!(asks(&written), None, "the app asked the terminal");
    assert_eq!(count(&written, INLINE), 2, "both versions go out once");
    assert_eq!(count(&written, TRANSMIT), 0);
    assert_eq!(count(&written, DROP), 0);
    Ok(())
}

/// A `tmux` under `dir` that logs how it was called and answers with the file it returns,
/// which a test writes as a client would answer; the `PATH` that finds it first.
fn tmux_stub(dir: &Path) -> eyre::Result<(std::path::PathBuf, String)> {
    use std::os::unix::fs::PermissionsExt as _;

    let bin = dir.join("bin");
    std::fs::create_dir(&bin)?;
    let answer = dir.join("answer");
    let script = indoc::formatdoc! {r#"
        #!/bin/sh
        printf '%s\n' "$*" >>'{called}'
        cat '{answer}'
    "#, called = dir.join("called").display(), answer = answer.display()};
    let tmux = bin.join("tmux");
    std::fs::write(&tmux, script)?;
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755))?;
    Ok((
        answer,
        format!("{}:{}", bin.display(), std::env::var("PATH")?),
    ))
}

/// What tmux answers for the one client `pid`, in the terminal `name` with 24-bit color and
/// kitty graphics, with passthrough as `passthrough` says.
fn client(pid: u32, name: &str, passthrough: &str) -> String {
    format!("1\t{pid}\t{name}\t{name}\tRGB,kittygraphics\t{passthrough}\n")
}

/// The app inside tmux, in `repo`, with tmux's answers from `path`'s stub.
fn in_tmux(dir: &Path, repo: &Path, path: &str) -> eyre::Result<Session> {
    Session::start(
        dir,
        repo,
        &["main..feature"],
        &[
            ("PATH", path),
            ("TERM", "tmux-256color"),
            ("TMUX", "/nonexistent/tmux,1,0"),
            ("TMUX_PANE", "%7"),
        ],
    )
}

/// Every call of tmux is the one reading of the pane and its client.
fn only_read(dir: &Path) -> eyre::Result<()> {
    let called = std::fs::read_to_string(dir.join("called"))?;
    let reading = "display-message -p -t %7 #{session_attached}\t#{client_pid}\t\
                   #{client_termname}\t#{client_termtype}\t#{client_termfeatures}\t\
                   #{allow-passthrough}";
    eyre::ensure!(called.lines().count() > 0, "tmux was never read");
    for line in called.lines() {
        eyre::ensure!(
            line == reading,
            "tmux was called for more than a reading: {line}"
        );
    }
    Ok(())
}

/// Inside tmux, the app only reads what tmux says about its pane and client, and shows images
/// through tmux's passthrough only when the client is a terminal that shows them and
/// passthrough is already on; never does it change a setting of tmux's.
#[test]
fn tmux_is_only_read() -> eyre::Result<()> {
    for (passthrough, shown) in [("on", true), ("off", false)] {
        let dir = tempfile::tempdir()?;
        let repo = image_repo(dir.path())?;
        let (answer, path) = tmux_stub(dir.path())?;
        std::fs::write(&answer, client(100, "xterm-ghostty", passthrough))?;
        let mut session = in_tmux(dir.path(), &repo, &path)?;
        session.wait_for("binary content changed")?;
        if shown {
            session.wait_for(std::str::from_utf8(TMUX_TRANSMIT)?)?;
        } else {
            // Long enough for tmux's answer to land and a frame to follow it.
            std::thread::sleep(Duration::from_millis(500));
        }
        session.send(b"q")?;
        let Ended { status, written } = session.end()?;
        assert!(status.success(), "{status}");
        assert_eq!(asks(&written), None, "the app asked the terminal");
        only_read(dir.path())?;
        assert_eq!(
            contains(&written, TMUX_TRANSMIT),
            shown,
            "passthrough {passthrough}"
        );
        // Every transmission is the one inside the passthrough.
        assert_eq!(
            count(&written, TRANSMIT),
            count(&written, TMUX_TRANSMIT),
            "an image went out unwrapped"
        );
    }
    Ok(())
}

/// Another client attaching to the pane, as after a detach, gets the images sent again once
/// the app regains focus, since its terminal never had them; a client whose terminal shows no
/// images gets none. Between, tmux is never read: no reading repeats on its own.
#[test]
fn another_tmux_client_gets_the_images_afresh() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = image_repo(dir.path())?;
    let (answer, path) = tmux_stub(dir.path())?;
    std::fs::write(&answer, client(100, "xterm-ghostty", "on"))?;
    let mut session = in_tmux(dir.path(), &repo, &path)?;
    let sent = |session: &Session| count(&session.written.lock(), TMUX_TRANSMIT);
    let readings = || {
        std::fs::read_to_string(dir.path().join("called"))
            .map_or(0, |called| called.lines().count())
    };
    let deadline = Instant::now() + DEADLINE;
    while sent(&session) < 2 {
        eyre::ensure!(
            Instant::now() < deadline,
            "the first client never got both images"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        contains(&session.written.lock(), b"\x1b[?1004h"),
        "the app never asked for focus reports"
    );

    // Another Ghostty attaches, which no reading notices until focus comes back.
    std::fs::write(&answer, client(200, "xterm-ghostty", "on"))?;
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(readings(), 1, "tmux was read without a reason");
    assert_eq!(sent(&session), 2);
    session.send(b"\x1b[I")?;
    while sent(&session) < 4 {
        eyre::ensure!(
            Instant::now() < deadline,
            "the new client never got the images"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // A terminal without images attaches: nothing more goes out.
    std::fs::write(&answer, client(300, "alacritty", "on"))?;
    session.send(b"\x1b[I")?;
    while readings() < 3 {
        eyre::ensure!(Instant::now() < deadline, "focus never read tmux again");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(500));
    session.send(b"q")?;
    let Ended { status, written } = session.end()?;
    assert!(status.success(), "{status}");
    assert_eq!(count(&written, TMUX_TRANSMIT), 4);
    assert_eq!(asks(&written), None, "the app asked the terminal");
    only_read(dir.path())
}
