//! The `review-tui` binary: the terminal review app on its own, as the QA scripts drive it.
//!
//! Releases ship it inside `cr`, which runs the app without a command.

use clap::Parser as _;
use color_eyre::eyre;

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    review_tui::run(&review_tui::Options::parse())
}
