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
            for (chunk, proven) in file_chunks.chunks.into_iter().zip(file_chunks.proven) {
                indices.push(self.push_unit(file_index, UnitKind::Chunk(chunk), proven));
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
    /// Whether the whole file is proven formatting-only.
    formatting_only: bool,
    /// For each chunk, whether it is proven formatting-only, by the whole-file proof or its
    /// own.
    proven: Vec<bool>,
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

    let mut proven = Vec::with_capacity(hunks.len());
    let mut budget = CHUNK_PROOF_BUDGET;
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
            proven.push(
                formatting_only
                    || (whitespace_only
                        && chunk_is_formatting_only(
                            language,
                            &old,
                            &new,
                            parsed.as_ref(),
                            hunk,
                            &mut budget,
                        )),
            );
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
        proven,
    }
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
    use super::{Side, chunk_is_formatting_only, chunks, escape_bytes};
    use crate::syntax::{self, Language};
    use crate::text;

    /// Each chunk's proof, and the whole file's.
    fn proofs(language: Language, old: &str, new: &str) -> (Vec<bool>, bool) {
        let file = chunks(Some(language), Some(old), Some(new));
        (file.proven, file.formatting_only)
    }

    /// Each chunk's token hint: whether only whitespace differs, which makes it a candidate.
    fn hints(language: Language, old: &str, new: &str) -> Vec<bool> {
        chunks(Some(language), Some(old), Some(new))
            .chunks
            .iter()
            .map(|chunk| chunk.whitespace_only)
            .collect()
    }

    /// A brace reflow is proven on its own, beside a real change elsewhere in the file.
    #[test]
    fn a_rust_brace_reflow_is_proven_beside_a_real_change() {
        let old = "fn f(xs: &[u32]) -> u32 {\n    for x in xs { g(*x); }\n    let a = 1;\n    let b = 2;\n    a + b\n}\n";
        let new = "fn f(xs: &[u32]) -> u32 {\n    for x in xs {\n        g(*x);\n    }\n    let a = 1;\n    let b = 3;\n    a + b\n}\n";
        assert_eq!(proofs(Language::Rust, old, new), (vec![true, false], false));
    }

    /// A file with one reformatted chunk and one real one proves only the first.
    #[test]
    fn only_the_reformatted_chunk_of_a_mixed_file_is_proven() {
        let old = "export function f(a: number, b: number) { return g(a, b); }\n\nexport const limit = 100;\n";
        let new = "export function f(a: number, b: number) {\n  return g(a, b);\n}\n\nexport const limit = 300;\n";
        assert_eq!(
            proofs(Language::TypeScript, old, new),
            (vec![true, false], false)
        );
    }

    /// Re-indenting a Python statement out of its block keeps its tokens but changes the
    /// program, so it is never proven.
    #[test]
    fn a_python_statement_moved_out_of_a_block_is_not_proven() {
        let old = "def f(x):\n    if x:\n        a()\n        b()\n    c = 0\n    return 1\n";
        let new = "def f(x):\n    if x:\n        a()\n    b()\n    c = 0\n    return 2\n";
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
        let old = "fn f() {\n    a();\n    b();\n}\n";
        let new = "fn f() {\n    a(\n    b());\n}\n";
        let parsed = syntax::parse(Language::Rust, old).zip(syntax::parse(Language::Rust, new));
        let (old_side, new_side) = (Side::new(old), Side::new(new));
        // Only the first line taken as the chunk: applied alone, its open call never closes.
        let hunk = text::Hunk {
            before: 1..2,
            after: 1..2,
        };
        let forward = "fn f() {\n    a(\n    b();\n}\n";
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
                "if ready:\n    a()\n    # \\\n    b()\n\nlimit = 1\n",
                "if ready:\n    a()\n    # \\\n      b()\n\nlimit = 2\n",
            ),
            (
                Language::JavaScript,
                "function f() {\n  throw new Error(\"denied\");\n}\nconst a = 1;\nconst b = 2;\n",
                "function f() {\n  throw\n  new Error(\"denied\");\n}\nconst a = 1;\nconst b = 3;\n",
            ),
            (
                Language::Rust,
                "const FLAG: bool = first_spacing!(++);\n\nconst LIMIT: u32 = 1;\n",
                "const FLAG: bool = first_spacing!(+ +);\n\nconst LIMIT: u32 = 2;\n",
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
        let old = "fn f() {\n    for x in xs { g(x); }\n    let z = 0;\n    let a = 1;\n}\n";
        let new = "fn f() {\n    for x in xs {\n        g(x);\n    }\n    let z = 0;\n    let a = 2;\n}\n";
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
        let old = "fn a() -> u32 {\r\n    1\r\n}\r\n\r\nfn b() { c(1, 2) }";
        let new = "fn a() -> u32 {\r\n    2\r\n}\r\n\r\nfn b() {\r\n    c(1, 2)\r\n}";
        assert_eq!(proofs(Language::Rust, old, new), (vec![false, true], false));
    }

    /// A parse error in the new file leaves even a pure reflow elsewhere unproven.
    #[test]
    fn a_file_with_a_parse_error_proves_nothing() {
        let old = "fn f() {\n    for x in xs { g(x); }\n    let a = 1;\n    let b = 2;\n}\n";
        let new =
            "fn f() {\n    for x in xs {\n        g(x);\n    }\n    let a = 1;\n    let b = ;\n}\n";
        assert_eq!(
            proofs(Language::Rust, old, new),
            (vec![false, false], false)
        );
    }

    /// Splitting `return x` over two lines lets automatic semicolon insertion end the return,
    /// so the grammar sees a different program.
    #[test]
    fn a_javascript_return_split_from_its_value_is_not_proven() {
        let old = "function f(x) {\n  return x;\n}\nconst a = 1;\nconst b = 2;\n";
        let new = "function f(x) {\n  return\n  x;\n}\nconst a = 1;\nconst b = 3;\n";
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
        let old = "fn f() {\n    let a = 1;\n    let x = 0;\n    let b = foo(2);\n    let y = 0;\n    let c = 3;\n}\n";
        let new = "fn f() {\n    let a = \"\n    let x = 0;\n    let b = foo( 2 );\n    let y = 0;\n    \";\n}\n";
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
