//! Temporary git repositories for the unit tests.

use std::path::Path;
use std::process::Command;

use color_eyre::eyre;

use crate::git::{PrivateRepo, Repo};

/// Isolates the tests' git commands from the user's and the system's configuration, with the
/// workspace's `tests/gitconfig` in place of the user's, and pins the commit identity.
pub(crate) const GIT_ENV: [(&str, &str); 6] = [
    (
        "GIT_CONFIG_GLOBAL",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/gitconfig"),
    ),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
];

/// Runs git in `dir`, isolated as [`GIT_ENV`] says, and returns its trimmed standard output.
pub(crate) fn git(dir: &Path, args: &[&str]) -> eyre::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(GIT_ENV)
        .output()?;
    eyre::ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// A repository on `main` with `files` committed, written as given.
pub(crate) fn committed(files: &[(&str, &str)]) -> eyre::Result<tempfile::TempDir> {
    let repo = tempfile::tempdir()?;
    git(repo.path(), &["init", "--quiet", "--initial-branch=main"])?;
    for (path, text) in files {
        std::fs::write(repo.path().join(path), text)?;
    }
    git(repo.path(), &["add", "--all"])?;
    git(
        repo.path(),
        &["commit", "--quiet", "--no-verify", "-m", "base"],
    )?;
    Ok(repo)
}

/// A private repository for `source` in a directory of its own, with the directory.
pub(crate) fn private(source: &Repo) -> eyre::Result<(tempfile::TempDir, PrivateRepo)> {
    let data = tempfile::tempdir()?;
    let private = PrivateRepo::open(&data.path().join("archive"), source)?;
    Ok((data, private))
}
