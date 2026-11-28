//! The review of a checkout: one review per working tree and clone that follows whatever is
//! checked out, compared with the base its subject resolves to on every capture.

use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{self, WrapErr as _};
use googletest::prelude::*;
use indoc::formatdoc;
use review_core::github::{BranchPullRequest, CheckoutPullRequest, PullRequestState, Repository};
use review_core::{
    BaseRef, Busy, Capture, Error, LiveCapture, MarkVerdict, ObjectId, Repo, Review, UnitState,
};

use crate::common::{GIT_ENV, TestRepo, find, summaries};

/// Three functions apart from each other, with the bodies given.
fn three_functions(a: &str, b: &str, c: &str) -> String {
    formatdoc! {"
        fn a() {{ {a} }}

        fn keep_one() {{}}

        fn b() {{ {b} }}

        fn keep_two() {{}}

        fn c() {{ {c} }}
    "}
}

/// Runs git in `dir` and returns its trimmed standard output.
fn git_at(dir: &Path, args: &[&str]) -> eyre::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(GIT_ENV)
        .output()
        .wrap_err_with(|| format!("failed to run git {}", args.join(" ")))?;
    eyre::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// A repository with `lib.rs` committed on `main` and pushed to `origin`, a bare repository at
/// a path ending in `owner/repo.git`, which the returned directory holds: `origin` is
/// `https://example.invalid/owner/repo.git`, which `insteadOf` sends to that path.
fn with_remote() -> eyre::Result<(TestRepo, tempfile::TempDir)> {
    let test = TestRepo::new()?;
    test.write("lib.rs", &three_functions("", "", ""))?;
    test.commit("base")?;
    let forge = tempfile::tempdir()?;
    let remote = forge.path().join("example.invalid/owner/repo.git");
    std::fs::create_dir_all(&remote)?;
    git_at(
        &remote,
        &["init", "--quiet", "--bare", "--initial-branch=main"],
    )?;
    test.git(&[
        "config",
        &format!("url.{}.insteadOf", remote.display()),
        "https://example.invalid/owner/repo.git",
    ])?;
    test.git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/owner/repo.git",
    ])?;
    test.git(&["push", "--quiet", "--set-upstream", "origin", "main"])?;
    Ok((test, forge))
}

/// Opens the checkout review of `test`.
fn open(test: &TestRepo) -> eyre::Result<Review> {
    Ok(Review::open_checkout(
        test.data_dir(),
        Repo::discover(test.path())?,
    )?)
}

/// Captures the working tree with the pull requests `found` for branches.
fn capture_with(review: &Review, found: &[CheckoutPullRequest]) -> eyre::Result<LiveCapture> {
    match review.live_history(found)? {
        Capture::Captured(captured) => Ok(*captured),
        Capture::Busy(busy) => eyre::bail!("git is busy: {busy}"),
        Capture::Moved => eyre::bail!("the repository moved during the capture"),
        Capture::Replaced => eyre::bail!("the clone was replaced"),
    }
}

/// Captures the working tree, without a pull request.
fn capture(review: &Review) -> eyre::Result<LiveCapture> {
    capture_with(review, &[])
}

/// The state of the one unit of the capture's current revision that adds `needle`.
fn state_of(captured: &LiveCapture, needle: &str) -> eyre::Result<UnitState> {
    let history = &captured.history;
    let current = history
        .current()
        .ok_or_else(|| eyre::eyre!("no revision"))?;
    Ok(find(&summaries(history, current)?, needle)?.state)
}

/// How many units the capture's current revision has.
fn unit_count(captured: &LiveCapture) -> eyre::Result<usize> {
    Ok(captured.history.current_view()?.unit_ids().len())
}

/// Marks every unit of the capture's current revision accepted, recording it first.
fn accept_all(review: &Review, captured: &LiveCapture) -> eyre::Result<()> {
    let view = captured.history.current_view()?;
    review.mark(&view, &view.unit_ids(), MarkVerdict::Accepted)?;
    Ok(())
}

/// One checkout review follows the reviewer from a clean default branch through an edit, a
/// commit, a new branch, a rename, unrelated work and back to the default branch once the work
/// landed upstream, keeping marks wherever the code is the code that was marked.
#[gtest]
fn one_review_follows_the_checkout_through_a_branch_lifecycle() -> eyre::Result<()> {
    let (test, forge) = with_remote()?;
    let review = open(&test)?;

    // A clean default branch level with its remote has nothing to review, and says so.
    let clean = capture(&review)?;
    expect_that!(unit_count(&clean)?, eq(0));
    expect_that!(clean.subject.branch.as_deref(), some(eq("main")));
    expect_that!(
        clean.subject.base,
        matches_pattern!(BaseRef::RemoteDefault {
            name: eq("origin/main"),
            ..
        })
    );

    // An edit shows, and marking it records it.
    test.write("lib.rs", &three_functions("one();", "", ""))?;
    let edited = capture(&review)?;
    expect_that!(state_of(&edited, "one();")?, eq(UnitState::New));
    accept_all(&review, &edited)?;

    // Committing it changes the snapshots, not the code: the mark holds.
    test.commit("one")?;
    let committed = capture(&review)?;
    expect_that!(
        state_of(&committed, "one();")?,
        matches_pattern!(UnitState::Reviewed { .. })
    );

    // So does moving the work to a new branch, with the default branch put back.
    test.git(&["checkout", "--quiet", "-b", "feat"])?;
    test.git(&["branch", "--force", "main", "origin/main"])?;
    let branched = capture(&review)?;
    expect_that!(branched.subject.branch.as_deref(), some(eq("feat")));
    expect_that!(
        state_of(&branched, "one();")?,
        matches_pattern!(UnitState::Reviewed { .. })
    );

    // And renaming the branch; a mark there records the revision with what it was taken of.
    test.git(&["branch", "--move", "feat", "feature"])?;
    let renamed = capture(&review)?;
    expect_that!(renamed.subject.branch.as_deref(), some(eq("feature")));
    expect_that!(
        state_of(&renamed, "one();")?,
        matches_pattern!(UnitState::Reviewed { .. })
    );
    accept_all(&review, &renamed)?;
    let recorded = review.history()?;
    let revisions = recorded.revisions();
    let last = revisions
        .last()
        .ok_or_else(|| eyre::eyre!("nothing recorded"))?;
    expect_that!(last.branch.as_deref(), some(eq("feature")));
    expect_that!(
        last.head_commit.as_ref().map(ObjectId::as_str),
        some(eq(test.git(&["rev-parse", "HEAD"])?.as_str()))
    );

    // Unrelated work on another branch is new.
    test.git(&["checkout", "--quiet", "-b", "other", "origin/main"])?;
    test.write("other.rs", "fn other() { two(); }\n")?;
    let other = capture(&review)?;
    expect_that!(state_of(&other, "two();")?, eq(UnitState::New));
    expect_that!(unit_count(&other)?, eq(1));
    test.commit("other")?;

    // The work lands upstream as a squash merge; back on the default branch, brought up to
    // date, there is nothing left to review.
    test.git(&["checkout", "--quiet", "feature"])?;
    test.git(&["push", "--quiet", "origin", "feature"])?;
    let upstream = forge.path().join("upstream");
    let remote = forge.path().join("example.invalid/owner/repo.git");
    git_at(
        forge.path(),
        &[
            "clone",
            "--quiet",
            &remote.display().to_string(),
            &upstream.display().to_string(),
        ],
    )?;
    git_at(
        &upstream,
        &["merge", "--quiet", "--squash", "origin/feature"],
    )?;
    git_at(
        &upstream,
        &["commit", "--quiet", "--no-verify", "-m", "squash"],
    )?;
    git_at(&upstream, &["push", "--quiet", "origin", "main"])?;
    test.git(&["fetch", "--quiet", "origin"])?;
    test.git(&["checkout", "--quiet", "main"])?;
    test.git(&["merge", "--quiet", "--ff-only", "origin/main"])?;
    let landed = capture(&review)?;
    expect_that!(unit_count(&landed)?, eq(0));
    expect_that!(landed.subject.branch.as_deref(), some(eq("main")));
    Ok(())
}

/// A repository whose branch is unborn shows its files against the empty tree.
#[gtest]
fn an_unborn_branch_is_reviewed_against_the_empty_tree() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("lib.rs", "fn first() {}\n")?;
    let review = open(&test)?;
    let captured = capture(&review)?;
    expect_that!(captured.subject.head, none());
    expect_that!(captured.subject.branch.as_deref(), some(eq("main")));
    expect_that!(
        captured.subject.base,
        eq(&BaseRef::Empty { unrelated: None })
    );
    expect_that!(state_of(&captured, "fn first()")?, eq(UnitState::New));

    // Recording it works the same way, and says what it was taken of.
    let recorded = review.refresh()?;
    expect_that!(
        recorded.subject.map(|subject| subject.base),
        some(eq(&BaseRef::Empty { unrelated: None }))
    );
    Ok(())
}

/// Without a default branch to compare with, the empty tree is the base too.
#[gtest]
fn no_default_branch_compares_with_the_empty_tree() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("lib.rs", "fn first() {}\n")?;
    test.commit("first")?;
    test.git(&["branch", "--move", "main", "trunk"])?;
    let review = open(&test)?;
    let captured = capture(&review)?;
    expect_that!(
        captured.subject.base,
        eq(&BaseRef::Empty { unrelated: None })
    );
    expect_that!(state_of(&captured, "fn first()")?, eq(UnitState::New));
    Ok(())
}

/// In a shallow clone whose history does not reach a commit shared with the default branch, a
/// capture says the history is shallow rather than that the two are unrelated, and fetches
/// nothing to find out.
#[gtest]
fn a_shallow_clone_says_its_history_is_shallow() -> eyre::Result<()> {
    let (test, forge) = with_remote()?;
    let remote = forge.path().join("example.invalid/owner/repo.git");
    let clones = tempfile::tempdir()?;
    let clone = clones.path().join("clone");
    git_at(
        clones.path(),
        &[
            "clone",
            "--quiet",
            "--depth=1",
            &format!("file://{}", remote.display()),
            "clone",
        ],
    )?;
    // The default branch moves on, and the clone fetches only its tip.
    test.write("other.rs", "fn other() {}\n")?;
    test.commit("more")?;
    test.git(&["push", "--quiet", "origin", "main"])?;
    git_at(&clone, &["fetch", "--quiet", "--depth=1", "origin", "main"])?;
    std::fs::write(clone.join("lib.rs"), three_functions("one();", "", ""))?;
    let shallow_before = std::fs::read_to_string(clone.join(".git/shallow"))?;

    let data = tempfile::tempdir()?;
    let review = Review::open_checkout(data.path(), Repo::discover(&clone)?)?;
    let captured = capture(&review)?;
    expect_that!(
        captured.subject.base,
        eq(&BaseRef::Shallow {
            name: "origin/main".to_owned()
        })
    );
    expect_that!(
        captured.subject.summary(),
        eq("main → empty tree (history is shallow; fetch more to compare with origin/main)")
    );
    expect_that!(
        std::fs::read_to_string(clone.join(".git/shallow"))?,
        eq(&shallow_before)
    );
    Ok(())
}

/// Nothing is captured while git is in the middle of an operation, and recording refuses.
#[gtest]
fn nothing_is_captured_while_git_is_busy() -> eyre::Result<()> {
    let (test, _forge) = with_remote()?;
    test.write("lib.rs", &three_functions("one();", "", ""))?;
    let review = open(&test)?;
    for (marker, busy) in [
        ("MERGE_HEAD", Busy::Merge),
        ("index.lock", Busy::IndexLocked),
    ] {
        let path = test.path().join(".git").join(marker);
        std::fs::write(&path, "")?;
        let captured = review.live_history(&[])?;
        expect_true!(matches!(captured, Capture::Busy(found) if found == busy));
        expect_that!(
            review.refresh(),
            err(matches_pattern!(Error::GitBusy(eq(&busy))))
        );
        std::fs::remove_file(&path)?;
    }
    expect_that!(review.history()?.revisions().len(), eq(0));
    expect_that!(unit_count(&capture(&review)?)?, eq(1));
    Ok(())
}

/// Another clone made at the same path gets a review of its own, and the old review no longer
/// captures it.
#[gtest]
fn another_clone_at_the_same_path_is_another_review() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("lib.rs", "fn first() {}\n")?;
    test.commit("first")?;
    let first = open(&test)?;
    expect_that!(open(&test)?.key(), eq(first.key()));
    expect_true!(first.verify_clone()?);

    std::fs::remove_dir_all(test.path().join(".git"))?;
    test.git(&["init", "--quiet", "--initial-branch=main"])?;
    test.commit("again")?;
    let second = open(&test)?;
    expect_that!(second.key(), not(eq(first.key())));
    expect_false!(first.verify_clone()?);
    let captured = first.live_history(&[])?;
    expect_true!(matches!(captured, Capture::Replaced));
    expect_that!(first.refresh(), err(matches_pattern!(Error::CloneReplaced)));
    Ok(())
}

/// The checkout is never trusted by default; one grant covers whatever it holds after later
/// edits and commits, until it is revoked, and only while it is the same clone; decisions older
/// versions recorded, for tools running in copies of the code, grant nothing.
#[gtest]
fn trust_in_a_checkout_is_one_grant_for_its_clone() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("lib.rs", "fn first() {}\n")?;
    test.commit("first")?;
    let review = open(&test)?;
    expect_false!(review.trusted()?);

    // Grants of the whole review, of one snapshot and of the checkout's copies, as older logs
    // hold them, still decode and apply to nothing.
    let tree = review
        .repo()
        .tree_of(&review.repo().resolve_commit("HEAD")?)?;
    std::fs::write(
        review.dir().events(),
        formatdoc! {r#"
            {{"event":"trust","trusted":true,"at":0}}
            {{"event":"trust","trusted":true,"at":0,"scope":{{"kind":"snapshot","tree":"{tree}"}}}}
            {{"event":"trust","trusted":true,"at":0,"scope":{{"kind":"checkout"}}}}
        "#},
    )?;
    expect_false!(review.trusted()?);
    expect_that!(review.history()?.revisions().len(), eq(0));

    // A grant covers later edits and commits, read back by a fresh handle, until it is revoked.
    review.set_trusted(true)?;
    test.write("lib.rs", "fn second() {}\n")?;
    test.commit("second")?;
    test.write("lib.rs", "fn third() {}\n")?;
    expect_true!(open(&test)?.trusted()?);
    review.set_trusted(false)?;
    expect_false!(review.trusted()?);

    // Another clone at the same path is another review, and the grant stays with the first.
    review.set_trusted(true)?;
    std::fs::remove_dir_all(test.path().join(".git"))?;
    test.git(&["init", "--quiet", "--initial-branch=main"])?;
    test.commit("again")?;
    expect_false!(review.trusted()?);
    expect_that!(
        review.tree_on_disk(),
        err(matches_pattern!(Error::CloneReplaced))
    );
    expect_false!(open(&test)?.trusted()?);
    Ok(())
}

/// The tree on disk is the one a capture records, read only while git is idle, and recording
/// nothing.
#[gtest]
fn the_tree_on_disk_is_what_a_capture_records() -> eyre::Result<()> {
    let (test, _forge) = with_remote()?;
    test.write("lib.rs", &three_functions("one();", "", ""))?;
    test.write("new.rs", "fn new() {}\n")?;
    let review = open(&test)?;
    let captured = capture(&review)?;
    let head = captured
        .history
        .revisions()
        .last()
        .ok_or_else(|| eyre::eyre!("no capture"))?
        .head
        .clone();
    expect_that!(
        review.tree_on_disk()?,
        eq(&review.snapshots().tree_of(&head)?)
    );

    std::fs::write(test.path().join(".git/MERGE_HEAD"), "")?;
    expect_that!(
        review.tree_on_disk(),
        err(matches_pattern!(Error::GitBusy(eq(&Busy::Merge))))
    );
    expect_that!(review.history()?.revisions().len(), eq(0));
    Ok(())
}

/// An open pull request for the branch decides the base: its base branch on the remote of its
/// base repository, and while that is not fetched, the default branch, saying so.
#[gtest]
fn an_open_pull_request_decides_the_base() -> eyre::Result<()> {
    let (test, _forge) = with_remote()?;
    test.git(&["checkout", "--quiet", "-b", "feature"])?;
    test.write("lib.rs", &three_functions("one();", "", ""))?;
    let review = open(&test)?;
    let mut found = BranchPullRequest {
        number: 7,
        url: "https://example.invalid/owner/repo/pull/7".to_owned(),
        state: PullRequestState::Open,
        base_branch: "develop".to_owned(),
        base_repository: Repository {
            host: "example.invalid".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        },
        head_branch: "feature".to_owned(),
        head_repository: None,
        author: None,
    };

    // What `gh` found for the branch, as it was then.
    let found_for = |branch: &str, pull_request: &BranchPullRequest| -> eyre::Result<_> {
        Ok(CheckoutPullRequest {
            branch: review.repo().branch_identity(branch)?,
            pull_request: Some(pull_request.clone()),
        })
    };

    let missing = capture_with(&review, &[found_for("feature", &found)?])?;
    expect_that!(
        missing.subject.missing_base.as_deref(),
        some(eq("origin/develop"))
    );
    expect_that!(
        missing.subject.base,
        matches_pattern!(BaseRef::RemoteDefault { .. })
    );

    test.git(&["push", "--quiet", "origin", "main:develop"])?;
    test.git(&["fetch", "--quiet", "origin"])?;
    // Of the pull requests found for several branches, the one checked out decides, whatever
    // name each pull request's head has.
    test.git(&["branch", "other"])?;
    let other = BranchPullRequest {
        number: 8,
        base_branch: "main".to_owned(),
        ..found.clone()
    };
    found.head_branch = "named-otherwise-upstream".to_owned();
    let fetched = capture_with(
        &review,
        &[found_for("other", &other)?, found_for("feature", &found)?],
    )?;
    expect_that!(fetched.subject.missing_base, none());
    expect_that!(
        fetched.subject.base,
        matches_pattern!(BaseRef::PullRequest {
            name: eq("origin/develop"),
            ..
        })
    );
    expect_that!(
        fetched
            .subject
            .pull_request
            .as_ref()
            .map(|found| found.number),
        some(eq(7))
    );
    accept_all(&review, &fetched)?;
    let recorded = review.history()?;
    expect_that!(
        recorded
            .revisions()
            .last()
            .and_then(|last| last.pull_request),
        some(eq(7))
    );

    // A merged pull request no longer decides the base.
    found.state = PullRequestState::Merged;
    let merged = capture_with(&review, &[found_for("feature", &found)?])?;
    expect_that!(
        merged.subject.base,
        matches_pattern!(BaseRef::RemoteDefault { .. })
    );
    // Nor is one found before the branch was pointed at another upstream its pull request.
    found.state = PullRequestState::Open;
    let before = found_for("feature", &found)?;
    test.git(&["config", "branch.feature.remote", "origin"])?;
    test.git(&["config", "branch.feature.merge", "refs/heads/someone-else"])?;
    let repointed = capture_with(&review, &[before])?;
    expect_that!(repointed.subject.pull_request, none());
    Ok(())
}

/// What `gh` finds for `branch` of `review`'s checkout: open pull request 7 into `base`.
fn pull_request_into(
    review: &Review,
    branch: &str,
    base: &str,
) -> eyre::Result<Vec<CheckoutPullRequest>> {
    Ok(vec![CheckoutPullRequest {
        branch: review.repo().branch_identity(branch)?,
        pull_request: Some(BranchPullRequest {
            number: 7,
            url: "https://example.invalid/owner/repo/pull/7".to_owned(),
            state: PullRequestState::Open,
            base_branch: base.to_owned(),
            base_repository: Repository {
                host: "example.invalid".to_owned(),
                owner: "owner".to_owned(),
                name: "repo".to_owned(),
            },
            head_branch: branch.to_owned(),
            head_repository: None,
            author: None,
        }),
    }])
}

/// A checkout of `feat/b` stacked on `feat/a`, which `origin/feat/a` is level with: `feat/a`
/// adds `one()` to `a()`, and `feat/b` adds `three()` to `c()` and `b.rs`; `origin/main`, which
/// `origin/HEAD` names as a clone records it, gained `m.rs` since `feat/a` forked from it.
fn stacked() -> eyre::Result<(TestRepo, tempfile::TempDir)> {
    let (test, forge) = with_remote()?;
    test.git(&["checkout", "--quiet", "-b", "feat/a"])?;
    test.write("lib.rs", &three_functions("one();", "", ""))?;
    test.commit("a")?;
    test.git(&["update-ref", "refs/remotes/origin/feat/a", "HEAD"])?;
    test.git(&["checkout", "--quiet", "-b", "feat/b"])?;
    test.write("lib.rs", &three_functions("one();", "", "three();"))?;
    test.write("b.rs", "fn b() { two(); }\n")?;
    test.commit("b")?;
    on_main(&test, &|| {
        test.write("m.rs", "fn m() {}\n")?;
        test.commit("m")?;
        Ok(())
    })?;
    test.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ])?;
    Ok((test, forge))
}

/// Runs `change` on `main`, which `origin/main` is then level with, and switches back to
/// `feat/b`.
fn on_main(test: &TestRepo, change: &dyn Fn() -> eyre::Result<()>) -> eyre::Result<()> {
    test.git(&["checkout", "--quiet", "main"])?;
    change()?;
    test.git(&["update-ref", "refs/remotes/origin/main", "HEAD"])?;
    test.git(&["checkout", "--quiet", "feat/b"])?;
    Ok(())
}

/// The text each unit of the capture's current revision adds, sorted.
fn added(captured: &LiveCapture) -> eyre::Result<Vec<String>> {
    let history = &captured.history;
    let current = history
        .current()
        .ok_or_else(|| eyre::eyre!("no revision"))?;
    let mut added: Vec<String> = summaries(history, current)?
        .into_iter()
        .map(|unit| unit.added.trim().to_owned())
        .collect();
    added.sort();
    Ok(added)
}

/// A stacked branch is compared with what it is built on: its pull request's base while it is
/// stacked on it, even once that base was squash-merged, and the default branch once it is
/// rebased onto it while the pull request still targets the old base, which the subject says;
/// a pull request retargeted to the default branch needs no word.
#[gtest]
fn a_stacked_branch_is_compared_with_what_it_is_built_on() -> eyre::Result<()> {
    let (test, _forge) = stacked()?;
    let review = open(&test)?;
    let into_a = pull_request_into(&review, "feat/b", "feat/a")?;
    let only_b = ["fn b() { two(); }", "fn c() { three(); }"];

    let open_parent = capture_with(&review, &into_a)?;
    expect_that!(
        open_parent.subject.base,
        matches_pattern!(BaseRef::PullRequest {
            name: eq("origin/feat/a"),
            ..
        })
    );
    expect_that!(open_parent.subject.ancestry, none());
    expect_that!(
        added(&open_parent)?,
        elements_are![eq(only_b[0]), eq(only_b[1])]
    );

    on_main(&test, &|| {
        test.git(&["merge", "--quiet", "--squash", "feat/a"])?;
        test.commit("squash a")?;
        Ok(())
    })?;
    let squashed = capture_with(&review, &into_a)?;
    expect_that!(
        squashed.subject.base,
        matches_pattern!(BaseRef::PullRequest { .. })
    );
    expect_that!(squashed.subject.ancestry, none());
    expect_that!(
        added(&squashed)?,
        elements_are![eq(only_b[0]), eq(only_b[1])]
    );

    // #25: rebased onto main, whose squash holds feat/a's work, while #7 still targets feat/a.
    // A default branch only guessed by its name, as in a fork's remote that names none, never
    // overrides the pull request's base.
    test.git(&["rebase", "--quiet", "--onto", "origin/main", "feat/a"])?;
    test.git(&["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"])?;
    let guessed = capture_with(&review, &into_a)?;
    expect_that!(
        guessed.subject.base,
        matches_pattern!(BaseRef::PullRequest {
            name: eq("origin/feat/a"),
            ..
        })
    );
    expect_that!(guessed.subject.ancestry, none());
    test.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ])?;
    let rebased = capture_with(&review, &into_a)?;
    expect_that!(
        rebased.subject.base,
        matches_pattern!(BaseRef::RemoteDefault {
            name: eq("origin/main"),
            ..
        })
    );
    expect_that!(
        rebased.subject.ancestry,
        some(matches_pattern!(review_core::Ancestry::Newer {
            target: eq("origin/feat/a")
        }))
    );
    expect_that!(
        added(&rebased)?,
        elements_are![eq(only_b[0]), eq(only_b[1])]
    );
    expect_that!(
        rebased.subject.summary(),
        eq("#7 feat/b → origin/main · PR targets feat/a")
    );

    let retargeted = capture_with(&review, &pull_request_into(&review, "feat/b", "main")?)?;
    expect_that!(
        retargeted.subject.base,
        matches_pattern!(BaseRef::PullRequest {
            name: eq("origin/main"),
            ..
        })
    );
    expect_that!(retargeted.subject.ancestry, none());
    expect_that!(retargeted.subject.summary(), eq("#7 feat/b → origin/main"));
    Ok(())
}

/// A branch that merged the default branch while its pull request's base did not shares
/// history with both that neither holds of the other: the pull request's base stays, and the
/// subject says the histories diverged. A parent merged with a merge commit gives both the
/// same merge base, which keeps the pull request's base too, until the branch merges the
/// default branch since.
#[gtest]
fn merged_histories_keep_the_pull_request_base_until_the_default_branch_is_newer()
-> eyre::Result<()> {
    let (test, _forge) = stacked()?;
    let review = open(&test)?;
    let into_a = pull_request_into(&review, "feat/b", "feat/a")?;
    test.git(&["merge", "--quiet", "--no-edit", "origin/main"])?;
    let diverged = capture_with(&review, &into_a)?;
    expect_that!(
        diverged.subject.base,
        matches_pattern!(BaseRef::PullRequest {
            name: eq("origin/feat/a"),
            ..
        })
    );
    expect_that!(
        diverged.subject.ancestry,
        some(matches_pattern!(review_core::Ancestry::Diverged {
            default: eq("origin/main")
        }))
    );

    let (test, _forge) = stacked()?;
    let review = open(&test)?;
    let into_a = pull_request_into(&review, "feat/b", "feat/a")?;
    on_main(&test, &|| {
        test.git(&["merge", "--quiet", "--no-ff", "--no-edit", "feat/a"])?;
        Ok(())
    })?;
    let merged = capture_with(&review, &into_a)?;
    expect_that!(
        merged.subject.base,
        matches_pattern!(BaseRef::PullRequest { .. })
    );
    expect_that!(merged.subject.ancestry, none());
    test.git(&["merge", "--quiet", "--no-edit", "origin/main"])?;
    let caught_up = capture_with(&review, &into_a)?;
    expect_that!(
        caught_up.subject.base,
        matches_pattern!(BaseRef::RemoteDefault { .. })
    );
    expect_that!(
        caught_up.subject.ancestry,
        some(matches_pattern!(review_core::Ancestry::Newer { .. }))
    );
    Ok(())
}

/// A switch of base while the checkout stays as it is, as when the pull request is found after
/// a comparison with the default branch, keeps a mark only where the file's base entry and the
/// correspondence still hold: a unit whose file the new base changed reopens, one the new base
/// holds leaves the diff, and the marks made before stay in the history.
#[gtest]
fn a_base_switch_carries_marks_only_where_the_base_entry_holds() -> eyre::Result<()> {
    let (test, _forge) = stacked()?;
    let review = open(&test)?;
    let fallback = capture(&review)?;
    expect_that!(
        added(&fallback)?,
        elements_are![
            eq("fn a() { one(); }"),
            eq("fn b() { two(); }"),
            eq("fn c() { three(); }")
        ]
    );
    accept_all(&review, &fallback)?;

    let switched = capture_with(&review, &pull_request_into(&review, "feat/b", "feat/a")?)?;
    expect_that!(
        state_of(&switched, "two();")?,
        matches_pattern!(UnitState::Reviewed { .. })
    );
    expect_that!(
        state_of(&switched, "three();")?,
        not(matches_pattern!(UnitState::Reviewed { .. }))
    );
    expect_that!(state_of(&switched, "one();"), err(anything()));
    let recorded = review.history()?;
    let first = recorded
        .revisions()
        .first()
        .map(|revision| revision.number)
        .ok_or_else(|| eyre::eyre!("nothing recorded"))?;
    expect_that!(
        find(&summaries(&recorded, first)?, "one();")?.state,
        matches_pattern!(UnitState::Reviewed { .. })
    );
    Ok(())
}
