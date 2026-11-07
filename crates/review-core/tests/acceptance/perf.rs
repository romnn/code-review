//! The performance budget: a 50,000-line diff is analyzed in under a second in release mode.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use color_eyre::eyre;
use indoc::writedoc;
use review_core::analysis::analyze;
use review_core::{ObjectId, Repo};

use crate::common::TestRepo;

/// Generates a Rust file of `lines` lines; `variant` changes three of every six lines.
fn generated_file(file: usize, lines: usize, variant: bool) -> String {
    let mut text = String::new();
    let mut line = 0;
    let mut function = 0;
    let (label, extra) = if variant {
        ("changed", " + 1")
    } else {
        ("original", "")
    };
    while line + 6 <= lines {
        // Writing to a `String` cannot fail.
        writedoc!(
            text,
            "
                pub fn file{file}_function{function}(input: u64) -> u64 {{
                    let scaled = input * {function}{extra};
                    let label = \"{label} {file} {function}\";
                    let offset = scaled + {line}{extra};
                    offset ^ label.len() as u64
                }}
            "
        )
        .ok();
        line += 6;
        function += 1;
    }
    text
}

/// Clones the fixture to `target` with branches `perf-base` and `perf-head`, so front ends can
/// open the same diff with the range `origin/perf-base..origin/perf-head`.
fn export_fixture(repo: &TestRepo, base: &str, head: &str, target: &Path) -> eyre::Result<()> {
    repo.git(&["branch", "--force", "perf-base", base])?;
    repo.git(&["branch", "--force", "perf-head", head])?;
    if target.exists() {
        std::fs::remove_dir_all(target)?;
    }
    let target = target
        .to_str()
        .ok_or_else(|| eyre::eyre!("the fixture path is not UTF-8"))?;
    let source = repo
        .path()
        .to_str()
        .ok_or_else(|| eyre::eyre!("the repository path is not UTF-8"))?;
    repo.git(&["clone", "--quiet", source, target])?;
    Ok(())
}

/// Analyzes a diff with 25 files of 2,400 lines each where half of all lines change, for
/// 60,000 changed lines in total (30,000 removed and 30,000 added), and reports the time.
///
/// Run in release mode with `cargo nextest run --release --run-ignored only -E 'test(perf)'`.
/// With `REVIEW_PERF_FIXTURE_DIR` set, the repository is also cloned there for front-end
/// benchmarks.
#[test]
#[ignore = "performance budget; run in release mode"]
fn analyzes_fifty_thousand_line_diff_quickly() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    for file in 0..25 {
        repo.write(
            &format!("src/file{file}.rs"),
            &generated_file(file, 2400, false),
        )?;
    }
    let base = repo.commit("base")?;
    for file in 0..25 {
        repo.write(
            &format!("src/file{file}.rs"),
            &generated_file(file, 2400, true),
        )?;
    }
    let head = repo.commit("head")?;
    if let Some(fixture) = std::env::var_os("REVIEW_PERF_FIXTURE_DIR") {
        export_fixture(&repo, &base, &head, Path::new(&fixture))?;
    }
    let git = Repo::discover(repo.path())?;
    let (base, head) = (ObjectId::parse(&base)?, ObjectId::parse(&head)?);

    let started = Instant::now();
    let analysis = analyze(&git, &base, &head)?;
    let elapsed = started.elapsed();

    let changed_lines: usize = analysis
        .units
        .iter()
        .filter_map(|unit| unit.chunk())
        .map(|chunk| chunk.old.len + chunk.new.len)
        .sum();
    println!(
        "analyzed {changed_lines} changed lines in {} units in {elapsed:?}",
        analysis.units.len()
    );
    eyre::ensure!(
        changed_lines >= 50_000,
        "only {changed_lines} changed lines"
    );
    if cfg!(not(debug_assertions)) {
        eyre::ensure!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    }
    Ok(())
}
