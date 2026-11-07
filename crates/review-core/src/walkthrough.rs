//! Walkthroughs: ordered steps over the units of the revision they were written for, carried to
//! later revisions through correspondences.
//!
//! For every revision displayed, a fresh partition puts each current unit in exactly one step.

use std::collections::{BTreeMap, HashSet};

use indoc::{indoc, writedoc};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::history::History;
use crate::ids::{ReviewKey, RevisionNumber, UnitId, UnitIndex};
use crate::standing::Standing;
use crate::store::events::{StoredStep, StoredUnit, WalkthroughEvent};
use crate::view::RevisionView;

/// The only walkthrough file version.
pub const WALKTHROUGH_VERSION: u32 = 1;

/// A walkthrough failed validation.
#[derive(Debug, thiserror::Error)]
pub enum WalkthroughError {
    /// The text is not a valid walkthrough JSON document.
    #[error("walkthrough is not valid JSON")]
    Json(#[source] serde_json::Error),
    /// The version is not [`WALKTHROUGH_VERSION`].
    #[error("unsupported walkthrough version {0}")]
    Version(u32),
    /// The walkthrough was written for another review.
    #[error("walkthrough is for review {found:?}, not {expected:?}")]
    Review {
        /// This review's key.
        expected: String,
        /// The key in the walkthrough.
        found: String,
    },
    /// The walkthrough's revision does not exist.
    #[error("walkthrough revision {0} does not exist")]
    Revision(u32),
    /// A unit ID is malformed.
    #[error("invalid unit id {0:?}")]
    InvalidUnit(String),
    /// A unit ID names another revision than the walkthrough's.
    #[error("unit {unit} is not from revision {revision}")]
    OtherRevision {
        /// The unit.
        unit: String,
        /// The walkthrough's revision.
        revision: u32,
    },
    /// A unit ID is out of range.
    #[error("unknown unit {0}")]
    UnknownUnit(String),
    /// A unit appears twice.
    #[error("unit {0} appears more than once")]
    DuplicateUnit(String),
}

/// A walkthrough document as written by an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkthroughFile {
    /// Must be [`WALKTHROUGH_VERSION`].
    pub version: u32,
    /// The review key.
    pub review: String,
    /// The revision the unit IDs refer to.
    pub revision: u32,
    /// The title.
    pub title: String,
    /// The ordered steps.
    pub steps: Vec<StepFile>,
}

/// One step of a walkthrough document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFile {
    /// The step's title.
    pub title: String,
    /// Why the step matters.
    #[serde(default)]
    pub why: String,
    /// Unit IDs of the walkthrough's revision.
    pub units: Vec<String>,
}

/// Validates a walkthrough document against the history and returns the event to store.
pub(crate) fn validate(
    history: &History,
    key: &ReviewKey,
    json: &str,
) -> Result<WalkthroughEvent, Error> {
    let file: WalkthroughFile = serde_json::from_str(json).map_err(WalkthroughError::Json)?;
    if file.version != WALKTHROUGH_VERSION {
        return Err(WalkthroughError::Version(file.version).into());
    }
    if file.review != key.as_str() {
        return Err(WalkthroughError::Review {
            expected: key.as_str().to_owned(),
            found: file.review,
        }
        .into());
    }
    let revision =
        RevisionNumber::new(file.revision).ok_or(WalkthroughError::Revision(file.revision))?;
    let view = history
        .view(revision)
        .map_err(|_| WalkthroughError::Revision(file.revision))?;
    let mut seen = HashSet::new();
    let mut steps = Vec::with_capacity(file.steps.len());
    for step in file.steps {
        let mut units = Vec::with_capacity(step.units.len());
        for text in step.units {
            let id: UnitId = text
                .parse()
                .map_err(|_| WalkthroughError::InvalidUnit(text.clone()))?;
            if id.revision() != revision {
                return Err(WalkthroughError::OtherRevision {
                    unit: text,
                    revision: file.revision,
                }
                .into());
            }
            let evidence = view
                .evidence_digest(id)
                .map_err(|_| WalkthroughError::UnknownUnit(text.clone()))?;
            if !seen.insert(id) {
                return Err(WalkthroughError::DuplicateUnit(text).into());
            }
            units.push(StoredUnit {
                unit: id.index(),
                evidence,
            });
        }
        steps.push(StoredStep {
            title: step.title,
            why: step.why,
            units,
        });
    }
    Ok(WalkthroughEvent {
        revision,
        title: file.title,
        steps,
        at: crate::store::now_seconds(),
    })
}

/// What a partition step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// A step from the imported walkthrough.
    Authored,
    /// A file, in the default order without a walkthrough.
    File,
    /// Units without a predecessor in the walkthrough.
    NewSinceWalkthrough,
    /// Unassigned units that are not noise.
    OtherChanges,
    /// Unassigned formatting-only, moved and noise units.
    Noise,
}

/// One step of a partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartitionStep {
    /// The title.
    pub title: String,
    /// Why the step matters.
    pub why: String,
    /// What the step is.
    pub kind: StepKind,
    /// The step's own units, in position order.
    pub units: Vec<UnitId>,
}

/// A partition of one revision's units into steps; each unit is in exactly one step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Partition {
    /// The revision partitioned.
    pub revision: RevisionNumber,
    /// The walkthrough's title, if one was used.
    pub title: Option<String>,
    /// The revision the walkthrough was written for, if one was used.
    pub written_for: Option<RevisionNumber>,
    /// The steps in order.
    pub steps: Vec<PartitionStep>,
    /// Stale references and unassigned units.
    pub warnings: Vec<String>,
}

/// Review progress of one partition step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StepProgress {
    /// Units counted as reviewed.
    pub reviewed: usize,
    /// All units of the step.
    pub total: usize,
}

impl Partition {
    /// Returns every unit in review order: step by step, each step in position order.
    #[must_use]
    pub fn order(&self) -> Vec<UnitId> {
        self.steps
            .iter()
            .flat_map(|step| step.units.iter().copied())
            .collect()
    }

    /// Returns the index of the step holding `unit`.
    #[must_use]
    pub fn step_of(&self, unit: UnitId) -> Option<usize> {
        self.steps
            .iter()
            .position(|step| step.units.contains(&unit))
    }

    /// Returns each step's progress in `view`, the revision this partition was made for.
    #[must_use]
    pub fn progress(&self, view: &RevisionView<'_>) -> Vec<StepProgress> {
        self.steps
            .iter()
            .map(|step| StepProgress {
                reviewed: step
                    .units
                    .iter()
                    .filter(|&&unit| view.standing(unit).is_ok_and(Standing::is_reviewed))
                    .count(),
                total: step.units.len(),
            })
            .collect()
    }

    /// Returns the unit `offset` places after (or, when negative, before) `from` in review order,
    /// clamped to the first and last unit; without `from`, the first unit.
    #[must_use]
    pub fn neighbor(&self, from: Option<UnitId>, offset: isize) -> Option<UnitId> {
        let order = self.order();
        let Some(position) = from.and_then(|from| order.iter().position(|&unit| unit == from))
        else {
            return order.first().copied();
        };
        let target = position
            .saturating_add_signed(offset)
            .min(order.len().saturating_sub(1));
        order.get(target).copied()
    }

    /// Returns the next unit after `from` in review order that still needs review, wrapping
    /// around to the start; without `from`, the first such unit. `None` when every unit is
    /// reviewed.
    #[must_use]
    pub fn next_unreviewed(&self, view: &RevisionView<'_>, from: Option<UnitId>) -> Option<UnitId> {
        let order = self.order();
        let start = from
            .and_then(|from| order.iter().position(|&unit| unit == from))
            .map_or(0, |position| position + 1);
        let len = order.len();
        (0..len)
            .filter_map(|step| order.get((start + step) % len).copied())
            .find(|&unit| view.standing(unit).is_ok_and(Standing::needs_review))
    }
}

/// Where a unit's step comes from while carrying a walkthrough forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Step(usize),
    Unassigned,
    New,
}

/// Partitions the units of `view` using the newest walkthrough written for it or an earlier
/// revision, or the default order when there is none.
#[must_use]
pub fn partition(view: &RevisionView<'_>) -> Partition {
    let walkthrough = view
        .history()
        .walkthroughs()
        .iter()
        .rev()
        .find(|walkthrough| walkthrough.revision <= view.number());
    match walkthrough {
        Some(walkthrough) => carried_partition(view, walkthrough),
        None => default_partition(view),
    }
}

/// A walkthrough unit and the step it was assigned to.
struct StepUnit {
    step: usize,
    unit: UnitIndex,
}

/// The walkthrough's own units with their steps.
struct Assignment {
    /// The origin of every unit of the walkthrough's revision.
    origins: Vec<Origin>,
    /// Every unit the walkthrough references, for stale-reference warnings.
    referenced: Vec<StepUnit>,
}

/// Assigns the walkthrough's own units to their steps, verifying their evidence.
fn assign_steps(
    walkthrough: &WalkthroughEvent,
    written: &RevisionView<'_>,
    warnings: &mut Vec<String>,
) -> Assignment {
    let analysis = written.analysis();
    let mut origins: Vec<Origin> = vec![Origin::Unassigned; written.unit_ids().len()];
    let mut referenced = Vec::new();
    for (step_index, step) in walkthrough.steps.iter().enumerate() {
        for stored in &step.units {
            let resolved = analysis.and_then(|analysis| {
                if analysis.evidence_digest(stored.unit).as_deref()
                    == Some(stored.evidence.as_str())
                {
                    Some(stored.unit)
                } else {
                    analysis.find_by_digest(&stored.evidence)
                }
            });
            let origin = resolved.and_then(|unit| origins.get_mut(unit.get()));
            match (resolved, origin) {
                (Some(unit), Some(origin)) => {
                    *origin = Origin::Step(step_index);
                    referenced.push(StepUnit {
                        step: step_index,
                        unit,
                    });
                }
                _ => warnings.push(format!(
                    "step {:?}: dropped {}, its evidence no longer matches",
                    step.title,
                    UnitId::new(walkthrough.revision, stored.unit)
                )),
            }
        }
    }
    Assignment {
        origins,
        referenced,
    }
}

/// Carries step origins forward along predecessor links, revision by revision.
fn carry_forward(
    history: &History,
    mut origins: Vec<Origin>,
    from: RevisionNumber,
    to: RevisionNumber,
) -> Vec<Origin> {
    let mut revision = from;
    while revision < to {
        let Some(next) = revision.next() else {
            break;
        };
        let Ok(next_view) = history.view(next) else {
            break;
        };
        origins = next_view
            .unit_ids()
            .into_iter()
            .map(|id| {
                let steps: Vec<Origin> = history
                    .predecessors(id)
                    .iter()
                    .map(|link| {
                        origins
                            .get(link.older.get())
                            .copied()
                            .unwrap_or(Origin::New)
                    })
                    .collect();
                match steps.as_slice() {
                    [] => Origin::New,
                    [first, rest @ ..] if rest.iter().all(|origin| origin == first) => *first,
                    _ => Origin::Unassigned,
                }
            })
            .collect();
        revision = next;
    }
    origins
}

fn carried_partition(view: &RevisionView<'_>, walkthrough: &WalkthroughEvent) -> Partition {
    let history = view.history();
    let mut warnings = Vec::new();
    let Ok(written) = history.view(walkthrough.revision) else {
        return default_partition(view);
    };
    let Assignment {
        origins,
        referenced,
    } = assign_steps(walkthrough, &written, &mut warnings);
    let origins = carry_forward(history, origins, walkthrough.revision, view.number());

    // Report walkthrough units that no longer reach this revision.
    for StepUnit { step, unit } in referenced {
        let start = UnitId::new(walkthrough.revision, unit);
        if !reaches(history, start, view.number()) {
            let title = walkthrough
                .steps
                .get(step)
                .map_or("", |step| step.title.as_str());
            warnings.push(format!("step {title:?}: {start} no longer exists, dropped"));
        }
    }

    let mut steps: Vec<PartitionStep> = walkthrough
        .steps
        .iter()
        .map(|step| PartitionStep {
            title: step.title.clone(),
            why: step.why.clone(),
            kind: StepKind::Authored,
            units: Vec::new(),
        })
        .collect();
    let mut new_units = Vec::new();
    let mut other = Vec::new();
    let mut noise = Vec::new();
    for (index, id) in view.unit_ids().into_iter().enumerate() {
        match origins.get(index).copied().unwrap_or(Origin::New) {
            Origin::Step(step) => match steps.get_mut(step) {
                Some(step) => step.units.push(id),
                None => other.push(id),
            },
            Origin::New => new_units.push(id),
            Origin::Unassigned if view.is_collapsed(id).unwrap_or(false) => noise.push(id),
            Origin::Unassigned => other.push(id),
        }
    }
    if !other.is_empty() {
        warnings.push(format!(
            "{} units are not in any walkthrough step",
            other.len()
        ));
    }
    push_step(
        &mut steps,
        "New since walkthrough",
        StepKind::NewSinceWalkthrough,
        new_units,
    );
    push_step(&mut steps, "Other changes", StepKind::OtherChanges, other);
    push_step(&mut steps, "Noise", StepKind::Noise, noise);
    Partition {
        revision: view.number(),
        title: Some(walkthrough.title.clone()),
        written_for: Some(walkthrough.revision),
        steps,
        warnings,
    }
}

/// Returns whether any chain of successor links leads from `start` to revision `target`.
fn reaches(history: &History, start: UnitId, target: RevisionNumber) -> bool {
    let mut current = vec![start];
    let mut revision = start.revision();
    while revision < target {
        let Some(next) = revision.next() else {
            return false;
        };
        let mut successors: Vec<UnitId> = current
            .iter()
            .flat_map(|&unit| history.successors(unit))
            .map(|link| UnitId::new(next, link.newer))
            .collect();
        successors.sort();
        successors.dedup();
        if successors.is_empty() {
            return false;
        }
        current = successors;
        revision = next;
    }
    true
}

fn push_step(steps: &mut Vec<PartitionStep>, title: &str, kind: StepKind, units: Vec<UnitId>) {
    if !units.is_empty() {
        steps.push(PartitionStep {
            title: title.to_owned(),
            why: String::new(),
            kind,
            units,
        });
    }
}

/// The default order: files by directory with tests after the code they test, one step per file,
/// and every collapsed unit in a final noise step.
fn default_partition(view: &RevisionView<'_>) -> Partition {
    let mut files: BTreeMap<OrderKey, Vec<UnitId>> = BTreeMap::new();
    let mut noise = Vec::new();
    for id in view.unit_ids() {
        if view.is_collapsed(id).unwrap_or(false) {
            noise.push(id);
            continue;
        }
        let path = view
            .file(id)
            .map(|file| file.display_path().to_owned())
            .unwrap_or_default();
        files.entry(order_key(&path)).or_default().push(id);
    }
    let mut steps: Vec<PartitionStep> = files
        .into_iter()
        .map(|(key, units)| PartitionStep {
            title: key.path,
            why: String::new(),
            kind: StepKind::File,
            units,
        })
        .collect();
    push_step(&mut steps, "Noise", StepKind::Noise, noise);
    Partition {
        revision: view.number(),
        title: None,
        written_for: None,
        steps,
        warnings: Vec::new(),
    }
}

/// Sorts a path by the code it belongs to, then puts tests after that code.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct OrderKey {
    directory: String,
    stem: String,
    is_test: bool,
    path: String,
}

fn order_key(path: &str) -> OrderKey {
    const TEST_DIRECTORIES: [&str; 4] = ["tests", "test", "__tests__", "spec"];
    const TEST_SUFFIXES: [&str; 4] = ["_test", ".test", "_spec", ".spec"];

    let (directory, file_name) = path.rsplit_once('/').unwrap_or(("", path));
    let components: Vec<&str> = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let in_test_directory = components
        .iter()
        .any(|part| TEST_DIRECTORIES.contains(part));
    let code_directory: Vec<&str> = components
        .into_iter()
        .filter(|part| !TEST_DIRECTORIES.contains(part))
        .collect();
    let stem = file_name
        .split_once('.')
        .map_or(file_name, |(stem, _)| stem);
    let full_stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    let mut code_stem = stem.strip_prefix("test_").unwrap_or(stem);
    let mut has_suffix = false;
    for suffix in TEST_SUFFIXES {
        if let Some(stripped) = full_stem.strip_suffix(suffix) {
            code_stem = stripped;
            has_suffix = true;
        }
    }
    OrderKey {
        directory: code_directory.join("/"),
        stem: code_stem.to_owned(),
        is_test: in_test_directory || has_suffix || stem.starts_with("test_"),
        path: path.to_owned(),
    }
}

/// Builds the prompt asking an agent for a walkthrough of `view`, with a compact unit catalog.
#[must_use]
pub fn prompt(view: &RevisionView<'_>, key: &ReviewKey) -> String {
    use std::fmt::Write as _;

    let mut prompt = String::new();
    let revision = view.number();
    // Writing to a `String` cannot fail.
    writeln!(
        prompt,
        indoc! {"
            Write a guided walkthrough for reviewing this change. Order the review units so a reviewer \
            reads foundations before what builds on them, group related units into steps, and say \
            why each step matters. Put every unit in at most one step. Units you leave out go to a \
            final step automatically, which suits formatting-only, moved and generated units.

            Answer with only this JSON:
        "}
    )
    .ok();
    let skeleton = serde_json::json!({
        "version": WALKTHROUGH_VERSION,
        "review": key.as_str(),
        "revision": revision.get(),
        "title": "<one-line title>",
        "steps": [{"title": "<step title>", "why": "<why it matters>", "units": [format!("u{revision}-0000")]}],
    });
    writedoc!(
        prompt,
        "
            {skeleton:#}

            Units (ID, location, kind, excerpt):
        "
    )
    .ok();
    for id in view.unit_ids() {
        let (Ok(location), Ok(excerpt)) = (view.location(id), view.excerpt(id, 80)) else {
            continue;
        };
        let mut tags = Vec::new();
        if let Ok(unit) = view.unit(id) {
            if unit.formatting_only {
                tags.push("formatting-only");
            }
            if unit.moved {
                tags.push("moved");
            }
        }
        if let Ok(Some(noise)) = view.noise(id) {
            tags.push(noise.name());
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(", "))
        };
        writeln!(
            prompt,
            "{id}  {}:{}-{}{tags}  {excerpt}",
            location.path, location.line, location.end_line
        )
        .ok();
    }
    prompt
}
