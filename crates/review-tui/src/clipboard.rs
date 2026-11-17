//! Putting copied text on the system clipboard where the terminal cannot.
//!
//! The app always asks the terminal to copy through OSC 52, which also works over ssh. Some
//! terminals ignore that request, such as macOS Terminal and the VTE-based ones like GNOME
//! Terminal, or have it turned off, so on a local session the text also goes to the platform's
//! clipboard command, as micromux does.

use std::io::Write as _;
use std::process::{Command, Stdio};

/// Which clipboard a copy goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    /// The clipboard that the paste keys paste from.
    Clipboard,
    /// Linux's primary selection, which the middle button pastes.
    Primary,
}

/// Clipboard commands to try in order for `board`, each taking the text on standard input.
fn commands(board: Board) -> Vec<(&'static str, &'static [&'static str])> {
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
        let mut commands: Vec<(&'static str, &'static [&'static str])> = Vec::new();
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            commands.push(("wl-copy", if primary { &["--primary"] } else { &[] }));
        }
        if std::env::var_os("DISPLAY").is_some() {
            commands.push((
                "xclip",
                if primary {
                    &["-selection", "primary"]
                } else {
                    &["-selection", "clipboard"]
                },
            ));
            commands.push((
                "xsel",
                if primary {
                    &["--primary", "--input"]
                } else {
                    &["--clipboard", "--input"]
                },
            ));
        }
        commands
    }
}

/// Whether the app runs over ssh, where a local clipboard command would copy on the remote
/// host.
fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

/// Hands `text` to the first clipboard command for `board` that runs, without waiting for it,
/// unless the app runs over ssh; a missing or failing command is skipped silently, since the
/// terminal may have copied the text already.
pub fn copy_locally(text: String, board: Board) {
    if over_ssh() {
        return;
    }
    for (program, args) in commands(board) {
        let mut command = Command::new(program);
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
        std::thread::spawn(move || {
            if let Some(mut stdin) = child.stdin.take() {
                // A command that stopped reading copied nothing more that could be saved.
                stdin.write_all(text.as_bytes()).ok();
            }
            // Reap the command once it exits, so it does not linger as a zombie.
            child.wait().ok();
        });
        return;
    }
}
