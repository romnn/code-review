//! `cr` without a command: the terminal review app, review-tui, run in this process.

use color_eyre::eyre;
use review_tui::launch;

use crate::{AppOptions, GlobalOptions};

/// Opens the review `app` names in the terminal app and shows it until the reviewer quits.
///
/// # Errors
///
/// Returns an error for an option only commands take, or when the app fails.
pub fn run(options: &GlobalOptions, app: AppOptions) -> eyre::Result<()> {
    review_tui::run(&app_options(options, app)?)
}

/// Refuses a target or `--worktree` given together with a command, which opens no app.
///
/// # Errors
///
/// Returns an error when `app` names a review.
pub fn refuse_with_command(app: &AppOptions) -> eyre::Result<()> {
    if app.target.is_some() || app.worktree.is_some() {
        eyre::bail!(
            "a target or --worktree opens the terminal app and takes no command; \
             `cr open` takes them too"
        );
    }
    Ok(())
}

/// The app's options for `cr`'s, refusing the options only commands take.
fn app_options(options: &GlobalOptions, app: AppOptions) -> eyre::Result<review_tui::Options> {
    if options.review.is_some() {
        eyre::bail!(
            "--review names the review of a command; without one, give a target or --worktree"
        );
    }
    if options.json {
        eyre::bail!("--json needs a command, such as `cr show`");
    }
    if options.no_auto_approve {
        eyre::bail!(
            "--no-auto-approve needs a command; the app follows `[review] auto_approve_formatting`"
        );
    }
    Ok(review_tui::Options {
        review: launch::Options {
            target: app.target,
            worktree: app.worktree,
            repo: options.repo.clone(),
            data_dir: options.data_dir.clone(),
            demo: options.demo,
            demo_dir: options.demo_dir.clone(),
            demo_revision: options.demo_revision,
            xdg_data_home: options.xdg_data_home.clone(),
            config: options.config.clone(),
            xdg_config_home: options.xdg_config_home.clone(),
            home: options.home.clone(),
        },
        terminal: app.terminal,
        qa: false,
        panic_after_first_frame: false,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser as _;

    use super::*;
    use crate::Cli;

    /// `cr` parsed from `args`.
    fn parse(args: &[&str]) -> eyre::Result<Cli> {
        Ok(Cli::try_parse_from(
            std::iter::once("cr").chain(args.iter().copied()),
        )?)
    }

    /// The app's options for `cr` run with `args`, which must name no command.
    fn opened(args: &[&str]) -> eyre::Result<review_tui::Options> {
        let cli = parse(args)?;
        eyre::ensure!(cli.command.is_none(), "{args:?} parsed as a command");
        app_options(&cli.options, cli.app)
    }

    /// The target, --worktree and every option the app shares with `cr` reach the app as given,
    /// values that look like options included.
    #[test]
    fn the_app_gets_what_cr_was_given() -> eyre::Result<()> {
        let review = opened(&["128"])?.review;
        assert_eq!(review.target.as_deref(), Some("128"));
        assert_eq!(review.worktree, None);

        let review = opened(&[
            "--repo=-checkout",
            "--worktree",
            "--data-dir=-data",
            "--config=-config.toml",
            "--xdg-data-home",
            "/isolated/data",
            "--xdg-config-home",
            "/isolated/config",
            "--home",
            "/isolated/home",
        ])?
        .review;
        assert_eq!(review.repo, Path::new("-checkout"));
        assert_eq!(review.worktree.as_deref(), Some("HEAD"));
        assert_eq!(review.data_dir.as_deref(), Some(Path::new("-data")));
        assert_eq!(review.config.as_deref(), Some(Path::new("-config.toml")));
        assert_eq!(
            review.xdg_data_home.as_deref(),
            Some(Path::new("/isolated/data"))
        );
        assert_eq!(
            review.xdg_config_home.as_deref(),
            Some(Path::new("/isolated/config"))
        );
        assert_eq!(review.home.as_deref(), Some(Path::new("/isolated/home")));

        assert_eq!(
            opened(&["--worktree", "main"])?.review.worktree.as_deref(),
            Some("main")
        );
        let review = opened(&["--demo-revision", "2", "--demo-dir", "/isolated/demo"])?.review;
        assert_eq!(review.demo_revision, Some(2));
        assert_eq!(review.demo_dir.as_deref(), Some(Path::new("/isolated/demo")));
        assert!(opened(&["--demo"])?.review.demo);
        Ok(())
    }

    /// Options that only mean something to a command are refused rather than ignored.
    #[test]
    fn command_options_are_refused() {
        for args in [
            ["--json"].as_slice(),
            &["--review", "key"],
            &["--no-auto-approve"],
        ] {
            assert!(opened(args).is_err(), "{args:?}");
        }
    }

    /// Global options may come before a command, a command's name is never a target, and a
    /// target never goes with a command or the demo.
    #[test]
    fn targets_and_commands_are_told_apart() -> eyre::Result<()> {
        for args in [
            ["open", "128"].as_slice(),
            &["show"],
            &["--json", "status"],
            &["-C", "repo", "open", "128"],
            &["--demo", "show"],
            &["--review", "key", "mark", "u1-0001", "seen"],
        ] {
            let cli = parse(args)?;
            assert!(cli.command.is_some(), "{args:?}");
            refuse_with_command(&cli.app)?;
        }
        let cli = parse(&["128", "show"])?;
        assert!(refuse_with_command(&cli.app).is_err());
        assert!(parse(&["--demo", "128"]).is_err());
        assert!(parse(&["--demo-revision", "2", "--worktree"]).is_err());
        Ok(())
    }
}
