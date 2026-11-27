//! Temporary git repositories and review helpers for the acceptance tests.

use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{self, WrapErr as _};
use indoc::formatdoc;
use review_core::analysis::{Analysis, Unit};
use review_core::{
    History, MarkVerdict, Recorded, Repo, Review, ReviewTarget, RevisionNumber, UnitId, UnitState,
};
use tempfile::TempDir;

/// Isolates the tests' git commands from the user's and the system's configuration, with the
/// workspace's `tests/gitconfig` in place of the user's, pins the commit identity and has git
/// never prompt.
pub const GIT_ENV: [(&str, &str); 7] = [
    (
        "GIT_CONFIG_GLOBAL",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/gitconfig"),
    ),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// A temporary repository with a `main` branch, plus a temporary data directory.
pub struct TestRepo {
    repo: TempDir,
    data: TempDir,
}

impl TestRepo {
    /// Creates an empty repository on `main`.
    pub fn new() -> eyre::Result<Self> {
        let test = Self {
            repo: tempfile::tempdir()?,
            data: tempfile::tempdir()?,
        };
        test.git(&["init", "--quiet", "--initial-branch=main"])?;
        Ok(test)
    }

    pub fn path(&self) -> &Path {
        self.repo.path()
    }

    pub fn data_dir(&self) -> &Path {
        self.data.path()
    }

    /// Runs git in the repository and returns its trimmed standard output.
    pub fn git(&self, args: &[&str]) -> eyre::Result<String> {
        git_in(self.path(), args)
    }

    /// Captures the working tree as a review of it against `HEAD` records it, and returns the
    /// review, whose snapshots hold the capture, with what it recorded: the capture is its
    /// head, `HEAD` itself when nothing is beyond it.
    pub fn capture(&self) -> eyre::Result<(Review, Recorded)> {
        let review = Review::open(
            self.data_dir(),
            Repo::discover(self.path())?,
            ReviewTarget::Worktree {
                base: "HEAD".to_owned(),
            },
        )?;
        let recorded = review.refresh()?;
        Ok((review, recorded))
    }

    /// Whether a capture records anything beyond `HEAD`: staged or unstaged changes, or
    /// untracked files that are not ignored.
    pub fn uncommitted(&self) -> eyre::Result<bool> {
        let (_, recorded) = self.capture()?;
        Ok(recorded.head.as_str() != self.git(&["rev-parse", "HEAD"])?)
    }

    pub fn write(&self, path: &str, content: &str) -> eyre::Result<()> {
        let full = self.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, content).wrap_err_with(|| format!("failed to write {path}"))
    }

    /// Stages everything and commits, returning the new commit ID.
    pub fn commit(&self, message: &str) -> eyre::Result<String> {
        self.git(&["add", "--all"])?;
        self.git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-verify",
            "-m",
            message,
        ])?;
        self.git(&["rev-parse", "HEAD"])
    }

    /// Commits what is staged, without staging anything else.
    pub fn commit_staged(&self, message: &str) -> eyre::Result<()> {
        self.git(&["commit", "--quiet", "--no-verify", "-m", message])?;
        Ok(())
    }

    /// Switches to `branch`, creating or resetting it at `start`.
    pub fn branch_at(&self, branch: &str, start: &str) -> eyre::Result<()> {
        self.git(&["checkout", "--quiet", "-B", branch, start])?;
        Ok(())
    }

    pub fn switch(&self, branch: &str) -> eyre::Result<()> {
        self.git(&["checkout", "--quiet", branch])?;
        Ok(())
    }

    /// Opens the review of `main..feature`.
    pub fn review(&self) -> eyre::Result<Review> {
        let repo = Repo::discover(self.path())?;
        let target = ReviewTarget::Range {
            base: "main".to_owned(),
            head: "feature".to_owned(),
        };
        Ok(Review::open(self.data_dir(), repo, target)?)
    }
}

/// Runs git in `directory`, a working tree or a git directory, and returns its trimmed standard
/// output.
pub fn git_in(directory: &Path, args: &[&str]) -> eyre::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .envs(GIT_ENV)
        .output()
        .wrap_err_with(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        eyre::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Every file and directory below `root`, with what writing to it would change: its kind, its
/// length and modification time, and a file's bytes; for comparing a git directory before and
/// after the engine read it.
pub fn files(root: &Path) -> eyre::Result<Vec<String>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let name = path.strip_prefix(root)?.display().to_string();
        if metadata.is_dir() {
            found.push(format!("{name}/ {modified}"));
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        } else {
            let mut digest = std::hash::DefaultHasher::new();
            std::hash::Hash::hash(&std::fs::read(&path)?, &mut digest);
            let digest = std::hash::Hasher::finish(&digest);
            found.push(format!("{name} {} {modified} {digest:x}", metadata.len()));
        }
    }
    found.sort();
    Ok(found)
}

/// Records the current state of the review's target and returns the revision number.
pub fn refresh(review: &Review) -> eyre::Result<RevisionNumber> {
    Ok(review.refresh()?.number)
}

/// Marks every unit of revision `revision` with `verdict`.
pub fn mark_all(
    review: &Review,
    revision: RevisionNumber,
    verdict: MarkVerdict,
) -> eyre::Result<()> {
    let history = review.history()?;
    let view = history.view(revision)?;
    review.mark(&view, &view.unit_ids(), verdict)?;
    Ok(())
}

/// A unit with its state and a readable summary, for assertions.
#[derive(Debug, Clone)]
pub struct UnitSummary {
    pub id: UnitId,
    pub path: String,
    pub removed: String,
    pub added: String,
    pub state: UnitState,
    pub unit: Unit,
}

/// Summarizes every unit of a revision.
pub fn summaries(history: &History, revision: RevisionNumber) -> eyre::Result<Vec<UnitSummary>> {
    let view = history.view(revision)?;
    view.unit_ids()
        .into_iter()
        .map(|id| {
            let unit = view.unit(id)?.clone();
            let (removed, added) = unit
                .chunk()
                .map(|chunk| (chunk.removed.clone(), chunk.added.clone()))
                .unwrap_or_default();
            Ok(UnitSummary {
                id,
                path: view.file(id)?.display_path().to_owned(),
                removed,
                added,
                state: view.state(id)?,
                unit,
            })
        })
        .collect()
}

/// Finds the one unit whose added (or, if none, removed) text contains `needle`.
pub fn find<'a>(units: &'a [UnitSummary], needle: &str) -> eyre::Result<&'a UnitSummary> {
    let mut found = units.iter().filter(|unit| {
        unit.added.contains(needle) || (unit.added.is_empty() && unit.removed.contains(needle))
    });
    let first = found
        .next()
        .ok_or_else(|| eyre::eyre!("no unit contains {needle:?}"))?;
    eyre::ensure!(found.next().is_none(), "several units contain {needle:?}");
    Ok(first)
}

/// Returns the latest revision's analysis.
pub fn latest_analysis(review: &Review) -> eyre::Result<Analysis> {
    let history = review.history()?;
    let view = history.current_view()?;
    view.analysis()
        .cloned()
        .ok_or_else(|| eyre::eyre!("no analysis"))
}

/// Builds a Rust file of `count` small functions, `f0` to `f{count-1}`, each returning its index.
pub fn rust_functions(count: usize) -> String {
    (0..count)
        .map(|index| {
            formatdoc!(
                "
                    fn f{index}() -> u32 {{
                        let value = {index};
                        value
                    }}

                "
            )
        })
        .collect::<Vec<_>>()
        .concat()
}
