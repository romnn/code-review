//! `cr`: the terminal review app without a command, and commands for scripts and agents.

mod app;
mod commands;
mod config;
mod reference;
mod render;

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::eyre;

/// Review large diffs and pull requests hunk by hunk, across pushes and force-pushes.
///
/// Without a command, opens the review in the terminal app; the commands drive the same reviews
/// from scripts and agents.
#[derive(Debug, Parser)]
#[command(name = "cr", version)]
struct Cli {
    #[command(flatten)]
    options: GlobalOptions,
    #[command(flatten)]
    app: AppOptions,
    #[command(subcommand)]
    command: Option<Command>,
}

/// What the terminal app opens, without a command.
#[derive(Debug, Clone, clap::Args)]
pub struct AppOptions {
    /// A pull request number or URL, or a `base..head` range.
    ///
    /// Without one (and without --worktree), reviews this checkout: whatever its working tree
    /// holds, on whatever branch, against the default branch.
    #[arg(conflicts_with_all = ["demo", "demo_revision"])]
    target: Option<String>,
    /// Review the working tree, including uncommitted and untracked files, against a ref.
    #[arg(long, value_name = "REF", num_args = 0..=1, default_missing_value = "HEAD", conflicts_with_all = ["target", "demo", "demo_revision"])]
    worktree: Option<String>,
    #[command(flatten)]
    terminal: review_tui::Terminal,
}

/// Options shared by every command.
#[derive(Debug, Clone, clap::Args)]
pub struct GlobalOptions {
    /// The review to act on; defaults to this checkout's review.
    #[arg(long, global = true)]
    review: Option<String>,
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "repo", global = true, default_value = ".")]
    repo: PathBuf,
    /// Where reviews are stored; defaults to `$XDG_DATA_HOME/code-review`.
    #[arg(long, global = true, env = "CR_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Rebuild the built-in demo pull request from scratch at revision 1 and act on it.
    #[arg(long, global = true)]
    demo: bool,
    /// Act on the built-in demo, recording its revisions up to this one and keeping marks.
    #[arg(long, global = true, value_name = "N", conflicts_with = "demo")]
    demo_revision: Option<u32>,
    /// Where the demo's repository and data live, instead of `target/demo`; QA runs use their
    /// own, so other front ends' demos cannot reset theirs.
    #[arg(long, global = true, env = "CODE_REVIEW_DEMO_DIR", hide = true)]
    demo_dir: Option<PathBuf>,
    /// When to color text output.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
    /// Count proven formatting-only units as needing review, whatever the configuration file
    /// says; otherwise `cr` follows `[review] auto_approve_formatting` there, as the apps do,
    /// and approves them automatically, without a mark, unless it is false.
    #[arg(long, global = true)]
    no_auto_approve: bool,
    /// The configuration file the apps share with `cr`, instead of
    /// `$XDG_CONFIG_HOME/code-review/config.toml`.
    #[arg(long, env = "CODE_REVIEW_CONFIG", hide = true, global = true)]
    config: Option<PathBuf>,
    #[arg(long, env = "XDG_CONFIG_HOME", hide = true, global = true)]
    xdg_config_home: Option<PathBuf>,
    #[arg(long, env = "XDG_DATA_HOME", hide = true, global = true)]
    xdg_data_home: Option<PathBuf>,
    #[arg(long, env = "HOME", hide = true, global = true)]
    home: Option<PathBuf>,
    #[arg(long, env = "NO_COLOR", hide = true, global = true)]
    no_color: Option<String>,
}

/// When to color output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Color when standard output is a terminal and `NO_COLOR` is unset.
    Auto,
    /// Always color.
    Always,
    /// Never color.
    Never,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start or refresh a review; records a revision if anything changed.
    Open {
        /// A pull request number or URL, or a `base..head` range.
        ///
        /// Without one (and without --worktree), reviews this checkout: whatever its working
        /// tree holds, on whatever branch, against the default branch.
        target: Option<String>,
        /// Review the working tree, including uncommitted and untracked files, against a ref.
        #[arg(long, value_name = "REF", num_args = 0..=1, default_missing_value = "HEAD", conflicts_with = "target")]
        worktree: Option<String>,
    },
    /// Show counts per file and in total.
    ///
    /// On this checkout's review, records what the checkout holds first, as `cr show` does.
    Status {
        /// The revision, `3` or `3@<tag>` as `cr show` prints it; defaults to the current (last)
        /// one.
        #[arg(long, value_name = "N[@TAG]")]
        revision: Option<commands::RevisionReference>,
    },
    /// Render the changes, step by step.
    ///
    /// On this checkout's review, records what the checkout holds first, so that the units
    /// shown can be marked.
    /// Unit IDs print qualified with the review's tag, `u1-0000@3fa2b1c0`, and the revision as
    /// `1@3fa2b1c0`.
    Show {
        /// Only units that still need review.
        #[arg(long)]
        unreviewed: bool,
        /// Only this step, numbered from 1.
        #[arg(long)]
        step: Option<usize>,
        /// Only this file.
        #[arg(long)]
        file: Option<String>,
        /// Expand formatting-only, moved and noise units instead of collapsing them.
        #[arg(long)]
        expand: bool,
        /// The revision, `3` or `3@<tag>` as `cr show` prints it; defaults to the current (last)
        /// one.
        #[arg(long, value_name = "N[@TAG]")]
        revision: Option<commands::RevisionReference>,
    },
    /// Set the review state of units, or of the units a plain move covers.
    ///
    /// Every mark names its review: unit and move IDs as `cr show` prints them, with the
    /// review's tag (`u3-0007@3fa2b1c0`), and marks of files and categories with the revision
    /// shown (`--revision 3@3fa2b1c0`).
    /// Bare IDs and revisions are taken only when --review names the review; an ID or revision
    /// of another review is refused.
    Mark {
        /// Unit IDs (`u3-0007@<tag>`) or move IDs (`m3-0001@<tag>`), followed by the verdict:
        /// seen, accepted, flagged or unseen.
        #[arg(required = true, num_args = 1..)]
        args: Vec<String>,
        /// Mark every unit of this category in the current revision instead.
        #[arg(long, value_enum)]
        category: Option<commands::Category>,
        /// Mark every unit of this file, or of every file below this directory, in the current
        /// revision instead; repeatable.
        #[arg(long = "file", value_name = "PATH")]
        files: Vec<String>,
        /// The revision the marks of --category or --file are for, `3@<tag>` as `cr show`
        /// prints it; refused when it is not the current one.
        #[arg(long, value_name = "N[@TAG]")]
        revision: Option<commands::RevisionReference>,
    },
    /// Attach a note to a unit.
    Note {
        /// The unit ID, `u3-0007@<tag>` as `cr show` prints it.
        unit: String,
        /// The note text.
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// List flagged, accepted and annotated units for an agent.
    Export {
        /// The output format.
        #[arg(long, value_enum, default_value_t = ExportFormat::Md)]
        format: ExportFormat,
        /// The revision, `3` or `3@<tag>` as `cr show` prints it; defaults to the current (last)
        /// one.
        #[arg(long, value_name = "N[@TAG]")]
        revision: Option<commands::RevisionReference>,
    },
    /// Agent-written walkthroughs.
    Walkthrough {
        #[command(subcommand)]
        command: WalkthroughCommand,
    },
    /// Drop a review: its revisions, marks, notes and snapshots, also once its checkout is
    /// gone.
    ///
    /// Refused while the review is open, in a window or another command.
    Drop {
        /// The review's key, as `--review` takes it.
        review: String,
    },
    /// Poll for new revisions and print what changed.
    Watch {
        /// Seconds between polls.
        #[arg(long, default_value_t = 30)]
        interval: u64,
        /// Stop after this many polls.
        #[arg(long)]
        count: Option<u64>,
    },
}

/// Export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    /// A numbered Markdown list.
    Md,
    /// JSON.
    Json,
}

#[derive(Debug, Subcommand)]
enum WalkthroughCommand {
    /// Print a prompt with a unit catalog for an agent to answer with a walkthrough.
    Prompt,
    /// Validate and store a walkthrough written for one revision.
    Import {
        /// The walkthrough JSON file, or `-` for standard input.
        file: PathBuf,
    },
    /// Show the walkthrough's steps for the current revision.
    Show {
        /// The revision, `3` or `3@<tag>` as `cr show` prints it; defaults to the current (last)
        /// one.
        #[arg(long, value_name = "N[@TAG]")]
        revision: Option<commands::RevisionReference>,
    },
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return app::run(&cli.options, cli.app);
    };
    app::refuse_with_command(&cli.app)?;
    let context = commands::Context::new(&cli.options)?;
    match command {
        Command::Open { target, worktree } => {
            commands::open(&context, target.as_deref(), worktree.as_deref())
        }
        Command::Status { revision } => commands::status(&context, revision.as_ref()),
        Command::Show {
            unreviewed,
            step,
            file,
            expand,
            revision,
        } => commands::show(
            &context,
            &commands::ShowOptions {
                unreviewed,
                step,
                file,
                expand,
                revision,
            },
        ),
        Command::Mark {
            args,
            category,
            files,
            revision,
        } => commands::mark(
            &context,
            &commands::MarkOptions {
                args: &args,
                category,
                files: &files,
                revision: revision.as_ref(),
            },
        ),
        Command::Note { unit, text } => commands::note(&context, &unit, &text.join(" ")),
        Command::Export { format, revision } => {
            commands::export(&context, format, revision.as_ref())
        }
        Command::Walkthrough { command } => match command {
            WalkthroughCommand::Prompt => commands::walkthrough_prompt(&context),
            WalkthroughCommand::Import { file } => commands::walkthrough_import(&context, &file),
            WalkthroughCommand::Show { revision } => {
                commands::walkthrough_show(&context, revision.as_ref())
            }
        },
        Command::Drop { review } => commands::drop_review(&context, &review),
        Command::Watch { interval, count } => commands::watch(&context, interval, count),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    /// The command line is consistent: no two arguments clash.
    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }
}
