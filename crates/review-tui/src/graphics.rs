//! Whether the terminal shows images, decided from what it says about itself without being
//! asked, and the kitty graphics protocol's escape sequences that review-tui writes.
//!
//! Nothing here writes a question to the terminal or reads its input, which crossterm's input
//! thread alone reads (ADR 12).
//! Outside tmux, kitty and Ghostty name themselves in `TERM` and show kitty graphics with
//! Unicode placeholders; WezTerm names itself in `TERM_PROGRAM` and shows iTerm2's inline
//! images, since it draws no placeholders.
//! Inside tmux, both name tmux, so a bounded call of `tmux display-message`, which only reads,
//! says which terminal the one attached client runs in and whether the pane passes escape
//! sequences through; images show only when that terminal is kitty or Ghostty with 24-bit
//! color and passthrough is already on, and review-tui never turns it on.
//! WezTerm shows none under tmux: an inline image goes wherever tmux last left the terminal's
//! cursor, and tmux, which never knew of it, draws over it.
//! The call repeats whenever the app regains focus or is resized, as when another client
//! attaches to the pane, whose terminal holds none of the images sent before; tmux reports focus
//! only with its `focus-events` on, without which a reattach shows images after a resize.
//! Anything unknown shows a changed image as the binary line it was.
//!
//! [`Uploads`] keeps the kitty images the terminal may hold, so every way out of the app, a
//! panic and a second signal included, has the terminal drop them.

use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How long the call that reads tmux's client and pane may take before it is given up.
const TMUX_DEADLINE: Duration = Duration::from_secs(2);

/// What tmux says about the pane and its client, a field per tab; the client's process tells
/// one client from the next.
const TMUX_FORMAT: &str = "#{session_attached}\t#{client_pid}\t#{client_termname}\t\
                           #{client_termtype}\t#{client_termfeatures}\t#{allow-passthrough}";

/// How the terminal shows images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Graphics {
    /// No image protocol is confirmed: a changed image shows as the binary line.
    None,
    /// The kitty graphics protocol with Unicode placeholders, as kitty and Ghostty show it,
    /// its escape sequences wrapped in tmux's passthrough when `tmux`.
    Kitty {
        /// Whether the app runs inside tmux.
        tmux: bool,
    },
    /// iTerm2's inline images, as WezTerm shows them outside tmux; an image lives in the cells
    /// it covers, so drawing over them removes it.
    Iterm2,
}

/// What the environment says about the terminal, as the app starts.
#[derive(Debug, Clone)]
pub struct Environment {
    /// `TERM`.
    pub term: Option<String>,
    /// `TERM_PROGRAM`.
    pub term_program: Option<String>,
    /// `TMUX_PANE`, the pane the app runs in when it runs inside tmux.
    pub tmux_pane: Option<String>,
}

impl Environment {
    /// How the terminal shows images, as far as `TERM` and `TERM_PROGRAM` say outside tmux;
    /// inside tmux, [`Graphics::None`] until [`Self::follow_tmux`] says.
    #[must_use]
    pub fn graphics(&self) -> Graphics {
        if self.tmux_pane.is_some() {
            return Graphics::None;
        }
        if self.term.as_deref().is_some_and(shows_kitty) {
            Graphics::Kitty { tmux: false }
        } else if self.term_program.as_deref() == Some("WezTerm") {
            Graphics::Iterm2
        } else {
            Graphics::None
        }
    }

    /// Inside tmux, reads how the terminal of the client attached to the app's pane shows
    /// images, at once and again each time `asks` says to, with a bounded call of `tmux` that
    /// only reads, and hands each change of what tmux says to `changed`, the first reading
    /// included, until `changed` says the app is gone or nothing can ask any more; returns at
    /// once outside tmux.
    /// `None` says tmux could not be read: it failed or did not answer in time.
    ///
    /// It runs for as long as the app does, so it runs on a thread of its own.
    pub fn follow_tmux(&self, asks: &Receiver<()>, changed: impl Fn(Option<Graphics>) -> bool) {
        let Some(pane) = self.tmux_pane.as_deref() else {
            return;
        };
        let mut said = None;
        loop {
            let answer = run_bounded(
                Command::new("tmux").args(["display-message", "-p", "-t", pane, TMUX_FORMAT]),
                TMUX_DEADLINE,
            );
            if said.as_ref() != Some(&answer) {
                if !changed(answer.as_deref().map(tmux_graphics)) {
                    return;
                }
                said = Some(answer);
            }
            // Asks that came while tmux was read are answered by the next reading.
            if asks.recv().is_err() {
                return;
            }
            while asks.try_recv().is_ok() {}
        }
    }
}

/// Whether the terminal named `name`, as `TERM` or tmux's client name it, shows kitty graphics
/// with Unicode placeholders.
fn shows_kitty(name: &str) -> bool {
    matches!(name, "xterm-kitty" | "xterm-ghostty")
}

/// How images show under tmux, from its `answer` in [`TMUX_FORMAT`]: kitty graphics only for
/// the one client attached, in kitty or Ghostty with 24-bit color, with passthrough on.
///
/// tmux knows a client's terminal by its `TERM` and by its answer to tmux's own question of
/// its version.
fn tmux_graphics(answer: &str) -> Graphics {
    let fields: Vec<&str> = answer.trim_end_matches('\n').split('\t').collect();
    let [attached, _client, name, kind, features, passthrough] = fields.as_slice() else {
        return Graphics::None;
    };
    let colors = features.split(',').any(|feature| feature == "RGB");
    let passes = matches!(*passthrough, "on" | "all");
    let terminal = shows_kitty(name) || kind.starts_with("kitty") || kind.starts_with("ghostty");
    if *attached == "1" && terminal && colors && passes {
        Graphics::Kitty { tmux: true }
    } else {
        Graphics::None
    }
}

/// What `command` prints, run without input for at most `deadline`, its output read
/// included; `None` when it fails or takes longer, when it is killed with everything it
/// started.
fn run_bounded(command: &mut Command, deadline: Duration) -> Option<String> {
    Running::start(command)?.finish(deadline)
}

/// A command [`run_bounded`] started, with the thread that reads its output.
struct Running {
    child: Child,
    started: Instant,
    output: Receiver<std::io::Result<String>>,
    reader: JoinHandle<()>,
}

impl Running {
    /// Starts `command` without input, its output read on a thread of its own; `None` when it
    /// cannot start.
    fn start(command: &mut Command) -> Option<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Its own process group, so a kill reaches whatever it started that holds its output.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(command, 0);
        let mut child = command.spawn().ok()?;
        let started = Instant::now();
        let mut pipe = child.stdout.take()?;
        let (sender, output) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut answer = String::new();
            let read = pipe.read_to_string(&mut answer).map(|_| answer);
            sender.send(read).ok();
        });
        Some(Self {
            child,
            started,
            output,
            reader,
        })
    }

    /// What the command printed, once it ended within `deadline` of its start; `None` when it
    /// fails or takes longer, when it is killed with everything it started.
    fn finish(mut self, deadline: Duration) -> Option<String> {
        let read = self
            .output
            .recv_timeout(deadline.saturating_sub(self.started.elapsed()));
        // The child is reaped only once its output is in, so a kill of its group always runs
        // while the child still holds the group's id, which then cannot belong to another group.
        let status = match read {
            Ok(_) => loop {
                match self.child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) if self.started.elapsed() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    // Too slow, or it cannot be waited for.
                    _ => break None,
                }
            },
            // Its output never came.
            Err(_) => None,
        };
        if status.is_none() {
            kill(&mut self.child);
        }
        // With every process that held the output gone, the reader ends at once and is joined;
        // one a process outside the group still holds is left to end on its own.
        let read = read.or_else(|_| self.output.recv_timeout(Duration::from_secs(1)));
        if read.is_ok() {
            self.reader.join().ok();
        }
        let answer = read.ok()?.ok()?;
        status?.success().then_some(answer)
    }
}

/// Kills `child` and, on Unix, the rest of the process group it leads, then reaps it.
fn kill(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child leads its process group, so the group's id is the child's.
        let group = rustix::process::Pid::from_child(child);
        rustix::process::kill_process_group(group, rustix::process::Signal::KILL).ok();
    }
    #[cfg(not(unix))]
    child.kill().ok();
    child.wait().ok();
}

/// A kitty image the terminal may hold: its identity, and whether it went through tmux's
/// passthrough, as its drop must.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upload {
    /// The kitty graphics protocol's identity of the image.
    pub id: u32,
    /// Whether it went through tmux.
    pub tmux: bool,
}

/// The images the terminal may hold for the app, which every way out of it drops.
static UPLOADS: Mutex<Uploads> = Mutex::new(Uploads {
    held: Vec::new(),
    retired: Vec::new(),
});

/// The images the terminal may hold: those a frame may draw, and those replaced since the last
/// frame, which the next one drops.
#[derive(Debug)]
pub struct Uploads {
    held: Vec<Upload>,
    retired: Vec<Upload>,
}

/// Keeps the tests that use the images the terminal may hold from running at the same time,
/// each starting from none.
#[cfg(test)]
pub fn alone() -> parking_lot::MutexGuard<'static, ()> {
    static ALONE: Mutex<()> = Mutex::new(());
    let alone = ALONE.lock();
    drop(Uploads::drops_now());
    alone
}

impl Uploads {
    /// Notes that the terminal may hold `upload` from the next frame on.
    pub fn hold(upload: Upload) {
        UPLOADS.lock().held.push(upload);
    }

    /// Notes that no frame draws image `id` any more, so the next frame drops it.
    pub fn retire(id: u32) {
        let mut uploads = UPLOADS.lock();
        let Uploads { held, retired } = &mut *uploads;
        retired.extend(held.iter().filter(|upload| upload.id == id));
        held.retain(|upload| upload.id != id);
    }

    /// The escape sequences that drop the images retired since the last call, and with `all`,
    /// every image, as the app quits.
    #[must_use]
    pub fn drops(all: bool) -> String {
        let mut uploads = UPLOADS.lock();
        let Uploads { held, retired } = &mut *uploads;
        if all {
            retired.append(held);
        }
        retired.drain(..).map(drop_sequence).collect()
    }

    /// The escape sequences that drop every image, for a way out that cannot wait: nothing
    /// when another thread holds the list, as a thread that panicked holding it might.
    #[must_use]
    pub fn drops_now() -> String {
        let Some(mut uploads) = UPLOADS.try_lock() else {
            return String::new();
        };
        let Uploads { held, retired } = &mut *uploads;
        retired.append(held);
        retired.drain(..).map(drop_sequence).collect()
    }
}

/// The escape sequence that tells the terminal to drop `upload` and its data; it asks for no
/// answer.
fn drop_sequence(upload: Upload) -> String {
    let Upload { id, tmux } = upload;
    delete(id, tmux)
}

/// The escape sequence that tells the terminal to drop image `id` and its data, wrapped for
/// tmux when `tmux`; it asks for no answer.
#[must_use]
pub fn delete(id: u32, tmux: bool) -> String {
    let command = format!("_Ga=d,d=I,i={id},q=2");
    if tmux {
        format!("\x1bPtmux;\x1b\x1b{command}\x1b\x1b\\\x1b\\")
    } else {
        format!("\x1b{command}\x1b\\")
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use rustix::process::{WaitId, WaitIdOptions};

    use super::*;

    /// Outside tmux, `TERM` or `TERM_PROGRAM` names the terminals that show images; inside tmux,
    /// neither does.
    #[test]
    fn term_names_the_terminal_outside_tmux() {
        let outside = |term: &str| Environment {
            term: Some(term.to_owned()),
            term_program: None,
            tmux_pane: None,
        };
        assert_eq!(
            outside("xterm-ghostty").graphics(),
            Graphics::Kitty { tmux: false }
        );
        assert_eq!(
            outside("xterm-kitty").graphics(),
            Graphics::Kitty { tmux: false }
        );
        assert_eq!(outside("xterm-256color").graphics(), Graphics::None);
        // WezTerm keeps the usual `TERM` and names itself in `TERM_PROGRAM`.
        let wezterm = Environment {
            term_program: Some("WezTerm".to_owned()),
            ..outside("xterm-256color")
        };
        assert_eq!(wezterm.graphics(), Graphics::Iterm2);
        let inside = Environment {
            tmux_pane: Some("%1".to_owned()),
            ..outside("xterm-kitty")
        };
        assert_eq!(inside.graphics(), Graphics::None);
        let inside = Environment {
            tmux_pane: Some("%1".to_owned()),
            ..wezterm
        };
        assert_eq!(inside.graphics(), Graphics::None);
    }

    /// Under tmux, images show only for one attached kitty or Ghostty client with 24-bit color
    /// and passthrough already on; a WezTerm client, as tmux knows it from its version answer,
    /// shows none.
    #[test]
    fn tmux_shows_images_only_when_everything_says_so() {
        let ghostty = "1\t77\txterm-ghostty\tghostty 1.3.1\t256,RGB,kittygraphics\ton\n";
        assert_eq!(tmux_graphics(ghostty), Graphics::Kitty { tmux: true });
        // kitty, known by its version answer under another `TERM`.
        let kitty = "1\t77\txterm-256color\tkitty(0.39.1)\tRGB\tall\n";
        assert_eq!(tmux_graphics(kitty), Graphics::Kitty { tmux: true });

        for refused in [
            "1\t77\txterm-ghostty\tghostty 1.3.1\tRGB\toff\n",
            "2\t77\txterm-ghostty\tghostty 1.3.1\tRGB\ton\n",
            "1\t77\talacritty\t\tRGB\ton\n",
            "1\t77\txterm-ghostty\tghostty 1.3.1\t256\ton\n",
            "1\t77\txterm-256color\tWezTerm 20261125-182040-7d0eb968\tRGB,sixel\ton\n",
            "",
        ] {
            assert_eq!(tmux_graphics(refused), Graphics::None, "{refused:?}");
        }
    }

    /// The images replaced are dropped with the next frame, and every image as the app quits
    /// or leaves in a hurry, each once and the way its image went.
    #[test]
    fn every_way_out_drops_the_images() {
        let _registry = alone();
        Uploads::hold(Upload { id: 7, tmux: false });
        Uploads::hold(Upload { id: 8, tmux: true });
        assert_eq!(Uploads::drops(false), "");
        Uploads::retire(7);
        assert_eq!(Uploads::drops(false), delete(7, false));
        assert_eq!(Uploads::drops(true), delete(8, true));
        assert_eq!(Uploads::drops(true), "");

        Uploads::hold(Upload { id: 9, tmux: false });
        assert_eq!(Uploads::drops_now(), delete(9, false));
        assert_eq!(Uploads::drops_now(), "");
    }

    /// A command that outlasts its deadline is killed, and says nothing, as is one that ends
    /// but leaves a process behind holding its output, which is killed with it.
    ///
    /// A named pipe orders the steps, so no outcome depends on the clock: reading it ends once
    /// every process holding it is gone.
    /// Every process the test starts ends on its own within ten minutes, out of reach as they
    /// are of the test profile's kill of the test's process group.
    #[test]
    #[cfg(unix)]
    fn a_slow_command_is_given_up() {
        let answer = run_bounded(Command::new("sleep").arg("600"), Duration::from_millis(100));
        assert_eq!(answer, None);

        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held");
        let made = Command::new("mkfifo").arg(&held).status();
        assert!(made.unwrap().success());
        // The command opens `held` to read and write, which waits for no other process, and
        // ends, leaving behind a shell that holds its output and `held`, and writes to it only
        // if it outlives the `sleep`.
        let script = r#"exec 3<>"$1"; { sleep 30; echo survived >&3; } &"#;
        let running =
            Running::start(Command::new("sh").args(["-c", script, "sh"]).arg(&held)).unwrap();
        // The command is waited for without being reaped, as `finish` finds it.
        let command = rustix::process::Pid::from_child(&running.child);
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        let ended = rustix::process::waitid(WaitId::Pid(command), options).unwrap();
        let status = ended.and_then(|ended| ended.exit_status());
        assert_eq!(status, Some(0), "the command could not open `held`");
        // The shell left behind holds `held`, so opening it waits for no one.
        let mut left = std::fs::File::open(&held).unwrap();
        assert_eq!(running.finish(Duration::ZERO), None);
        // The shell left behind is gone without having written.
        let mut written = String::new();
        left.read_to_string(&mut written).unwrap();
        assert_eq!(written, "");

        // With no deadline to miss, a command that ends says what it printed.
        assert_eq!(
            run_bounded(Command::new("echo").arg("on"), Duration::MAX).as_deref(),
            Some("on\n")
        );
    }
}
