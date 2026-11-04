//! Correspondence between the units of two revisions.
//!
//! Review state never moves by content alone.
//! A unit of the newer revision corresponds to a unit of the older one only when the older unit's
//! position maps onto it through line diffs of the two heads (for added lines) or the two bases
//! (for removed lines and insertion points).
//! A correspondence is **unchanged**, the only kind that carries a mark, when all of these hold:
//!
//! - the file mapping is unambiguous and the link is one-to-one (no merge, no split);
//! - both sides of the change are byte-for-byte equal;
//! - both footprints map exactly, on the head side and on the base side;
//! - the unit is distinguishable: its change appears once in the file on both revisions, or its
//!   context lines tell it apart from every identical change and match across the revisions.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::analysis::{
    ANALYSIS_FORMAT, Analysis, Chunk, FileChange, FileUnit, LineRange, Unit, UnitKind,
};
use crate::error::Error;
use crate::git::{Blob, BlobReader, ChangeStatus, Repo};
use crate::ids::{ObjectId, UnitIndex};
use crate::text::{LineMap, split_lines};

/// The version of the correspondence format and algorithm.
pub const CORRESPONDENCE_FORMAT: u32 = 1;

/// Renames are ambiguous when more candidates than this would need comparing.
const MAX_RENAME_CANDIDATES: usize = 64;

/// How a unit of the newer revision relates to a unit of the older one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// The same change at a trusted position; review state carries over.
    Unchanged,
    /// The change differs; the unit is changed since reviewed and gets an interdiff.
    Changed,
    /// The change is equal but its position cannot be trusted, so it is reviewed again.
    Untrusted,
    /// A merge, split or ambiguous file mapping; review state and notes do not carry over.
    Ambiguous,
}

/// One link between consecutive revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// The unit in the older revision.
    pub older: UnitIndex,
    /// The unit in the newer revision.
    pub newer: UnitIndex,
    /// The kind of link.
    pub kind: LinkKind,
}

/// The links between two consecutive revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correspondence {
    /// The [`CORRESPONDENCE_FORMAT`] this was computed with.
    pub format: u32,
    /// The [`ANALYSIS_FORMAT`] of the analyses whose unit indices the links use.
    pub analysis_format: u32,
    /// Every link, sorted by newer then older unit.
    pub links: Vec<Link>,
}

/// Where a path of one snapshot went in the next.
struct PathMap {
    renames: HashMap<String, String>,
    deleted: HashSet<String>,
    ambiguous: HashSet<String>,
}

/// A mapped path, and whether the rename that produced it was ambiguous.
struct MappedPath<'a> {
    path: &'a str,
    ambiguous: bool,
}

impl PathMap {
    fn identity() -> Self {
        Self {
            renames: HashMap::new(),
            deleted: HashSet::new(),
            ambiguous: HashSet::new(),
        }
    }

    fn map<'a>(&'a self, path: &'a str) -> Option<MappedPath<'a>> {
        if self.deleted.contains(path) {
            return None;
        }
        Some(MappedPath {
            path: self.renames.get(path).map_or(path, String::as_str),
            ambiguous: self.ambiguous.contains(path),
        })
    }
}

/// Reads and caches blob texts by object ID.
struct Texts {
    reader: BlobReader,
    cache: HashMap<ObjectId, Option<String>>,
}

impl Texts {
    fn get(&mut self, oid: &ObjectId) -> Result<Option<&str>, Error> {
        if !self.cache.contains_key(oid) {
            let text = match self.reader.read(oid)? {
                Blob::Content(bytes) => String::from_utf8(bytes).ok(),
                Blob::Oversized { .. } => None,
            };
            self.cache.insert(oid.clone(), text);
        }
        Ok(self.cache.get(oid).and_then(Option::as_deref))
    }
}

/// Computes the links from the units of `older` to the units of `newer`.
///
/// # Errors
///
/// Returns an error when git fails or a needed object is missing.
pub fn correspond(
    repo: &Repo,
    older: &Analysis,
    newer: &Analysis,
) -> Result<Correspondence, Error> {
    let mut texts = Texts {
        reader: repo.blob_reader()?,
        cache: HashMap::new(),
    };
    let head_map = path_map(repo, &mut texts, &older.head, &newer.head)?;
    let base_map = path_map(repo, &mut texts, &older.base, &newer.base)?;
    let pairs = file_pairs(older, newer, &head_map, &base_map);

    // Candidate edges, each with the index of the file pair whose line maps produced it.
    let mut edges: BTreeMap<Edge, usize> = BTreeMap::new();
    let mut pair_maps = Vec::with_capacity(pairs.len());
    for (pair_index, pair) in pairs.iter().enumerate() {
        let (Some(older_file), Some(newer_file)) =
            (older.files.get(pair.older), newer.files.get(pair.newer))
        else {
            pair_maps.push(SideMaps::default());
            continue;
        };
        let maps = SideMaps {
            head: side_map(
                &mut texts,
                pair.head,
                older_file.new.as_ref(),
                newer_file.new.as_ref(),
            )?,
            base: side_map(
                &mut texts,
                pair.base,
                older_file.old.as_ref(),
                newer_file.old.as_ref(),
            )?,
        };
        let found = chunk_edges(older, older_file, newer, newer_file, &maps)
            .into_iter()
            .chain(file_unit_edges(older, older_file, newer, newer_file));
        for edge in found {
            edges.insert(edge, pair_index);
        }
        pair_maps.push(maps);
    }

    let mut predecessors: HashMap<UnitIndex, usize> = HashMap::new();
    let mut successors: HashMap<UnitIndex, usize> = HashMap::new();
    for edge in edges.keys() {
        *successors.entry(edge.older).or_default() += 1;
        *predecessors.entry(edge.newer).or_default() += 1;
    }
    let older_duplicates = Duplicates::new(older);
    let newer_duplicates = Duplicates::new(newer);
    let mut links: Vec<Link> = edges
        .iter()
        .map(|(edge, &pair_index)| {
            let one_to_one = predecessors.get(&edge.newer) == Some(&1)
                && successors.get(&edge.older) == Some(&1);
            let ambiguous_files = pairs.get(pair_index).is_none_or(|pair| pair.ambiguous);
            let kind = match pair_maps.get(pair_index) {
                Some(maps) if one_to_one && !ambiguous_files => classify(
                    &Side {
                        analysis: older,
                        unit: edge.older,
                        duplicates: &older_duplicates,
                    },
                    &Side {
                        analysis: newer,
                        unit: edge.newer,
                        duplicates: &newer_duplicates,
                    },
                    maps,
                ),
                _ => LinkKind::Ambiguous,
            };
            Link {
                older: edge.older,
                newer: edge.newer,
                kind,
            }
        })
        .collect();
    links.sort_by(|left, right| {
        left.newer
            .cmp(&right.newer)
            .then(left.older.cmp(&right.older))
    });
    Ok(Correspondence {
        format: CORRESPONDENCE_FORMAT,
        analysis_format: ANALYSIS_FORMAT,
        links,
    })
}

/// Maps the paths of snapshot `from` to snapshot `to` through git's rename detection.
fn path_map(
    repo: &Repo,
    texts: &mut Texts,
    from: &ObjectId,
    to: &ObjectId,
) -> Result<PathMap, Error> {
    let mut map = PathMap::identity();
    if from == to {
        return Ok(map);
    }
    let changes = repo.diff_tree(from, to)?;
    let sources: Vec<_> = changes
        .iter()
        .filter(|change| {
            matches!(
                change.status,
                ChangeStatus::Deleted | ChangeStatus::Renamed { .. }
            )
        })
        .filter_map(|change| change.old.clone())
        .collect();
    let destinations: Vec<_> = changes
        .iter()
        .filter(|change| {
            matches!(
                change.status,
                ChangeStatus::Added | ChangeStatus::Renamed { .. }
            )
        })
        .filter_map(|change| change.new.clone())
        .collect();
    for change in &changes {
        match (&change.status, &change.old, &change.new) {
            (ChangeStatus::Deleted, Some(old), _) => {
                map.deleted.insert(old.path.clone());
            }
            (ChangeStatus::Renamed { similarity }, Some(old), Some(new)) => {
                map.renames.insert(old.path.clone(), new.path.clone());
                let ambiguous = if *similarity == 100 {
                    sources
                        .iter()
                        .any(|other| other.path != old.path && other.oid == old.oid)
                        || destinations
                            .iter()
                            .any(|other| other.path != new.path && other.oid == new.oid)
                } else {
                    inexact_rename_is_ambiguous(texts, old, new, &sources, &destinations)?
                };
                if ambiguous {
                    map.ambiguous.insert(old.path.clone());
                }
            }
            _ => {}
        }
    }
    Ok(map)
}

/// Decides whether another source or destination is at least as similar as the chosen pair.
///
/// The similarity is a line-multiset estimate rather than git's exact score, so ties and near
/// ties count as ambiguous, which only ever reopens more code.
fn inexact_rename_is_ambiguous(
    texts: &mut Texts,
    old: &crate::git::RawSide,
    new: &crate::git::RawSide,
    sources: &[crate::git::RawSide],
    destinations: &[crate::git::RawSide],
) -> Result<bool, Error> {
    if sources.len() + destinations.len() > MAX_RENAME_CANDIDATES {
        return Ok(true);
    }
    let Some(old_text) = texts.get(&old.oid)?.map(str::to_owned) else {
        return Ok(true);
    };
    let Some(new_text) = texts.get(&new.oid)?.map(str::to_owned) else {
        return Ok(true);
    };
    let chosen = similarity(&old_text, &new_text);
    for other in destinations.iter().filter(|other| other.path != new.path) {
        let Some(other_text) = texts.get(&other.oid)? else {
            continue;
        };
        if similarity(&old_text, other_text) >= chosen {
            return Ok(true);
        }
    }
    for other in sources.iter().filter(|other| other.path != old.path) {
        let Some(other_text) = texts.get(&other.oid)? else {
            continue;
        };
        if similarity(other_text, &new_text) >= chosen {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Returns the share of common lines, as per mille, between two texts.
fn similarity(left: &str, right: &str) -> usize {
    let mut counts: HashMap<&str, isize> = HashMap::new();
    for line in left.lines() {
        *counts.entry(line).or_default() += 1;
    }
    let mut common: usize = 0;
    for line in right.lines() {
        if let Some(count) = counts.get_mut(line)
            && *count > 0
        {
            *count -= 1;
            common += 1;
        }
    }
    let total = left.lines().count() + right.lines().count();
    (2000 * common).checked_div(total).unwrap_or(1000)
}

/// Which sides link an older file to a newer one.
#[derive(Debug, Clone, Default)]
struct FilePair {
    older: usize,
    newer: usize,
    head: bool,
    base: bool,
    ambiguous: bool,
}

fn file_pairs(
    older: &Analysis,
    newer: &Analysis,
    head_map: &PathMap,
    base_map: &PathMap,
) -> Vec<FilePair> {
    let by_new_path: HashMap<&str, usize> = newer
        .files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| Some((file.new.as_ref()?.path.as_str(), index)))
        .collect();
    let by_old_path: HashMap<&str, usize> = newer
        .files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| Some((file.old.as_ref()?.path.as_str(), index)))
        .collect();
    let mut pairs: BTreeMap<(usize, usize), FilePair> = BTreeMap::new();
    for (older_index, file) in older.files.iter().enumerate() {
        let sides = [
            (file.new.as_ref(), head_map, &by_new_path, true),
            (file.old.as_ref(), base_map, &by_old_path, false),
        ];
        for (side, map, index, is_head) in sides {
            let Some(mapped) = side.and_then(|side| map.map(&side.path)) else {
                continue;
            };
            let Some(&newer_index) = index.get(mapped.path) else {
                continue;
            };
            let pair = pairs
                .entry((older_index, newer_index))
                .or_insert_with(|| FilePair {
                    older: older_index,
                    newer: newer_index,
                    ..FilePair::default()
                });
            pair.ambiguous |= mapped.ambiguous;
            if is_head {
                pair.head = true;
            } else {
                pair.base = true;
            }
        }
    }
    pairs.into_values().collect()
}

/// Line maps between the head files and between the base files of a file pair.
#[derive(Debug, Clone, Default)]
struct SideMaps {
    head: Option<LineMap>,
    base: Option<LineMap>,
}

/// A candidate link between an older and a newer unit, ordered by older then newer unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    older: UnitIndex,
    newer: UnitIndex,
}

fn side_map(
    texts: &mut Texts,
    linked: bool,
    older: Option<&crate::analysis::EntrySide>,
    newer: Option<&crate::analysis::EntrySide>,
) -> Result<Option<LineMap>, Error> {
    let (true, Some(older), Some(newer)) = (linked, older, newer) else {
        return Ok(None);
    };
    let Some(older_text) = texts.get(&older.oid)?.map(str::to_owned) else {
        return Ok(None);
    };
    if older.oid == newer.oid {
        return Ok(Some(LineMap::identity(split_lines(&older_text).len())));
    }
    let Some(newer_text) = texts.get(&newer.oid)? else {
        return Ok(None);
    };
    Ok(Some(LineMap::new(
        &split_lines(&older_text),
        &split_lines(newer_text),
    )))
}

/// A probe into the newer file: a mapped line or a mapped insertion point.
enum Probe {
    Line(usize),
    Gap(usize),
}

fn probes(map: &LineMap, range: LineRange) -> Vec<Probe> {
    if range.len == 0 {
        return map
            .gap_lenient(range.start)
            .map(Probe::Gap)
            .into_iter()
            .collect();
    }
    range
        .range()
        .filter_map(|line| map.line(line))
        .map(Probe::Line)
        .collect()
}

fn hits(probe: &Probe, range: LineRange) -> bool {
    match *probe {
        Probe::Line(line) => range.range().contains(&line),
        Probe::Gap(gap) if range.len == 0 => gap == range.start,
        Probe::Gap(gap) => range.start <= gap && gap <= range.end(),
    }
}

/// A chunk unit of a file.
struct ChunkRef<'a> {
    unit: UnitIndex,
    chunk: &'a Chunk,
}

fn chunk_units<'a>(
    analysis: &'a Analysis,
    file: &'a FileChange,
) -> impl Iterator<Item = ChunkRef<'a>> {
    file.units.iter().filter_map(|&unit| {
        Some(ChunkRef {
            unit,
            chunk: analysis.unit(unit)?.chunk()?,
        })
    })
}

/// Finds candidate links between the chunks of two paired files.
fn chunk_edges(
    older: &Analysis,
    older_file: &FileChange,
    newer: &Analysis,
    newer_file: &FileChange,
    maps: &SideMaps,
) -> BTreeSet<Edge> {
    let newer_chunks: Vec<ChunkRef<'_>> = chunk_units(newer, newer_file).collect();
    let mut edges = BTreeSet::new();
    for older_ref in chunk_units(older, older_file) {
        let sides = [
            (maps.head.as_ref(), older_ref.chunk.new, true),
            (maps.base.as_ref(), older_ref.chunk.old, false),
        ];
        for (map, range, is_head) in sides {
            let Some(map) = map else {
                continue;
            };
            for probe in probes(map, range) {
                let position = |chunk: &Chunk| if is_head { chunk.new } else { chunk.old };
                let first = newer_chunks.partition_point(|newer_ref| {
                    let target = position(newer_ref.chunk);
                    match probe {
                        Probe::Line(line) => target.end() <= line,
                        Probe::Gap(gap) => target.end() < gap,
                    }
                });
                for newer_ref in newer_chunks.get(first..).unwrap_or_default().iter().take(2) {
                    if hits(&probe, position(newer_ref.chunk)) {
                        edges.insert(Edge {
                            older: older_ref.unit,
                            newer: newer_ref.unit,
                        });
                    }
                }
            }
        }
    }
    edges
}

/// Pairs file units of the same kind in two paired files.
fn file_unit_edges(
    older: &Analysis,
    older_file: &FileChange,
    newer: &Analysis,
    newer_file: &FileChange,
) -> Vec<Edge> {
    let file_units = |analysis: &Analysis, file: &FileChange| -> Vec<FileUnitRef> {
        file.units
            .iter()
            .filter_map(|&unit| match analysis.unit(unit)?.kind {
                UnitKind::File(kind) => Some(FileUnitRef { unit, kind }),
                UnitKind::Chunk(_) => None,
            })
            .collect()
    };
    let newer_units = file_units(newer, newer_file);
    let mut edges = Vec::new();
    for older_unit in file_units(older, older_file) {
        for newer_unit in newer_units
            .iter()
            .filter(|newer_unit| newer_unit.kind == older_unit.kind)
        {
            edges.push(Edge {
                older: older_unit.unit,
                newer: newer_unit.unit,
            });
        }
    }
    edges
}

/// A file unit of a file.
struct FileUnitRef {
    unit: UnitIndex,
    kind: FileUnit,
}

/// A chunk's change within its file, optionally with its context digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ChangeKey<'a> {
    file: usize,
    removed: &'a str,
    added: &'a str,
    context: Option<&'a str>,
}

impl<'a> ChangeKey<'a> {
    fn new(unit: &Unit, chunk: &'a Chunk, with_context: bool) -> Self {
        Self {
            file: unit.file,
            removed: &chunk.removed,
            added: &chunk.added,
            context: with_context.then_some(chunk.context.as_str()),
        }
    }
}

/// Counts of identical changes per file, to decide whether a unit is distinguishable.
struct Duplicates<'a> {
    counts: HashMap<ChangeKey<'a>, usize>,
}

impl<'a> Duplicates<'a> {
    fn new(analysis: &'a Analysis) -> Self {
        let mut counts: HashMap<ChangeKey<'a>, usize> = HashMap::new();
        for unit in &analysis.units {
            if let Some(chunk) = unit.chunk() {
                for with_context in [false, true] {
                    *counts
                        .entry(ChangeKey::new(unit, chunk, with_context))
                        .or_default() += 1;
                }
            }
        }
        Self { counts }
    }

    /// Returns whether no other chunk in the file has the same change.
    fn unique(&self, unit: &Unit, chunk: &Chunk) -> bool {
        self.counts.get(&ChangeKey::new(unit, chunk, false)) == Some(&1)
    }

    /// Returns whether no other chunk in the file has the same change and context.
    fn unique_in_context(&self, unit: &Unit, chunk: &Chunk) -> bool {
        self.counts.get(&ChangeKey::new(unit, chunk, true)) == Some(&1)
    }
}

/// One end of a link, with what is needed to judge it.
struct Side<'a> {
    analysis: &'a Analysis,
    unit: UnitIndex,
    duplicates: &'a Duplicates<'a>,
}

fn classify(older: &Side<'_>, newer: &Side<'_>, maps: &SideMaps) -> LinkKind {
    let (Some(older_unit), Some(newer_unit)) = (
        older.analysis.unit(older.unit),
        newer.analysis.unit(newer.unit),
    ) else {
        return LinkKind::Ambiguous;
    };
    let (Some(older_file), Some(newer_file)) = (
        older.analysis.file_of(older_unit),
        newer.analysis.file_of(newer_unit),
    ) else {
        return LinkKind::Ambiguous;
    };
    match (&older_unit.kind, &newer_unit.kind) {
        (UnitKind::Chunk(older_chunk), UnitKind::Chunk(newer_chunk)) => {
            if older_chunk.removed != newer_chunk.removed || older_chunk.added != newer_chunk.added
            {
                return LinkKind::Changed;
            }
            let head_exact = exact(
                maps.head.as_ref(),
                older_file.new.is_some(),
                newer_file.new.is_some(),
                older_chunk.new,
                newer_chunk.new,
            );
            let base_exact = exact(
                maps.base.as_ref(),
                older_file.old.is_some(),
                newer_file.old.is_some(),
                older_chunk.old,
                newer_chunk.old,
            );
            let distinguishable = (older.duplicates.unique(older_unit, older_chunk)
                && newer.duplicates.unique(newer_unit, newer_chunk))
                || (older.duplicates.unique_in_context(older_unit, older_chunk)
                    && newer.duplicates.unique_in_context(newer_unit, newer_chunk)
                    && older_chunk.context == newer_chunk.context);
            if head_exact && base_exact && distinguishable {
                LinkKind::Unchanged
            } else {
                LinkKind::Untrusted
            }
        }
        (UnitKind::File(kind), UnitKind::File(_)) => {
            if file_unit_evidence_equal(*kind, older_file, newer_file) {
                LinkKind::Unchanged
            } else {
                LinkKind::Changed
            }
        }
        _ => LinkKind::Ambiguous,
    }
}

/// Decides whether a footprint maps exactly onto another; a side absent on both revisions
/// (an added or deleted file) agrees trivially.
fn exact(
    map: Option<&LineMap>,
    older_has: bool,
    newer_has: bool,
    older: LineRange,
    newer: LineRange,
) -> bool {
    match (older_has, newer_has) {
        (false, false) => true,
        (true, true) => {
            let Some(map) = map else {
                return false;
            };
            let mapped: Option<Range<usize>> = if older.len == 0 {
                map.gap_exact(older.start).map(|gap| gap..gap)
            } else {
                map.range_exact(&older.range())
            };
            mapped == Some(newer.range())
        }
        _ => false,
    }
}

/// Compares what a file unit asserts on two revisions.
fn file_unit_evidence_equal(kind: FileUnit, older: &FileChange, newer: &FileChange) -> bool {
    let paths = |file: &FileChange| {
        (
            file.old.as_ref().map(|side| side.path.clone()),
            file.new.as_ref().map(|side| side.path.clone()),
        )
    };
    let modes = |file: &FileChange| {
        (
            file.old.as_ref().map(|side| side.mode),
            file.new.as_ref().map(|side| side.mode),
        )
    };
    let objects = |file: &FileChange| {
        (
            file.old.as_ref().map(|side| side.oid.clone()),
            file.new.as_ref().map(|side| side.oid.clone()),
        )
    };
    match kind {
        FileUnit::Rename => paths(older) == paths(newer),
        FileUnit::Mode | FileUnit::EmptyAdded | FileUnit::EmptyDeleted => {
            modes(older) == modes(newer)
        }
        FileUnit::Type
        | FileUnit::Submodule
        | FileUnit::Symlink
        | FileUnit::Binary
        | FileUnit::Oversized
        | FileUnit::NotUtf8
        | FileUnit::Other => modes(older) == modes(newer) && objects(older) == objects(newer),
    }
}
