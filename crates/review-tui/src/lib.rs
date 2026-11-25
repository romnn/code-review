//! Terminal review app: review-ui's model drawn with ratatui, working as review-gui does.
//!
//! Takes over the terminal and draws its first frame at once, while the review (a pull request,
//! a `base..head` range, the working tree, or the built-in demo) opens behind it; then shows the
//! changed files and the selected file's diff with the same keys, marks, folds, themes, diff
//! styles and configuration as review-gui.
//! Both share the review's saved view state, such as its folded folders.
//!
//! Nothing run for the review, the fetch that opens it included, can prompt, which would draw
//! on the terminal the review is drawn on.
//!
//! [`run`] is the whole app; `cr` without a command runs it, and so does the `review-tui` binary,
//! which the QA scripts drive.

mod app;
mod clipboard;
mod color;
mod draw;
mod graphics;
mod images;
mod keys;
mod log;
mod measure;
mod qa;
mod terminal;
#[cfg(feature = "trace")]
mod trace;

use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre;
use review_ui::front_end::FrontEnd;
pub use review_ui::launch;
use review_ui::model::{Opener, Phase};

/// Review a pull request, a range or the working tree in the terminal.
#[derive(Debug, Clone, Parser)]
#[command(name = "review-tui", version)]
pub struct Options {
    /// The review to open.
    #[command(flatten)]
    pub review: launch::Options,
    /// What the terminal is.
    #[command(flatten)]
    pub terminal: Terminal,
    /// Read QA commands from standard input and answer each on one line of standard output,
    /// drawing off screen; see `scripts/qa/run.sh`.
    #[arg(long)]
    pub qa: bool,
    /// Panic once the first frame is drawn, to check that the terminal comes back.
    #[arg(long, hide = true)]
    pub panic_after_first_frame: bool,
    /// Write a Perfetto trace of where the app spends its time to this file, for profiling.
    ///
    /// Only a build with the `trace` feature takes it.
    #[cfg_attr(feature = "trace", arg(long, value_name = "FILE"))]
    #[cfg_attr(not(feature = "trace"), arg(skip))]
    pub trace_output: Option<PathBuf>,
}

/// What the terminal is, as the environment says.
#[derive(Debug, Clone, clap::Args)]
pub struct Terminal {
    /// Whether the terminal shows 24-bit color; anything but `truecolor` or `24bit` gets
    /// xterm's 256 colors.
    #[arg(long, env = "COLORTERM", hide = true)]
    pub colorterm: Option<String>,
    /// The base of the log file's directory, `$XDG_STATE_HOME`.
    #[arg(long, env = "XDG_STATE_HOME", hide = true)]
    pub xdg_state_home: Option<PathBuf>,
    /// The terminal's name, which says whether it shows images outside tmux.
    #[arg(long, env = "TERM", hide = true)]
    pub term: Option<String>,
    /// The program the terminal is, which WezTerm says here rather than in `TERM`.
    #[arg(long, env = "TERM_PROGRAM", hide = true)]
    pub term_program: Option<String>,
    /// The tmux pane the app runs in, inside tmux, whose client says whether it shows images.
    #[arg(long, env = "TMUX_PANE", hide = true)]
    pub tmux_pane: Option<String>,
}

/// Shows the review `options` name on the terminal until the reviewer quits, opening it behind
/// the first frame.
///
/// A review that cannot be opened shows why until the reviewer quits.
///
/// # Errors
///
/// Returns an error once the terminal is back when the review could not be opened, which the
/// diagnostics printed before it explain; quitting before it opened is no failure.
/// Returns an error as well when the configuration has no theme or diff style at all, or the
/// terminal cannot be used.
pub fn run(options: &Options) -> eyre::Result<()> {
    #[cfg(feature = "trace")]
    let recording = options
        .trace_output
        .as_deref()
        .map(trace::record)
        .transpose()?;
    let shown = show(options);
    #[cfg(feature = "trace")]
    if let Some(recording) = recording {
        recording.finish()?;
    }
    shown
}

/// [`run`] apart from its trace.
#[tracing::instrument(skip_all)]
fn show(options: &Options) -> eyre::Result<()> {
    let settings = tracing::info_span!("settings")
        .in_scope(|| launch::settings(launch::load_config(&options.review), FrontEnd::Terminal))?;
    for problem in &settings.problems {
        eprintln!("cr: {problem}");
    }
    let log_file = if options.qa {
        None
    } else {
        log::path(
            options.terminal.xdg_state_home.as_deref(),
            options.review.home.as_deref(),
        )
        .map(log::LogFile::start)
    };
    let log = if options.qa {
        log::Log::direct()
    } else {
        log::Log::held(log_file.as_ref())
    };
    terminal::install_panic_hook(log.clone());

    let opener = opener(options, log.clone());
    if options.qa {
        return qa::run(settings, opener);
    }
    let colors = color::Colors::from_colorterm(options.terminal.colorterm.as_deref());
    let environment = graphics::Environment {
        term: options
            .terminal
            .term
            .clone()
            .filter(|term| !term.is_empty()),
        term_program: options
            .terminal
            .term_program
            .clone()
            .filter(|program| !program.is_empty()),
        tmux_pane: options
            .terminal
            .tmux_pane
            .clone()
            .filter(|pane| !pane.is_empty()),
    };
    let shown = terminal::run(
        settings,
        opener,
        colors,
        &log,
        terminal::Checks {
            panic_after_first_frame: options.panic_after_first_frame,
        },
        &environment,
    );
    // The process ends soon after, which would cut the log file's last writes short.
    if let Some(log_file) = &log_file {
        log_file.flush();
    }
    log.replay();
    // The diagnostics just printed say why.
    if let Phase::Failed(_) = shown? {
        eyre::bail!("the review could not be opened");
    }
    Ok(())
}

/// The model's opener of the review `options` name, which tells `log` how it found the review
/// and runs no command for it that could prompt on the terminal.
fn opener(options: &Options, log: log::Log) -> Opener {
    let review = options.review.clone();
    Box::new(move || launch::open(&review, &|found| log.write(found), FrontEnd::Terminal))
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    /// The command line is consistent: no two arguments clash.
    #[test]
    fn the_command_line_is_consistent() {
        Options::command().debug_assert();
    }
}
