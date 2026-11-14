//! The built-in demo pull request, a shared fixture for every front end.
//!
//! The fixture lives in `crates/review-core/demo/` as plain files: full snapshot trees of a small
//! multi-language project, a manifest of the entries that cannot be plain files (modes, symlinks,
//! binary content) and of the revisions, a walkthrough and a few marks for revision 1. Its
//! revisions cover every engine feature: formatting-only files, leaf highlighting, plain moves
//! and moves with edits, a rename with edits, file units, deletion-only chunks, noise, and a
//! force-push whose units carry over, change, appear, and return to an earlier form.
//!
//! [`prepare`] materializes it deterministically with `git fast-import`, with a fixed identity
//! and fixed dates, so commit IDs are identical on every machine.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use indoc::formatdoc;
use serde::Deserialize;

use crate::error::Error;
use crate::git::Repo;
use crate::ids::{RevisionNumber, UnitId};
use crate::review::{Review, ReviewTarget};
use crate::store::events::MarkVerdict;
use crate::view::RevisionView;

/// Where the demo repository and its review data live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoPaths {
    /// The bare demo repository.
    pub repo: PathBuf,
    /// The data directory holding only the demo review.
    pub data: PathBuf,
}

impl DemoPaths {
    /// Returns the paths under `root`.
    #[must_use]
    pub fn under(root: &Path) -> Self {
        Self {
            repo: root.join("repo"),
            data: root.join("data"),
        }
    }

    /// Returns the paths inside this project's build directory, `target/demo`, which is ignored
    /// by git and never touches real reviews.
    #[must_use]
    pub fn in_project() -> Self {
        Self::under(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/demo"))
    }
}

/// Returns the directory holding the fixture files.
#[must_use]
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("demo")
}

/// Which demo state to prepare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoRequest {
    /// Rebuild everything from scratch and stop at revision 1, with the fixture's marks.
    Fresh,
    /// Keep the existing demo review, building it first if there is none, and record the next
    /// revisions up to this one; marks made meanwhile carry over.
    UpTo(RevisionNumber),
}

/// The fixed identity and clock of every demo commit.
const AUTHOR: &str = "Demo Author <demo@example.com>";
/// 2000-01-01T00:00:00Z; each commit is one hour after the previous one.
const EPOCH: u64 = 946_684_800;

#[derive(Debug, Deserialize)]
struct Manifest {
    trees: Vec<TreeSpec>,
    special: BTreeMap<String, Special>,
    revisions: Vec<RevisionSpec>,
}

#[derive(Debug, Deserialize)]
struct TreeSpec {
    name: String,
    parent: Option<String>,
    message: String,
}

#[derive(Debug, Default, Deserialize)]
struct Special {
    #[serde(default)]
    executable: Vec<String>,
    #[serde(default)]
    symlinks: BTreeMap<String, String>,
    #[serde(default)]
    binary: BTreeMap<String, BinarySpec>,
}

#[derive(Debug, Deserialize)]
struct BinarySpec {
    seed: u32,
    len: usize,
}

#[derive(Debug, Deserialize)]
struct RevisionSpec {
    base: String,
    head: String,
}

/// Chooses units by path, and optionally by text in their change or description.
#[derive(Debug, Deserialize)]
struct Selector {
    path: String,
    contains: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WalkthroughSpec {
    title: String,
    steps: Vec<StepSpec>,
}

#[derive(Debug, Deserialize)]
struct StepSpec {
    title: String,
    why: String,
    units: Vec<Selector>,
}

#[derive(Debug, Deserialize)]
struct MarksSpec {
    marks: Vec<MarkSpec>,
    notes: Vec<NoteSpec>,
}

#[derive(Debug, Deserialize)]
struct MarkSpec {
    #[serde(flatten)]
    selector: Selector,
    verdict: MarkVerdict,
}

#[derive(Debug, Deserialize)]
struct NoteSpec {
    #[serde(flatten)]
    selector: Selector,
    text: String,
}

fn demo_error(detail: impl Into<String>) -> Error {
    Error::Demo(detail.into())
}

fn read_fixture<T: for<'de> Deserialize<'de>>(fixture: &Path, name: &str) -> Result<T, Error> {
    let path = fixture.join(name);
    let bytes = std::fs::read(&path).map_err(|source| Error::io("read", &path, source))?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Decode { path, source })
}

/// Prepares the demo review described by the fixture in `fixture` under `paths`.
///
/// # Errors
///
/// Returns [`Error::Demo`] for a malformed fixture or a revision beyond the last one, and an
/// error when git or the engine fails.
pub fn prepare(paths: &DemoPaths, fixture: &Path, request: DemoRequest) -> Result<Review, Error> {
    let manifest: Manifest = read_fixture(fixture, "manifest.json")?;
    let review = match request {
        DemoRequest::Fresh => build(paths, fixture, &manifest)?,
        DemoRequest::UpTo(_) => match open_existing(paths)? {
            Some(review) => review,
            None => build(paths, fixture, &manifest)?,
        },
    };
    if let DemoRequest::UpTo(target) = request {
        advance(&review, &manifest, target)?;
    }
    // The demo plays a pull request; refreshing a range would store its commits instead.
    let info: crate::ReviewInfo = read_fixture(fixture, "pr.json")?;
    review.store_info(&info)?;
    Ok(review)
}

fn range_target() -> ReviewTarget {
    ReviewTarget::Range {
        base: "main".to_owned(),
        head: "feature".to_owned(),
    }
}

fn open_existing(paths: &DemoPaths) -> Result<Option<Review>, Error> {
    if !paths.repo.exists() || !paths.data.exists() {
        return Ok(None);
    }
    let review = Review::open(&paths.data, Repo::discover(&paths.repo)?, range_target())?;
    let has_revisions = review.history()?.current().is_some();
    Ok(has_revisions.then_some(review))
}

/// Builds the repository and the review from scratch, at revision 1.
fn build(paths: &DemoPaths, fixture: &Path, manifest: &Manifest) -> Result<Review, Error> {
    for directory in [&paths.repo, &paths.data] {
        if directory.exists() {
            std::fs::remove_dir_all(directory)
                .map_err(|source| Error::io("remove", directory, source))?;
        }
    }
    crate::store::create_dir(&paths.repo)?;
    git(
        &paths.repo,
        &[
            "init",
            "--quiet",
            "--bare",
            "--object-format=sha1",
            "--initial-branch=main",
        ],
        None,
    )?;
    let stream = fast_import_stream(fixture, manifest)?;
    git(
        &paths.repo,
        &["fast-import", "--quiet", "--done"],
        Some(&stream),
    )?;

    let repo = Repo::discover(&paths.repo)?;
    move_refs(&repo, manifest, RevisionNumber::FIRST)?;
    let review = Review::open(&paths.data, repo, range_target())?;
    review.refresh()?;

    let history = review.history()?;
    let view = history.current_view()?;
    let walkthrough: WalkthroughSpec = read_fixture(fixture, "walkthrough.json")?;
    review.import_walkthrough(&history, &walkthrough_json(&review, &view, &walkthrough)?)?;
    let marks: MarksSpec = read_fixture(fixture, "marks.json")?;
    for mark in &marks.marks {
        review.mark(&view, &select(&view, &mark.selector), mark.verdict)?;
    }
    for note in &marks.notes {
        for unit in select(&view, &note.selector) {
            review.note(&view, unit, &note.text)?;
        }
    }
    Ok(review)
}

/// Records revisions after the current one, up to `target`.
fn advance(review: &Review, manifest: &Manifest, target: RevisionNumber) -> Result<(), Error> {
    if target.index() >= manifest.revisions.len() {
        return Err(demo_error(format!(
            "the demo has {} revisions, not {target}",
            manifest.revisions.len()
        )));
    }
    let mut current = review.history()?.current().ok_or(Error::NoRevisions)?;
    while current < target {
        let Some(next) = current.next() else {
            break;
        };
        move_refs(review.repo(), manifest, next)?;
        review.refresh()?;
        current = next;
    }
    Ok(())
}

/// Points `main` and `feature` at the trees of `revision`.
fn move_refs(repo: &Repo, manifest: &Manifest, revision: RevisionNumber) -> Result<(), Error> {
    let spec = manifest
        .revisions
        .get(revision.index())
        .ok_or_else(|| demo_error(format!("no demo revision {revision}")))?;
    for (branch, tree) in [
        ("refs/heads/main", &spec.base),
        ("refs/heads/feature", &spec.head),
    ] {
        let commit = repo.resolve_commit(&format!("refs/demo/{tree}"))?;
        repo.update_ref(branch, &commit)?;
    }
    Ok(())
}

/// Runs git in the demo repository, isolated from the user's configuration.
fn git(repo: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<(), Error> {
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Spawn {
            program: "git",
            args: args.join(" "),
            source,
        })?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input)
            .map_err(|source| Error::io("write to git", repo, source))?;
    }
    let output = child.wait_with_output().map_err(|source| Error::Spawn {
        program: "git",
        args: args.join(" "),
        source,
    })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "git",
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(())
}

/// Builds one `git fast-import` stream that commits every tree as `refs/demo/<name>`.
fn fast_import_stream(fixture: &Path, manifest: &Manifest) -> Result<Vec<u8>, Error> {
    let mut stream = Vec::new();
    let mut marks: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, tree) in manifest.trees.iter().enumerate() {
        let mark = index + 1;
        marks.insert(&tree.name, mark);
        let time = EPOCH + 3600 * u64::try_from(index).unwrap_or_default();
        let mut header = String::new();
        // Writing to a `String` cannot fail.
        writeln!(header, "commit refs/demo/{}", tree.name).ok();
        writeln!(header, "mark :{mark}").ok();
        writeln!(header, "author {AUTHOR} {time} +0000").ok();
        writeln!(header, "committer {AUTHOR} {time} +0000").ok();
        writeln!(header, "data {}", tree.message.len()).ok();
        header.push_str(&tree.message);
        header.push('\n');
        if let Some(parent) = &tree.parent {
            let parent_mark = marks.get(parent.as_str()).ok_or_else(|| {
                demo_error(format!("tree {} names unknown parent {parent}", tree.name))
            })?;
            writeln!(header, "from :{parent_mark}").ok();
        }
        header.push_str("deleteall\n");
        stream.extend_from_slice(header.as_bytes());

        let special = manifest.special.get(&tree.name);
        let executable: HashSet<&str> = special
            .map(|special| special.executable.iter().map(String::as_str).collect())
            .unwrap_or_default();
        for file in tree_files(&fixture.join("trees").join(&tree.name))? {
            let mode = if executable.contains(file.path.as_str()) {
                "100755"
            } else {
                "100644"
            };
            push_file(&mut stream, mode, &file.path, &file.content);
        }
        if let Some(special) = special {
            for (path, target) in &special.symlinks {
                push_file(&mut stream, "120000", path, target.as_bytes());
            }
            for (path, binary) in &special.binary {
                push_file(&mut stream, "100644", path, &binary_content(binary));
            }
        }
        stream.push(b'\n');
    }
    stream.extend_from_slice(b"done\n");
    Ok(stream)
}

fn push_file(stream: &mut Vec<u8>, mode: &str, path: &str, content: &[u8]) {
    stream.extend_from_slice(
        formatdoc!(
            "
                M {mode} inline {path}
                data {}
            ",
            content.len()
        )
        .as_bytes(),
    );
    stream.extend_from_slice(content);
    stream.push(b'\n');
}

/// Deterministic binary content: a header with a NUL byte, then a linear congruential sequence.
fn binary_content(spec: &BinarySpec) -> Vec<u8> {
    let mut content = b"LOGO\0".to_vec();
    let mut state = spec.seed;
    while content.len() < spec.len {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        content.push(state.to_be_bytes().first().copied().unwrap_or_default());
    }
    content
}

/// A file of a fixture tree.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TreeFile {
    /// The repository-relative path.
    path: String,
    content: Vec<u8>,
}

/// Lists every file under `root` with its repository-relative path, sorted.
fn tree_files(root: &Path) -> Result<Vec<TreeFile>, Error> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|source| Error::io("read", &directory, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| Error::io("read", &directory, source))?;
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| demo_error(format!("{} is outside the tree", path.display())))?
                .to_string_lossy()
                .replace('\\', "/");
            let content =
                std::fs::read(&path).map_err(|source| Error::io("read", &path, source))?;
            files.push(TreeFile {
                path: relative,
                content,
            });
        }
    }
    files.sort();
    Ok(files)
}

/// Returns the units of `view` the selector names.
fn select(view: &RevisionView<'_>, selector: &Selector) -> Vec<UnitId> {
    view.unit_ids()
        .into_iter()
        .filter(|&unit| {
            let Ok(file) = view.file(unit) else {
                return false;
            };
            let on_path = file.display_path() == selector.path
                || file
                    .old
                    .as_ref()
                    .is_some_and(|side| side.path == selector.path);
            let Some(needle) = &selector.contains else {
                return on_path;
            };
            let in_change = view
                .unit(unit)
                .ok()
                .and_then(|record| record.chunk())
                .is_some_and(|chunk| {
                    chunk.added.contains(needle.as_str()) || chunk.removed.contains(needle.as_str())
                });
            let in_description = view
                .describe(unit)
                .ok()
                .flatten()
                .is_some_and(|text| text.contains(needle.as_str()));
            on_path && (in_change || in_description)
        })
        .collect()
}

/// Resolves the walkthrough's selectors into a walkthrough document for `view`.
fn walkthrough_json(
    review: &Review,
    view: &RevisionView<'_>,
    spec: &WalkthroughSpec,
) -> Result<String, Error> {
    let mut used = HashSet::new();
    let steps: Vec<serde_json::Value> = spec
        .steps
        .iter()
        .map(|step| {
            let units: Vec<String> = step
                .units
                .iter()
                .flat_map(|selector| select(view, selector))
                .filter(|unit| used.insert(*unit))
                .map(|unit| unit.to_string())
                .collect();
            serde_json::json!({ "title": step.title, "why": step.why, "units": units })
        })
        .collect();
    let document = serde_json::json!({
        "version": crate::walkthrough::WALKTHROUGH_VERSION,
        "review": review.key().as_str(),
        "revision": view.number().get(),
        "title": spec.title,
        "steps": steps,
    });
    serde_json::to_string(&document).map_err(|source| Error::Encode {
        what: "demo walkthrough",
        source,
    })
}
