//! Standalone desktop review app: review-ui's model drawn with gpui and gpui-component.
//!
//! Opens a review (a pull request, a `base..head` range, the working tree, or the built-in demo)
//! and shows the changed files and the selected file's diff, with keyboard and mouse marking
//! through the engine and go-to-definition into the head worktree.
//! review-tui draws the same model in a terminal, with the same keys and configuration.

mod app;
mod color;

use std::time::Instant;

use clap::{Parser, ValueEnum};
use color_eyre::eyre;
use gpui::{
    App, AppContext as _, Bounds, Font, FontFallbacks, FontFeatures, FontStyle, FontWeight,
    TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px, size,
};
use gpui_component::{ActiveTheme as _, TitleBar};
use review_ui::front_end::FrontEnd;
use review_ui::{config, launch, model};

/// Review a pull request, a range or the working tree.
#[derive(Debug, Clone, Parser)]
#[command(name = "review-gui", version)]
struct Options {
    #[command(flatten)]
    review: launch::Options,
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
    app::Fonts { code, advance }
}

fn main() -> eyre::Result<()> {
    let process_started = Instant::now();
    color_eyre::install()?;
    let options = Options::parse();
    let mut configured = launch::load_config(&options.review);
    if let Some(family) = &options.font_family {
        configured.config.font.family = Some(family.clone());
    }
    if let Some(size) = options.font_size {
        configured.config.font.size_points = Some(size);
    }
    let font = configured.config.font.clone();
    let settings = launch::settings(configured, FrontEnd::Window)?;
    for problem in &settings.problems {
        eprintln!("review-gui: {problem}");
    }
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
        &options.review.target,
        &options.review.worktree,
        options.review.is_demo(),
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
            let fonts = code_font(&font, cx);
            let bounds = Bounds::centered(None, size(px(1440.0), px(920.0)), cx);
            let opener: model::Opener = Box::new(move || {
                launch::open(&options.review, &|found| eprintln!("review-gui: {found}"))
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
                eprintln!("review-gui: failed to open a window: {err:#}");
                cx.quit();
            }
            cx.activate(true);
        });
    Ok(())
}
