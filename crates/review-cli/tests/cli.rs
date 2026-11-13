//! End-to-end tests of the `cr` binary against a temporary repository.

use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{self, WrapErr as _};
use indoc::{formatdoc, indoc};
use serde_json::Value;

const GIT_ENV: [(&str, &str); 6] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
];

fn git(repo: &Path, args: &[&str]) -> eyre::Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .envs(GIT_ENV)
        .status()?;
    eyre::ensure!(status.success(), "git {} failed", args.join(" "));
    Ok(())
}

/// The configuration directory, `$XDG_CONFIG_HOME`, of a `cr` storing reviews under `data`, so
/// the user's own configuration never decides a test's outcome.
fn config_home(data: &Path) -> PathBuf {
    data.join("config")
}

/// Runs `cr` with `args` in `repo`, storing reviews under `data`, and returns standard output.
fn cr(repo: &Path, data: &Path, args: &[&str]) -> eyre::Result<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_cr"))
        .args(args)
        .arg("-C")
        .arg(repo)
        .env("CR_DATA_DIR", data)
        .env("XDG_CONFIG_HOME", config_home(data))
        .env_remove("CODE_REVIEW_CONFIG")
        .envs(GIT_ENV)
        .output()
        .wrap_err("failed to run cr")?;
    eyre::ensure!(
        output.status.success(),
        "cr {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn cr_json(repo: &Path, data: &Path, args: &[&str]) -> eyre::Result<Value> {
    let mut args = args.to_vec();
    args.push("--json");
    let output = cr(repo, data, &args)?;
    serde_json::from_str(&output)
        .wrap_err_with(|| format!("cr {} printed invalid JSON: {output}", args.join(" ")))
}

const BASE_LIB: &str = indoc! {"
    fn a() -> u32 {
        1
    }

    fn b() -> u32 {
        2
    }
"};

const FEATURE_LIB: &str = indoc! {"
    fn a() -> u32 {
        10
    }

    fn b() -> u32 {
        20
    }
"};

/// Opens a review, marks, notes, exports and imports a walkthrough through the CLI.
#[test]
fn every_command_works_with_json() -> eyre::Result<()> {
    let repo_dir = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    git(repo, &["init", "--quiet", "--initial-branch=main"])?;
    std::fs::write(repo.join("lib.rs"), BASE_LIB)?;
    std::fs::write(repo.join("Cargo.lock"), "version = 3\n")?;
    git(repo, &["add", "--all"])?;
    git(repo, &["commit", "--quiet", "-m", "base"])?;
    git(repo, &["checkout", "--quiet", "-b", "feature"])?;
    std::fs::write(repo.join("lib.rs"), FEATURE_LIB)?;
    std::fs::write(repo.join("Cargo.lock"), "version = 4\n")?;
    git(repo, &["commit", "--quiet", "--all", "-m", "feature"])?;

    // Open records revision 1 and remembers the review for this branch.
    let opened = cr_json(repo, data, &["open", "main..feature"])?;
    assert_eq!(opened["recorded"]["number"], 1);
    assert_eq!(opened["status"]["total"]["units"], 3);
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["total"]["new"], 3);
    assert_eq!(status["total"]["noise"], 1);

    // Show lists every unit once; the lockfile lands in the noise step.
    let shown = cr_json(repo, data, &["show"])?;
    let steps = shown["steps"].as_array().cloned().unwrap_or_default();
    let ids: Vec<String> = steps
        .iter()
        .flat_map(|step| step["units"].as_array().cloned().unwrap_or_default())
        .filter_map(|unit| unit["id"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        steps.last().map(|step| step["kind"].clone()),
        Some(Value::from("noise"))
    );

    // Marking, a category mark and a note.
    let code_units: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| *id != "u1-0000")
        .collect();
    let marked = cr_json(
        repo,
        data,
        &[&["mark"], code_units.as_slice(), &["flagged"]].concat(),
    )?;
    assert_eq!(marked["marked"].as_array().map(Vec::len), Some(2));
    cr_json(repo, data, &["mark", "--category", "noise", "seen"])?;
    cr_json(repo, data, &["note", code_units[0], "is", "ten", "right?"])?;
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["total"]["flagged"], 2);
    assert_eq!(status["total"]["reviewed"], 1);
    let unreviewed = cr_json(repo, data, &["show", "--unreviewed"])?;
    assert_eq!(unreviewed["steps"].as_array().map(Vec::len), Some(0));

    // Export, as Markdown and JSON.
    let markdown = cr(repo, data, &["export"])?;
    assert!(
        markdown.contains("1. **flagged** `lib.rs:2-2`"),
        "{markdown}"
    );
    assert!(markdown.contains("is ten right?"), "{markdown}");
    let exported = cr_json(repo, data, &["export", "--format", "json"])?;
    assert_eq!(exported["items"].as_array().map(Vec::len), Some(2));

    // Walkthrough round trip.
    let prompt = cr_json(repo, data, &["walkthrough", "prompt"])?;
    assert!(
        prompt["prompt"]
            .as_str()
            .is_some_and(|text| text.contains("u1-0001"))
    );
    let key = opened["review"].as_str().unwrap_or_default();
    let walkthrough = serde_json::json!({
        "version": 1, "review": key, "revision": 1, "title": "Tour",
        "steps": [{"title": "Values", "why": "the point", "units": code_units}],
    });
    let file = repo.join("walkthrough.json");
    std::fs::write(&file, walkthrough.to_string())?;
    cr_json(
        repo,
        data,
        &["walkthrough", "import", &file.display().to_string()],
    )?;
    let partition = cr_json(repo, data, &["walkthrough", "show"])?;
    assert_eq!(partition["steps"][0]["title"], "Values");

    // Watch records nothing new, and the browsing worktree exists at the head.
    cr(repo, data, &["watch", "--count", "1", "--interval", "0"])?;
    let worktree = cr_json(repo, data, &["worktree"])?;
    let path = worktree["path"].as_str().unwrap_or_default();
    assert!(Path::new(path).join("lib.rs").exists());
    Ok(())
}

/// Creates a repository with `base` files on `main` and `changed` files on `feature`.
fn repo_with_change(
    files: &[(&str, &str)],
    changed: &[(&str, &str)],
) -> eyre::Result<tempfile::TempDir> {
    let repo_dir = tempfile::tempdir()?;
    let repo = repo_dir.path();
    git(repo, &["init", "--quiet", "--initial-branch=main"])?;
    for (path, text) in files {
        std::fs::write(repo.join(path), text)?;
    }
    git(repo, &["add", "--all"])?;
    git(repo, &["commit", "--quiet", "-m", "base"])?;
    git(repo, &["checkout", "--quiet", "-b", "feature"])?;
    for (path, text) in changed {
        std::fs::write(repo.join(path), text)?;
    }
    git(repo, &["add", "--all"])?;
    git(
        repo,
        &["commit", "--quiet", "--allow-empty", "-m", "change"],
    )?;
    Ok(repo_dir)
}

/// `cr` approves proven formatting-only units automatically unless the configuration file the
/// apps share turns it off, and `--no-auto-approve` turns it off whatever the file says.
#[test]
fn status_follows_the_shared_configuration() -> eyre::Result<()> {
    let repo_dir = repo_with_change(
        &[(
            "lib.rs",
            indoc! {"
                fn main() {
                    let total = compute(alpha, beta, gamma);
                    let other = 0;
                    let limit = 1;
                }
            "},
        )],
        &[(
            "lib.rs",
            indoc! {"
                fn main() {
                    let total = compute(
                        alpha,
                        beta,
                        gamma
                    );
                    let other = 0;
                    let limit = 2;
                }
            "},
        )],
    )?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    cr_json(repo, data, &["open", "main..feature"])?;

    // Without a configuration file, the re-wrap is approved automatically and the edit is open.
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["total"]["units"], 2);
    assert_eq!(status["total"]["auto_approved"], 1);
    assert_eq!(status["total"]["new"], 1);

    // The apps' setting turns it off for `cr` too: both units are open.
    let file = config_home(data).join("code-review").join("config.toml");
    std::fs::create_dir_all(file.parent().unwrap_or(data))?;
    std::fs::write(
        &file,
        indoc! {"
            [theme]
            name = \"nightfox\"

            [review]
            auto_approve_formatting = false
        "},
    )?;
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["total"]["auto_approved"], 0);
    assert_eq!(status["total"]["new"], 2);

    // The flag wins over a file that leaves it on.
    std::fs::write(
        &file,
        indoc! {"
            [review]
            auto_approve_formatting = true
        "},
    )?;
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["total"]["auto_approved"], 1);
    let status = cr_json(repo, data, &["status", "--no-auto-approve"])?;
    assert_eq!(status["total"]["auto_approved"], 0);
    assert_eq!(status["total"]["new"], 2);
    Ok(())
}

/// `cr show` renders a move with edits expanded, with the inner diff showing the edited line.
#[test]
fn show_renders_inner_diff_of_move_with_edits() -> eyre::Result<()> {
    let function = indoc! {"
        fn moved_function(input: u32) -> u32 {
            let doubled = input * 2;
            let shifted = doubled + 7;
            let masked = shifted & 0xff;
            let scaled = masked * 3;
            scaled - 1
        }
    "};
    let edited = function.replace("0xff", "0x0f");
    let repo_dir = repo_with_change(
        &[
            (
                "a.rs",
                &formatdoc!(
                    "
                        fn keep() {{}}
                        {function}fn tail() {{}}
                    "
                ),
            ),
            ("b.rs", "fn other() {}\n"),
        ],
        &[
            (
                "a.rs",
                indoc! {"
                    fn keep() {}
                    fn tail() {}
                "},
            ),
            (
                "b.rs",
                &formatdoc!(
                    "
                        fn other() {{}}
                        {edited}"
                ),
            ),
        ],
    )?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    cr(repo, data, &["open", "main..feature"])?;
    let shown = cr(repo, data, &["show", "--color", "never"])?;
    assert!(shown.contains("moved with edits from a.rs:2-8"), "{shown}");
    assert!(
        shown.contains("     -     let masked = shifted & 0xff;"),
        "{shown}"
    );
    assert!(
        shown.contains("     +     let masked = shifted & 0x0f;"),
        "{shown}"
    );
    assert!(
        shown.contains("           let doubled = input * 2;"),
        "{shown}"
    );
    Ok(())
}

/// Commits a symlink to `old` on `main` and retargets it to `new` on `feature`, opens the review
/// and returns the symlink unit from `cr show --json`.
fn symlink_retarget_unit(old: &[u8], new: &[u8]) -> eyre::Result<Value> {
    let repo_dir = repo_with_change(&[("keep.txt", "keep\n")], &[])?;
    let repo = repo_dir.path();
    let stage_link = |target: &[u8]| -> eyre::Result<()> {
        let blob_file = repo.join(".git/link-blob");
        std::fs::write(&blob_file, target)?;
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["hash-object", "-w"])
            .arg(&blob_file)
            .envs(GIT_ENV)
            .output()?;
        let blob = String::from_utf8(output.stdout)?.trim().to_owned();
        git(
            repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("120000,{blob},link"),
            ],
        )
    };
    git(repo, &["checkout", "--quiet", "main"])?;
    stage_link(old)?;
    git(repo, &["commit", "--quiet", "-m", "link"])?;
    git(repo, &["branch", "--quiet", "--force", "feature"])?;
    git(repo, &["symbolic-ref", "HEAD", "refs/heads/feature"])?;
    stage_link(new)?;
    git(repo, &["commit", "--quiet", "-m", "retarget"])?;

    let data_dir = tempfile::tempdir()?;
    let data = data_dir.path();
    cr(repo, data, &["open", "main..feature"])?;
    let shown = cr_json(repo, data, &["show"])?;
    shown
        .pointer("/steps/0/units/0")
        .cloned()
        .ok_or_else(|| eyre::eyre!("cr show listed no unit"))
}

/// `cr show --json` carries a symlink retarget's old and new targets.
#[test]
fn show_json_carries_symlink_targets() -> eyre::Result<()> {
    let unit = symlink_retarget_unit(b"old-target", b"new-target")?;
    assert_eq!(unit["kind"], "symlink");
    assert_eq!(unit["old"]["link_target"], "old-target");
    assert_eq!(unit["new"]["link_target"], "new-target");
    assert_eq!(
        unit["description"],
        "symlink target changed: \"old-target\" -> \"new-target\""
    );
    Ok(())
}

/// Targets that differ only in bytes that are not UTF-8 stay distinct in `cr show --json`.
#[test]
fn show_json_escapes_undecodable_symlink_targets() -> eyre::Result<()> {
    let unit = symlink_retarget_unit(b"dir/\x80", b"dir/\x81")?;
    assert_eq!(unit["old"]["link_target"], "dir/\\x80");
    assert_eq!(unit["new"]["link_target"], "dir/\\x81");
    assert_eq!(
        unit["description"],
        r#"symlink target changed: "dir/\x80" -> "dir/\x81""#
    );
    Ok(())
}

/// A force-push back to an earlier head records a new revision, which default commands use.
#[test]
fn commands_follow_a_force_push_back() -> eyre::Result<()> {
    let repo_dir = repo_with_change(&[("a.txt", "a\n")], &[("a.txt", "b\n")])?;
    let repo = repo_dir.path();
    let data_dir = tempfile::tempdir()?;
    let data = data_dir.path();
    cr(repo, data, &["open", "main..feature"])?;
    let first_head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()?
            .stdout,
    )?;
    std::fs::write(repo.join("a.txt"), "c\n")?;
    git(repo, &["commit", "--quiet", "--all", "-m", "second"])?;
    cr(repo, data, &["open", "main..feature"])?;
    cr(repo, data, &["mark", "u2-0000", "seen"])?;
    git(repo, &["reset", "--quiet", "--hard", first_head.trim()])?;
    let watched = cr(repo, data, &["watch", "--count", "1", "--interval", "0"])?;
    assert!(watched.contains("New revision 3"), "{watched}");
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["revision"], 3);
    // The line reviewed on revision 2 changed back, so it is changed since reviewed.
    assert_eq!(status["total"]["changed"], 1);
    assert_eq!(status["total"]["reviewed"], 0);
    Ok(())
}

/// `cr mark --file` marks every unit of a file, or below a directory, in the current revision.
#[test]
fn mark_file_covers_a_file_or_directory() -> eyre::Result<()> {
    let repo_dir = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    git(repo, &["init", "--quiet", "--initial-branch=main"])?;
    std::fs::create_dir_all(repo.join("src/app"))?;
    for path in ["src/app/a.rs", "src/b.rs", "c.rs"] {
        std::fs::write(repo.join(path), "fn one() {}\n")?;
    }
    git(repo, &["add", "--all"])?;
    git(repo, &["commit", "--quiet", "-m", "base"])?;
    git(repo, &["checkout", "--quiet", "-b", "feature"])?;
    for path in ["src/app/a.rs", "src/b.rs", "c.rs"] {
        std::fs::write(repo.join(path), "fn two() {}\n")?;
    }
    git(repo, &["commit", "--quiet", "--all", "-m", "feature"])?;
    cr(repo, data, &["open", "main..feature"])?;

    // A directory covers both files below it; the top-level file stays new.
    let marked = cr_json(repo, data, &["mark", "--file", "src", "accepted"])?;
    assert_eq!(marked["marked"].as_array().map(Vec::len), Some(2));
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["total"]["accepted"], 2);
    assert_eq!(status["total"]["new"], 1);

    // A path without changes fails instead of marking nothing.
    assert!(cr(repo, data, &["mark", "--file", "docs", "seen"]).is_err());
    Ok(())
}

/// `cr open` without a target, with `PATH` pointing at no `gh` or at a fake one; the fake is a
/// shell script, so these run on Unix only.
#[cfg(unix)]
mod open_without_target {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::Command;

    use color_eyre::eyre::{self, WrapErr as _};
    use indoc::formatdoc;
    use serde_json::Value;

    use super::{GIT_ENV, git, repo_with_change};

    /// Runs `cr --json` with `args` in `repo` with `PATH` set to `path`, and returns its JSON output
    /// and standard error.
    fn cr_json_with_path(
        repo: &Path,
        data: &Path,
        path: &OsStr,
        args: &[&str],
    ) -> eyre::Result<(Value, String)> {
        let output = Command::new(env!("CARGO_BIN_EXE_cr"))
            .args(args)
            .arg("--json")
            .arg("-C")
            .arg(repo)
            .env("CR_DATA_DIR", data)
            .env("XDG_CONFIG_HOME", super::config_home(data))
            .env_remove("CODE_REVIEW_CONFIG")
            .env("PATH", path)
            .envs(GIT_ENV)
            .output()
            .wrap_err("failed to run cr")?;
        let stderr = String::from_utf8(output.stderr)?;
        eyre::ensure!(
            output.status.success(),
            "cr {} failed: {stderr}",
            args.join(" ")
        );
        Ok((serde_json::from_slice(&output.stdout)?, stderr))
    }

    /// Returns a `PATH` that finds `git` but no `gh`: a directory holding only a link to `git`.
    fn path_without_gh(directory: &Path) -> eyre::Result<OsString> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let git = std::env::split_paths(&path)
            .map(|dir| dir.join("git"))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| eyre::eyre!("git is not on PATH"))?;
        std::os::unix::fs::symlink(git, directory.join("git"))?;
        Ok(directory.as_os_str().to_owned())
    }

    /// Returns a `PATH` whose `gh` prints `json` for any command, ahead of the inherited `PATH`.
    fn path_with_fake_gh(directory: &Path, json: &str) -> eyre::Result<OsString> {
        let gh = directory.join("gh");
        std::fs::write(
            &gh,
            formatdoc!(
                "
                    #!/bin/sh
                    cat <<'JSON'
                    {json}
                    JSON
                "
            ),
        )?;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))?;
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let dirs =
            std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&inherited));
        Ok(std::env::join_paths(dirs)?)
    }

    /// What a fake `gh pr view` reports for pull request 7 of `example.invalid/owner/repo`.
    fn pull_request_json(state: &str) -> String {
        format!(
            r#"{{"number":7,"url":"https://example.invalid/owner/repo/pull/7","state":"{state}","headRefName":"feature","baseRefName":"main","title":"Feature","body":"","author":{{"login":"someone"}},"labels":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z"}}"#
        )
    }

    /// `cr open` without a target and without `gh` reviews the default branch against the current
    /// branch, says why on standard error, and uses the key an explicit range gets.
    #[test]
    fn falls_back_to_the_default_branch_range() -> eyre::Result<()> {
        let repo_dir = repo_with_change(&[("a.txt", "a\n")], &[("a.txt", "b\n")])?;
        let (data_dir, bin_dir) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let (repo, data) = (repo_dir.path(), data_dir.path());
        let path = path_without_gh(bin_dir.path())?;

        let (opened, stderr) = cr_json_with_path(repo, data, &path, &["open"])?;
        assert_eq!(
            stderr.trim(),
            "cr: reviewing main..feature: gh is not installed"
        );
        let (explicit, _) = cr_json_with_path(repo, data, &path, &["open", "main..feature"])?;
        assert_eq!(opened["review"], explicit["review"]);

        // A second open without a target returns to the remembered review.
        let (again, stderr) = cr_json_with_path(repo, data, &path, &["open"])?;
        assert_eq!(
            stderr.trim(),
            "cr: reopened the review last opened on this branch"
        );
        assert_eq!(again["review"], opened["review"]);
        Ok(())
    }

    /// `cr open` without a target reviews the branch's open pull request under the same key as
    /// opening it by number.
    #[test]
    fn reviews_the_branch_pull_request() -> eyre::Result<()> {
        let repo_dir = repo_with_change(&[("a.txt", "a\n")], &[("a.txt", "b\n")])?;
        let (data_dir, bin_dir, forge_dir) = (
            tempfile::tempdir()?,
            tempfile::tempdir()?,
            tempfile::tempdir()?,
        );
        let (repo, data) = (repo_dir.path(), data_dir.path());

        // The remote's path ends in the pull request's host, owner and repository, so the engine
        // fetches the pull request's refs from it instead of the network.
        let remote = forge_dir.path().join("example.invalid/owner/repo");
        std::fs::create_dir_all(&remote)?;
        git(&remote, &["init", "--quiet", "--bare"])?;
        let remote_url = remote.display().to_string();
        git(repo, &["remote", "add", "origin", &remote_url])?;
        git(
            repo,
            &[
                "push",
                "--quiet",
                "origin",
                "main",
                "feature:refs/pull/7/head",
            ],
        )?;
        let path = path_with_fake_gh(bin_dir.path(), &pull_request_json("OPEN"))?;

        let (opened, stderr) = cr_json_with_path(repo, data, &path, &["open"])?;
        assert_eq!(
            stderr.trim(),
            "cr: reviewing pull request #7 from feature (https://example.invalid/owner/repo/pull/7)"
        );
        assert_eq!(opened["recorded"]["number"], 1);
        let review = opened["review"].as_str().unwrap_or_default();
        assert!(
            review.starts_with("example.invalid/owner/repo#7@"),
            "{review}"
        );
        let (by_number, _) = cr_json_with_path(repo, data, &path, &["open", "7"])?;
        assert_eq!(by_number["review"], opened["review"]);
        Ok(())
    }

    /// A merged pull request is not reviewed; the default branch range is, and stderr says why.
    #[test]
    fn skips_a_merged_pull_request() -> eyre::Result<()> {
        let repo_dir = repo_with_change(&[("a.txt", "a\n")], &[("a.txt", "b\n")])?;
        let (data_dir, bin_dir) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let (repo, data) = (repo_dir.path(), data_dir.path());
        let path = path_with_fake_gh(bin_dir.path(), &pull_request_json("MERGED"))?;

        let (_, stderr) = cr_json_with_path(repo, data, &path, &["open"])?;
        assert_eq!(
            stderr.trim(),
            "cr: reviewing main..feature: its pull request #7 is merged"
        );
        Ok(())
    }
}

/// `--demo-dir` puts the demo a command acts on there, as it does the app's, so a QA run's demo
/// never resets the default one.
#[test]
fn commands_use_the_demo_directory() -> eyre::Result<()> {
    let demo = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_cr"))
        .args(["--demo", "--demo-dir"])
        .arg(demo.path())
        .args(["--json", "status"])
        .env_remove("CODE_REVIEW_DEMO_DIR")
        .env("XDG_CONFIG_HOME", config_home(demo.path()))
        .env_remove("CODE_REVIEW_CONFIG")
        .envs(GIT_ENV)
        .output()
        .wrap_err("failed to run cr")?;
    eyre::ensure!(
        output.status.success(),
        "cr failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(demo.path().join("repo").is_dir());
    assert!(demo.path().join("data").is_dir());
    Ok(())
}
