//! Computing an [`Analysis`] from a snapshot pair.

use std::ops::Range;

use super::{
    ANALYSIS_FORMAT, Analysis, Chunk, EntrySide, FileChange, FileUnit, LineRange, Unit, UnitKind,
    hex_digest, moves, noise,
};
use crate::error::Error;
use crate::git::{Blob, BlobReader, ChangeStatus, EntryType, RawChange, RawSide, Repo};
use crate::ids::{ObjectId, UnitIndex};
use crate::syntax::{self, Language};
use crate::text::{self, changed_token_ranges, generic_tokens, split_lines};

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
    };
    for change in changes {
        let is_generated = generated
            .get(display_path(&change))
            .copied()
            .unwrap_or(false);
        builder.add_file(change, is_generated)?;
    }
    let Builder {
        files, mut units, ..
    } = builder;
    let moves = moves::detect(&mut units);
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
            for chunk in file_chunks.chunks {
                indices.push(self.push_unit(file_index, UnitKind::Chunk(chunk), formatting_only));
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
    chunks: Vec<Chunk>,
    formatting_only: bool,
}

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

    let chunks = hunks
        .iter()
        .map(|hunk| {
            let removed = old.join(&hunk.before);
            let added = new.join(&hunk.after);
            let (removed_tokens, added_tokens) = match &parsed {
                Some((old_parsed, new_parsed)) => (
                    clip_tokens(&old_parsed.tokens, &old.byte_range(&hunk.before)),
                    clip_tokens(&new_parsed.tokens, &new.byte_range(&hunk.after)),
                ),
                None => (generic_tokens(&removed), generic_tokens(&added)),
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
            let highlights = if formatting_only || removed.is_empty() || added.is_empty() {
                text::ChangedRanges::default()
            } else {
                changed_token_ranges(&removed, &removed_tokens, &added, &added_tokens)
            };
            let mut context = old.context(&hunk.before);
            context.push('\0');
            context.push_str(&new.context(&hunk.after));
            Chunk {
                old: LineRange {
                    start: hunk.before.start,
                    len: hunk.before.len(),
                },
                new: LineRange {
                    start: hunk.after.start,
                    len: hunk.after.len(),
                },
                removed_highlights: highlights.before,
                added_highlights: highlights.after,
                removed,
                added,
                whitespace_only,
                context: hex_digest(context.as_bytes()),
            }
        })
        .collect();
    FileChunks {
        chunks,
        formatting_only,
    }
}

/// Returns the tokens overlapping `range`, clipped to it and made relative to its start.
fn clip_tokens(tokens: &[Range<usize>], range: &Range<usize>) -> Vec<Range<usize>> {
    let first = tokens.partition_point(|token| token.end <= range.start);
    tokens
        .get(first..)
        .unwrap_or_default()
        .iter()
        .take_while(|token| token.start < range.end)
        .map(|token| {
            token.start.max(range.start) - range.start..token.end.min(range.end) - range.start
        })
        .filter(|token| !token.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::escape_bytes;

    #[test]
    fn escaping_is_lossless_and_printable() {
        assert_eq!(escape_bytes(b"dir/\x80"), "dir/\\x80");
        assert_eq!(escape_bytes(b"dir/\x81"), "dir/\\x81");
        // A literal backslash cannot be confused with an escaped byte.
        assert_eq!(escape_bytes(b"dir/\\x80"), "dir/\\\\x80");
        assert_eq!(escape_bytes("q\"\t\u{e9}".as_bytes()), "q\\\"\\x09\u{e9}");
    }
}
