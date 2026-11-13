//! Terminal review app: review-ui's model drawn with ratatui, working as review-gui does.
//!
//! Opens a review (a pull request, a `base..head` range, the working tree, or the built-in demo)
//! before it takes over the terminal, so a fetch can still ask for credentials, then shows the
//! changed files and the selected file's diff with the same keys, marks, folds, themes, diff
//! styles and configuration as review-gui.
//! Both share the review's saved view state, such as its folded folders.
//!
//! Fetches started later, by `R`, run without git's and OpenSSH's prompts, which would draw on
//! the terminal the review is drawn on.
//!
//! [`run`] is the whole app; `cr` without a command runs it, and so does the `review-tui` binary,
//! which the QA scripts drive.

mod app;
mod color;
mod draw;
mod keys;
mod log;
mod measure;
mod qa;
mod terminal;

use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre;
use review_ui::front_end::FrontEnd;
pub use review_ui::launch;

/// Review a pull request, a range or the working tree in the terminal.
#[derive(Debug, Clone, Parser)]
#[command(name = "review-tui", version)]
pub struct Options {
    /// The review to open.
    #[command(flatten)]
    pub review: launch::Options,
    /// What the terminal and git's ssh are.
    #[command(flatten)]
    pub terminal: Terminal,
    /// Read QA commands from standard input and answer each on one line of standard output,
    /// drawing off screen; see `scripts/qa/run.sh`.
    #[arg(long)]
    pub qa: bool,
    /// Panic once the first frame is drawn, to check that the terminal comes back.
    #[arg(long, hide = true)]
    pub panic_after_first_frame: bool,
}

/// What the terminal and git's ssh are, as the environment says.
#[derive(Debug, Clone, clap::Args)]
pub struct Terminal {
    /// Whether the terminal shows 24-bit color; anything but `truecolor` or `24bit` gets
    /// xterm's 256 colors.
    #[arg(long, env = "COLORTERM", hide = true)]
    pub colorterm: Option<String>,
    /// The base of the log file's directory, `$XDG_STATE_HOME`.
    #[arg(long, env = "XDG_STATE_HOME", hide = true)]
    pub xdg_state_home: Option<PathBuf>,
    /// The ssh command git uses, which fetches started in the app run in batch mode.
    #[arg(long, env = "GIT_SSH_COMMAND", hide = true)]
    pub git_ssh_command: Option<String>,
    /// The ssh program git uses when no ssh command is set.
    #[arg(long, env = "GIT_SSH", hide = true)]
    pub git_ssh: Option<String>,
    /// Which ssh git's ssh program is, over `ssh.variant` and git's guess from its name.
    #[arg(long, env = "GIT_SSH_VARIANT", hide = true)]
    pub git_ssh_variant: Option<String>,
}

/// The ssh git runs for fetches, as the environment and the repository's configuration set it.
#[derive(Debug, Clone, Default)]
struct Ssh {
    /// `$GIT_SSH_COMMAND`, a command line for the shell.
    command: Option<String>,
    /// `core.sshCommand`, a command line for the shell.
    configured: Option<String>,
    /// `$GIT_SSH`, a program run without a shell.
    program: Option<String>,
    /// Which ssh it is: `$GIT_SSH_VARIANT`, else `ssh.variant`.
    variant: Option<String>,
}

/// The environment that keeps a fetch from prompting on the terminal: no credential prompt
/// from git, no ssh askpass helper, and OpenSSH in batch mode.
fn non_interactive(ssh: &Ssh) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("GIT_TERMINAL_PROMPT", "0".to_owned()),
        ("SSH_ASKPASS_REQUIRE", "never".to_owned()),
    ];
    if let Some(command) = batch_ssh_command(ssh) {
        env.push(("GIT_SSH_COMMAND", command));
    }
    env
}

/// The ssh command line git would run, in batch mode, when git takes it for OpenSSH; `None`
/// leaves git's ssh as it is, as for plink, which takes no `-o`.
///
/// git picks its ssh as `$GIT_SSH_COMMAND`, else `core.sshCommand`, else the program
/// `$GIT_SSH`, else `ssh`, and tells which ssh it is as `connect.c` does: by
/// `$GIT_SSH_VARIANT` or `ssh.variant`, else by the program's name.
/// A program of another name is one git probes with `-G` before each connection; it is left
/// as it is too.
fn batch_ssh_command(ssh: &Ssh) -> Option<String> {
    let set = |value: &Option<String>| {
        value
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
    };
    let (command, program) = match (
        set(&ssh.command).or_else(|| set(&ssh.configured)),
        set(&ssh.program),
    ) {
        (Some(command), _) => {
            let program = first_word(&command);
            (command, program)
        }
        (None, Some(program)) => (shell_quoted(&program), program),
        (None, None) => ("ssh".to_owned(), "ssh".to_owned()),
    };
    is_openssh(ssh.variant.as_deref(), &program).then(|| format!("{command} -o BatchMode=yes"))
}

/// Whether git takes the ssh program `program` for OpenSSH, given the variant it was told.
fn is_openssh(variant: Option<&str>, program: &str) -> bool {
    match variant {
        Some("plink" | "putty" | "tortoiseplink" | "simple") => false,
        // git takes any other variant, `ssh` among them, for OpenSSH.
        Some(variant) if variant != "auto" => true,
        _ => {
            let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
            name.eq_ignore_ascii_case("ssh") || name.eq_ignore_ascii_case("ssh.exe")
        }
    }
}

/// The first word of the shell command line `command`, its quotes removed.
fn first_word(command: &str) -> String {
    let command = command.trim_start();
    match command.chars().next() {
        Some(quote @ ('\'' | '"')) => command
            .get(1..)
            .and_then(|rest| rest.split(quote).next())
            .unwrap_or_default()
            .to_owned(),
        _ => command
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

/// `word` quoted for the shell, as one word.
fn shell_quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Opens the review `options` name, then shows it on the terminal until the reviewer quits.
///
/// # Errors
///
/// Returns an error when the review cannot be opened or the terminal cannot be used.
pub fn run(options: &Options) -> eyre::Result<()> {
    let settings = launch::settings(launch::load_config(&options.review), FrontEnd::Terminal)?;
    for problem in &settings.problems {
        eprintln!("cr: {problem}");
    }
    let log = if options.qa {
        log::Log::direct()
    } else {
        log::Log::held(log::path(
            options.terminal.xdg_state_home.as_deref(),
            options.review.home.as_deref(),
        ))
    };
    terminal::install_panic_hook(log.clone());

    // Opening, and any fetch it needs, happens before the terminal is taken over, so git and
    // ssh may still ask on it.
    eprintln!("cr: opening the review…");
    let mut loaded = launch::open(&options.review, &|found| eprintln!("cr: {found}"))
        .map_err(|err| eyre::eyre!(err))?;
    // A configuration git cannot read counts as unset.
    let configured = |key: &str| loaded.review.repo().config_value(key).ok().flatten();
    let ssh = Ssh {
        command: options.terminal.git_ssh_command.clone(),
        configured: configured("core.sshCommand"),
        program: options.terminal.git_ssh.clone(),
        variant: options
            .terminal
            .git_ssh_variant
            .clone()
            .or_else(|| configured("ssh.variant")),
    };
    loaded.review.set_command_env(non_interactive(&ssh));

    if options.qa {
        return qa::run(settings, loaded);
    }
    let colors = color::Colors::from_colorterm(options.terminal.colorterm.as_deref());
    let shown = terminal::run(
        settings,
        loaded,
        colors,
        &log,
        terminal::Checks {
            panic_after_first_frame: options.panic_after_first_frame,
        },
    );
    log.replay();
    Ok(shown?)
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

    /// The batch-mode ssh command for `ssh`, if any.
    fn batch(ssh: &Ssh) -> Option<String> {
        non_interactive(ssh)
            .into_iter()
            .find(|(name, _)| *name == "GIT_SSH_COMMAND")
            .map(|(_, command)| command)
    }

    /// Fetches in the app never prompt: git's own prompt is off, ssh has no askpass helper,
    /// and plain ssh runs in batch mode.
    #[test]
    fn fetches_never_prompt() {
        let env = non_interactive(&Ssh::default());
        assert!(env.contains(&("GIT_TERMINAL_PROMPT", "0".to_owned())));
        assert!(env.contains(&("SSH_ASKPASS_REQUIRE", "never".to_owned())));
        assert_eq!(
            batch(&Ssh::default()).as_deref(),
            Some("ssh -o BatchMode=yes")
        );
    }

    /// The ssh in batch mode is the one git would run: `$GIT_SSH_COMMAND` over
    /// `core.sshCommand` over the program `$GIT_SSH`, quoted for the shell.
    #[test]
    fn batch_mode_follows_gits_choice_of_ssh() {
        let ssh = |command: Option<&str>, configured: Option<&str>, program: Option<&str>| Ssh {
            command: command.map(str::to_owned),
            configured: configured.map(str::to_owned),
            program: program.map(str::to_owned),
            variant: None,
        };
        assert_eq!(
            batch(&ssh(
                Some("ssh -i ~/.ssh/review"),
                Some("ssh -v"),
                Some("/bin/ssh")
            ))
            .as_deref(),
            Some("ssh -i ~/.ssh/review -o BatchMode=yes")
        );
        assert_eq!(
            batch(&ssh(None, Some("/usr/bin/ssh -F cfg"), Some("/bin/ssh"))).as_deref(),
            Some("/usr/bin/ssh -F cfg -o BatchMode=yes")
        );
        assert_eq!(
            batch(&ssh(Some("  "), None, Some("/opt/it's/ssh"))).as_deref(),
            Some("'/opt/it'\\''s/ssh' -o BatchMode=yes")
        );
    }

    /// Only OpenSSH gets `-o BatchMode=yes`: plink takes no `-o`, and a program git
    /// cannot name is left as it is; the variant git is told wins over the name.
    #[test]
    fn batch_mode_is_only_for_openssh() {
        let program = |program: &str, variant: Option<&str>| Ssh {
            program: Some(program.to_owned()),
            variant: variant.map(str::to_owned),
            ..Ssh::default()
        };
        assert_eq!(batch(&program("C:\\PuTTY\\plink.exe", None)), None);
        assert_eq!(batch(&program("/usr/bin/tortoiseplink", None)), None);
        assert_eq!(batch(&program("/usr/local/bin/my-ssh", None)), None);
        assert!(batch(&program("C:\\Git\\usr\\bin\\SSH.EXE", None)).is_some());
        assert_eq!(batch(&program("/usr/bin/ssh", Some("putty"))), None);
        assert_eq!(batch(&program("/usr/bin/ssh", Some("simple"))), None);
        assert!(batch(&program("/usr/local/bin/my-ssh", Some("ssh"))).is_some());
        assert!(batch(&program("/usr/bin/ssh", Some("auto"))).is_some());
        let quoted = Ssh {
            command: Some("\"/c/Program Files/PuTTY/plink.exe\" -batch".to_owned()),
            ..Ssh::default()
        };
        assert_eq!(batch(&quoted), None);
    }
}
