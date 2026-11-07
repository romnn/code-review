//! The `cr` command-line interface to the review engine.

mod commands;
mod config;
mod render;

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::eyre;

/// Review large diffs and pull requests hunk by hunk, across pushes and force-pushes.
#[derive(Debug, Parser)]
#[command(name = "cr", version)]
struct Cli {
    #[command(flatten)]
    options: GlobalOptions,
    #[command(subcommand)]
    command: Command,
}

/// Options shared by every command.
#[derive(Debug, Clone, clap::Args)]
pub struct GlobalOptions {
    /// The review to act on; defaults to the review last opened for this repository and branch.
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
        /// Without one (and without --worktree), reopens the review last opened on this branch,
        /// or else reviews the branch's open pull request, or else `<default branch>..<branch>`.
        target: Option<String>,
        /// Review the working tree, including uncommitted and untracked files, against a ref.
        #[arg(long, value_name = "REF", num_args = 0..=1, default_missing_value = "HEAD", conflicts_with = "target")]
        worktree: Option<String>,
    },
    /// Show counts per file and in total.
    Status {
        /// The revision; defaults to the current (last) one.
        #[arg(long)]
        revision: Option<u32>,
    },
    /// Render the changes, step by step.
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
        /// The revision; defaults to the current (last) one.
        #[arg(long)]
        revision: Option<u32>,
    },
    /// Set the review state of units, or of the units a plain move covers.
    Mark {
        /// Unit IDs (`u3-0007`) or move IDs (`m3-0001`), followed by the verdict: seen, accepted,
        /// flagged or unseen.
        #[arg(required = true, num_args = 1..)]
        args: Vec<String>,
        /// Mark every unit of this category in the current revision instead.
        #[arg(long, value_enum)]
        category: Option<commands::Category>,
        /// Mark every unit of this file, or of every file below this directory, in the current
        /// revision instead; repeatable.
        #[arg(long = "file", value_name = "PATH")]
        files: Vec<String>,
    },
    /// Attach a note to a unit.
    Note {
        /// The unit ID.
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
        /// The revision; defaults to the current (last) one.
        #[arg(long)]
        revision: Option<u32>,
    },
    /// Agent-written walkthroughs.
    Walkthrough {
        #[command(subcommand)]
        command: WalkthroughCommand,
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
    /// Print the path of a detached worktree at a revision's head, for browsing.
    Worktree {
        /// The revision; defaults to the current (last) one.
        #[arg(long)]
        revision: Option<u32>,
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
        /// The revision; defaults to the current (last) one.
        #[arg(long)]
        revision: Option<u32>,
    },
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let context = commands::Context::new(&cli.options)?;
    match cli.command {
        Command::Open { target, worktree } => {
            commands::open(&context, target.as_deref(), worktree.as_deref())
        }
        Command::Status { revision } => commands::status(&context, revision),
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
        } => commands::mark(&context, &args, category, &files),
        Command::Note { unit, text } => commands::note(&context, &unit, &text.join(" ")),
        Command::Export { format, revision } => commands::export(&context, format, revision),
        Command::Walkthrough { command } => match command {
            WalkthroughCommand::Prompt => commands::walkthrough_prompt(&context),
            WalkthroughCommand::Import { file } => commands::walkthrough_import(&context, &file),
            WalkthroughCommand::Show { revision } => commands::walkthrough_show(&context, revision),
        },
        Command::Watch { interval, count } => commands::watch(&context, interval, count),
        Command::Worktree { revision } => commands::worktree(&context, revision),
    }
}
