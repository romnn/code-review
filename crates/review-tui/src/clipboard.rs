//! Putting copied text on the system clipboard where the terminal cannot.
//!
//! The app always asks the terminal to copy through OSC 52, which also works over ssh. Some
//! terminals ignore that request, such as macOS Terminal and the VTE-based ones like GNOME
//! Terminal, or have it turned off, so on a local session the text also goes to the platform's
//! clipboard command, as micromux does.
//!
//! The commands run on a thread of their own, one copy at a time in the order made, so the
//! main loop never waits for one and a later copy normally wins.
//! That holds only as far as the commands keep it: `xsel`'s daemon may take the selection
//! after its parent exits, so with `xsel` the order is best effort.

use std::io::Write as _;
use std::process::Stdio;
use std::time::Duration;

use review_ui::worker::Worker;

/// How long the app waits at exit, once the terminal is back, for the copies made last to
/// reach their commands.
pub const DRAIN_DEADLINE: Duration = Duration::from_millis(500);

/// Which clipboard a copy goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    /// The clipboard that the paste keys paste from.
    Clipboard,
    /// Linux's primary selection, which the middle button pastes.
    Primary,
}

/// Text to put on a clipboard.
#[derive(Debug)]
pub struct Clip {
    /// The text.
    pub text: String,
    /// The clipboard it goes to.
    pub board: Board,
}

/// Starts the thread that puts each [`Clip`] it is handed on its clipboard, in order.
#[must_use]
pub fn start() -> Worker<Clip> {
    Worker::start(copy_locally)
}

/// A clipboard command and its arguments, which takes the text on standard input.
type Command = (&'static str, &'static [&'static str]);

/// Clipboard commands to try in order for `board`.
fn commands(board: Board) -> Vec<Command> {
    let primary = board == Board::Primary;
    if cfg!(target_os = "macos") {
        // macOS has no primary selection.
        if primary {
            Vec::new()
        } else {
            vec![("pbcopy", &[])]
        }
    } else if cfg!(windows) {
        // Windows Terminal supports OSC 52, and `clip` would garble text outside the console's
        // code page.
        Vec::new()
    } else {
        let wayland: Command = ("wl-copy", if primary { &["--primary"] } else { &[] });
        let x11: [Command; 2] = [
            (
                "xclip",
                if primary {
                    &["-selection", "primary"]
                } else {
                    &["-selection", "clipboard"]
                },
            ),
            (
                "xsel",
                if primary {
                    &["--primary", "--input"]
                } else {
                    &["--clipboard", "--input"]
                },
            ),
        ];
        let has = |name| std::env::var_os(name).is_some();
        let x11 = if has("DISPLAY") {
            x11.to_vec()
        } else {
            Vec::new()
        };
        let wayland = if has("WAYLAND_DISPLAY") {
            vec![wayland]
        } else {
            Vec::new()
        };
        // GNOME gives no Wayland clipboard to a program without a window, so `wl-copy` maps one
        // for a moment there, which flashes in the dock; the X11 tools copy through XWayland,
        // which passes the text on to Wayland, with a window that never shows.
        if gnome() {
            [x11, wayland].concat()
        } else {
            [wayland, x11].concat()
        }
    }
}

/// Whether the desktop is GNOME, as `XDG_CURRENT_DESKTOP` names it, such as `ubuntu:GNOME`.
fn gnome() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktops| {
        desktops
            .split(':')
            .any(|desktop| desktop.eq_ignore_ascii_case("gnome"))
    })
}

/// Whether the app runs over ssh, where a local clipboard command would copy on the remote
/// host.
fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

/// Hands the clip's text to the first clipboard command for its board that runs and waits for
/// the command to exit, unless the app runs over ssh; a missing or failing command is skipped
/// silently, since the terminal may have copied the text already.
///
/// The command is never stopped.
/// One that hangs only delays the copies after it, while one stopped early can leave the text
/// behind, as `wl-copy` does with the temporary file it reads the text into.
fn copy_locally(clip: Clip) {
    let Clip { text, board } = clip;
    if over_ssh() {
        return;
    }
    for (program, args) in commands(board) {
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // A process group of its own, so a stop sent to the app's group does not also stop a
        // clipboard daemon that `wl-copy` or `xclip` leaves serving the copied text.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let Ok(mut child) = command.spawn() else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            // A command that stopped reading copied nothing more that could be saved.
            stdin.write_all(text.as_bytes()).ok();
        }
        // Waiting for the command keeps the next copy from landing before this one, and reaps
        // the command, so it does not linger as a zombie.
        child.wait().ok();
        return;
    }
}
