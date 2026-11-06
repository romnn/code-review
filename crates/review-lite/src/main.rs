//! Standalone desktop review app built on gpui and gpui-component.
//!
//! Opens a review (a pull request, a `base..head` range, the working tree, or the built-in demo)
//! and shows its walkthrough steps with progress, the selected step's changes in file sections,
//! keyboard and mouse marking through the engine, and go-to-definition into the head worktree.

mod app;
mod config;
mod look;
mod lsp;
mod rows;
mod syntax;
mod theme;

use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser, ValueEnum};
use color_eyre::eyre::{self, OptionExt as _};
use gpui::{
    App, AppContext as _, Bounds, Font, FontFallbacks, FontFeatures, FontStyle, FontWeight,
    TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px, size,
};
use gpui_component::{ActiveTheme as _, TitleBar};
use review_core::demo::{DemoPaths, DemoRequest, fixture_dir};
use review_core::{Detected, Repo, Review, ReviewTarget, RevisionNumber};

/// Review a pull request, a range or the working tree.
#[derive(Debug, Clone, Parser)]
#[command(name = "review-lite", version)]
struct Options {
    /// A pull request number or URL, or a `base..head` range.
    ///
    /// Without one (and without --worktree), reopens the review last opened on this branch, or
    /// else reviews the branch's open pull request, or else `<default branch>..<branch>`.
    target: Option<String>,
    /// Review the working tree, including uncommitted and untracked files, against a ref.
    #[arg(long, value_name = "REF", num_args = 0..=1, default_missing_value = "HEAD", conflicts_with = "target")]
    worktree: Option<String>,
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "repo", default_value = ".")]
    repo: PathBuf,
    /// Where reviews are stored; defaults to `$XDG_DATA_HOME/code-review`.
    #[arg(long, env = "CR_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Rebuild the built-in demo pull request from scratch and open revision 1.
    #[arg(long, conflicts_with_all = ["target", "worktree"])]
    demo: bool,
    /// Where the demo's repository and data live, instead of `target/demo`; QA runs use their
    /// own, so other front ends' demos cannot reset theirs.
    #[arg(long, env = "CODE_REVIEW_DEMO_DIR", hide = true)]
    demo_dir: Option<PathBuf>,
    /// Open the built-in demo, recording its revisions up to this one and keeping marks.
    #[arg(long, value_name = "N", conflicts_with_all = ["demo", "target", "worktree"])]
    demo_revision: Option<u32>,
    /// The code font family; overrides the configuration file.
    #[arg(long)]
    font_family: Option<String>,
    /// The code font size in points; overrides the configuration file.
    #[arg(long)]
    font_size: Option<f32>,
    /// Scroll through every row once, print frame timing statistics, and exit.
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "fling")]
    bench_scroll: Option<BenchArg>,
    /// Exit once the first hunk is visible, printing how long that took.
    #[arg(long)]
    exit_after_first_frame: bool,
    /// Read QA commands from standard input and answer each on one line of standard output
    /// after the next frame; see `scripts/qa/run.sh`.
    #[arg(long)]
    qa: bool,
    /// Which window decorations to ask for; the compositor may still choose client-side ones.
    #[arg(long, value_enum, default_value = "server", hide = true)]
    decorations: DecorationsArg,
    #[arg(long, env = "XDG_DATA_HOME", hide = true)]
    xdg_data_home: Option<PathBuf>,
    /// The configuration file, instead of `$XDG_CONFIG_HOME/code-review/config.toml`; QA
    /// sessions use their own, so remembered settings never touch the user's.
    #[arg(long, env = "CODE_REVIEW_CONFIG", hide = true)]
    config: Option<PathBuf>,
    #[arg(long, env = "XDG_CONFIG_HOME", hide = true)]
    xdg_config_home: Option<PathBuf>,
    #[arg(long, env = "HOME", hide = true)]
    home: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BenchArg {
    Fling,
    Animated,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DecorationsArg {
    Server,
    Client,
}

/// Opens the review the options name and records its current revision; with neither a target
/// nor a worktree, also how it was found for the current branch.
fn open_review(options: &Options) -> eyre::Result<(Review, Option<Detected>)> {
    let demo = match (options.demo, options.demo_revision) {
        (true, _) => Some(DemoRequest::Fresh),
        (false, Some(revision)) => Some(DemoRequest::UpTo(
            RevisionNumber::new(revision).ok_or_eyre("demo revisions are numbered from 1")?,
        )),
        (false, None) => None,
    };
    if let Some(request) = demo {
        let review = review_core::demo::prepare(
            &options
                .demo_dir
                .as_deref()
                .map_or_else(DemoPaths::in_project, DemoPaths::under),
            &fixture_dir(),
            request,
        )?;
        return Ok((review, None));
    }
    let data_dir = match &options.data_dir {
        Some(data_dir) => data_dir.clone(),
        None => review_core::store::default_data_dir(
            options.xdg_data_home.as_deref(),
            options.home.as_deref(),
        )
        .ok_or_eyre("cannot find a data directory; pass --data-dir")?,
    };
    let repo = Repo::discover(&options.repo)?;
    let (review, detected) = if options.target.is_none() && options.worktree.is_none() {
        let (review, detected) = Review::open_for_current_branch(&data_dir, repo)?;
        eprintln!("review-lite: {detected}");
        (review, Some(detected))
    } else {
        let target = ReviewTarget::from_arguments(
            options.target.as_deref(),
            options.worktree.as_deref(),
            &repo,
        )?;
        (Review::open(&data_dir, repo, target)?, None)
    };
    review.refresh()?;
    review_core::store::remember_review(&data_dir, review.repo(), review.key())?;
    Ok((review, detected))
}

/// The code font: the configured family, then `SFMono Nerd Font`, then `SF Mono`, then the theme's
/// monospace font, whichever is installed first; Medium weight and no ligatures by default.
fn code_font(config: &config::FontConfig, cx: &App) -> app::Fonts {
    let installed = cx.text_system().all_font_names();
    let mut candidates: Vec<String> = config.family.iter().cloned().collect();
    candidates.extend(["SFMono Nerd Font".to_owned(), "SF Mono".to_owned()]);
    let theme_mono = cx.theme().mono_font_family.to_string();
    let family = candidates
        .iter()
        .find(|family| installed.iter().any(|name| name == *family))
        .cloned()
        .unwrap_or_else(|| theme_mono.clone());
    let fallbacks: Vec<String> = candidates
        .into_iter()
        .chain([theme_mono])
        .filter(|candidate| *candidate != family)
        .collect();
    let size = config
        .size_pixels()
        .clamp(app::ZOOM_RANGE.start, app::ZOOM_RANGE.end);
    let code = Font {
        family: family.into(),
        features: FontFeatures::disable_ligatures(),
        fallbacks: Some(FontFallbacks::from_fonts(fallbacks)),
        weight: FontWeight(config.weight.unwrap_or(config::DEFAULT_WEIGHT)),
        style: FontStyle::Normal,
    };
    // Measured at a large size, so rounding to whole pixels hardly skews the share.
    let measured = 100.0;
    let text_system = cx.text_system();
    let advance = text_system
        .em_advance(text_system.resolve_font(&code), px(measured))
        .map_or(0.6, |width| f32::from(width) / measured);
    app::Fonts {
        code,
        code_size: size,
        default_code_size: size,
        // The window sets it from the look's density when it opens.
        line_height: look::line_height(config::Density::default()),
        advance,
    }
}

/// Loads every theme, the built-in ones and those next to the configuration file, and picks
/// `wanted`, or the default when it does not exist.
fn choose_theme(
    config_path: Option<&std::path::Path>,
    wanted: Option<&str>,
    problems: &mut Vec<String>,
) -> eyre::Result<(std::sync::Arc<theme::Theme>, Vec<theme::Theme>)> {
    let themes_dir = config_path
        .and_then(std::path::Path::parent)
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
        .ok_or_eyre("no theme is available")?;
    Ok((std::sync::Arc::new(chosen), themes))
}

/// The window's settings from `config`: its themes and diff styles, and the look it opens
/// with. `problems` are the configuration's problems found so far; the ones found here join
/// them, and all of them are printed and kept for the window to show.
fn settings(
    config: &config::Config,
    config_path: Option<PathBuf>,
    keymap: Vec<app::keymap::Binding>,
    mut problems: Vec<String>,
) -> eyre::Result<app::Settings> {
    problems.extend(config.problems.iter().cloned());
    let (chosen, themes) = choose_theme(
        config_path.as_deref(),
        config.theme.as_deref(),
        &mut problems,
    )?;
    let styles = look::styles(&config.styles);
    let wanted = config.diff_style.as_deref().unwrap_or(look::DEFAULT_STYLE);
    let style = look::find_style(&styles, wanted)
        .or_else(|problem| {
            problems.push(format!("{problem}; using {}", look::DEFAULT_STYLE));
            look::find_style(&styles, look::DEFAULT_STYLE)
        })
        .map_err(|problem| eyre::eyre!(problem))?;
    for problem in &problems {
        eprintln!("review-lite: {problem}");
    }
    Ok(app::Settings {
        config_path,
        appearance: look::Appearance {
            applied: look::Look {
                theme: chosen,
                style,
                density: config.density,
                layout: config.diff_layout,
                token_emphasis: config.token_emphasis,
            },
            preview: None,
        },
        themes,
        styles,
        context: config.context,
        approved: config.approved,
        removed: config.removed,
        added: config.added,
        layout: config.layout,
        keymap,
        problems,
    })
}

fn main() -> eyre::Result<()> {
    let process_started = Instant::now();
    color_eyre::install()?;
    let options = Options::parse();
    let config_path = options
        .config
        .clone()
        .or_else(|| config::path(options.xdg_config_home.as_deref(), options.home.as_deref()));
    let mut problems = Vec::new();
    let mut config = match &config_path {
        Some(path) => config::load(path).unwrap_or_else(|err| {
            problems.push(format!("{err}; using the defaults"));
            config::Config::default()
        }),
        None => config::Config::default(),
    };
    if let Some(family) = &options.font_family {
        config.font.family = Some(family.clone());
    }
    if let Some(size) = options.font_size {
        config.font.size_points = Some(size);
    }
    let font = config.font.clone();
    let (keymap, key_problems) = app::keymap::keymap(&config.keys);
    problems.extend(key_problems.iter().map(ToString::to_string));
    let settings = settings(&config, config_path, keymap.clone(), problems)?;
    let startup = app::Startup {
        process_started,
        bench_scroll: options.bench_scroll.map(|mode| match mode {
            BenchArg::Fling => app::BenchMode::Fling,
            BenchArg::Animated => app::BenchMode::Animated,
        }),
        exit_after_first_frame: options.exit_after_first_frame,
        qa: options.qa,
    };
    let decorations = match options.decorations {
        DecorationsArg::Server => WindowDecorations::Server,
        DecorationsArg::Client => WindowDecorations::Client,
    };
    let title = match (
        &options.target,
        &options.worktree,
        options.demo || options.demo_revision.is_some(),
    ) {
        (_, _, true) => "Demo review".to_owned(),
        (Some(target), _, _) => format!("Review {target}"),
        (None, Some(_), _) => "Review of the working tree".to_owned(),
        (None, None, false) => "Review".to_owned(),
    };
    gpui_platform::application()
        .with_assets(gpui_kit_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            // Dark only, whatever the system appearance.
            gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
            // Square corners throughout: a precise, tool-like look.
            let theme = gpui_component::Theme::global_mut(cx);
            theme.radius = px(0.0);
            theme.radius_lg = px(0.0);
            app::bind_keys(&keymap, cx);
            let fonts = code_font(&font, cx);
            let bounds = Bounds::centered(None, size(px(1440.0), px(920.0)), cx);
            let opener: app::Opener = Box::new(move || {
                let (review, detected) = open_review(&options).map_err(|err| format!("{err:#}"))?;
                let history = review.history().map_err(|err| err.to_string())?;
                Ok(app::Loaded {
                    review,
                    history,
                    found: detected.map(|detected| detected.to_string()),
                })
            });
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(title.into()),
                        ..TitleBar::title_bar_options()
                    }),
                    window_decorations: Some(decorations),
                    window_min_size: Some(size(px(640.0), px(400.0))),
                    app_id: Some("code-review".to_owned()),
                    ..TitleBar::window_options()
                },
                |window, cx| {
                    cx.new(|cx| app::ReviewApp::new(opener, fonts, settings, startup, window, cx))
                },
            );
            if let Err(err) = window {
                eprintln!("review-lite: failed to open a window: {err:#}");
                cx.quit();
            }
            cx.activate(true);
        });
    Ok(())
}
