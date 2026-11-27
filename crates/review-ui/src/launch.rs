//! Starting a front end: the command line every front end takes to name a review, the
//! configuration with its problems, and opening the review.
//!
//! A front end flattens [`Options`] into its own command line beside its own flags, loads the
//! configuration with [`load_config`], adjusts it from those flags, turns it into the model's
//! [`Settings`] with [`settings`], and opens the review with [`open`] in the model's opener,
//! which runs in the background behind the first frame.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::time::{Duration, Instant};

use review_core::demo::{DemoPaths, DemoRequest, fixture_dir};
use review_core::{Busy, Capture, LiveCapture, Repo, Review, ReviewTarget, RevisionNumber};

use crate::config::{self, Config};
use crate::front_end::FrontEnd;
use crate::model::{Following, Loaded, PaneLayout, Settings, ZOOM_RANGE};
use crate::{keymap, look, theme};

/// The command line that names a review and where its data and settings are.
///
/// Its argument group is `review`, apart from the front end's own command line.
#[derive(Debug, Clone, clap::Args)]
#[group(id = "review")]
pub struct Options {
    /// A pull request number or URL, or a `base..head` range.
    ///
    /// Without one (and without --worktree), reviews this checkout: whatever its working tree
    /// holds, on whatever branch, against the default branch.
    pub target: Option<String>,
    /// Review the working tree, including uncommitted and untracked files, against a ref.
    #[arg(long, value_name = "REF", num_args = 0..=1, default_missing_value = "HEAD", conflicts_with = "target")]
    pub worktree: Option<String>,
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "repo", default_value = ".")]
    pub repo: PathBuf,
    /// Where reviews are stored; defaults to `$XDG_DATA_HOME/code-review`.
    #[arg(long, env = "CR_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// Rebuild the built-in demo pull request from scratch and open revision 1.
    #[arg(long, conflicts_with_all = ["target", "worktree"])]
    pub demo: bool,
    /// Where the demo's repository and data live, instead of `target/demo`; QA runs use their
    /// own, so other front ends' demos cannot reset theirs.
    #[arg(long, env = "CODE_REVIEW_DEMO_DIR", hide = true)]
    pub demo_dir: Option<PathBuf>,
    /// Open the built-in demo, recording its revisions up to this one and keeping marks.
    #[arg(long, value_name = "N", conflicts_with_all = ["demo", "target", "worktree"])]
    pub demo_revision: Option<u32>,
    /// The base of the default data directory, `$XDG_DATA_HOME`.
    #[arg(long, env = "XDG_DATA_HOME", hide = true)]
    pub xdg_data_home: Option<PathBuf>,
    /// The configuration file, instead of `$XDG_CONFIG_HOME/code-review/config.toml`; QA
    /// sessions use their own, so remembered settings never touch the user's.
    #[arg(long, env = "CODE_REVIEW_CONFIG", hide = true)]
    pub config: Option<PathBuf>,
    /// The base of the default configuration file, `$XDG_CONFIG_HOME`.
    #[arg(long, env = "XDG_CONFIG_HOME", hide = true)]
    pub xdg_config_home: Option<PathBuf>,
    /// The home directory, which the defaults fall back to without the XDG variables.
    #[arg(long, env = "HOME", hide = true)]
    pub home: Option<PathBuf>,
}

impl Options {
    /// Whether the options name the built-in demo.
    #[must_use]
    pub fn is_demo(&self) -> bool {
        self.demo || self.demo_revision.is_some()
    }
}

/// The configuration as loaded, with where it is and what was wrong with it.
#[derive(Debug, Clone)]
pub struct Configured {
    /// The settings, the defaults where the file is missing or cannot be read.
    pub config: Config,
    /// The file, which choices made in the front end are written back to.
    pub path: Option<PathBuf>,
    /// What went wrong reading it so far, for the front end to show.
    pub problems: Vec<String>,
}

/// Loads the configuration the options name; a file that cannot be read gives the defaults
/// and a problem, except for `[review] auto_approve_formatting`, which is read on its own as
/// `cr` reads it (see [`config::fallback`]).
#[must_use]
pub fn load_config(options: &Options) -> Configured {
    let path = options
        .config
        .clone()
        .or_else(|| config::path(options.xdg_config_home.as_deref(), options.home.as_deref()));
    let mut problems = Vec::new();
    let config = match &path {
        Some(path) => config::load(path).unwrap_or_else(|err| {
            problems.push(format!("{err}; using the defaults"));
            config::fallback(path)
        }),
        None => Config::default(),
    };
    Configured {
        config,
        path,
        problems,
    }
}

/// Why the model's settings could not be made at all; lesser problems only join
/// [`Settings::problems`].
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// No theme loads, not even a built-in one.
    #[error("no theme is available")]
    NoTheme,
    /// The default diff style is missing.
    #[error("{0}")]
    NoStyle(String),
}

/// The model's settings for `front_end` from `configured`: the keymap without what the front
/// end cannot do, the themes and diff styles, the look it opens with and the front end's own
/// pane width.
///
/// The configuration's problems so far, then the keymap's, then the file's own, then the theme's
/// and the style's, end up in [`Settings::problems`] in that order, for the front end to print
/// and show.
///
/// # Errors
///
/// Returns an error when no theme or no default diff style is available.
pub fn settings(configured: Configured, front_end: FrontEnd) -> Result<Settings, SettingsError> {
    let Configured {
        config,
        path,
        mut problems,
    } = configured;
    let (mut keymap, key_problems) = keymap::keymap(&config.keys);
    problems.extend(key_problems);
    let capabilities = front_end.capabilities();
    keymap.retain(|binding| {
        crate::model::Action::from_name(binding.action)
            .is_none_or(|action| capabilities.allows(action))
    });
    problems.extend(config.problems.iter().cloned());
    let mouse = front_end.mouse(&config).unwrap_or_else(|problem| {
        problems.push(problem);
        config::Mouse::default()
    });
    let (chosen, themes) = choose_theme(path.as_deref(), config.theme.as_deref(), &mut problems)?;
    let styles = look::styles(&config.styles);
    let wanted = config.diff_style.as_deref().unwrap_or(look::DEFAULT_STYLE);
    let style = look::find_style(&styles, wanted)
        .or_else(|problem| {
            problems.push(format!("{problem}; using {}", look::DEFAULT_STYLE));
            look::find_style(&styles, look::DEFAULT_STYLE)
        })
        .map_err(SettingsError::NoStyle)?;
    Ok(Settings {
        config_path: path,
        code_size: config
            .font
            .size_pixels()
            .clamp(ZOOM_RANGE.start, ZOOM_RANGE.end),
        appearance: look::Appearance {
            applied: look::Look {
                theme: chosen,
                style,
                density: config.density,
                layout: config.diff_layout,
                view: config.diff_view,
                hunk_position: config.hunk_position,
                cross_files: config.cross_files,
                soft_unselected: config.soft_unselected,
                token_emphasis: config.token_emphasis,
                wrap: config.wrap,
            },
            preview: None,
        },
        themes,
        styles,
        context: config.context,
        shorten_chains: config.shorten_chains,
        mouse,
        approved: config.approved,
        policy: review_core::ReviewPolicy {
            auto_approve_formatting: config.auto_approve_formatting,
        },
        language_servers: config.language_servers,
        live: config.live,
        removed: config.removed,
        added: config.added,
        layout: PaneLayout {
            sidebar: front_end.sidebar_width(&config.layout),
            comparison: config.layout.comparison,
            sidebar_visible: config.layout.sidebar_visible,
        },
        keymap,
        front_end,
        problems,
    })
}

/// Loads every theme, the built-in ones and those next to the configuration file, and picks
/// `wanted`, or the default when it does not exist.
fn choose_theme(
    config_path: Option<&Path>,
    wanted: Option<&str>,
    problems: &mut Vec<String>,
) -> Result<(Arc<theme::Theme>, Vec<theme::Theme>), SettingsError> {
    let themes_dir = config_path
        .and_then(Path::parent)
        .map(|dir| dir.join("themes"));
    let (themes, theme_problems) = theme::all(themes_dir.as_deref());
    problems.extend(theme_problems.iter().map(ToString::to_string));
    let wanted = wanted.unwrap_or(theme::DEFAULT);
    let chosen = themes
        .iter()
        .find(|theme| theme.name == wanted)
        .or_else(|| {
            problems.push(format!(
                "there is no theme {wanted:?}; using {}",
                theme::DEFAULT
            ));
            themes.first()
        })
        .cloned()
        .ok_or(SettingsError::NoTheme)?;
    Ok((Arc::new(chosen), themes))
}

/// Why the review could not be opened.
#[derive(Debug, thiserror::Error)]
enum OpenError {
    #[error("demo revisions are numbered from 1")]
    DemoRevision,
    #[error("cannot find a data directory; pass --data-dir")]
    NoDataDir,
    #[error("git is busy in this checkout ({0}); open it again once that is done")]
    Busy(Busy),
    #[error("git kept changing the checkout while it was captured; open it again")]
    Moved,
    #[error("the checkout was replaced by another clone while it opened; open it again")]
    Replaced,
    #[error(transparent)]
    Core(#[from] review_core::Error),
}

/// How many times the opening capture is taken again when git moved while it was taken.
const OPENING_ATTEMPTS: u32 = 5;

/// How long the opening waits for git to finish what it is busy with, when the review has no
/// revision to show meanwhile.
const BUSY_WAIT: Duration = Duration::from_secs(10);

/// Opens the review the options name, records its current revision and reads its history, as
/// the model's opener does.
///
/// In a terminal, no command run for the review's repository, from the first after finding it,
/// a fetch included, may prompt ([`Repo::forbid_prompts`]): a prompt would draw over the review.
/// A window's commands prompt where they would, over nothing of the window's.
///
/// With neither a target nor a worktree, the review is the checkout's, opened as
/// [`open_checkout`] does, which records nothing, and `say` is told so as soon as it opens; it
/// is also in [`Loaded::found`].
///
/// # Errors
///
/// Returns why the review could not be opened, with every cause.
pub fn open(options: &Options, say: &dyn Fn(&str), front_end: FrontEnd) -> Result<Loaded, String> {
    open_review(options, say, front_end).map_err(|err| chain(&err))
}

/// Opens the review of the checkout `repo` is, with data under `data_dir`, as the model's
/// opener does: starts the session that pins what it shows, and captures the working tree
/// under it, recording nothing.
///
/// While git is busy in the checkout, the review opens on its last recorded revision, or
/// waits a moment for git without one; while cr's data is at its limit, it opens on its last
/// recorded revision.
///
/// # Errors
///
/// Returns why the review could not be opened, with every cause.
pub fn open_checkout(data_dir: &Path, repo: Repo) -> Result<Loaded, String> {
    checkout(data_dir, repo, None).map_err(|err| chain(&err))
}

/// See [`open_checkout`]; `found` is how the review was found, for the status line.
#[tracing::instrument(skip_all)]
fn checkout(data_dir: &Path, repo: Repo, found: Option<String>) -> Result<Loaded, OpenError> {
    let review = Review::open_checkout(data_dir, repo)?;
    let session = review.start_session();
    let started = Instant::now();
    let mut moved = 0;
    loop {
        // No watch tells the opening of a change made while it captures, so the capture looks
        // twice.
        let captured = match review.confirmed_live_history(&session, &[], &[]) {
            // At its limit, cr's data takes no capture, and the review opens on its last
            // recorded revision; the window's first capture says why.
            Err(review_core::Error::StorageFull { .. }) => {
                return Ok(Loaded {
                    history: review.history()?,
                    review,
                    found,
                    following: Some(Following {
                        session,
                        subject: None,
                        filtered: Vec::new(),
                        busy: None,
                    }),
                });
            }
            captured => captured?,
        };
        match captured {
            Capture::Captured(captured) => {
                let LiveCapture {
                    history,
                    subject,
                    filtered,
                } = *captured;
                return Ok(Loaded {
                    review,
                    history,
                    found,
                    following: Some(Following {
                        session,
                        subject: Some(subject),
                        filtered,
                        busy: None,
                    }),
                });
            }
            Capture::Moved => {
                moved += 1;
                if moved >= OPENING_ATTEMPTS {
                    return Err(OpenError::Moved);
                }
            }
            Capture::Busy(busy) => {
                let history = review.history()?;
                if history.current().is_some() {
                    return Ok(Loaded {
                        review,
                        history,
                        found,
                        following: Some(Following {
                            session,
                            subject: None,
                            filtered: Vec::new(),
                            busy: Some(busy),
                        }),
                    });
                }
                if started.elapsed() >= BUSY_WAIT {
                    return Err(OpenError::Busy(busy));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Capture::Replaced => return Err(OpenError::Replaced),
        }
    }
}

/// Opens the review the options name: the checkout's (see [`open_checkout`]), saying so, with
/// neither a target nor a worktree, and otherwise the one named, recording its current
/// revision unless cr's data is at its limit; see [`open`] for `front_end`.
#[tracing::instrument(skip_all)]
fn open_review(
    options: &Options,
    say: &dyn Fn(&str),
    front_end: FrontEnd,
) -> Result<Loaded, OpenError> {
    let no_prompts = front_end == FrontEnd::Terminal;
    let demo = match (options.demo, options.demo_revision) {
        (true, _) => Some(DemoRequest::Fresh),
        (false, Some(revision)) => Some(DemoRequest::UpTo(
            RevisionNumber::new(revision).ok_or(OpenError::DemoRevision)?,
        )),
        (false, None) => None,
    };
    if let Some(request) = demo {
        let mut review = review_core::demo::prepare(
            &options
                .demo_dir
                .as_deref()
                .map_or_else(DemoPaths::in_project, DemoPaths::under),
            &fixture_dir(),
            request,
        )?;
        if no_prompts {
            review.forbid_prompts();
        }
        return loaded(review, None);
    }
    let data_dir = match &options.data_dir {
        Some(data_dir) => data_dir.clone(),
        None => review_core::store::default_data_dir(
            options.xdg_data_home.as_deref(),
            options.home.as_deref(),
        )
        .ok_or(OpenError::NoDataDir)?,
    };
    let mut repo = Repo::discover(&options.repo)?;
    if no_prompts {
        repo.forbid_prompts();
    }
    if options.target.is_none() && options.worktree.is_none() {
        let found = "following this checkout's working tree".to_owned();
        say(&found);
        return checkout(&data_dir, repo, Some(found));
    }
    let target = ReviewTarget::from_arguments(
        options.target.as_deref(),
        options.worktree.as_deref(),
        &repo,
    )?;
    let review = Review::open(&data_dir, repo, target)?;
    // At its limit, cr's data takes no new revision, and the review opens on its last recorded
    // one, saying why.
    let found = match review.refresh() {
        Ok(_) => None,
        Err(err @ review_core::Error::StorageFull { .. }) => Some(err.to_string()),
        Err(err) => return Err(err.into()),
    };
    loaded(review, found)
}

/// The review with its history, for a review that is not of the checkout.
#[tracing::instrument(skip_all)]
fn loaded(review: Review, found: Option<String>) -> Result<Loaded, OpenError> {
    let history = review.history()?;
    Ok(Loaded {
        review,
        history,
        found,
        following: None,
    })
}

/// `err` and every cause under it, joined by `: `.
fn chain(err: &dyn Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A front end without a code size or a window of its own gets no keys for them, while
    /// review-gui keeps every key, each reads its own pane width, and only review-tui reads
    /// `[tui.mouse]`: review-gui always has the whole mouse.
    #[test]
    fn settings_follow_the_front_end() {
        let configured = || Configured {
            config: config::parse(indoc::indoc! {"
                [tui.mouse]
                navigate = false
                copy_on_select = true
            "})
            .unwrap(),
            path: None,
            problems: Vec::new(),
        };
        let window = settings(configured(), FrontEnd::Window).unwrap();
        let terminal = settings(configured(), FrontEnd::Terminal).unwrap();
        let bound = |settings: &Settings, action: &str| {
            settings
                .keymap
                .iter()
                .any(|binding| binding.action == action)
        };
        for action in ["ZoomIn", "ZoomReset", "ToggleFullscreen", "CloseWindow"] {
            assert!(bound(&window, action), "{action}");
            assert!(!bound(&terminal, action), "{action}");
        }
        assert!(bound(&terminal, "Quit"));
        assert_eq!(
            window.keymap.len(),
            keymap::keymap(&std::collections::BTreeMap::new()).0.len(),
            "a window keeps every key"
        );
        assert_eq!(window.layout.sidebar, crate::panes::SidebarWidth::Fit);
        assert_eq!(terminal.layout.sidebar, crate::panes::SidebarWidth::Fit);
        assert_eq!(window.mouse, config::Mouse::default());
        assert!(!terminal.mouse.navigate && terminal.mouse.select && terminal.mouse.copy_on_select);
    }

    /// A mistake in `[tui.mouse]` costs review-gui nothing, its theme included, and review-tui
    /// only its mouse settings, which it names.
    #[test]
    fn a_mistake_in_the_terminal_s_mouse_spares_the_window() {
        let configured = || Configured {
            config: config::parse(indoc::indoc! {"
                [theme]
                name = \"one-dark\"
                [tui.mouse]
                navigat = false
            "})
            .unwrap(),
            path: None,
            problems: Vec::new(),
        };
        let window = settings(configured(), FrontEnd::Window).unwrap();
        assert_eq!(window.appearance.applied.theme.name, "one-dark");
        assert_eq!(window.problems, Vec::<String>::new());
        let terminal = settings(configured(), FrontEnd::Terminal).unwrap();
        assert_eq!(terminal.appearance.applied.theme.name, "one-dark");
        assert_eq!(terminal.mouse, config::Mouse::default());
        assert!(
            terminal.problems.len() == 1
                && terminal.problems[0]
                    .starts_with("[tui] cannot be used: unknown field `navigat`"),
            "{:?}",
            terminal.problems
        );
    }

    /// An error's causes follow it, as a report prints them.
    #[test]
    fn errors_carry_their_causes() {
        let core = review_core::Error::Spawn {
            program: "git",
            args: "fetch".to_owned(),
            source: std::io::Error::other("no such program"),
        };
        assert_eq!(
            chain(&OpenError::from(core)),
            "failed to run git fetch: no such program"
        );
    }
}
