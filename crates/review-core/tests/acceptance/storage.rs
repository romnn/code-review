//! The event store: serialized revisions, complete-or-absent appends and safe tailing.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;

use color_eyre::eyre::{self, WrapErr as _};
use googletest::prelude::*;
use review_core::store::ReviewDir;
use review_core::store::events::{Event, MarkEvent, MarkVerdict};
use review_core::store::log::LogTail;
use review_core::{
    ObjectId, Repo, Review, ReviewKey, ReviewTarget, RevisionInput, RevisionNumber, UnitIndex,
};

use crate::common::{TestRepo, refresh, rust_functions};

/// Environment variables that turn [`record_revision_child`] into a worker.
const CHILD_REPO: &str = "REVIEW_CORE_TEST_CHILD_REPO";
const CHILD_DATA: &str = "REVIEW_CORE_TEST_CHILD_DATA";
const CHILD_BASE: &str = "REVIEW_CORE_TEST_CHILD_BASE";
const CHILD_HEAD: &str = "REVIEW_CORE_TEST_CHILD_HEAD";

fn range_target() -> ReviewTarget {
    ReviewTarget::Range {
        base: "main".to_owned(),
        head: "feature".to_owned(),
    }
}

/// The worker half of [`concurrent_processes_get_distinct_revisions`]: records one revision
/// when the parent test runs this binary with the child environment, and does nothing otherwise.
#[test]
fn record_revision_child() -> eyre::Result<()> {
    let (Ok(repo), Ok(data), Ok(base), Ok(head)) = (
        std::env::var(CHILD_REPO),
        std::env::var(CHILD_DATA),
        std::env::var(CHILD_BASE),
        std::env::var(CHILD_HEAD),
    ) else {
        return Ok(());
    };
    let review = Review::open(
        &PathBuf::from(data),
        Repo::discover(&PathBuf::from(repo))?,
        range_target(),
    )?;
    let base = ObjectId::parse(&base)?;
    review.record_revision(RevisionInput {
        base: base.clone(),
        head: ObjectId::parse(&head)?,
        target: Some(base.clone()),
        merge_bases: vec![base],
    })?;
    Ok(())
}

/// Two processes recording a revision for different heads at once get distinct revision numbers
/// and consistent pins.
#[test]
fn concurrent_processes_get_distinct_revisions() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base_text = rust_functions(200);
    repo.write("src/lib.rs", &base_text)?;
    let base = repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let mut heads = Vec::new();
    for round in 0..6 {
        repo.write(
            "src/lib.rs",
            &base_text.replace("let value = 7;", &format!("let value = {round}00;")),
        )?;
        heads.push(repo.commit(&format!("head {round}"))?);
    }
    let executable = std::env::current_exe()?;
    for pair in heads.chunks(2) {
        let children = pair
            .iter()
            .map(|head| {
                Command::new(&executable)
                    .args(["--exact", "storage::record_revision_child", "--nocapture"])
                    .env(CHILD_REPO, repo.path())
                    .env(CHILD_DATA, repo.data_dir())
                    .env(CHILD_BASE, &base)
                    .env(CHILD_HEAD, head)
                    .spawn()
                    .wrap_err("failed to spawn the child test process")
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        for mut child in children {
            eyre::ensure!(child.wait()?.success(), "a child process failed");
        }
    }

    let review = Review::open(
        repo.data_dir(),
        Repo::discover(repo.path())?,
        range_target(),
    )?;
    let history = review.history()?;
    let revisions = history.revisions();
    let numbers: Vec<u32> = revisions
        .iter()
        .map(|revision| revision.number.get())
        .collect();
    assert_that!(
        numbers,
        elements_are![eq(&1), eq(&2), eq(&3), eq(&4), eq(&5), eq(&6)]
    );
    let mut recorded: Vec<String> = revisions
        .iter()
        .map(|revision| revision.head.to_string())
        .collect();
    recorded.sort();
    let mut expected = heads.clone();
    expected.sort();
    assert_that!(recorded, eq(&expected));
    for oid in heads.iter().chain([&base]) {
        let oid = ObjectId::parse(oid)?;
        let pinned = repo.git(&["rev-parse", "--verify", &review.pin_ref(&oid)])?;
        assert_that!(pinned.as_str(), eq(oid.as_str()));
    }
    Ok(())
}

/// Builds a mark event; unit indices are only constructible inside the engine or by decoding.
fn mark_event(unit: usize) -> eyre::Result<Event> {
    Ok(Event::Mark(MarkEvent {
        revision: RevisionNumber::FIRST,
        unit: serde_json::from_value::<UnitIndex>(serde_json::json!(unit))?,
        evidence: format!("digest-{unit}"),
        verdict: MarkVerdict::Seen,
        at: 0,
    }))
}

fn encoded(events: &[Event]) -> eyre::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend(serde_json::to_vec(event)?);
        bytes.push(b'\n');
    }
    Ok(bytes)
}

/// A writer killed mid-append leaves a log that reopens with the partial record truncated, and
/// the next append lands where the partial record was.
#[test]
fn killed_writer_partial_record_is_truncated() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(3);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 1;", "let value = 10;"),
    )?;
    repo.commit("edit")?;
    let review = repo.review()?;
    refresh(&review)?;
    let log_path = review.dir().events();
    let complete = std::fs::read(&log_path)?;

    // Simulate a writer that died after writing half a record.
    let partial = serde_json::to_vec(&mark_event(0)?)?;
    let mut file = std::fs::OpenOptions::new().append(true).open(&log_path)?;
    file.write_all(partial.get(..partial.len() / 2).unwrap_or_default())?;
    drop(file);

    // The next writer truncates the partial record before appending its own.
    let history = review.history()?;
    let view = history.current_view()?;
    review.mark(&view, &view.unit_ids(), MarkVerdict::Flagged)?;
    let after = std::fs::read(&log_path)?;
    assert_that!(after.get(..complete.len()), some(eq(complete.as_slice())));
    let tail = after.get(complete.len()..).unwrap_or_default();
    assert!(
        tail.strip_suffix(b"\n")
            .is_some_and(|record| !record.contains(&b'\n'))
    );
    let reopened = review.history()?;
    let state = reopened.current_view()?.state(view.unit_ids()[0])?;
    assert!(state.is_reviewed());
    Ok(())
}

/// A reader stopped at any byte boundary loses no complete record, including when the partial
/// record it saw is truncated and replaced by a different one.
#[test]
fn reader_at_any_byte_boundary_loses_nothing() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let dir = test_review_dir(directory.path())?;
    let path = dir.events();
    let events: Vec<Event> = (0..4).map(mark_event).collect::<eyre::Result<_>>()?;
    let bytes = encoded(&events)?;
    let replacement = mark_event(99)?;
    let replacement_bytes = encoded(std::slice::from_ref(&replacement))?;
    let last_boundary = encoded(events.get(..3).unwrap_or_default())?.len();

    for cut in 0..=bytes.len() {
        // The reader polls while the log holds only the first `cut` bytes.
        std::fs::write(&path, bytes.get(..cut).unwrap_or_default())?;
        let mut tail = LogTail::new(&dir);
        let mut seen = tail.poll()?;

        // If the cut fell inside the last record, recovery truncates it and a different record
        // takes its place; otherwise the log is completed as written.
        let completed = if cut > last_boundary && cut < bytes.len() {
            let mut replaced = bytes.get(..last_boundary).unwrap_or_default().to_vec();
            replaced.extend_from_slice(&replacement_bytes);
            replaced
        } else {
            bytes.clone()
        };
        std::fs::write(&path, &completed)?;
        seen.extend(tail.poll()?);

        let mut expected: Vec<Event> = events.get(..3).unwrap_or_default().to_vec();
        if cut > last_boundary && cut < bytes.len() {
            expected.push(replacement.clone());
        } else {
            expected.extend(events.get(3..).unwrap_or_default().iter().cloned());
        }
        assert_that!(seen, eq(&expected), "cut at byte {cut}");
    }
    Ok(())
}

fn test_review_dir(data: &std::path::Path) -> eyre::Result<ReviewDir> {
    let dir = ReviewDir::new(data, &ReviewKey::parse("test")?);
    std::fs::create_dir_all(dir.root())?;
    Ok(dir)
}

/// A poll cannot run while a writer holds the lock: a reader that saw a partial record from a
/// dead writer waits while the next writer truncates it and appends a different record, then
/// reads exactly the new record instead of stitching old and new bytes together.
#[test]
fn reader_waits_while_writer_truncates_and_replaces() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let dir = test_review_dir(directory.path())?;
    let first = encoded(&[mark_event(0)?])?;
    let dead_writer = encoded(&[mark_event(1)?])?;
    let mut log = first.clone();
    log.extend_from_slice(dead_writer.get(..dead_writer.len() / 2).unwrap_or_default());
    std::fs::write(dir.events(), &log)?;

    // The reader holds the complete prefix and has seen the partial record.
    let mut tail = LogTail::new(&dir);
    assert_that!(tail.poll()?, eq(&vec![mark_event(0)?]));

    // A writer takes the lock; a poll started now must wait for it.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.lock())?;
    lock.lock()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let polled = tail.poll().map_err(|err| err.to_string());
        sender.send(polled).ok();
    });
    assert!(matches!(
        receiver.recv_timeout(std::time::Duration::from_millis(300)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));

    // Recovery truncates the partial record and appends a different one, then unlocks.
    let replacement = mark_event(7)?;
    let mut recovered = first;
    recovered.extend(encoded(std::slice::from_ref(&replacement))?);
    std::fs::write(dir.events(), &recovered)?;
    lock.unlock()?;
    drop(lock);

    let polled = receiver
        .recv_timeout(std::time::Duration::from_secs(10))?
        .map_err(|err| eyre::eyre!(err))?;
    reader
        .join()
        .map_err(|_| eyre::eyre!("reader thread panicked"))?;
    assert_that!(polled, eq(&vec![replacement]));
    Ok(())
}

/// A crash after pinning but before the revision event leaves a state from which the next
/// attempt succeeds.
#[test]
fn crash_after_pinning_recovers() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base_text = rust_functions(3);
    repo.write("src/lib.rs", &base_text)?;
    let base = repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "src/lib.rs",
        &base_text.replace("let value = 1;", "let value = 10;"),
    )?;
    let head = repo.commit("edit")?;
    let review = repo.review()?;

    // The crashed attempt pinned both snapshots and left a half-written analysis temporary
    // and an empty log, but never appended its event.
    for oid in [&base, &head] {
        repo.git(&["update-ref", &review.pin_ref(&ObjectId::parse(oid)?), oid])?;
    }
    let analysis_dir = review.dir().root().join("analysis");
    std::fs::create_dir_all(&analysis_dir)?;
    std::fs::write(
        analysis_dir.join(format!(".{base}-{head}.json.123-456.tmp")),
        b"{\"form",
    )?;
    std::fs::write(review.dir().events(), b"")?;

    let recorded = review.refresh()?;
    assert!(recorded.created);
    assert_that!(recorded.number, eq(RevisionNumber::FIRST));
    let history = review.history()?;
    assert_that!(history.revisions().len(), eq(1));
    assert_that!(history.current_view()?.unit_ids().len(), eq(1));
    Ok(())
}

/// A mark whose recorded evidence matches no unit, as after an analysis was recomputed with
/// different numbering, is dropped with a warning instead of marking the wrong code.
#[test]
fn mark_with_mismatched_evidence_is_ignored() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(3);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 1;", "let value = 10;"),
    )?;
    repo.commit("edit")?;
    let review = repo.review()?;
    refresh(&review)?;
    let mut record = serde_json::to_vec(&mark_event(0)?)?;
    record.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(review.dir().events())?;
    file.write_all(&record)?;
    drop(file);

    let history = review.history()?;
    let view = history.current_view()?;
    assert!(!view.state(view.unit_ids()[0])?.is_reviewed());
    assert_that!(history.warnings().len(), eq(1));
    Ok(())
}

/// A pull request review is keyed per clone: opening the same pull request from a second clone
/// starts a separate review and leaves the first clone's review, and its pinned history, intact,
/// while a worktree of the first clone shares its review.
#[test]
fn pull_request_reviews_are_per_clone() -> eyre::Result<()> {
    let first = TestRepo::new()?;
    first.write("a.txt", "a\n")?;
    first.commit("base")?;
    let second = TestRepo::new()?;
    let target = ReviewTarget::PullRequest {
        host: "github.com".to_owned(),
        owner: "owner".to_owned(),
        repo: "repo".to_owned(),
        number: 7,
    };
    let data = first.data_dir();
    let in_first = Review::open(data, Repo::discover(first.path())?, target.clone())?;
    let in_second = Review::open(data, Repo::discover(second.path())?, target.clone())?;
    assert_that!(in_first.key(), not(eq(in_second.key())));
    let reloaded = Review::load(data, in_first.key())?;
    assert_that!(
        reloaded.repo().common_dir(),
        eq(Repo::discover(first.path())?.common_dir())
    );

    let linked = tempfile::tempdir()?;
    let linked_path = linked.path().join("linked");
    first.git(&[
        "worktree",
        "add",
        "--quiet",
        "--detach",
        &linked_path.display().to_string(),
    ])?;
    let in_linked = target.key(&Repo::discover(&linked_path)?)?;
    assert_that!(&in_linked, eq(in_first.key()));
    Ok(())
}

/// A cached analysis of an older format is recomputed and replaced, never trusted.
#[test]
fn stale_analysis_cache_format_is_replaced() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(3);
    repo.write("src/lib.rs", &base)?;
    let base_commit = repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 1;", "let value = 10;"),
    )?;
    let head = repo.commit("edit")?;
    let review = repo.review()?;
    refresh(&review)?;
    let path = review
        .dir()
        .analysis(&ObjectId::parse(&base_commit)?, &ObjectId::parse(&head)?);

    // Rewrite the cache as format 2 with its units removed: trusting it would hide the change.
    let mut cached: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    cached["format"] = serde_json::json!(2);
    cached["units"] = serde_json::json!([]);
    cached["moves"] = serde_json::json!([]);
    std::fs::write(&path, serde_json::to_vec(&cached)?)?;

    let history = review.history()?;
    assert_that!(history.current_view()?.unit_ids().len(), eq(1));
    let rewritten: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_that!(
        rewritten["format"],
        eq(&serde_json::json!(review_core::analysis::ANALYSIS_FORMAT))
    );
    assert_that!(rewritten["units"].as_array().map(Vec::len), some(eq(1)));
    Ok(())
}
