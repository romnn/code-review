//! Temporary git repositories for the unit tests.

use std::path::Path;
use std::process::Command;

use color_eyre::eyre;

use crate::git::{PrivateRepo, Repo};

/// Runs git in `dir`, isolated from the user's configuration and with a fixed identity, and
/// returns its trimmed standard output.
pub(crate) fn git(dir: &Path, args: &[&str]) -> eyre::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
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
