//! The reviewed repository is only ever read: captures, pins, sessions, fetches and the clone's
//! identity live in cr's data, and what a review recorded stays readable whatever happens to
//! the repository afterwards.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::{Capture, Error, MarkVerdict, Repo, Review, ReviewTarget};

use crate::common::{GIT_ENV, TestRepo, files, git_in, rust_functions};

/// A checkout whose index is split, with a linked worktree whose index is sparse, each with an
/// edit, a file staged but not committed and an untracked file.
fn awkward_checkout() -> eyre::Result<(TestRepo, tempfile::TempDir)> {
    let test = TestRepo::new()?;
    test.write("src/lib.rs", &rust_functions(4))?;
    test.write("docs/guide.md", "# Guide\n")?;
    test.write("top.txt", "top\n")?;
    test.commit("base")?;
    test.git(&["update-ref", "refs/remotes/origin/main", "HEAD"])?;
    let worktree = tempfile::tempdir()?;
    let linked = worktree.path().join("linked");
    test.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "linked",
        &linked.display().to_string(),
    ])?;
    // Git cannot read an index both split and sparse, so the linked worktree's is sparse and
    // the main one's split, each as its own configuration says.
    git_in(
        &linked,
        &["sparse-checkout", "set", "--cone", "--sparse-index", "src"],
    )?;
    test.git(&["config", "--worktree", "core.splitIndex", "true"])?;
    test.git(&["update-index", "--split-index"])?;
    for checkout in [test.path(), linked.as_path()] {
        std::fs::write(checkout.join("src/staged.rs"), "fn staged() {}\n")?;
        git_in(checkout, &["add", "src/staged.rs"])?;
        std::fs::write(checkout.join("src/lib.rs"), rust_functions(5))?;
        std::fs::write(checkout.join("src/untracked.rs"), "fn untracked() {}\n")?;
    }
    Ok((test, worktree))
}

/// Opening, capturing, following, marking, trusting and asking what git ignores, in a checkout
/// and its linked worktree, recording a range and the working tree against a ref, and moving a
/// review an earlier version kept with no analysis cached, which reads the repository's
/// attributes, leave every file of the repository as it was: its objects, packs, refs, index
/// and shared index, with their dates.
#[gtest]
fn the_reviewed_repository_is_never_written() -> eyre::Result<()> {
    let (test, worktree) = awkward_checkout()?;
    let linked = worktree.path().join("linked");
    // The sparse checkout keeps `docs/` off disk, which no capture takes for a deletion.
    expect_false!(linked.join("docs").exists());
    // A branch of what the linked worktree staged, and the review of it as an earlier version
    // left it: its snapshots pinned in the repository, no archive and nothing cached.
    let tree = git_in(&linked, &["write-tree"])?;
    let side = test.git(&["commit-tree", &tree, "-p", "HEAD", "-m", "side"])?;
    test.git(&["update-ref", "refs/heads/side", &side])?;
    let range = ReviewTarget::Range {
        base: "origin/main".to_owned(),
        head: "side".to_owned(),
    };
    let earlier = Review::open(test.data_dir(), Repo::discover(test.path())?, range.clone())?;
    earlier.refresh()?;
    for revision in earlier.history()?.revisions() {
        for oid in [&revision.base, &revision.head] {
            let pin = format!("refs/code-review/{}/snap/{oid}", earlier.key().encoded());
            test.git(&["update-ref", &pin, oid.as_str()])?;
        }
    }
    std::fs::remove_dir_all(earlier.snapshots().git_dir())?;
    std::fs::remove_dir_all(earlier.dir().root().join("analysis"))?;
    let before = (files(test.path())?, files(&linked)?);

    for checkout in [test.path(), &linked] {
        let review = Review::open_checkout(test.data_dir(), Repo::discover(checkout)?)?;
        let recorded = review.refresh()?;
        let history = review.history()?;
        let view = history.view(recorded.number)?;
        let paths = view
            .unit_ids()
            .into_iter()
            .map(|unit| Ok(view.file(unit)?.display_path().to_owned()))
            .collect::<eyre::Result<Vec<String>>>()?;
        expect_that!(paths, each(starts_with("src/")));
        review.mark(&view, &view.unit_ids(), MarkVerdict::Accepted)?;
        let session = review.start_session()?;
        let captured = review.live_history(&session, &[], &[])?;
        expect_true!(matches!(captured, Capture::Captured(_)));
        review.set_trusted(true)?;
        expect_true!(review.trusted()?);
        review.tree_on_disk()?;
        review
            .ignore_rules()
            .read()?
            .ignored(["src/", "target/", "top.txt"])?;
    }
    let moved = Review::open(test.data_dir(), Repo::discover(test.path())?, range)?;
    expect_that!(moved.history()?.revisions().len(), eq(1));
    moved.refresh()?;
    let against = Review::open(
        test.data_dir(),
        Repo::discover(test.path())?,
        ReviewTarget::Worktree {
            base: "origin/main".to_owned(),
        },
    )?;
    against.refresh()?;

    expect_that!((files(test.path())?, files(&linked)?), eq(&before));
    Ok(())
}

/// Linked worktrees share their clone's identity, so each has its own review of the same
/// clone; another clone has another identity, and so does a clone made anew at the same path,
/// whose review leaves the old one stored and readable under its key, which saves nothing
/// more.
#[gtest]
fn a_clone_is_known_by_its_common_git_directory() -> eyre::Result<()> {
    let (test, worktree) = awkward_checkout()?;
    let linked = worktree.path().join("linked");
    let open = |path: &std::path::Path| -> eyre::Result<Review> {
        Ok(Review::open_checkout(
            test.data_dir(),
            Repo::discover(path)?,
        )?)
    };
    let clone_of = |review: &Review| match review.target() {
        ReviewTarget::Checkout { clone } => Ok(clone.clone()),
        other => Err(eyre::eyre!("not a checkout: {other:?}")),
    };
    let main = open(test.path())?;
    let shared = open(&linked)?;
    expect_that!(clone_of(&shared)?, eq(&clone_of(&main)?));
    expect_that!(shared.key(), not(eq(main.key())));
    expect_that!(open(test.path())?.key(), eq(main.key()));

    let other = tempfile::tempdir()?;
    let copy = other.path().join("copy");
    test.git(&["clone", "--quiet", ".", &copy.display().to_string()])?;
    expect_that!(clone_of(&open(&copy)?)?, not(eq(&clone_of(&main)?)));

    // A clone made anew at the same path is another review; the old one stays by its key.
    main.refresh()?;
    let recorded = main.history()?;
    std::fs::remove_dir_all(test.path().join(".git"))?;
    test.git(&["init", "--quiet", "--initial-branch=main"])?;
    test.commit("again")?;
    expect_false!(main.verify_clone()?);
    let view = recorded.current_view()?;
    expect_that!(
        main.mark(&view, &view.unit_ids(), MarkVerdict::Seen),
        err(matches_pattern!(Error::CloneReplaced))
    );
    let anew = open(test.path())?;
    expect_that!(anew.key(), not(eq(main.key())));
    expect_that!(
        Review::load(test.data_dir(), main.key())?.history()?,
        eq(&recorded)
    );
    Ok(())
}

/// A clone that still sets `core.fsyncObjectFiles`, which has git warn on every command, is
/// captured and asked what git ignores all the same.
#[gtest]
fn a_deprecated_setting_of_the_clone_stops_nothing() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("a.rs", "fn a() {}\n")?;
    test.commit("base")?;
    test.git(&["config", "core.fsyncObjectFiles", "true"])?;
    test.write("b.rs", "fn b() {}\n")?;
    expect_true!(test.uncommitted()?);
    let review = Review::open_checkout(test.data_dir(), Repo::discover(test.path())?)?;
    expect_that!(review.ignore_rules().read()?.ignored(["b.rs"])?, is_empty());
    Ok(())
}

/// A review of a linked worktree stops trusting the checkout, and reading it, once the path is
/// a worktree of another clone, though the clone it was opened in is still there.
#[gtest]
fn a_worktree_path_another_clone_takes_is_not_trusted() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("a.rs", "fn a() {}\n")?;
    test.commit("base")?;
    let worktrees = tempfile::tempdir()?;
    let path = worktrees.path().join("linked").display().to_string();
    test.git(&["worktree", "add", "--quiet", "-b", "linked", &path])?;
    let review = Review::open_checkout(test.data_dir(), Repo::discover(path.as_ref())?)?;
    review.set_trusted(true)?;
    expect_true!(review.trusted()?);

    let moved = worktrees.path().join("moved").display().to_string();
    test.git(&["worktree", "move", &path, &moved])?;
    let other = TestRepo::new()?;
    other.write("b.rs", "fn b() {}\n")?;
    other.commit("other")?;
    other.git(&["worktree", "add", "--quiet", "-b", "taken", &path])?;
    expect_false!(review.trusted()?);
    expect_that!(
        review.tree_on_disk(),
        err(matches_pattern!(Error::CloneReplaced))
    );
    Ok(())
}

/// A capture into an archive that never saw the trees a sparse index names for the directories
/// it leaves out takes them all the same, though files there on disk have git expand them:
/// here the index keeps a `docs/` newer than `HEAD`'s, and a generator wrote a file in it.
#[gtest]
fn a_cold_sparse_capture_takes_the_trees_its_index_names() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("src/lib.rs", "fn lib() {}\n")?;
    test.write("docs/guide.md", "one\n")?;
    test.commit("base")?;
    test.write("docs/guide.md", "two\n")?;
    test.commit("guide")?;
    let guide = test.git(&["rev-parse", "HEAD:docs/guide.md"])?;
    test.git(&["sparse-checkout", "set", "--cone", "--sparse-index", "src"])?;
    test.git(&["reset", "--quiet", "--soft", "HEAD~1"])?;
    test.write("docs/generated.md", "generated\n")?;

    // Reading the index without the trees, as the capture's listing would if git expanded
    // `docs/`, git drops it and says so with an error the listing fails on, but succeeds.
    let empty = tempfile::tempdir()?;
    let listed = std::process::Command::new("git")
        .arg("-C")
        .arg(test.path())
        .args(["ls-files", "--sparse"])
        .env("GIT_OBJECT_DIRECTORY", empty.path())
        .envs(GIT_ENV)
        .output()?;
    expect_true!(listed.status.success());
    expect_that!(
        String::from_utf8_lossy(&listed.stderr),
        contains_substring("error: invalid object")
    );
    expect_that!(
        String::from_utf8_lossy(&listed.stdout),
        not(contains_substring("docs"))
    );

    let (review, recorded) = test.capture()?;
    let spec = format!("{}:docs/guide.md", recorded.head);
    expect_that!(
        git_in(review.snapshots().git_dir(), &["rev-parse", &spec])?,
        eq(&guide)
    );
    Ok(())
}

/// Storage whose path climbs out of a directory that does not exist yet, where it could lead
/// into the checkout once that directory is made, is refused before anything is made.
#[gtest]
fn a_data_directory_climbing_out_of_a_missing_one_is_refused() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("a.rs", "fn a() {}\n")?;
    test.commit("base")?;
    let other = tempfile::tempdir()?;
    let name = test
        .path()
        .file_name()
        .ok_or_else(|| eyre::eyre!("no name"))?;
    let climbing = other.path().join("new/../..").join(name).join("data");
    // Unix refuses the unresolved `..`; Windows resolves it by the path's text, which leads
    // into the checkout, and refuses that.
    expect_that!(
        Review::open_checkout(&climbing, Repo::discover(test.path())?),
        err(anything())
    );
    expect_false!(other.path().join("new").exists());
    expect_false!(test.path().join("data").exists());
    Ok(())
}

/// Two first opens of a checkout at once agree on its identity.
#[gtest]
fn simultaneous_first_opens_agree_on_the_clone() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("a.rs", "fn a() {}\n")?;
    test.commit("base")?;
    let keys = std::thread::scope(|scope| {
        let opens: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| -> eyre::Result<String> {
                    let review =
                        Review::open_checkout(test.data_dir(), Repo::discover(test.path())?)?;
                    Ok(review.key().to_string())
                })
            })
            .collect();
        opens
            .into_iter()
            .map(|open| open.join().map_err(|_| eyre::eyre!("an open panicked"))?)
            .collect::<eyre::Result<Vec<String>>>()
    })?;
    expect_that!(keys.iter().all(|key| *key == keys[0]), eq(true));
    Ok(())
}

/// A review recorded in a repository of SHA-256 objects keeps them in an archive of SHA-256
/// objects.
#[gtest]
fn a_sha256_repository_is_archived_as_one() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    std::fs::remove_dir_all(test.path().join(".git"))?;
    test.git(&[
        "init",
        "--quiet",
        "--initial-branch=main",
        "--object-format=sha256",
    ])?;
    test.write("a.rs", &rust_functions(3))?;
    test.commit("base")?;
    test.write("a.rs", &rust_functions(4))?;
    let review = Review::open_checkout(test.data_dir(), Repo::discover(test.path())?)?;
    let recorded = review.refresh()?;
    expect_that!(recorded.head.as_str().len(), eq(64));
    let history = review.history()?;
    let view = history.view(recorded.number)?;
    review.mark(&view, &view.unit_ids(), MarkVerdict::Accepted)?;
    expect_that!(
        git_in(
            review.snapshots().git_dir(),
            &["rev-parse", "--show-object-format"]
        )?,
        eq("sha256")
    );
    Ok(())
}

/// What a review recorded stays readable once the repository lost it: a branch deleted and
/// its objects collected, with no analysis cached, derives the same history from the archive.
#[gtest]
fn a_recorded_history_outlives_the_objects_it_was_read_from() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("src/lib.rs", &rust_functions(4))?;
    test.commit("base")?;
    test.branch_at("feature", "main")?;
    test.write("src/lib.rs", &rust_functions(5))?;
    test.commit("one")?;
    let review = test.review()?;
    review.refresh()?;
    test.write("src/lib.rs", &rust_functions(6))?;
    test.commit("two")?;
    let recorded = review.refresh()?;
    let history = review.history()?;
    let view = history.view(recorded.number)?;
    review.mark(&view, &view.unit_ids(), MarkVerdict::Accepted)?;
    let history = review.history()?;

    test.switch("main")?;
    test.git(&["branch", "--quiet", "-D", "feature"])?;
    test.git(&["reflog", "expire", "--expire=now", "--all"])?;
    test.git(&["gc", "--quiet", "--prune=now"])?;
    expect_true!(
        test.git(&["cat-file", "-e", recorded.head.as_str()])
            .is_err()
    );
    std::fs::remove_dir_all(review.dir().root().join("analysis"))?;
    let reopened = Review::load(test.data_dir(), review.key())?;
    expect_that!(reopened.history()?, eq(&history));
    Ok(())
}

/// A partial clone that lacks an object a snapshot reaches fails naming it, and fetches
/// nothing.
#[gtest]
fn a_partial_clone_reports_the_object_it_lacks() -> eyre::Result<()> {
    let origin = TestRepo::new()?;
    origin.write("a.txt", "one\n")?;
    let base = origin.commit("base")?;
    origin.write("a.txt", "two\n")?;
    origin.commit("two")?;
    origin.git(&["config", "uploadpack.allowFilter", "true"])?;
    let missing = origin.git(&["rev-parse", &format!("{base}:a.txt")])?;
    let local = tempfile::tempdir()?;
    let clone = local.path().join("clone");
    origin.git(&[
        "clone",
        "--quiet",
        "--filter=blob:none",
        &format!("file://{}", origin.path().display()),
        &clone.display().to_string(),
    ])?;
    let before = files(&clone.join(".git"))?;
    let data = tempfile::tempdir()?;
    let opened = Review::open(
        data.path(),
        Repo::discover(&clone)?,
        ReviewTarget::Range {
            base: base.clone(),
            head: "main".to_owned(),
        },
    )?;
    expect_that!(
        opened.refresh(),
        err(matches_pattern!(Error::MissingObject(eq(
            &review_core::ObjectId::parse(&missing)?
        ))))
    );
    expect_that!(files(&clone.join(".git"))?, eq(&before));
    Ok(())
}
