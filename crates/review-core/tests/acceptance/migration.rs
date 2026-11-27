//! Moving a review an earlier version kept in the reviewed repository into its archive: every
//! recorded snapshot is imported while that version's refs still pin it, and nothing the
//! reviewer recorded is lost or changed.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{self, WrapErr as _};
use googletest::prelude::*;
use review_core::{
    History, MarkVerdict, ObjectId, Repo, Review, ReviewKey, ReviewTarget, SinceReviewed,
};

use crate::common::{GIT_ENV, TestRepo, git_in, rust_functions};

/// How many revisions the generated review records, as many as the longest real one.
const REVISIONS: u32 = 117;

/// `src/lib.rs` at revision `revision`: two functions, then thirty constants, of which two far
/// apart take values of their own, others at every revision.
fn library(revision: u32) -> eyre::Result<String> {
    let mut text = rust_functions(2);
    let edited = [revision % 30, (revision + 15) % 30];
    for index in 0..30 {
        if edited.contains(&index) {
            writeln!(
                text,
                "const ITEM_{index}: u32 = {};",
                revision * 100 + index
            )?;
        } else {
            writeln!(text, "const ITEM_{index}: u32 = {index};")?;
        }
    }
    Ok(text)
}

/// `src/other.rs` at revision `revision`: eight functions, one of which returns its own value.
fn other(revision: u32) -> String {
    rust_functions(8).replace(
        &format!("let value = {};", revision % 8),
        &format!("let value = {revision};"),
    )
}

/// A review of `main..feature` with [`REVISIONS`] revisions, each a commit of its own, with
/// marks of every verdict, partial approvals, notes and walkthroughs along the way.
fn long_review() -> eyre::Result<(TestRepo, Review)> {
    let test = TestRepo::new()?;
    test.write("src/lib.rs", &rust_functions(2))?;
    test.write("src/other.rs", &rust_functions(8))?;
    test.commit("base")?;
    test.branch_at("feature", "main")?;
    let review = test.review()?;
    for revision in 1..=REVISIONS {
        test.write("src/lib.rs", &library(revision)?)?;
        test.write("src/other.rs", &other(revision))?;
        test.commit(&format!("revision {revision}"))?;
        let number = review.refresh()?.number;
        let history = review.history()?;
        let view = history.view(number)?;
        let units = view.unit_ids();
        let first: Vec<_> = units.iter().copied().take(1).collect();
        match revision % 10 {
            0 | 4 => review.mark(&view, &units, MarkVerdict::Accepted)?,
            2 => review.mark(&view, &first, MarkVerdict::Seen)?,
            6 => review.mark(&view, &first, MarkVerdict::Flagged)?,
            7 => review.mark(&view, &first, MarkVerdict::Unseen)?,
            // A revision after one accepted shows the block's edits since, each approved on
            // its own.
            1 | 5 => {
                for unit in units {
                    if let Some(SinceReviewed::Delta(delta)) = view.since_reviewed(unit)?
                        && let Some(run) = delta.runs().first()
                    {
                        review.approve_run(&view, &run.shown())?;
                        break;
                    }
                }
            }
            3 => {
                for &unit in &first {
                    review.note(&view, unit, &format!("note on revision {revision}"))?;
                }
            }
            _ => {}
        }
        if revision % 40 == 1 {
            let history = review.history()?;
            let steps: Vec<String> = history
                .view(number)?
                .unit_ids()
                .iter()
                .map(ToString::to_string)
                .collect();
            let json = serde_json::json!({
                "version": 1,
                "review": review.key().clone(),
                "revision": number.get(),
                "title": "Tour",
                "steps": [{"title": "All", "why": "because", "units": steps}],
            });
            review.import_walkthrough(&history, &json.to_string())?;
        }
    }
    Ok((test, review))
}

/// Makes the review as an earlier version left it: its snapshots pinned only by that version's
/// refs in the repository, and no archive.
fn as_left_by_an_earlier_version(test: &TestRepo, review: &Review) -> eyre::Result<()> {
    let history = review.history()?;
    for revision in history.revisions() {
        for oid in [&revision.base, &revision.head] {
            let pin = format!("refs/code-review/{}/snap/{oid}", review.key().encoded());
            test.git(&["update-ref", &pin, oid.as_str()])?;
        }
    }
    test.switch("main")?;
    test.git(&["branch", "--force", "feature", "main"])?;
    test.switch("feature")?;
    std::fs::remove_dir_all(review.snapshots().git_dir())?;
    Ok(())
}

/// The path of the loose object `oid` in the repository at `repo`.
fn loose(repo: &Path, oid: &ObjectId) -> PathBuf {
    let (directory, file) = oid.as_str().split_at(2);
    repo.join(".git/objects").join(directory).join(file)
}

/// A review of `main..feature` of three revisions, each accepted, as an earlier version left
/// it, with the history it recorded.
fn short_review() -> eyre::Result<(TestRepo, Review, History)> {
    let test = TestRepo::new()?;
    test.write("src/lib.rs", &rust_functions(2))?;
    test.commit("base")?;
    test.branch_at("feature", "main")?;
    let review = test.review()?;
    for count in 3..6 {
        test.write("src/lib.rs", &rust_functions(count))?;
        test.commit(&format!("{count} functions"))?;
        let number = review.refresh()?.number;
        let history = review.history()?;
        let view = history.view(number)?;
        review.mark(&view, &view.unit_ids(), MarkVerdict::Accepted)?;
    }
    let recorded = review.history()?;
    as_left_by_an_earlier_version(&test, &review)?;
    Ok((test, review, recorded))
}

/// The refs of the repository at `repo`, with what they point to.
fn refs(repo: &Path) -> eyre::Result<String> {
    git_in(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
}

/// How many packs the repository whose git directory is `git_dir` holds.
fn packs(git_dir: &Path) -> eyre::Result<usize> {
    let mut count = 0;
    for entry in std::fs::read_dir(git_dir.join("objects/pack"))? {
        if entry?
            .path()
            .extension()
            .is_some_and(|extension| extension == "pack")
        {
            count += 1;
        }
    }
    Ok(count)
}

/// The first open of a review of 117 revisions an earlier version kept moves it into its
/// archive without changing a mark, an approval, a note, a walkthrough, the log or the
/// repository's refs, and compacts the packs the move filled into one; the archive alone
/// derives the same history once the repository lost its objects and no analysis is cached.
#[gtest]
fn a_long_review_moves_into_its_archive_losing_nothing() -> eyre::Result<()> {
    let (test, review) = long_review()?;
    let recorded = review.history()?;
    let log = std::fs::read(review.dir().events())?;
    // The log holds every kind of record the move must keep.
    let text = String::from_utf8_lossy(&log);
    for kind in ["partial_approval", "note", "walkthrough"] {
        expect_that!(
            text.contains(&format!(r#""event":"{kind}""#)),
            eq(true),
            "{kind}"
        );
    }
    for verdict in ["accepted", "seen", "flagged", "unseen"] {
        expect_that!(
            text.contains(&format!(r#""verdict":"{verdict}""#)),
            eq(true),
            "{verdict}"
        );
    }
    expect_that!(recorded.revisions().len(), eq(usize::try_from(REVISIONS)?));
    as_left_by_an_earlier_version(&test, &review)?;
    let before = refs(test.path())?;
    // The first open after the upgrade has the review to itself.
    let (dir, key) = (review.dir().clone(), review.key().clone());
    let archive = review.snapshots().git_dir().to_path_buf();
    drop(review);

    expect_that!(test.review()?.history()?, eq(&recorded));
    expect_true!(archive.join("cutover").exists());
    expect_true!(archive.join("compacted").exists());
    expect_that!(packs(&archive)?, eq(1));
    expect_that!(std::fs::read(dir.events())?, eq(&log));
    expect_that!(refs(test.path())?, eq(&before));

    for entry in std::fs::read_dir(test.path().join(".git/objects"))? {
        let path = entry?.path();
        if path.file_name().is_some_and(|name| name != "info") {
            std::fs::remove_dir_all(path)?;
        }
    }
    std::fs::remove_dir_all(dir.root().join("analysis"))?;
    let alone = Review::load(test.data_dir(), &key)?;
    expect_that!(alone.history()?, eq(&recorded));
    Ok(())
}

/// A move that cannot finish changes nothing, and one cut short completes on a later open:
/// an object missing from the repository stops it, naming the object, and leaves the log, an
/// unfinished record at its end included, and the repository's refs as they were; then two
/// first opens at once complete it, and so does an open after its pins or its cutover were
/// lost. A pin lost after the move, its objects still there, is put back.
#[gtest]
fn a_move_cut_short_completes_changing_nothing() -> eyre::Result<()> {
    let (test, review, recorded) = short_review()?;
    let events = review.dir().events();
    // What a writer that died while appending leaves.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&events)?
        .write_all(br#"{"event":"mark""#)?;
    let log = std::fs::read(&events)?;
    let before = refs(test.path())?;
    let archive = review.snapshots().git_dir().to_path_buf();
    let cutover = archive.join("cutover");

    let head = recorded
        .revisions()
        .last()
        .ok_or_else(|| eyre::eyre!("no revision"))?
        .head
        .clone();
    let aside = test.data_dir().join("aside");
    std::fs::rename(loose(test.path(), &head), &aside)?;
    expect_that!(
        test.review(),
        err(displays_as(contains_substring(head.as_str())))
    );
    expect_that!(std::fs::read(&events)?, eq(&log));
    expect_that!(refs(test.path())?, eq(&before));
    expect_that!(refs(&archive)?, eq(""));
    expect_false!(cutover.exists());
    std::fs::rename(&aside, loose(test.path(), &head))?;

    let opened: Vec<History> = std::thread::scope(|scope| {
        let opens: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| -> eyre::Result<History> { Ok(test.review()?.history()?) }))
            .collect();
        opens
            .into_iter()
            .map(|open| open.join().map_err(|_| eyre::eyre!("an open panicked"))?)
            .collect::<eyre::Result<_>>()
    })?;
    for history in &opened {
        expect_that!(history, eq(&recorded));
    }
    expect_true!(cutover.exists());

    for pins in [true, false] {
        std::fs::remove_file(&cutover)?;
        if pins {
            for pin in git_in(&archive, &["for-each-ref", "--format=%(refname)"])?.lines() {
                git_in(&archive, &["update-ref", "-d", pin])?;
            }
        }
        expect_that!(test.review()?.history()?, eq(&recorded));
        expect_true!(cutover.exists());
    }
    let pin = format!("refs/snap/{head}");
    git_in(&archive, &["update-ref", "-d", &pin])?;
    test.review()?;
    expect_that!(git_in(&archive, &["rev-parse", &pin])?, eq(head.as_str()));
    expect_that!(std::fs::read(&events)?, eq(&log));
    expect_that!(refs(test.path())?, eq(&before));
    Ok(())
}

/// Rewrites every correspondence cached in the review's directory `dir` as the version before
/// the archive wrote it, with `links` in place of its links when given, and returns the files
/// with their contents.
fn as_an_earlier_version_cached(
    dir: &Path,
    links: Option<&serde_json::Value>,
) -> eyre::Result<Vec<(PathBuf, Vec<u8>)>> {
    let cache = dir.join("analysis");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&cache)?
        .map(|entry| Ok(entry?.path()))
        .collect::<eyre::Result<_>>()?;
    entries.sort();
    let analysis_format = entries
        .iter()
        .find(|path| !path.to_string_lossy().contains("--"))
        .map(|path| -> eyre::Result<serde_json::Value> {
            let analysis: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
            analysis
                .get("format")
                .cloned()
                .ok_or_else(|| eyre::eyre!("an analysis without a format"))
        })
        .ok_or_else(|| eyre::eyre!("no analysis cached"))??;
    let mut written = Vec::new();
    for path in entries {
        if !path.to_string_lossy().contains("--") {
            continue;
        }
        let current: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let links = match links {
            Some(links) => links,
            None => current
                .get("links")
                .ok_or_else(|| eyre::eyre!("a correspondence without links"))?,
        };
        let legacy = serde_json::json!({
            "format": 3,
            "analysis_format": analysis_format,
            "links": links,
        });
        let bytes = serde_json::to_vec(&legacy)?;
        std::fs::write(&path, &bytes)?;
        written.push((path, bytes));
    }
    expect_that!(written.len(), gt(0));
    Ok(written)
}

/// The move compares the archive with what the reviewer saw, reading the correspondences the
/// version before the archive cached as that version wrote them and leaving them as they are:
/// links the archive does not derive stop the move, and once they are those it derives, the
/// move completes.
#[gtest]
fn a_move_compares_with_the_links_an_earlier_version_cached() -> eyre::Result<()> {
    let (test, review, recorded) = short_review()?;
    let dir = review.dir().root().to_path_buf();
    let cutover = review.snapshots().git_dir().join("cutover");
    let original = as_an_earlier_version_cached(&dir, None)?;

    let unlinked = as_an_earlier_version_cached(&dir, Some(&serde_json::json!([])))?;
    expect_that!(test.review().map(drop), err(anything()));
    expect_false!(cutover.exists());
    for (path, bytes) in &unlinked {
        expect_that!(&std::fs::read(path)?, eq(bytes), "{}", path.display());
    }

    for (path, bytes) in &original {
        std::fs::write(path, bytes)?;
    }
    expect_that!(test.review()?.history()?, eq(&recorded));
    expect_true!(cutover.exists());
    Ok(())
}

/// A revision an earlier version still running records after the move, its snapshots only in
/// the repository, moves into the archive on the next open.
#[gtest]
fn a_revision_an_earlier_version_records_later_follows_the_move() -> eyre::Result<()> {
    let (test, review, recorded) = short_review()?;
    test.review()?;

    // As that version records: the commit pinned by its ref, and the record appended.
    test.write("src/lib.rs", &rust_functions(6))?;
    let head = test.commit("6 functions")?;
    let pin = format!("refs/code-review/{}/snap/{head}", review.key().encoded());
    test.git(&["update-ref", &pin, &head])?;
    let events = review.dir().events();
    let log = std::fs::read_to_string(&events)?;
    let last = log
        .lines()
        .rfind(|line| line.contains(r#""event":"revision""#))
        .ok_or_else(|| eyre::eyre!("no revision recorded"))?;
    let mut revision: serde_json::Value = serde_json::from_str(last)?;
    revision["number"] = (recorded.revisions().len() + 1).into();
    revision["head"] = head.clone().into();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&events)?
        .write_all(format!("{revision}\n").as_bytes())?;

    let history = test.review()?.history()?;
    let revisions = history.revisions();
    expect_that!(revisions.len(), eq(recorded.revisions().len() + 1));
    let last = revisions.last().ok_or_else(|| eyre::eyre!("no revision"))?;
    expect_that!(last.head.as_str(), eq(head.as_str()));
    let archive = review.snapshots().git_dir();
    expect_that!(
        git_in(archive, &["rev-parse", &format!("refs/snap/{head}")])?,
        eq(&head)
    );
    Ok(())
}

/// An archive that lost objects its pins still name gets them again from the repository,
/// where the earlier version's refs still pin them: on the first move, commits whose trees
/// went, and after it, the whole object database.
#[gtest]
fn an_archive_that_lost_objects_gets_them_again() -> eyre::Result<()> {
    let (test, review, recorded) = short_review()?;
    let archive = review.snapshots().git_dir().to_path_buf();
    let head = recorded
        .revisions()
        .last()
        .ok_or_else(|| eyre::eyre!("no revision"))?
        .head
        .clone();

    // A move cut short with the commits in the archive but none of their trees.
    let aside = test.data_dir().join("aside");
    std::fs::rename(loose(test.path(), &head), &aside)?;
    expect_true!(test.review().is_err());
    std::fs::rename(&aside, loose(test.path(), &head))?;
    for revision in recorded.revisions() {
        for oid in [&revision.base, &revision.head] {
            let commit = test.git(&["cat-file", "commit", oid.as_str()])?;
            let written = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&archive)
                .args(["hash-object", "-t", "commit", "-w", "--stdin"])
                .envs(GIT_ENV)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()?;
            let mut stdin = written
                .stdin
                .as_ref()
                .ok_or_else(|| eyre::eyre!("no stdin"))?;
            stdin.write_all(format!("{commit}\n").as_bytes())?;
            let written = written.wait_with_output()?;
            expect_that!(
                String::from_utf8_lossy(&written.stdout).trim(),
                eq(oid.as_str())
            );
        }
    }
    expect_that!(test.review()?.history()?, eq(&recorded));

    // The archive's objects gone after the move, its pins left.
    std::fs::remove_dir_all(archive.join("objects"))?;
    std::fs::create_dir_all(archive.join("objects/pack"))?;
    expect_that!(test.review()?.history()?, eq(&recorded));
    expect_that!(
        git_in(&archive, &["cat-file", "-t", head.as_str()])?,
        eq("commit")
    );
    Ok(())
}

/// A review of the checkout `test` is, as an earlier version left it: its identity in a ref of
/// the clone, its captures, each with a mark, pinned there, and no archive and no identity in
/// the data. Returns its key and its history.
fn legacy_checkout(test: &TestRepo) -> eyre::Result<(ReviewKey, History)> {
    test.write("a.rs", &rust_functions(4))?;
    test.commit("base")?;
    let legacy = test.git(&["hash-object", "-w", "--stdin"])?;
    test.git(&["update-ref", "refs/code-review/clone-id", &legacy])?;
    // Stored under the ref's id, which the first open of the checkout then adopts.
    Review::open(
        test.data_dir(),
        Repo::discover(test.path())?,
        ReviewTarget::Checkout {
            clone: ObjectId::parse(&legacy)?,
        },
    )?;
    let review = open_checkout(test, test.path())?;
    for count in 5..8 {
        test.write("a.rs", &rust_functions(count))?;
        let number = review.refresh()?.number;
        let history = review.history()?;
        let view = history.view(number)?;
        let first: Vec<_> = view.unit_ids().into_iter().take(1).collect();
        review.mark(&view, &first, MarkVerdict::Accepted)?;
    }
    let pins = format!(
        "+refs/snap/*:refs/code-review/{}/snap/*",
        review.key().encoded()
    );
    let archive = review.snapshots().git_dir().display().to_string();
    test.git(&["fetch", "--quiet", "--no-write-fetch-head", &archive, &pins])?;
    remove_dir(review.snapshots().git_dir())?;
    remove_dir(&test.data_dir().join("clones"))?;
    Ok((review.key().clone(), review.history()?))
}

/// Opens the review of the checkout at `path`, storing it in `test`'s data directory.
fn open_checkout(test: &TestRepo, path: &Path) -> eyre::Result<Review> {
    Ok(Review::open_checkout(
        test.data_dir(),
        Repo::discover(path)?,
    )?)
}

/// The review of a checkout an earlier version kept keeps its key and every mark as it moves
/// into its archive: opened by its key first, where a mark saves, and then as the checkout's.
#[gtest]
fn a_checkout_review_keeps_its_key() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    let (key, recorded) = legacy_checkout(&test)?;
    let loaded = Review::load(test.data_dir(), &key)?;
    expect_that!(loaded.history()?, eq(&recorded));
    let view = recorded.current_view()?;
    loaded.mark(&view, &view.unit_ids(), MarkVerdict::Seen)?;

    let moved = open_checkout(&test, test.path())?;
    expect_that!(moved.key(), eq(&key));
    expect_true!(moved.verify_clone()?);
    expect_that!(
        moved.history()?.revisions().len(),
        eq(recorded.revisions().len())
    );
    Ok(())
}

/// A copy of the clone, which carries its ref, is another clone even when it is opened first:
/// it gets an identity of its own, and the review stays the original's.
#[gtest]
fn a_copy_opened_first_does_not_take_the_review() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    let (key, _) = legacy_checkout(&test)?;
    let copies = tempfile::tempdir()?;
    let copy = copies.path().join("copy");
    copy_dir(&test.path().join(".git"), &copy.join(".git"))?;
    let copied = open_checkout(&test, &copy)?;
    let original = open_checkout(&test, test.path())?;
    expect_that!(original.key(), eq(&key));
    expect_that!(clone_of(&copied)?, not(eq(&clone_of(&original)?)));
    Ok(())
}

/// A binding a crash left unpublished, as the temporary file of its atomic write, does not
/// count as holding the legacy id, so the clone still takes its review.
#[gtest]
fn an_unpublished_binding_holds_no_id() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    let (key, _) = legacy_checkout(&test)?;
    let legacy = test.git(&["rev-parse", "refs/code-review/clone-id"])?;
    let clones = test.data_dir().join("clones");
    std::fs::create_dir_all(&clones)?;
    let binding = serde_json::json!({
        "common_dir": std::fs::canonicalize(test.path().join(".git"))?,
        "identity": "",
        "id": legacy,
    });
    std::fs::write(
        clones.join(".0123456789abcdef.json.1-2.tmp"),
        binding.to_string(),
    )?;
    let review = open_checkout(&test, test.path())?;
    expect_that!(review.key(), eq(&key));
    Ok(())
}

/// The clone a review of a checkout is of.
fn clone_of(review: &Review) -> eyre::Result<ObjectId> {
    match review.target() {
        ReviewTarget::Checkout { clone } => Ok(clone.clone()),
        other => Err(eyre::eyre!("not a checkout: {other:?}")),
    }
}

/// A copy of a clone whose reviewed worktree moved aside, with a worktree of its own at the
/// review's path, does not take the clone's identity, nor the trust its review holds, though
/// the copy carries the ref and the review's path now leads into it.
#[gtest]
fn a_copy_with_a_worktree_at_the_reviews_path_takes_nothing() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("a.rs", "fn a() {}\n")?;
    test.commit("base")?;
    let legacy = test.git(&["hash-object", "-w", "--stdin"])?;
    test.git(&["update-ref", "refs/code-review/clone-id", &legacy])?;
    let worktrees = tempfile::tempdir()?;
    let path = worktrees.path().join("linked").display().to_string();
    test.git(&["worktree", "add", "--quiet", "-b", "linked", &path])?;
    Review::open(
        test.data_dir(),
        Repo::discover(path.as_ref())?,
        ReviewTarget::Checkout {
            clone: ObjectId::parse(&legacy)?,
        },
    )?;
    let trusted = open_checkout(&test, path.as_ref())?;
    expect_that!(clone_of(&trusted)?.as_str(), eq(legacy.as_str()));
    trusted.set_trusted(true)?;

    let aside = worktrees.path().join("aside").display().to_string();
    test.git(&["worktree", "move", &path, &aside])?;
    let copy = worktrees.path().join("copy");
    copy_dir(&test.path().join(".git"), &copy.join(".git"))?;
    git_in(&copy, &["worktree", "add", "--quiet", "-b", "taken", &path])?;
    let taken = open_checkout(&test, path.as_ref())?;
    expect_that!(clone_of(&taken)?.as_str(), not(eq(legacy.as_str())));
    expect_false!(taken.trusted()?);
    Ok(())
}

/// A clone made anew at the path of one that was opened, and then the old clone restored
/// there, ref and all, each get a review of their own: once a path is bound, the ref never
/// counts there again.
#[gtest]
fn a_clone_restored_at_a_bound_path_starts_afresh() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    let (key, _) = legacy_checkout(&test)?;
    let saved = tempfile::tempdir()?;
    copy_dir(&test.path().join(".git"), &saved.path().join(".git"))?;
    expect_that!(open_checkout(&test, test.path())?.key().clone(), eq(&key));

    remove_dir(&test.path().join(".git"))?;
    test.git(&["init", "--quiet", "--initial-branch=main"])?;
    test.commit("anew")?;
    let anew = open_checkout(&test, test.path())?.key().clone();
    expect_that!(anew, not(eq(&key)));

    remove_dir(&test.path().join(".git"))?;
    copy_dir(&saved.path().join(".git"), &test.path().join(".git"))?;
    let restored = open_checkout(&test, test.path())?.key().clone();
    expect_that!(restored, not(eq(&key)));
    expect_that!(restored, not(eq(&anew)));
    Ok(())
}

/// A linked worktree of the clone, opened before the checkout the review was of, takes the
/// clone's identity from it, so both reviews are the clone's.
#[gtest]
fn a_linked_worktree_opened_first_takes_its_clone_identity() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    let (key, _) = legacy_checkout(&test)?;
    let worktrees = tempfile::tempdir()?;
    let linked = worktrees.path().join("linked");
    let linked_text = linked.display().to_string();
    test.git(&["worktree", "add", "--quiet", "-b", "linked", &linked_text])?;
    let first = open_checkout(&test, &linked)?;
    let original = open_checkout(&test, test.path())?;
    expect_that!(original.key().clone(), eq(&key));
    expect_that!(clone_of(&first)?, eq(&clone_of(&original)?));
    Ok(())
}

/// Copies the directory `from`, with everything in it, to `to`.
fn copy_dir(from: &Path, to: &Path) -> eyre::Result<()> {
    std::fs::create_dir_all(to).wrap_err_with(|| format!("failed to create {}", to.display()))?;
    let entries =
        std::fs::read_dir(from).wrap_err_with(|| format!("failed to list {}", from.display()))?;
    for entry in entries {
        let entry = entry.wrap_err_with(|| format!("failed to list {}", from.display()))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .wrap_err_with(|| format!("failed to read the type of {}", source.display()))?;
        if kind.is_dir() {
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).wrap_err_with(|| {
                format!(
                    "failed to copy {} to {}",
                    source.display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Removes the directory `path` with everything in it.
fn remove_dir(path: &Path) -> eyre::Result<()> {
    std::fs::remove_dir_all(path).wrap_err_with(|| format!("failed to remove {}", path.display()))
}
