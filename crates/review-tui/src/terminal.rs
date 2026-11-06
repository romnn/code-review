//! The real terminal: taking it over, the main loop, and giving it back as it was, on quit, on
//! a signal and on a panic alike.
//!
//! While the review shows, the terminal is in raw mode on its alternate screen with the mouse,
//! bracketed paste and, where the terminal supports it, the kitty keyboard protocol's
//! disambiguation; the title shows the review, on top of the title it had, which comes back.
//! Every frame is wrapped in synchronized output, so the terminal never shows half of one.
//! Copies go to the clipboard through OSC 52, which also works over ssh.
//!
//! Raw mode keeps the terminal's own keys from sending signals, so the ones that come are from
//! outside: a hang-up, an interrupt or a termination quits once the marks queued and in flight
//! are saved or have failed, since nobody may be there to see a failure, and a second one gives
//! the terminal back and exits at once.
//! A stop from outside, which Ctrl+Z cannot send in raw mode, stops the app as it is; when it
//! continues, it takes the terminal again, which the shell may have changed, and redraws it.

use std::io::{self, Stdout, Write as _};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    SetTitle, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use review_ui::model::{Loaded, QuitKind, Settings};

use crate::app::{App, Message, Outgoing, next_batch};
use crate::color::Colors;
use crate::log::Log;

/// Whether raw mode is on, so the terminal's own mode comes back.
static RAW: AtomicBool = AtomicBool::new(false);

/// Whether the alternate screen, the mouse, bracketed paste and the hidden cursor are on.
static SCREEN: AtomicBool = AtomicBool::new(false);

/// Whether the kitty keyboard protocol's flags were pushed, so they are popped again.
static ENHANCED: AtomicBool = AtomicBool::new(false);

/// Whether the terminal's title was pushed on its stack, so it is popped again.
static TITLE: AtomicBool = AtomicBool::new(false);

/// Whether the terminal supports the kitty keyboard protocol, asked once: the question reads
/// the terminal's answer, which only the first take-over can, before the input thread reads.
static KEYBOARD: OnceLock<bool> = OnceLock::new();

/// xterm's control sequences that save the window title on its stack and restore it.
const PUSH_TITLE: &str = "\x1b[22;0t";
const POP_TITLE: &str = "\x1b[23;0t";

/// Takes the terminal over: raw mode, the alternate screen, the mouse, bracketed paste, the
/// keyboard protocol where supported, and a saved title; each step is marked before it runs,
/// so [`give_back`] undoes whatever a failure left behind.
///
/// It takes the terminal again when the app continues after a stop, setting raw mode and the
/// screen's modes anew, since the shell may have put its own modes back meanwhile.
fn take_over(stdout: &mut Stdout) -> io::Result<()> {
    // crossterm counts raw mode as on even when a shell turned it off during a stop.
    if RAW.swap(true, Ordering::SeqCst) {
        disable_raw_mode()?;
    }
    enable_raw_mode()?;
    let enhanced = *KEYBOARD
        .get_or_init(|| crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false));
    SCREEN.store(true, Ordering::SeqCst);
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        cursor::Hide
    )?;
    if enhanced && !ENHANCED.swap(true, Ordering::SeqCst) {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    if !TITLE.swap(true, Ordering::SeqCst) {
        write!(stdout, "{PUSH_TITLE}")?;
    }
    stdout.flush()
}

/// Gives the terminal back as it was, once, however far taking it over got; every step runs
/// even when an earlier one fails, and the first failure is returned.
pub fn give_back() -> io::Result<()> {
    let mut stdout = io::stdout();
    let mut first: io::Result<()> = Ok(());
    let mut step = |result: io::Result<()>| {
        if first.is_ok() {
            first = result;
        }
    };
    let screen = SCREEN.swap(false, Ordering::SeqCst);
    if screen {
        // A frame cut short, by a panic or a second signal, leaves synchronized output on.
        step(execute!(stdout, EndSynchronizedUpdate));
    }
    if ENHANCED.swap(false, Ordering::SeqCst) {
        step(execute!(stdout, PopKeyboardEnhancementFlags));
    }
    if screen {
        step(execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            cursor::Show
        ));
    }
    if TITLE.swap(false, Ordering::SeqCst) {
        step(write!(stdout, "{POP_TITLE}").and_then(|()| stdout.flush()));
    }
    if RAW.swap(false, Ordering::SeqCst) {
        step(disable_raw_mode());
    }
    first
}

/// Installs a panic hook that gives the terminal back and prints the kept diagnostics before
/// the panic's report, then ends the process: a job thread's panic must not leave the review
/// running on a terminal no longer set up for it.
pub fn install_panic_hook(log: Log) {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The terminal is going away with the process; a failure here cannot be reported
        // anywhere better than the panic's own report below.
        give_back().ok();
        log.replay();
        report(info);
        std::process::exit(101);
    }));
}

/// The terminal's text for the window title `title`: the review's title without control
/// characters, which could end the title's sequence and act on the terminal.
fn clean_title(title: &str) -> String {
    title
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

/// Does what the model asked of the terminal's output.
fn send(stdout: &mut impl io::Write, outgoing: Vec<Outgoing>) -> io::Result<()> {
    for item in outgoing {
        match item {
            Outgoing::Copy(text) => queue!(stdout, CopyToClipboard::to_clipboard_from(text))?,
            Outgoing::CopyPrimary(text) => {
                queue!(stdout, CopyToClipboard::to_primary_from(text))?;
            }
            Outgoing::Title(title) => queue!(stdout, SetTitle(clean_title(&title)))?,
        }
    }
    stdout.flush()
}

/// What the terminal session ends with besides the review: a panic on purpose after the first
/// frame, to check that the terminal comes back.
#[derive(Debug, Clone, Copy, Default)]
pub struct Checks {
    /// Panics once the first frame is drawn.
    pub panic_after_first_frame: bool,
}

/// Shows the opened review in the terminal until the reviewer quits.
///
/// # Errors
///
/// Returns an error when the terminal cannot be taken over or drawn to; the terminal is given
/// back either way, and a terminal that fails to draw ends the session only once the marks
/// queued and in flight are saved or have failed.
pub fn run(
    settings: Settings,
    loaded: Loaded,
    colors: Colors,
    log: &Log,
    checks: Checks,
) -> io::Result<()> {
    let mut stdout = io::stdout();
    if let Err(err) = take_over(&mut stdout) {
        // The failure to take it over is the one to report.
        give_back().ok();
        return Err(err);
    }
    let shown = show(settings, loaded, colors, log, checks);
    let given_back = give_back();
    shown.and(given_back)
}

/// The main loop: terminal input and signals on threads of their own, job completions and
/// timers, and one frame after each batch.
fn show(
    settings: Settings,
    loaded: Loaded,
    colors: Colors,
    log: &Log,
    checks: Checks,
) -> io::Result<()> {
    let (sender, receiver) = mpsc::channel();
    let input = sender.clone();
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(event) => {
                    if input.send(Message::Input(event)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    input.send(Message::InputEnded(err.to_string())).ok();
                    break;
                }
            }
        }
    });
    #[cfg(unix)]
    let signals = signals::watch(sender.clone(), log.clone())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let size = terminal.size()?;
    let mut app = App::new(
        settings,
        loaded,
        Rect::new(0, 0, size.width, size.height),
        sender,
        colors,
        log.clone(),
    );
    // The first failure to reach the terminal; once one fails, it gets no more frames.
    let mut lost: Option<io::Error> = None;
    let mut drawn = false;
    loop {
        if lost.is_none() {
            let outgoing = app.take_outgoing();
            let reached =
                send(terminal.backend_mut(), outgoing).and_then(|()| draw(&mut terminal, &mut app));
            if let Err(err) = reached {
                // The marks are saved before the session ends, which nobody sees any more.
                log.write(&format!("the terminal cannot be drawn to: {err}"));
                lost = Some(err);
                app.update(|model| model.quit(QuitKind::Unattended));
            }
        }
        if app.quitting() {
            break;
        }
        if checks.panic_after_first_frame && !drawn {
            panic_on_purpose();
        }
        drawn = true;
        let Some(batch) = next_batch(&receiver, app.wake_at(Instant::now())) else {
            break;
        };
        for message in batch {
            match message {
                #[cfg(unix)]
                Message::Signal(signal) => {
                    let drawing = lost.is_none();
                    signals::handle(signal, &mut app, &mut terminal, drawing, log);
                }
                other => app.handle(other),
            }
        }
        app.tick(Instant::now());
    }
    #[cfg(unix)]
    signals.stop();
    app.shut_down();
    lost.map_or(Ok(()), Err)
}

/// Draws one frame inside synchronized output.
fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let rendered = app.render(terminal);
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    rendered
}

/// Signals from outside the terminal, which only a Unix terminal has.
#[cfg(unix)]
mod signals {
    use std::io::{self, Stdout};
    use std::sync::mpsc::{self, Sender};
    use std::time::Duration;

    use crossterm::execute;
    use crossterm::terminal::{Clear, ClearType};
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use review_ui::model::QuitKind;
    use signal_hook::consts::signal::{SIGCONT, SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::{Handle, Signals};
    use signal_hook::low_level::signal_name;

    use super::{give_back, take_over};
    use crate::app::{App, Message, Signal};
    use crate::log::Log;

    /// How long a second signal waits for the terminal to be given back before the app exits
    /// anyway.
    const EXIT_DEADLINE: Duration = Duration::from_millis(500);

    /// Signals watched on a thread of their own.
    pub struct Watch(Handle);

    impl Watch {
        /// Stops watching the signals, ending the watching thread.
        pub fn stop(self) {
            let Self(handle) = self;
            handle.close();
        }
    }

    /// Sends the signals the app acts on to the main loop from a thread of its own, until
    /// [`Watch::stop`]: the first hang-up, interrupt or termination asks the app to quit, and a
    /// second gives the terminal back and exits at once, whatever the app still waits for.
    ///
    /// A stop is left to its default action, which stops the app as it is, so only its
    /// continuing comes here.
    pub fn watch(sender: Sender<Message>, log: Log) -> io::Result<Watch> {
        let mut signals = Signals::new([SIGHUP, SIGINT, SIGTERM, SIGCONT])?;
        let handle = signals.handle();
        std::thread::spawn(move || {
            let mut ending = false;
            for signal in signals.forever() {
                let message = match signal {
                    SIGCONT => Signal::Continue,
                    _ if ending => exit_now(signal, &log),
                    _ => {
                        ending = true;
                        Signal::End(signal_name(signal).unwrap_or("a signal"))
                    }
                };
                if sender.send(Message::Signal(message)).is_err() {
                    break;
                }
            }
        });
        Ok(Watch(handle))
    }

    /// Gives the terminal back and replays the log on a thread of its own, then exits with
    /// `signal`'s status once they are done or [`EXIT_DEADLINE`] passed, whichever comes first.
    ///
    /// A terminal that stopped reading blocks every write to it, and the main loop may hold
    /// standard output while it waits; neither may keep the process from exiting.
    fn exit_now(signal: i32, log: &Log) -> ! {
        let (done, finished) = mpsc::channel();
        let log = log.clone();
        std::thread::spawn(move || {
            // A failure cannot be reported anywhere better than the exit itself.
            give_back().ok();
            log.replay();
            // Past the deadline nobody waits for this any more.
            done.send(()).ok();
        });
        // Either way the process exits now: the work is done, or it is stuck.
        finished.recv_timeout(EXIT_DEADLINE).ok();
        // `std::process::exit` would flush standard output, which can block as well.
        signal_hook::low_level::exit(128 + signal)
    }

    /// Does what `signal` asks of the app on `terminal`, while `drawing` says the terminal can
    /// still be drawn to: quits, which nobody may be there to see, or takes the terminal again
    /// after a stop.
    pub fn handle(
        signal: Signal,
        app: &mut App,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        drawing: bool,
        log: &Log,
    ) {
        match signal {
            Signal::End(name) => {
                log.write(&format!("{name} received; quitting"));
                app.update(|model| model.quit(QuitKind::Unattended));
            }
            // The shell may have reset the terminal's modes and drawn over the screen while the
            // app was stopped.
            Signal::Continue if drawing => {
                if let Err(err) = take_over(&mut io::stdout()).and_then(|()| redraw_whole(terminal))
                {
                    log.write(&format!("cannot take the terminal again: {err}"));
                }
            }
            // A terminal that cannot be drawn to is not taken again.
            Signal::Continue => {}
        }
    }

    /// Has the next frame draw every cell, after the app was stopped: clears the screen and
    /// forgets what was drawn.
    ///
    /// A new [`Terminal`] forgets it; ratatui's own `clear` would ask the terminal where its
    /// cursor is, an answer the input thread reads first.
    fn redraw_whole(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
        *terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        execute!(terminal.backend_mut(), Clear(ClearType::All))
    }
}

/// Panics, to check that the terminal comes back after a panic.
#[expect(
    clippy::panic,
    reason = "a panic on purpose, which the hidden check flag asks for"
)]
fn panic_on_purpose() -> ! {
    panic!("a panic on purpose after the first frame");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A title from the review cannot carry control sequences to the terminal.
    #[test]
    fn titles_lose_control_characters() {
        assert_eq!(
            clean_title("#12 Fix\x07 it\x1b]0;evil\x1b\\"),
            "#12 Fix it]0;evil\\"
        );
        assert_eq!(clean_title("Review of x\u{9c}y"), "Review of xy");
    }

    /// The model's copies reach the terminal as OSC 52 sequences, to the clipboard or the
    /// primary selection.
    #[test]
    fn copies_go_out_as_osc_52() {
        let mut out = Vec::new();
        send(
            &mut out,
            vec![
                Outgoing::Copy("unit".to_owned()),
                Outgoing::CopyPrimary("x".to_owned()),
            ],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b]52;c;dW5pdA=="), "{text:?}");
        assert!(text.contains("\x1b]52;p;eA=="), "{text:?}");
    }
}
