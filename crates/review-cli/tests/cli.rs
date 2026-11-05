//! End-to-end tests of the `cr` binary against a temporary repository.

use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{self, WrapErr as _};
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

/// Runs `cr` with `args` in `repo`, storing reviews under `data`, and returns standard output.
fn cr(repo: &Path, data: &Path, args: &[&str]) -> eyre::Result<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_cr"))
        .args(args)
        .arg("-C")
        .arg(repo)
        .env("CR_DATA_DIR", data)
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

/// Opens a review, marks, notes, exports and imports a walkthrough through the CLI.
#[test]
fn every_command_works_with_json() -> eyre::Result<()> {
    let repo_dir = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let (repo, data) = (repo_dir.path(), data_dir.path());
    git(repo, &["init", "--quiet", "--initial-branch=main"])?;
    std::fs::write(
        repo.join("lib.rs"),
        "fn a() -> u32 {\n    1\n}\n\nfn b() -> u32 {\n    2\n}\n",
    )?;
    std::fs::write(repo.join("Cargo.lock"), "version = 3\n")?;
    git(repo, &["add", "--all"])?;
    git(repo, &["commit", "--quiet", "-m", "base"])?;
    git(repo, &["checkout", "--quiet", "-b", "feature"])?;
    std::fs::write(
        repo.join("lib.rs"),
        "fn a() -> u32 {\n    10\n}\n\nfn b() -> u32 {\n    20\n}\n",
    )?;
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

/// `cr show` renders a move with edits expanded, with the inner diff showing the edited line.
#[test]
fn show_renders_inner_diff_of_move_with_edits() -> eyre::Result<()> {
    let function = "fn moved_function(input: u32) -> u32 {\n    let doubled = input * 2;\n    let shifted = doubled + 7;\n    let masked = shifted & 0xff;\n    let scaled = masked * 3;\n    scaled - 1\n}\n";
    let edited = function.replace("0xff", "0x0f");
    let repo_dir = repo_with_change(
        &[
            (
                "a.rs",
                &format!("fn keep() {{}}\n{function}fn tail() {{}}\n"),
            ),
            ("b.rs", "fn other() {}\n"),
        ],
        &[
            ("a.rs", "fn keep() {}\nfn tail() {}\n"),
            ("b.rs", &format!("fn other() {{}}\n{edited}")),
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
