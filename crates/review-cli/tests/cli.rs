//! End-to-end tests of the `cr` binary against a temporary repository.

use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{self, WrapErr as _};
use indoc::{formatdoc, indoc};
use serde_json::Value;

/// Isolates the tests' git commands, and `cr`'s, from the user's and the system's
/// configuration, with the workspace's `tests/gitconfig` in place of the user's, and pins the
/// commit identity.
const GIT_ENV: [(&str, &str); 6] = [
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

/// Runs `cr` with `args` in `repo`, as [`cr`] does, expecting it to fail, and returns standard
/// error.
fn cr_fails(repo: &Path, data: &Path, args: &[&str]) -> eyre::Result<String> {
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
        !output.status.success(),
        "cr {} succeeded: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(String::from_utf8(output.stderr)?)
}

fn cr_json(repo: &Path, data: &Path, args: &[&str]) -> eyre::Result<Value> {
    let mut args = args.to_vec();
    args.push("--json");
    let output = cr(repo, data, &args)?;
    serde_json::from_str(&output)
        .wrap_err_with(|| format!("cr {} printed invalid JSON: {output}", args.join(" ")))
}

/// Runs `cr` with `args` on the review `key`, as [`cr`] does.
fn cr_in(repo: &Path, data: &Path, key: &str, args: &[&str]) -> eyre::Result<String> {
    cr(repo, data, &[args, &["--review", key]].concat())
}

/// Runs `cr --json` with `args` on the review `key`, as [`cr_json`] does.
fn cr_json_in(repo: &Path, data: &Path, key: &str, args: &[&str]) -> eyre::Result<Value> {
    cr_json(repo, data, &[args, &["--review", key]].concat())
}

/// Opens the review of `main..feature` and returns its key, which commands without
/// `--review` would not act on: they act on the checkout's review.
fn open_range(repo: &Path, data: &Path) -> eyre::Result<String> {
    let opened = cr_json(repo, data, &["open", "main..feature"])?;
    opened
        .get("review")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| eyre::eyre!("no review key in {opened}"))
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
    let key = opened["review"].as_str().unwrap_or_default().to_owned();
    assert_eq!(opened["recorded"]["number"], 1);
    assert_eq!(opened["status"]["total"]["units"], 3);
    let status = cr_json_in(repo, data, &key, &["status"])?;
    assert_eq!(status["total"]["new"], 3);
    assert_eq!(status["total"]["noise"], 1);

    // Show lists every unit once; the lockfile lands in the noise step.
    let shown = cr_json_in(repo, data, &key, &["show"])?;
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
    let tag = shown["tag"].as_str().unwrap_or_default();
    let code_units: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| *id != format!("u1-0000@{tag}"))
        .collect();
    let marked = cr_json_in(
        repo,
        data,
        &key,
        &[&["mark"], code_units.as_slice(), &["flagged"]].concat(),
    )?;
    assert_eq!(marked["marked"].as_array().map(Vec::len), Some(2));
    cr_json_in(
        repo,
        data,
        &key,
        &["mark", "--category", "noise", "--revision", "1", "seen"],
    )?;
    cr_json_in(
        repo,
        data,
        &key,
        &["note", code_units[0], "is", "ten", "right?"],
    )?;
    let status = cr_json_in(repo, data, &key, &["status"])?;
    assert_eq!(status["total"]["flagged"], 2);
    assert_eq!(status["total"]["reviewed"], 1);
    let unreviewed = cr_json_in(repo, data, &key, &["show", "--unreviewed"])?;
    assert_eq!(unreviewed["steps"].as_array().map(Vec::len), Some(0));

    // Export, as Markdown and JSON.
    let markdown = cr_in(repo, data, &key, &["export"])?;
    assert!(
        markdown.contains("1. **flagged** `lib.rs:2-2`"),
        "{markdown}"
    );
    assert!(markdown.contains("is ten right?"), "{markdown}");
    let exported = cr_json_in(repo, data, &key, &["export", "--format", "json"])?;
    assert_eq!(exported["items"].as_array().map(Vec::len), Some(2));

    // Walkthrough round trip.
    let prompt = cr_json_in(repo, data, &key, &["walkthrough", "prompt"])?;
    assert!(
        prompt["prompt"]
            .as_str()
            .is_some_and(|text| text.contains("u1-0001"))
    );
    let walkthrough = serde_json::json!({
        "version": 1, "review": key, "revision": 1, "title": "Tour",
        "steps": [{"title": "Values", "why": "the point", "units": code_units}],
    });
    let file = repo.join("walkthrough.json");
    std::fs::write(&file, walkthrough.to_string())?;
    cr_json_in(
        repo,
        data,
        &key,
        &["walkthrough", "import", &file.display().to_string()],
    )?;
    let partition = cr_json_in(repo, data, &key, &["walkthrough", "show"])?;
    assert_eq!(partition["steps"][0]["title"], "Values");

    // Watch records nothing new.
    cr_in(
        repo,
        data,
        &key,
        &["watch", "--count", "1", "--interval", "0"],
    )?;
    Ok(())
}

/// `cr drop` removes a review by its key once its checkout is gone, after which the key names
/// no review, and refuses a key that names none.
#[test]
fn drop_removes_a_review_whose_checkout_is_gone() -> eyre::Result<()> {
    let repo_dir = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let elsewhere = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    git(repo, &["init", "--quiet", "--initial-branch=main"])?;
    std::fs::write(repo.join("lib.rs"), BASE_LIB)?;
    git(repo, &["add", "--all"])?;
    git(repo, &["commit", "--quiet", "-m", "base"])?;
    git(repo, &["checkout", "--quiet", "-b", "feature"])?;
    std::fs::write(repo.join("lib.rs"), FEATURE_LIB)?;
    git(repo, &["commit", "--quiet", "--all", "-m", "feature"])?;
    let key = open_range(repo, data)?;
    drop(repo_dir);

    let dropped = cr_json(elsewhere.path(), data, &["drop", &key])?;
    assert_eq!(dropped["dropped"], key.as_str());
    let refused = cr_fails(elsewhere.path(), data, &["status", "--review", &key])?;
    assert!(refused.contains("no review"), "{refused}");
    let refused = cr_fails(elsewhere.path(), data, &["drop", &key])?;
    assert!(refused.contains("no review"), "{refused}");
    Ok(())
}

/// At its limit, cr's data takes no capture, and `cr show` on the checkout shows the last
/// recorded revision, saying so, rather than failing.
#[cfg(unix)]
#[test]
fn show_at_the_limit_shows_the_recorded_revision() -> eyre::Result<()> {
    let repo_dir = repo_with_change(&[("lib.rs", BASE_LIB)], &[("lib.rs", FEATURE_LIB)])?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    let recorded = cr_json(repo, data, &["status"])?;
    // A sparse file as large as the limit counts, though it takes no space on disk.
    std::fs::File::create(data.join("filler"))?.set_len(10 << 30)?;

    let output = Command::new(env!("CARGO_BIN_EXE_cr"))
        .args(["status", "--json", "-C"])
        .arg(repo)
        .env("CR_DATA_DIR", data)
        .env("XDG_CONFIG_HOME", config_home(data))
        .env_remove("CODE_REVIEW_CONFIG")
        .envs(GIT_ENV)
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("showing the last recorded revision"),
        "{stderr}"
    );
    let shown: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(shown["revision"], recorded["revision"]);
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
    let key = open_range(repo, data)?;

    // Without a configuration file, the re-wrap is approved automatically and the edit is open.
    let status = cr_json_in(repo, data, &key, &["status"])?;
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
    let status = cr_json_in(repo, data, &key, &["status"])?;
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
    let status = cr_json_in(repo, data, &key, &["status"])?;
    assert_eq!(status["total"]["auto_approved"], 1);
    let status = cr_json_in(repo, data, &key, &["status", "--no-auto-approve"])?;
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
    let key = open_range(repo, data)?;
    let shown = cr_in(repo, data, &key, &["show", "--color", "never"])?;
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
    let key = open_range(repo, data)?;
    let shown = cr_json_in(repo, data, &key, &["show"])?;
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
    let key = open_range(repo, data)?;
    let first_head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .envs(GIT_ENV)
            .output()?
            .stdout,
    )?;
    std::fs::write(repo.join("a.txt"), "c\n")?;
    git(repo, &["commit", "--quiet", "--all", "-m", "second"])?;
    cr(repo, data, &["open", "main..feature"])?;
    cr_in(repo, data, &key, &["mark", "u2-0000", "seen"])?;
    git(repo, &["reset", "--quiet", "--hard", first_head.trim()])?;
    let watched = cr_in(
        repo,
        data,
        &key,
        &["watch", "--count", "1", "--interval", "0"],
    )?;
    assert!(watched.contains("New revision 3"), "{watched}");
    let status = cr_json_in(repo, data, &key, &["status"])?;
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
    let key = open_range(repo, data)?;

    // A directory covers both files below it; the top-level file stays new.
    let marked = cr_json_in(
        repo,
        data,
        &key,
        &["mark", "--file", "src", "--revision", "1", "accepted"],
    )?;
    assert_eq!(marked["marked"].as_array().map(Vec::len), Some(2));
    let status = cr_json_in(repo, data, &key, &["status"])?;
    assert_eq!(status["total"]["accepted"], 2);
    assert_eq!(status["total"]["new"], 1);

    // A path without changes fails instead of marking nothing.
    assert!(
        cr_in(
            repo,
            data,
            &key,
            &["mark", "--file", "docs", "--revision", "1", "seen"]
        )
        .is_err()
    );
    Ok(())
}

/// `cr open` without a target opens this checkout's review, which records the working tree
/// against the default branch, and commands without `--review` act on it; a review opened by
/// name needs `--review`.
#[test]
fn commands_without_a_review_act_on_the_checkout() -> eyre::Result<()> {
    let repo_dir = repo_with_change(&[("a.txt", "a\n")], &[("a.txt", "b\n")])?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    std::fs::write(repo.join("a.txt"), "c\n")?;

    let opened = cr_json(repo, data, &["open"])?;
    let key = opened["review"].as_str().unwrap_or_default().to_owned();
    assert!(key.contains(":WORKTREE@"), "{key}");
    assert_eq!(opened["recorded"]["number"], 1);
    assert_eq!(opened["status"]["total"]["units"], 1);
    let again = cr_json(repo, data, &["open"])?;
    assert_eq!(again["review"], opened["review"]);
    assert_eq!(again["recorded"]["number"], 1);

    let range = open_range(repo, data)?;
    assert_ne!(range, key);
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["revision"], 1);
    let shown = cr_json(repo, data, &["show"])?;
    assert!(shown.to_string().contains("\"c\\n\""), "{shown}");
    Ok(())
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

/// Returns the IDs of the units `cr show --json` printed.
fn shown_ids(shown: &Value) -> Vec<String> {
    shown
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|step| step.get("units").and_then(Value::as_array))
        .flatten()
        .filter_map(|unit| unit.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// `cr show` records what the checkout holds and prints its unit IDs qualified with the review's
/// tag, which `cr mark` and `cr note` take back without `--review`.
#[test]
fn qualified_ids_round_trip_from_show_to_mark() -> eyre::Result<()> {
    let repo_dir = repo_with_change(&[("a.txt", "a\n")], &[("a.txt", "b\n")])?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());

    // Showing the checkout's review records its first revision.
    let shown = cr_json(repo, data, &["show"])?;
    let tag = shown["tag"].as_str().unwrap_or_default().to_owned();
    assert_eq!(tag.len(), 8, "{shown}");
    assert_eq!(shown["revision"], 1);
    assert_eq!(shown["subject"]["branch"], "feature");
    assert_eq!(shown["subject"]["base"]["name"], "main");
    let ids = shown_ids(&shown);
    assert_eq!(ids, [format!("u1-0000@{tag}")]);

    // The qualified ID marks and notes the unit of this review.
    let marked = cr_json(repo, data, &["mark", &ids[0], "accepted"])?;
    assert_eq!(marked["marked"], serde_json::json!(ids));
    cr(repo, data, &["note", &ids[0], "fine"])?;
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["tag"], tag.as_str());
    assert_eq!(status["total"]["accepted"], 1);

    // The text rendering prints the same qualified IDs and the subject.
    let text = cr(repo, data, &["show", "--color", "never"])?;
    assert!(text.contains(&format!("revision 1@{tag} ")), "{text}");
    assert!(
        text.contains(&format!("  u1-0000@{tag} [accepted]")),
        "{text}"
    );
    assert!(text.starts_with("feature → main\n"), "{text}");
    Ok(())
}

/// A unit ID qualified with another review's tag is refused, whichever review the command acts
/// on, and a bare ID only with `--review`.
#[test]
fn mark_refuses_foreign_tags_and_bare_ids() -> eyre::Result<()> {
    let repo_dir = repo_with_change(&[("a.txt", "a\n")], &[("a.txt", "b\n")])?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    let range = open_range(repo, data)?;
    let range_tag = cr_json_in(repo, data, &range, &["show"])?["tag"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let checkout_tag = cr_json(repo, data, &["show"])?["tag"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert_ne!(range_tag, checkout_tag);

    // The range's ID on the checkout's review, and the checkout's on the range's
    let error = cr_fails(
        repo,
        data,
        &["mark", &format!("u1-0000@{range_tag}"), "seen"],
    )?;
    assert!(error.contains("belongs to another review"), "{error}");
    let error = cr_fails(
        repo,
        data,
        &[
            "mark",
            &format!("u1-0000@{checkout_tag}"),
            "seen",
            "--review",
            &range,
        ],
    )?;
    assert!(error.contains("belongs to another review"), "{error}");
    let error = cr_fails(
        repo,
        data,
        &["note", &format!("u1-0000@{range_tag}"), "why?"],
    )?;
    assert!(error.contains("belongs to another review"), "{error}");

    // A bare ID names no review: refused with the qualified form, taken with --review.
    let error = cr_fails(repo, data, &["mark", "u1-0000", "seen"])?;
    assert!(error.contains("u1-0000@<tag>"), "{error}");
    assert!(cr_fails(repo, data, &["note", "u1-0000", "why?"])?.contains("u1-0000@<tag>"));
    cr_in(repo, data, &range, &["mark", "u1-0000", "seen"])?;
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["total"]["reviewed"], 0);
    let status = cr_json_in(repo, data, &range, &["status"])?;
    assert_eq!(status["total"]["reviewed"], 1);
    Ok(())
}

/// Marks of files go on the revision the caller saw, `--revision N@<tag>`, and are refused once
/// the checkout recorded a newer one, or without the revision.
#[test]
fn file_marks_refuse_a_stale_revision() -> eyre::Result<()> {
    let repo_dir = repo_with_change(&[("a.txt", "a\n")], &[("a.txt", "b\n")])?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    let tag = cr_json(repo, data, &["show"])?["tag"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let first = format!("1@{tag}");

    // Without the revision, and with a bare one without --review
    let error = cr_fails(repo, data, &["mark", "--file", "a.txt", "seen"])?;
    assert!(error.contains("--revision"), "{error}");
    let error = cr_fails(
        repo,
        data,
        &["mark", "--file", "a.txt", "--revision", "1", "seen"],
    )?;
    assert!(error.contains("1@<tag>"), "{error}");

    // An edit shown since records revision 2, so revision 1 is stale.
    std::fs::write(repo.join("a.txt"), "c\n")?;
    let shown = cr_json(repo, data, &["show"])?;
    assert_eq!(shown["revision"], 2);
    let error = cr_fails(
        repo,
        data,
        &["mark", "--file", "a.txt", "--revision", &first, "seen"],
    )?;
    assert!(error.contains("revision 1 is not current"), "{error}");
    let error = cr_fails(
        repo,
        data,
        &["mark", "--category", "noise", "--revision", &first, "seen"],
    )?;
    assert!(error.contains("revision 1 is not current"), "{error}");
    let marked = cr_json(
        repo,
        data,
        &[
            "mark",
            "--file",
            "a.txt",
            "--revision",
            &format!("2@{tag}"),
            "seen",
        ],
    )?;
    assert_eq!(
        marked["marked"],
        serde_json::json!([format!("u2-0000@{tag}")])
    );
    Ok(())
}

/// `cr show` on a checkout in the middle of a merge records nothing and says why.
#[test]
fn show_refuses_a_busy_checkout() -> eyre::Result<()> {
    let repo_dir = repo_with_change(&[("a.txt", "a\n")], &[("a.txt", "b\n")])?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    std::fs::write(repo.join(".git/MERGE_HEAD"), "")?;
    let error = cr_fails(repo, data, &["show"])?;
    assert!(
        error.contains("git is busy: merge in progress; try again when it finishes"),
        "{error}"
    );
    std::fs::remove_file(repo.join(".git/MERGE_HEAD"))?;
    let status = cr_json(repo, data, &["status"])?;
    assert_eq!(status["revision"], 1);
    Ok(())
}

/// A clean `main` level with `origin/main` shows an empty revision that says so.
#[test]
fn show_on_a_clean_main_says_nothing_is_to_review() -> eyre::Result<()> {
    let repo_dir = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    git(repo, &["init", "--quiet", "--initial-branch=main"])?;
    std::fs::write(repo.join("a.txt"), "a\n")?;
    git(repo, &["add", "--all"])?;
    git(repo, &["commit", "--quiet", "-m", "base"])?;
    // A remote-tracking branch without a configured remote, so no pull request is looked up.
    git(repo, &["update-ref", "refs/remotes/origin/main", "HEAD"])?;

    let shown = cr(repo, data, &["show", "--color", "never"])?;
    assert!(shown.starts_with("main → origin/main\n"), "{shown}");
    assert!(
        shown.contains(
            "main is level with origin/main. Nothing to review; changes appear here as you make \
             them."
        ),
        "{shown}"
    );
    let json = cr_json(repo, data, &["show"])?;
    assert_eq!(json["steps"], serde_json::json!([]));
    assert_eq!(json["subject"]["base"]["kind"], "remote_default");
    Ok(())
}

/// `cr` run from elsewhere, with a relative data directory and `GIT_*` variables that point at
/// another repository, index, object database and configuration file, reviews the checkout `-C`
/// names as its clone has git read it, a conditional include on its git directory included,
/// stores the review where it was told, and leaves every file of the checkout's git directory as
/// it was.
#[test]
fn cr_run_from_elsewhere_reads_only_the_checkout() -> eyre::Result<()> {
    let parent = tempfile::tempdir()?;
    let (repo, other) = (parent.path().join("checkout"), parent.path().join("other"));
    for dir in [&repo, &other] {
        std::fs::create_dir(dir)?;
        git(dir, &["init", "--quiet", "--initial-branch=main"])?;
        std::fs::write(dir.join("a.txt"), "a\n")?;
        git(dir, &["add", "--all"])?;
        git(dir, &["commit", "--quiet", "-m", "base"])?;
    }
    // In the clone, a conditional include on its git directory has git ignore `secret.txt`.
    let excludes = parent.path().join("excludes");
    std::fs::write(&excludes, "secret.txt\n")?;
    let included = parent.path().join("included.config").display().to_string();
    let excludes = excludes.display().to_string();
    git(
        &repo,
        &[
            "config",
            "--file",
            &included,
            "core.excludesFile",
            &excludes,
        ],
    )?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--absolute-git-dir"])
        .envs(GIT_ENV)
        .output()?;
    let git_dir = String::from_utf8(output.stdout)?.trim().to_owned();
    git(
        &repo,
        &[
            "config",
            &format!("includeIf.gitdir:{git_dir}.path"),
            &included,
        ],
    )?;
    std::fs::write(repo.join("new.txt"), "new\n")?;
    std::fs::write(repo.join("secret.txt"), "secret\n")?;
    let before = files(&repo.join(".git"))?;

    let other_git = other.join(".git");
    let output = Command::new(env!("CARGO_BIN_EXE_cr"))
        .current_dir(parent.path())
        .args(["-C", "checkout", "--data-dir", "data", "show", "--json"])
        .env("GIT_DIR", &other_git)
        .env("GIT_INDEX_FILE", other_git.join("index"))
        .env("GIT_OBJECT_DIRECTORY", other_git.join("objects"))
        .env("GIT_CONFIG", repo.join(".git/config"))
        .env("XDG_CONFIG_HOME", config_home(parent.path()))
        .env_remove("CR_DATA_DIR")
        .env_remove("CODE_REVIEW_CONFIG")
        .envs(GIT_ENV)
        .output()?;
    eyre::ensure!(
        output.status.success(),
        "cr show failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown = String::from_utf8(output.stdout)?;
    assert!(shown.contains("new.txt"), "{shown}");
    assert!(!shown.contains("secret.txt"), "{shown}");
    assert!(parent.path().join("data/reviews").is_dir());
    assert_eq!(files(&repo.join(".git"))?, before);
    Ok(())
}

/// Every file under `dir`, with its bytes and when it was last modified, sorted.
fn files(dir: &Path) -> eyre::Result<Vec<(PathBuf, Vec<u8>, std::time::SystemTime)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            found.extend(files(&entry.path())?);
        } else {
            let modified = entry.metadata()?.modified()?;
            found.push((entry.path(), std::fs::read(entry.path())?, modified));
        }
    }
    found.sort();
    Ok(found)
}
