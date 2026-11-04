//! Standalone desktop review app built on gpui and gpui-component.
//!
//! Opens a review (a pull request, a `base..head` range, the working tree, or the built-in demo)
//! and shows its walkthrough steps with progress, a virtualized list of each step's hunks,
//! keyboard marking through the engine, and go-to-definition into the head worktree.

mod app;
mod lsp;
mod rows;

use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre::{self, OptionExt as _};
use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use review_core::demo::{DemoPaths, DemoRequest, fixture_dir};
use review_core::{Repo, Review, ReviewTarget, RevisionNumber};

/// Review a pull request, a range or the working tree.
#[derive(Debug, Parser)]
#[command(name = "review-lite", version)]
struct Options {
    /// A pull request number or URL, or a `base..head` range; defaults to the review last opened
    /// for this repository and branch.
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
    /// Open the built-in demo, recording its revisions up to this one and keeping marks.
    #[arg(long, value_name = "N", conflicts_with_all = ["demo", "target", "worktree"])]
    demo_revision: Option<u32>,
    /// Scroll through every row once, print frame timing statistics, and exit.
    #[arg(long)]
    bench_scroll: bool,
    /// Exit once the first frame is rendered, to check that the app opens.
    #[arg(long)]
    exit_after_first_frame: bool,
    /// Read QA commands (`key`, `action`, `resize`, `state`, `quit`) from standard input and
    /// answer each on one line of standard output after the next frame.
    #[arg(long)]
    qa: bool,
    #[arg(long, env = "XDG_DATA_HOME", hide = true)]
    xdg_data_home: Option<PathBuf>,
    #[arg(long, env = "HOME", hide = true)]
    home: Option<PathBuf>,
}

/// Opens the review the options name and records its current revision.
fn open_review(options: &Options) -> eyre::Result<Review> {
    let demo = match (options.demo, options.demo_revision) {
        (true, _) => Some(DemoRequest::Fresh),
        (false, Some(revision)) => Some(DemoRequest::UpTo(
            RevisionNumber::new(revision).ok_or_eyre("demo revisions are numbered from 1")?,
        )),
        (false, None) => None,
    };
    if let Some(request) = demo {
        return Ok(review_core::demo::prepare(
            &DemoPaths::in_project(),
            &fixture_dir(),
            request,
        )?);
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
    let review = if options.target.is_none() && options.worktree.is_none() {
        let key = review_core::store::remembered_review(&data_dir, &repo)?
            .ok_or_eyre("no review opened for this repository and branch; pass a target")?;
        Review::load(&data_dir, &key)?
    } else {
        let target = ReviewTarget::from_arguments(
            options.target.as_deref(),
            options.worktree.as_deref(),
            &repo,
        )?;
        Review::open(&data_dir, repo, target)?
    };
    review.refresh()?;
    review_core::store::remember_review(&data_dir, review.repo(), review.key())?;
    Ok(review)
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let options = Options::parse();
    let review = open_review(&options)?;
    let history = review.history()?;
    let startup = app::Startup {
        bench_scroll: options.bench_scroll,
        exit_after_first_frame: options.exit_after_first_frame,
        qa: options.qa,
    };
    gpui_platform::application().run(move |cx: &mut App| {
        gpui_component::init(cx);
        // Dark only, whatever the system appearance.
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        app::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1400.0), px(900.0)), cx);
        let title = format!("Review {}", review.key());
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..TitlebarOptions::default()
                }),
                ..WindowOptions::default()
            },
            |window, cx| cx.new(|cx| app::ReviewApp::new(review, history, startup, window, cx)),
        );
        if let Err(err) = opened {
            eprintln!("review-lite: failed to open a window: {err:#}");
            cx.quit();
        }
        cx.activate(true);
    });
    Ok(())
}
