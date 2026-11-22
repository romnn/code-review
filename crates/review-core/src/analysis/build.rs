//! Computing an [`Analysis`] from a snapshot pair.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use super::edits::{self, ScriptSide};
use super::{
    ANALYSIS_FORMAT, Analysis, Chunk, EntrySide, FileChange, FileUnit, LineRange, Move, Unit,
    UnitKind, hex_digest, moves, noise,
};
use crate::error::Error;
use crate::git::{Blob, BlobReader, ChangeStatus, EntryType, RawChange, RawSide, Repo};
use crate::ids::{ObjectId, UnitIndex};
use crate::syntax::{self, Language};
use crate::text::{self, generic_tokens, split_lines};

/// Git's binary detection window: a NUL byte in the first 8000 bytes means binary.
const BINARY_PROBE_BYTES: usize = 8000;

/// Lines of context on each side that identify a chunk among identical changes.
pub(crate) const CONTEXT_LINES: usize = 3;

/// Computes the analysis of the changes from `base` to `head`.
///
/// # Errors
///
/// Returns an error when git fails or an object is missing.
pub fn analyze(repo: &Repo, base: &ObjectId, head: &ObjectId) -> Result<Analysis, Error> {
    let mut changes = repo.diff_tree(base, head)?;
    changes.sort_by(|left, right| display_path(left).cmp(display_path(right)));

    let head_paths: Vec<&str> = changes
        .iter()
        .filter_map(|change| change.new.as_ref().map(|side| side.path.as_str()))
        .collect();
    let base_paths: Vec<&str> = changes
        .iter()
        .filter(|change| change.new.is_none())
        .filter_map(|change| change.old.as_ref().map(|side| side.path.as_str()))
        .collect();
    let mut generated = repo.generated_attributes(head, &head_paths)?;
    generated.extend(repo.generated_attributes(base, &base_paths)?);

    let mut builder = Builder {
        blobs: repo.blob_reader()?,
        files: Vec::with_capacity(changes.len()),
        units: Vec::new(),
        regions: HashMap::new(),
    };
    for change in changes {
        let is_generated = generated
            .get(display_path(&change))
            .copied()
            .unwrap_or(false);
        builder.add_file(change, is_generated)?;
    }
    let Builder {
        files,
        mut units,
        mut blobs,
        regions,
    } = builder;
    let mut proof = MoveProof {
        blobs: &mut blobs,
        texts: HashMap::new(),
        budget: MOVE_PROOF_BUDGET,
    };
    let moves = moves::detect(&mut units, &regions, |found, units| {
        move_is_formatting_only(found, units, &files, &mut proof)
    });
    Ok(Analysis {
        format: ANALYSIS_FORMAT,
        base: base.clone(),
        head: head.clone(),
        files,
        units,
        moves,
    })
}

fn display_path(change: &RawChange) -> &str {
    change
        .new
        .as_ref()
        .or(change.old.as_ref())
        .map_or("", |side| side.path.as_str())
}

/// The content of one side of an entry, as far as analysis can use it.
enum SideContent {
    Text(String),
    Opaque(FileUnit),
    Submodule,
    /// A symlink and its target.
    Symlink(String),
}

struct Builder {
    blobs: BlobReader,
    files: Vec<FileChange>,
    units: Vec<Unit>,
    /// Each chunk unit's region in its file, see [`regions`].
    regions: HashMap<UnitIndex, usize>,
}

impl Builder {
    fn read_side(&mut self, side: &RawSide) -> Result<SideContent, Error> {
        match side.mode.entry_type() {
            EntryType::Submodule => return Ok(SideContent::Submodule),
            EntryType::Symlink => {
                let target = match self.blobs.read(&side.oid)? {
                    Blob::Content(bytes) => escape_bytes(&bytes),
                    Blob::Oversized { size } => format!("({size} bytes)"),
                };
                return Ok(SideContent::Symlink(target));
            }
            EntryType::File | EntryType::Other => {}
        }
        Ok(match self.blobs.read(&side.oid)? {
            Blob::Oversized { .. } => SideContent::Opaque(FileUnit::Oversized),
            Blob::Content(bytes) => {
                let probe = bytes.get(..BINARY_PROBE_BYTES).unwrap_or(&bytes);
                if probe.contains(&0) {
                    SideContent::Opaque(FileUnit::Binary)
                } else {
                    match String::from_utf8(bytes) {
                        Ok(text) => SideContent::Text(text),
                        Err(_) => SideContent::Opaque(FileUnit::NotUtf8),
                    }
                }
            }
        })
    }

    fn add_file(&mut self, change: RawChange, is_generated: bool) -> Result<(), Error> {
        let RawChange { old, new, status } = change;
        let file_index = self.files.len();
        let path = new
            .as_ref()
            .or(old.as_ref())
            .map_or("", |side| side.path.as_str());
        let language = Language::from_path(path);
        let mut file_units: Vec<FileUnit> = Vec::new();
        if matches!(status, ChangeStatus::Renamed { .. }) {
            file_units.push(FileUnit::Rename);
        }

        let old_type = old.as_ref().map(|side| side.mode.entry_type());
        let new_type = new.as_ref().map(|side| side.mode.entry_type());
        let oids_differ = old.as_ref().map(|side| &side.oid) != new.as_ref().map(|side| &side.oid);
        let mut texts: Option<FileTexts> = None;
        let type_changed =
            matches!((old_type, new_type), (Some(before), Some(after)) if before != after);
        if type_changed {
            file_units.push(FileUnit::Type);
        } else if let (Some(old), Some(new)) = (&old, &new)
            && old.mode != new.mode
        {
            file_units.push(FileUnit::Mode);
        }
        let mut old_content = old.as_ref().map(|side| self.read_side(side)).transpose()?;
        let mut new_content = new.as_ref().map(|side| self.read_side(side)).transpose()?;
        let old_target = link_target(old_content.as_ref());
        let new_target = link_target(new_content.as_ref());
        if type_changed {
            // The type unit covers the metadata; content stays its own evidence.
            // A side that is a regular file is diffed against an empty other side, so introduced or
            // removed code is always shown, while symlink targets and submodule commits appear in
            // the type unit's description.
            let regular = |content: Option<SideContent>| {
                content.filter(|content| {
                    !matches!(content, SideContent::Submodule | SideContent::Symlink(_))
                })
            };
            old_content = regular(old_content);
            new_content = regular(new_content);
        }
        match content_change(old_content, new_content, oids_differ || type_changed) {
            ContentChange::None => {}
            ContentChange::Unit(unit) => file_units.push(unit),
            ContentChange::Text(file_texts) => texts = Some(file_texts),
        }

        let head_text = texts
            .as_ref()
            .and_then(|file_texts| file_texts.new.as_deref());
        let noise = noise::classify(path, is_generated, head_text);
        let mut indices = Vec::new();
        for kind in &file_units {
            indices.push(self.push_unit(file_index, UnitKind::File(*kind), false));
        }
        let mut formatting_only = false;
        if let Some(file_texts) = &texts {
            let file_chunks = chunks(
                language,
                file_texts.old.as_deref(),
                file_texts.new.as_deref(),
            );
            formatting_only = file_chunks.formatting_only;
            for FileChunk {
                chunk,
                proven,
                region,
            } in file_chunks.chunks
            {
                let index = self.push_unit(file_index, UnitKind::Chunk(chunk), proven);
                self.regions.insert(index, region);
                indices.push(index);
            }
        }
        if indices.is_empty() {
            // Every changed entry yields at least one unit, so nothing can escape review.
            indices.push(self.push_unit(file_index, UnitKind::File(FileUnit::Other), false));
        }
        self.files.push(FileChange {
            old: old.map(|side| entry_side(side, old_target)),
            new: new.map(|side| entry_side(side, new_target)),
            status,
            language,
            noise,
            formatting_only,
            units: indices,
        });
        Ok(())
    }

    fn push_unit(&mut self, file: usize, kind: UnitKind, formatting_only: bool) -> UnitIndex {
        let index = UnitIndex::new(self.units.len());
        self.units.push(Unit {
            file,
            kind,
            formatting_only,
            moved: false,
        });
        index
    }
}

fn entry_side(side: RawSide, link_target: Option<String>) -> EntrySide {
    let RawSide { path, mode, oid } = side;
    EntrySide {
        path,
        mode,
        oid,
        link_target,
    }
}

/// Escapes bytes into a lossless, printable form: valid UTF-8 stays as is, except that `\\`
/// and `"` get a backslash, and invalid or control bytes become `\xNN`.
///
/// Distinct byte strings always escape differently, so two symlink targets that differ only in
/// undecodable bytes never look alike.
pub(crate) fn escape_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut escaped = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                '\\' => escaped.push_str("\\\\"),
                '"' => escaped.push_str("\\\""),
                // Writing to a `String` cannot fail.
                control if control.is_ascii_control() => {
                    write!(escaped, "\\x{:02x}", u32::from(control)).ok();
                }
                other => escaped.push(other),
            }
        }
        for byte in chunk.invalid() {
            write!(escaped, "\\x{byte:02x}").ok();
        }
    }
    escaped
}

fn link_target(content: Option<&SideContent>) -> Option<String> {
    match content {
        Some(SideContent::Symlink(target)) => Some(target.clone()),
        _ => None,
    }
}

/// The text of both sides of an entry; a side is absent for an addition or deletion.
struct FileTexts {
    old: Option<String>,
    new: Option<String>,
}

enum ContentChange {
    None,
    Unit(FileUnit),
    Text(FileTexts),
}

/// Decides how the content of an entry changed, given both sides (absent for an addition or
/// deletion).
fn content_change(
    old: Option<SideContent>,
    new: Option<SideContent>,
    oids_differ: bool,
) -> ContentChange {
    use SideContent::{Opaque, Submodule, Symlink, Text};

    let opaque = |unit: FileUnit| {
        if oids_differ {
            ContentChange::Unit(unit)
        } else {
            ContentChange::None
        }
    };
    match (old, new) {
        (None, None) => ContentChange::None,
        (Some(Submodule), _) | (_, Some(Submodule)) => opaque(FileUnit::Submodule),
        (Some(Symlink(_)), _) | (_, Some(Symlink(_))) => opaque(FileUnit::Symlink),
        (Some(Opaque(unit)), _) | (_, Some(Opaque(unit))) => opaque(unit),
        (None, Some(Text(new))) if new.is_empty() => ContentChange::Unit(FileUnit::EmptyAdded),
        (Some(Text(old)), None) if old.is_empty() => ContentChange::Unit(FileUnit::EmptyDeleted),
        (old, new) if oids_differ => {
            let text = |content: Option<SideContent>| match content {
                Some(Text(text)) => Some(text),
                _ => None,
            };
            ContentChange::Text(FileTexts {
                old: text(old),
                new: text(new),
            })
        }
        _ => ContentChange::None,
    }
}

struct FileChunks {
    chunks: Vec<FileChunk>,
    /// Whether the whole file is proven formatting-only.
    formatting_only: bool,
}

/// A chunk, with what the analysis learns of it that the chunk does not keep.
struct FileChunk {
    chunk: Chunk,
    /// Whether it is proven formatting-only, by the whole-file proof or its own.
    proven: bool,
    /// Its region in the file, see [`regions`].
    region: usize,
}

/// How many bytes the proofs that moves with edits differ only in formatting may parse, across
/// a revision; the first move past the budget and every move after it keep their edits.
const MOVE_PROOF_BUDGET: usize = 64 << 20;

/// Whether the move with edits `found` differs only in formatting, by the same proof as a
/// chunk's: its source block written into the target file in place of its target block leaves
/// the target file's syntax tree as it is, and its target block written into the source file in
/// place of the source block leaves that file's. Both files must be in one language the proof
/// supports and parse; a re-indent that changes what the code means, as in Python, changes a
/// tree, so it stays an edit.
fn move_is_formatting_only(
    found: &Move,
    units: &[Unit],
    files: &[FileChange],
    proof: &mut MoveProof<'_>,
) -> bool {
    let side = |unit: UnitIndex| {
        let unit = units.get(unit.get())?;
        Some((unit.chunk()?, files.get(unit.file)?))
    };
    let (Some((source_chunk, source_file)), Some((target_chunk, target_file))) =
        (side(found.source.unit), side(found.target.unit))
    else {
        return false;
    };
    let (Some(language), Some(target_language)) = (source_file.language, target_file.language)
    else {
        return false;
    };
    let (Some(old), Some(new)) = (source_file.old.as_ref(), target_file.new.as_ref()) else {
        return false;
    };
    if language != target_language {
        return false;
    }
    if proof.budget == 0 {
        return false;
    }
    let (Some(old_text), Some(new_text)) = (proof.text(&old.oid), proof.text(&new.oid)) else {
        return false;
    };
    let cost = 2 * (old_text.len() + new_text.len());
    if cost > proof.budget {
        // The first move that does not fit ends the proofs, so files read for moves that are
        // never proven cannot pile up in the cache.
        proof.budget = 0;
        proof.texts.clear();
        return false;
    }
    proof.budget -= cost;
    let source_lines = source_chunk.old.start + found.source.offset;
    let target_lines = target_chunk.new.start + found.target.offset;
    let (Some(source), Some(target)) = (
        line_bytes(&old_text, source_lines..source_lines + found.source.len),
        line_bytes(&new_text, target_lines..target_lines + found.target.len),
    ) else {
        return false;
    };
    let (Some(source_text), Some(target_text)) =
        (old_text.get(source.clone()), new_text.get(target.clone()))
    else {
        return false;
    };
    let splice = |text: &str, range: &Range<usize>, with: &str| {
        Some([text.get(..range.start)?, with, text.get(range.end..)?].concat())
    };
    let canonical = |text: &str| syntax::parse(language, text).and_then(|parsed| parsed.canonical);
    let (Some(forward), Some(backward)) = (
        splice(&new_text, &target, source_text),
        splice(&old_text, &source, target_text),
    ) else {
        return false;
    };
    let new_canonical = canonical(&new_text);
    let old_canonical = canonical(&old_text);
    new_canonical.is_some()
        && old_canonical.is_some()
        && canonical(&forward) == new_canonical
        && canonical(&backward) == old_canonical
}

/// What the proofs of a revision's moves share: the blob reader, the texts already read, each
/// read once however many moves it takes part in, and what is left of the parse budget, which
/// bounds the texts kept too: a move past it ends the proofs and drops them.
struct MoveProof<'a> {
    blobs: &'a mut BlobReader,
    texts: HashMap<ObjectId, Option<Rc<str>>>,
    budget: usize,
}

impl MoveProof<'_> {
    /// The text of blob `oid`, read once; `None` for one that is not UTF-8 text or cannot be
    /// read.
    fn text(&mut self, oid: &ObjectId) -> Option<Rc<str>> {
        if let Some(text) = self.texts.get(oid) {
            return text.clone();
        }
        let text = match self.blobs.read(oid) {
            Ok(Blob::Content(bytes)) => String::from_utf8(bytes).ok().map(Rc::from),
            _ => None,
        };
        self.texts.insert(oid.clone(), text.clone());
        text
    }
}

/// The bytes of `text` that its zero-based `lines` span, each with its line ending.
fn line_bytes(text: &str, lines: Range<usize>) -> Option<Range<usize>> {
    let mut starts = std::iter::once(0).chain(
        text.match_indices('\n')
            .map(|(at, _)| at + 1)
            .filter(|&at| at < text.len()),
    );
    let start = starts.nth(lines.start)?;
    let end = if lines.is_empty() {
        start
    } else {
        starts
            .nth(lines.end - lines.start - 1)
            .unwrap_or(text.len())
    };
    Some(start..end)
}

/// How many bytes the per-chunk proofs of one file may parse, beyond the two whole files.
/// Each candidate chunk parses two hybrids about the file's size; candidates past the budget
/// stay unproven.
const CHUNK_PROOF_BUDGET: usize = 64 << 20;

/// One side of a file prepared for chunking: its lines, their byte offsets and its tokens.
struct Side<'a> {
    text: &'a str,
    lines: Vec<&'a str>,
    /// Byte offset of each line start, plus the text length at the end.
    offsets: Vec<usize>,
}

impl<'a> Side<'a> {
    fn new(text: &'a str) -> Self {
        let lines = split_lines(text);
        let mut offsets = Vec::with_capacity(lines.len() + 1);
        let mut offset = 0;
        for line in &lines {
            offsets.push(offset);
            offset += line.len();
        }
        offsets.push(offset);
        Self {
            text,
            lines,
            offsets,
        }
    }

    fn byte_range(&self, lines: &Range<usize>) -> Range<usize> {
        let start = self
            .offsets
            .get(lines.start)
            .copied()
            .unwrap_or(self.text.len());
        let end = self
            .offsets
            .get(lines.end)
            .copied()
            .unwrap_or(self.text.len());
        start..end
    }

    fn join(&self, lines: &Range<usize>) -> String {
        self.text
            .get(self.byte_range(lines))
            .unwrap_or("")
            .to_owned()
    }

    fn context(&self, lines: &Range<usize>) -> String {
        let before = lines.start.saturating_sub(CONTEXT_LINES)..lines.start;
        let after = lines.end..(lines.end + CONTEXT_LINES).min(self.lines.len());
        let mut context = self.join(&before);
        context.push('\0');
        context.push_str(&self.join(&after));
        context
    }
}

fn chunks(
    language: Option<Language>,
    old_text: Option<&str>,
    new_text: Option<&str>,
) -> FileChunks {
    let old = Side::new(old_text.unwrap_or(""));
    let new = Side::new(new_text.unwrap_or(""));
    let hunks = text::diff_lines(&old.lines, &new.lines);

    // The formatting proof needs both whole files; additions and deletions have nothing to prove.
    let parsed = match (language, old_text, new_text) {
        (Some(language), Some(old_text), Some(new_text)) if !hunks.is_empty() => {
            syntax::parse(language, old_text).zip(syntax::parse(language, new_text))
        }
        _ => None,
    };
    let formatting_only = parsed.as_ref().is_some_and(|(old_parsed, new_parsed)| {
        old_parsed.canonical.is_some() && old_parsed.canonical == new_parsed.canonical
    });

    let mut budget = CHUNK_PROOF_BUDGET;
    let chunks = hunks
        .iter()
        .zip(regions(&old, &new, &hunks))
        .map(|(hunk, region)| {
            let removed = old.join(&hunk.before);
            let added = new.join(&hunk.after);
            let ((removed_tokens, removed_keys), (added_tokens, added_keys)) =
                if let Some((old_parsed, new_parsed)) = &parsed {
                    (
                        clip_tokens(old_parsed, &old.byte_range(&hunk.before)),
                        clip_tokens(new_parsed, &new.byte_range(&hunk.after)),
                    )
                } else {
                    let (removed_tokens, added_tokens) =
                        (generic_tokens(&removed), generic_tokens(&added));
                    let (removed_keys, added_keys) = (
                        vec![None; removed_tokens.len()],
                        vec![None; added_tokens.len()],
                    );
                    ((removed_tokens, removed_keys), (added_tokens, added_keys))
                };
            let token_text = |text: &str, tokens: &[Range<usize>]| -> Vec<String> {
                tokens
                    .iter()
                    .map(|range| text.get(range.clone()).unwrap_or("").to_owned())
                    .collect()
            };
            let whitespace_only = !removed.is_empty()
                && !added.is_empty()
                && token_text(&removed, &removed_tokens) == token_text(&added, &added_tokens);
            let proven = formatting_only
                || (whitespace_only
                    && chunk_is_formatting_only(
                        language,
                        &old,
                        &new,
                        parsed.as_ref(),
                        hunk,
                        &mut budget,
                    ));
            let edits = edits::edit_script(
                &ScriptSide {
                    text: &removed,
                    tokens: &removed_tokens,
                    keys: &removed_keys,
                },
                &ScriptSide {
                    text: &added,
                    tokens: &added_tokens,
                    keys: &added_keys,
                },
            );
            let mut context = old.context(&hunk.before);
            context.push('\0');
            context.push_str(&new.context(&hunk.after));
            let chunk = Chunk {
                old: LineRange {
                    start: hunk.before.start,
                    len: hunk.before.len(),
                },
                new: LineRange {
                    start: hunk.after.start,
                    len: hunk.after.len(),
                },
                edits,
                removed,
                added,
                whitespace_only,
                context: hex_digest(context.as_bytes()),
            };
            FileChunk {
                chunk,
                proven,
                region,
            }
        })
        .collect();
    FileChunks {
        chunks,
        formatting_only,
    }
}

/// The region of each of a file's `hunks`: how many boundaries come before it, a boundary being an
/// unchanged line of code whose trimmed text occurs once in each version of the file, so it marks
/// one place. Repeated lines such as Ruby's `end` mark none, nor do blank lines and brackets.
fn regions(old: &Side<'_>, new: &Side<'_>, hunks: &[text::Hunk]) -> Vec<usize> {
    if hunks.len() < 2 {
        return vec![0; hunks.len()];
    }
    let mut occurrences: HashMap<&str, (usize, usize)> = HashMap::new();
    for line in &old.lines {
        occurrences.entry(line.trim()).or_default().0 += 1;
    }
    for line in &new.lines {
        occurrences.entry(line.trim()).or_default().1 += 1;
    }
    let boundary = |line: &str| {
        let text = line.trim();
        moves::anchorable(text) && occurrences.get(text) == Some(&(1, 1))
    };
    let mut region = 0;
    let mut unchanged_from = 0;
    hunks
        .iter()
        .map(|hunk| {
            region += new
                .lines
                .get(unchanged_from..hunk.after.start)
                .unwrap_or_default()
                .iter()
                .filter(|line| boundary(line))
                .count();
            unchanged_from = hunk.after.end;
            region
        })
        .collect()
}

/// Proves one chunk formatting-only on its own, for a file whose other changes are real.
///
/// Two hybrids are parsed: the old file with only this chunk applied, and the new file with
/// only this chunk reverted. The chunk is a pure reformat only when the first has the old
/// file's canonical tree and the second the new file's, so it is proven both on its own and
/// beside every other change of the file; another chunk that, say, opens a string around it
/// breaks the second comparison. A parse error in any of the four trees, or an unsupported
/// language, leaves it unproven. The grammar decides what is significant, such as Python's
/// indentation or JavaScript's automatic semicolons.
fn chunk_is_formatting_only(
    language: Option<Language>,
    old: &Side<'_>,
    new: &Side<'_>,
    parsed: Option<&(syntax::ParsedFile, syntax::ParsedFile)>,
    hunk: &text::Hunk,
    budget: &mut usize,
) -> bool {
    let (Some(language), Some((old_parsed, new_parsed))) = (language, parsed) else {
        return false;
    };
    let (Some(old_canonical), Some(new_canonical)) = (&old_parsed.canonical, &new_parsed.canonical)
    else {
        return false;
    };
    let cost = old.text.len() + new.text.len();
    if cost > *budget {
        return false;
    }
    *budget -= cost;
    let (removed, added) = (old.byte_range(&hunk.before), new.byte_range(&hunk.after));
    let splice = |text: &str, range: &Range<usize>, with: &str| {
        let (Some(before), Some(after)) = (text.get(..range.start), text.get(range.end..)) else {
            return None;
        };
        Some([before, with, after].concat())
    };
    let (Some(removed_text), Some(added_text)) =
        (old.text.get(removed.clone()), new.text.get(added.clone()))
    else {
        return false;
    };
    let (Some(forward), Some(backward)) = (
        splice(old.text, &removed, added_text),
        splice(new.text, &added, removed_text),
    ) else {
        return false;
    };
    let canonical = |text: &str| syntax::parse(language, text).and_then(|parsed| parsed.canonical);
    canonical(&forward).as_ref() == Some(old_canonical)
        && canonical(&backward).as_ref() == Some(new_canonical)
}

/// Returns the tokens of `parsed` overlapping `range`, clipped to it and made relative to its
/// start, each with the key of the gap before it when it lies wholly inside `range`.
fn clip_tokens<'a>(
    parsed: &'a syntax::ParsedFile,
    range: &Range<usize>,
) -> (Vec<Range<usize>>, Vec<Option<&'a [u8]>>) {
    let first = parsed
        .tokens
        .partition_point(|token| token.end <= range.start);
    parsed
        .tokens
        .get(first..)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .take_while(|(_, token)| token.start < range.end)
        .filter_map(|(offset, token)| {
            let clipped =
                token.start.max(range.start) - range.start..token.end.min(range.end) - range.start;
            let whole = token.start >= range.start && token.end <= range.end;
            let key = whole.then(|| parsed.gap_key(first + offset)).flatten();
            (!clipped.is_empty()).then_some((clipped, key))
        })
        .unzip()
}

#[cfg(test)]
mod tests {
    use indoc::{formatdoc, indoc};

    use super::{Chunk, Side, chunk_is_formatting_only, chunks, escape_bytes};
    use crate::analysis::EditKind;
    use crate::syntax::{self, Language};
    use crate::text;

    /// Only an unchanged line of code that occurs once in each version separates regions: a
    /// repeated one, such as Ruby's `end`, marks no place.
    #[test]
    fn a_unique_line_of_code_separates_regions() {
        let regions = |separator: &str| {
            let old = formatdoc! {"
                first
                {separator}
                second
                end
            "};
            let new = formatdoc! {"
                first changed
                {separator}
                second changed
                end
            "};
            let file = chunks(None, Some(&old), Some(&new));
            file.chunks
                .iter()
                .map(|chunk| chunk.region)
                .collect::<Vec<_>>()
        };
        assert_eq!(regions("middle"), [0, 1]);
        assert_eq!(regions("end"), [0, 0]);
        assert_eq!(regions("});"), [0, 0]);
    }

    /// The edit script of the one chunk between `old` and `new`, checked to cover both texts in
    /// order, byte for byte, with layout only between whitespace.
    fn script(
        language: Option<Language>,
        old: &str,
        new: &str,
    ) -> (Chunk, Vec<(EditKind, String, String)>) {
        let file = chunks(language, Some(old), Some(new));
        assert_eq!(file.chunks.len(), 1, "one chunk");
        let chunk = file
            .chunks
            .into_iter()
            .next()
            .map(|chunk| chunk.chunk)
            .unwrap();
        let (mut removed_at, mut added_at) = (0, 0);
        let mut steps = Vec::new();
        for edit in &chunk.edits {
            assert_eq!(
                (edit.removed.start, edit.added.start),
                (removed_at, added_at)
            );
            let removed = &chunk.removed[edit.removed.clone()];
            let added = &chunk.added[edit.added.clone()];
            match edit.kind {
                EditKind::Same => assert_eq!(removed, added),
                EditKind::Layout | EditKind::Whitespace => {
                    assert!(removed.trim().is_empty() && added.trim().is_empty());
                }
                EditKind::Changed => {}
            }
            steps.push((edit.kind, removed.to_owned(), added.to_owned()));
            (removed_at, added_at) = (edit.removed.end, edit.added.end);
        }
        assert_eq!(
            (removed_at, added_at),
            (chunk.removed.len(), chunk.added.len())
        );
        (chunk, steps)
    }

    /// The changes of an edit script, as removed and added text.
    fn changes(steps: &[(EditKind, String, String)]) -> Vec<(&str, &str)> {
        steps
            .iter()
            .filter(|(kind, _, _)| *kind == EditKind::Changed)
            .map(|(_, removed, added)| (removed.as_str(), added.as_str()))
            .collect()
    }

    /// Code wrapped in a call and rewrapped by rustfmt changes only its tokens: the line broken
    /// anew and the fields re-indented are layout.
    #[test]
    fn a_rewrapped_call_changes_only_its_tokens() {
        let old = indoc! {"
            fn reasoning(id: &str) -> ThreadItem {
                ThreadItem::Reasoning(Box::new(ReasoningThreadItem {
                    content: Some(Vec::new()),
                    id: id.to_owned(),
                }))
            }
        "};
        let new = indoc! {"
            fn reasoning(id: &str) -> ThreadItem {
                item(ThreadItemVariant::Reasoning(Box::new(
                    ReasoningThreadItem {
                        content: Some(Vec::new()),
                        id: id.to_owned(),
                    },
                )))
            }
        "};
        let (chunk, steps) = script(Some(Language::Rust), old, new);
        // The comma and one of the three closing parentheses are inserted as one run, broken
        // inside the brackets, where a line break means nothing.
        assert_eq!(
            changes(&steps),
            [
                ("ThreadItem", "item(ThreadItemVariant"),
                ("", ","),
                ("", ")")
            ]
        );
        assert!(
            steps
                .iter()
                .all(|(kind, _, _)| *kind != EditKind::Whitespace),
            "{steps:?}"
        );
        let words: Vec<&str> = chunk
            .added_highlights()
            .into_iter()
            .map(|range| &chunk.added[range])
            .collect();
        assert_eq!(words, ["item(ThreadItemVariant", ",", ")"]);
        assert!(steps.iter().any(|(kind, _, _)| *kind == EditKind::Layout));
    }

    /// Where whitespace carries meaning, or the proof cannot tell, a whitespace change is a change:
    /// Python's indentation, touching tokens a Rust macro sees, also beside a deleted token, a
    /// line ending, and any change in a language the proof does not support.
    #[test]
    fn significant_whitespace_is_a_change() {
        let whitespace_changed = |language: Option<Language>, old: &str, new: &str| {
            let (_, steps) = script(language, old, new);
            assert!(
                steps
                    .iter()
                    .any(|(kind, _, _)| *kind == EditKind::Whitespace),
                "{steps:?}"
            );
            assert!(
                steps.iter().all(|(kind, _, _)| *kind != EditKind::Layout),
                "{steps:?}"
            );
        };
        whitespace_changed(
            Some(Language::Python),
            indoc! {"
                def f():
                    return 1
            "},
            indoc! {"
                def f():
                        return 1
            "},
        );
        whitespace_changed(
            Some(Language::Rust),
            indoc! {"
                tokens!(+
                -);
            "},
            "tokens!(+-);\n",
        );
        // A deleted `*` would show touching the `-`, which it did not.
        whitespace_changed(
            Some(Language::Rust),
            indoc! {"
                tokens!(+*
                -);
            "},
            "tokens!(+-);\n",
        );
        whitespace_changed(Some(Language::Rust), "fn f() {}\n", "fn f() {}\r\n");
        whitespace_changed(
            None,
            indoc! {"
                call(a,
                  b)
            "},
            "call(a, b)\n",
        );
    }

    /// A line inserted or deleted whole is one change, from its indentation through its line
    /// break, and changes no whitespace of the code around it: a comment above an edited line, a
    /// statement deleted above an edited one, a Python statement beside its neighbors. A
    /// Python statement that indents the next one changes its whitespace.
    #[test]
    fn a_line_inserted_or_deleted_whole_is_one_change() {
        let changes = |language: Language, old: &str, new: &str| -> Vec<(String, String)> {
            let (_, steps) = script(Some(language), old, new);
            assert!(
                steps
                    .iter()
                    .all(|(kind, _, _)| *kind != EditKind::Whitespace),
                "{steps:?}"
            );
            steps
                .into_iter()
                .filter(|(kind, _, _)| *kind == EditKind::Changed)
                .map(|(_, old, new)| (old, new))
                .collect()
        };
        let pair = |old: &str, new: &str| (old.to_owned(), new.to_owned());
        assert_eq!(
            changes(
                Language::Rust,
                indoc! {"
                    fn f(ceiling: u64) {
                        if ceiling == 0 {
                            return;
                        }
                    }
                "},
                indoc! {"
                    fn f(ceiling: u64) {
                        // Nothing to spread.
                        if ceiling <= 1 {
                            return;
                        }
                    }
                "},
            ),
            [
                pair("", "    // Nothing to spread.\n"),
                pair("== 0", "<= 1")
            ]
        );
        assert_eq!(
            changes(
                Language::Rust,
                indoc! {"
                    fn f() {
                        a(1);
                        obsolete();
                        b(2);
                    }
                "},
                indoc! {"
                    fn f() {
                        a(1);
                        b(20);
                    }
                "},
            ),
            [pair("    obsolete();\n", ""), pair("2", "20")]
        );
        let before = indoc! {"
            def f():
                prepare()
                send()
        "};
        assert_eq!(
            changes(
                Language::Python,
                before,
                indoc! {"
                    def f():
                        prepare()
                        check()
                        send()
                "},
            ),
            [pair("", "    check()\n")]
        );
        let (_, steps) = script(
            Some(Language::Python),
            before,
            indoc! {"
                def f():
                    prepare()
                    if ready:
                        send()
            "},
        );
        assert!(
            steps
                .iter()
                .any(|(kind, _, _)| *kind == EditKind::Whitespace)
        );
    }

    /// A Python call renamed and dedented out of its block shows both: its indentation is a
    /// whitespace change of its own, never folded into the rename.
    #[test]
    fn a_dedent_beside_a_rename_stays_a_whitespace_change() {
        let (_, steps) = script(
            Some(Language::Python),
            indoc! {"
                if ready:
                    prepare()
                    send(payload)
            "},
            indoc! {"
                if ready:
                    prepare()
                deliver(payload)
            "},
        );
        assert_eq!(changes(&steps), [("send", "deliver")]);
        assert!(
            steps
                .iter()
                .any(|(kind, removed, added)| *kind == EditKind::Whitespace
                    && removed.ends_with("    ")
                    && !added.ends_with(' ')),
            "{steps:?}"
        );
    }

    /// A line ending that changes inside a chunk is never layout, even where the proof counts
    /// line breaks alike.
    #[test]
    fn a_changed_line_ending_inside_a_chunk_shows() {
        let crlf = |text: &str| text.replace('\n', "\r\n");
        let old = crlf(indoc! {"
            fn f() {
                let a = 1;
            }
        "});
        // The first line's ending turns LF, and the next line's number changes.
        let rest = crlf(indoc! {"
                let a = 2;
            }
        "});
        let new = ["fn f() {\n", &rest].concat();
        let (_, steps) = script(Some(Language::Rust), &old, &new);
        assert!(
            steps
                .iter()
                .any(|(kind, removed, _)| *kind == EditKind::Whitespace && removed.contains('\r')),
            "{steps:?}"
        );
        assert!(
            steps.iter().all(|(kind, _, _)| *kind != EditKind::Layout),
            "{steps:?}"
        );
    }

    /// Each chunk's proof, and the whole file's.
    fn proofs(language: Language, old: &str, new: &str) -> (Vec<bool>, bool) {
        let file = chunks(Some(language), Some(old), Some(new));
        let proven = file.chunks.iter().map(|chunk| chunk.proven).collect();
        (proven, file.formatting_only)
    }

    /// Each chunk's token hint: whether only whitespace differs, which makes it a candidate.
    fn hints(language: Language, old: &str, new: &str) -> Vec<bool> {
        chunks(Some(language), Some(old), Some(new))
            .chunks
            .iter()
            .map(|file_chunk| file_chunk.chunk.whitespace_only)
            .collect()
    }

    /// A brace reflow is proven on its own, beside a real change elsewhere in the file.
    #[test]
    fn a_rust_brace_reflow_is_proven_beside_a_real_change() {
        let old = indoc! {"
            fn f(xs: &[u32]) -> u32 {
                for x in xs { g(*x); }
                let a = 1;
                let b = 2;
                a + b
            }
        "};
        let new = indoc! {"
            fn f(xs: &[u32]) -> u32 {
                for x in xs {
                    g(*x);
                }
                let a = 1;
                let b = 3;
                a + b
            }
        "};
        assert_eq!(proofs(Language::Rust, old, new), (vec![true, false], false));
    }

    /// A file with one reformatted chunk and one real one proves only the first.
    #[test]
    fn only_the_reformatted_chunk_of_a_mixed_file_is_proven() {
        let old = indoc! {"
            export function f(a: number, b: number) { return g(a, b); }

            export const limit = 100;
        "};
        let new = indoc! {"
            export function f(a: number, b: number) {
              return g(a, b);
            }

            export const limit = 300;
        "};
        assert_eq!(
            proofs(Language::TypeScript, old, new),
            (vec![true, false], false)
        );
    }

    /// Re-indenting a Python statement out of its block keeps its tokens but changes the
    /// program, so it is never proven.
    #[test]
    fn a_python_statement_moved_out_of_a_block_is_not_proven() {
        let old = indoc! {"
            def f(x):
                if x:
                    a()
                    b()
                c = 0
                return 1
        "};
        let new = indoc! {"
            def f(x):
                if x:
                    a()
                b()
                c = 0
                return 2
        "};
        assert_eq!(
            hints(Language::Python, old, new),
            [true, false],
            "a candidate"
        );
        assert_eq!(
            proofs(Language::Python, old, new),
            (vec![false, false], false)
        );
    }

    /// Any parse error among the four trees leaves a chunk unproven: here a hybrid that does
    /// not parse, though both files do.
    #[test]
    fn a_hybrid_that_does_not_parse_is_not_proven() {
        let old = indoc! {"
            fn f() {
                a();
                b();
            }
        "};
        let new = indoc! {"
            fn f() {
                a(
                b());
            }
        "};
        let parsed = syntax::parse(Language::Rust, old).zip(syntax::parse(Language::Rust, new));
        let (old_side, new_side) = (Side::new(old), Side::new(new));
        // Only the first line taken as the chunk: applied alone, its open call never closes.
        let hunk = text::Hunk {
            before: 1..2,
            after: 1..2,
        };
        let forward = indoc! {"
            fn f() {
                a(
                b();
            }
        "};
        assert!(
            syntax::parse(Language::Rust, forward)
                .and_then(|parsed| parsed.canonical)
                .is_none()
        );
        let mut budget = usize::MAX;
        assert!(!chunk_is_formatting_only(
            Some(Language::Rust),
            &old_side,
            &new_side,
            parsed.as_ref(),
            &hunk,
            &mut budget,
        ));
    }

    /// The three canonical-form gaps a mixed file exposed: each chunk keeps its tokens beside
    /// a real change elsewhere, and none is a reformat.
    #[test]
    fn significant_whitespace_chunks_are_not_proven_beside_real_changes() {
        let cases = [
            (
                Language::Python,
                indoc! {"
                    if ready:
                        a()
                        # \\
                        b()

                    limit = 1
                "},
                indoc! {"
                    if ready:
                        a()
                        # \\
                          b()

                    limit = 2
                "},
            ),
            (
                Language::JavaScript,
                indoc! {"
                    function f() {
                      throw new Error(\"denied\");
                    }
                    const a = 1;
                    const b = 2;
                "},
                indoc! {"
                    function f() {
                      throw
                      new Error(\"denied\");
                    }
                    const a = 1;
                    const b = 3;
                "},
            ),
            (
                Language::Rust,
                indoc! {"
                    const FLAG: bool = first_spacing!(++);

                    const LIMIT: u32 = 1;
                "},
                indoc! {"
                    const FLAG: bool = first_spacing!(+ +);

                    const LIMIT: u32 = 2;
                "},
            ),
        ];
        for (language, old, new) in cases {
            assert_eq!(
                hints(language, old, new),
                [true, false],
                "{language:?} candidate"
            );
            assert_eq!(
                proofs(language, old, new),
                (vec![false, false], false),
                "{language:?}"
            );
        }
    }

    /// A candidate past the budget stays unproven and spends nothing.
    #[test]
    fn candidates_past_the_budget_stay_unproven() {
        let old = indoc! {"
            fn f() {
                for x in xs { g(x); }
                let z = 0;
                let a = 1;
            }
        "};
        let new = indoc! {"
            fn f() {
                for x in xs {
                    g(x);
                }
                let z = 0;
                let a = 2;
            }
        "};
        let parsed = syntax::parse(Language::Rust, old).zip(syntax::parse(Language::Rust, new));
        let (old_side, new_side) = (Side::new(old), Side::new(new));
        let hunk = text::diff_lines(&old_side.lines, &new_side.lines)
            .into_iter()
            .next()
            .unwrap();
        let prove = |budget: &mut usize| {
            chunk_is_formatting_only(
                Some(Language::Rust),
                &old_side,
                &new_side,
                parsed.as_ref(),
                &hunk,
                budget,
            )
        };
        let cost = old.len() + new.len();
        let mut short = cost - 1;
        assert!(!prove(&mut short));
        assert_eq!(short, cost - 1, "an unproven candidate spends nothing");
        let mut enough = cost;
        assert!(prove(&mut enough));
        assert_eq!(enough, 0);
    }

    /// CRLF line endings and a last line without a terminator: the reflow at the end of the
    /// file is proven, the real change is not, and the splices keep every byte.
    #[test]
    fn a_reflow_at_a_crlf_end_of_file_is_proven() {
        let old = indoc! {"
            fn a() -> u32 {\r
                1\r
            }\r
            \r
            fn b() { c(1, 2) }"};
        let new = indoc! {"
            fn a() -> u32 {\r
                2\r
            }\r
            \r
            fn b() {\r
                c(1, 2)\r
            }"};
        assert_eq!(proofs(Language::Rust, old, new), (vec![false, true], false));
    }

    /// A parse error in the new file leaves even a pure reflow elsewhere unproven.
    #[test]
    fn a_file_with_a_parse_error_proves_nothing() {
        let old = indoc! {"
            fn f() {
                for x in xs { g(x); }
                let a = 1;
                let b = 2;
            }
        "};
        let new = indoc! {"
            fn f() {
                for x in xs {
                    g(x);
                }
                let a = 1;
                let b = ;
            }
        "};
        assert_eq!(
            proofs(Language::Rust, old, new),
            (vec![false, false], false)
        );
    }

    /// Splitting `return x` over two lines lets automatic semicolon insertion end the return,
    /// so the grammar sees a different program.
    #[test]
    fn a_javascript_return_split_from_its_value_is_not_proven() {
        let old = indoc! {"
            function f(x) {
              return x;
            }
            const a = 1;
            const b = 2;
        "};
        let new = indoc! {"
            function f(x) {
              return
              x;
            }
            const a = 1;
            const b = 3;
        "};
        assert_eq!(
            hints(Language::JavaScript, old, new),
            [true, false],
            "a candidate"
        );
        assert_eq!(
            proofs(Language::JavaScript, old, new),
            (vec![false, false], false)
        );
    }

    /// A chunk inside a string that another chunk opens is a reformat in the old file but not
    /// in the new one, so the backward hybrid rejects it.
    #[test]
    fn a_chunk_inside_a_string_another_chunk_opens_is_not_proven() {
        let old = indoc! {"
            fn f() {
                let a = 1;
                let x = 0;
                let b = foo(2);
                let y = 0;
                let c = 3;
            }
        "};
        let new = indoc! {"
            fn f() {
                let a = \"
                let x = 0;
                let b = foo( 2 );
                let y = 0;
                \";
            }
        "};
        let (proven, formatting_only) = proofs(Language::Rust, old, new);
        assert!(!formatting_only);
        assert!(proven.iter().all(|&proven| !proven), "{proven:?}");
        // Even taken as a whitespace-only candidate, the proof refuses it.
        let (old_side, new_side) = (Side::new(old), Side::new(new));
        let hunk = text::diff_lines(&old_side.lines, &new_side.lines)
            .into_iter()
            .find(|hunk| hunk.before == (3..4))
            .unwrap();
        let parsed = syntax::parse(Language::Rust, old).zip(syntax::parse(Language::Rust, new));
        let mut budget = usize::MAX;
        assert!(!chunk_is_formatting_only(
            Some(Language::Rust),
            &old_side,
            &new_side,
            parsed.as_ref(),
            &hunk,
            &mut budget,
        ));
    }

    #[test]
    fn escaping_is_lossless_and_printable() {
        assert_eq!(escape_bytes(b"dir/\x80"), "dir/\\x80");
        assert_eq!(escape_bytes(b"dir/\x81"), "dir/\\x81");
        // A literal backslash cannot be confused with an escaped byte.
        assert_eq!(escape_bytes(b"dir/\\x80"), "dir/\\\\x80");
        assert_eq!(escape_bytes("q\"\t\u{e9}".as_bytes()), "q\\\"\\x09\u{e9}");
    }
}
