//! The bat tier: bat's syntaxes, through two-face, highlighted by syntect on the fancy-regex
//! engine, for every file and injection the tree-house tier does not cover.
//!
//! syntect cannot stop inside a line, and a line can be the whole file, so an input with a line
//! longer than [`MAX_LINE_BYTES`] stays plain: it is checked before syntect matches a first line
//! or parses anything.
//! Skipping only the long line and going on would carry the wrong state past it, as after an
//! unterminated string.
//! Each input is parsed from its start with one [`ParseState`] and one [`ScopeStack`], and the
//! deadline is checked between lines; once it passes, the rest of the input stays plain.

use std::collections::HashMap;
use std::sync::LazyLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use super::budget::Deadline;
use super::tones::{Tone, scope_tone};
use super::{Run, Runs};

/// The longest line syntect is given; an input with a longer one stays plain.
pub(super) const MAX_LINE_BYTES: usize = 16 * 1024;

/// The name of bat's syntax for plain text, which highlights nothing.
const PLAIN_TEXT: &str = "Plain Text";

/// bat's syntaxes, loaded once; the newline variant, as each line is handed over with its `\n`.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// The syntax of this name, such as `SQL`; never plain text.
pub(super) fn by_name(name: &str) -> Option<&'static SyntaxReference> {
    highlighting(SYNTAXES.find_syntax_by_name(name))
}

/// The syntax a document's name for a language selects, such as a Markdown fence's `kotlin`:
/// by the syntax's name or one of its extensions, ignoring case.
pub(super) fn by_token(token: &str) -> Option<&'static SyntaxReference> {
    highlighting(SYNTAXES.find_syntax_by_token(token))
}

/// The syntax of a file at `path`: by its whole file name, then by its extension, the longest
/// first, as `.d.ts` before `.ts`.
pub(super) fn by_path(path: &str) -> Option<&'static SyntaxReference> {
    highlighting(by_path_or_plain(path))
}

/// The syntax of a file at `path` as [`by_path`] finds it, plain text included, which says the
/// file is known to hold plain text, such as a `.txt` file.
pub(super) fn by_path_or_plain(path: &str) -> Option<&'static SyntaxReference> {
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    SYNTAXES.find_syntax_by_extension(file_name).or_else(|| {
        file_name
            .match_indices('.')
            .filter_map(|(dot, _)| file_name.get(dot + 1..))
            .find_map(|extension| SYNTAXES.find_syntax_by_extension(extension))
    })
}

/// Whether `syntax` is bat's plain text, which highlights nothing.
pub(super) fn is_plain(syntax: &SyntaxReference) -> bool {
    syntax.name == PLAIN_TEXT
}

/// How many syntaxes bat has.
pub(super) fn count() -> usize {
    SYNTAXES.syntaxes().len()
}

/// The position among bat's syntaxes of the syntax of this name.
pub(super) fn position(name: &str) -> Option<usize> {
    SYNTAXES
        .syntaxes()
        .iter()
        .position(|syntax| syntax.name == name)
}

/// The syntax `interpreter` selects on a `#!` line, by the syntaxes' first-line rules.
pub(super) fn by_interpreter(interpreter: &str) -> Option<&'static SyntaxReference> {
    highlighting(SYNTAXES.find_syntax_by_first_line(&format!("#!/usr/bin/env {interpreter}")))
}

/// The syntax `text`'s first line selects, such as `<?xml` or a `#!` line; none when `text` has
/// a line too long for syntect.
pub(super) fn by_first_line(text: &str) -> Option<&'static SyntaxReference> {
    if !fits(text) {
        return None;
    }
    let first = text.lines().next()?;
    highlighting(SYNTAXES.find_syntax_by_first_line(first))
}

/// `syntax`, unless it is plain text, which is no highlighting at all.
fn highlighting(syntax: Option<&'static SyntaxReference>) -> Option<&'static SyntaxReference> {
    syntax.filter(|syntax| !is_plain(syntax))
}

/// Whether every line of `text` is short enough for syntect.
pub(super) fn fits(text: &str) -> bool {
    text.split('\n').all(|line| line.len() <= MAX_LINE_BYTES)
}

/// Highlights `text` from its start as `syntax`: its runs in bytes of `text`, without plain
/// ones.
///
/// Nothing is highlighted when a line is too long, and nothing after the line at which
/// `deadline` passes or syntect fails.
pub(super) fn highlight(syntax: &SyntaxReference, text: &str, deadline: Deadline) -> Vec<Run> {
    let mut runs = Runs::default();
    if !fits(text) {
        return runs.into_vec();
    }
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut tones = ToneCache::default();
    let mut tone = Tone::Plain;
    let mut line = String::new();
    let mut line_start = 0;
    for piece in text.split_inclusive('\n') {
        if deadline.expired() {
            break;
        }
        let content = piece.strip_suffix('\n').unwrap_or(piece);
        let content = content.strip_suffix('\r').unwrap_or(content);
        // syntect's newline syntaxes match a line with its `\n`, never a `\r`.
        line.clear();
        line.push_str(content);
        line.push('\n');
        let Ok(operations) = state.parse_line(&line, &SYNTAXES) else {
            break;
        };
        // Each operation applies at its offset: the text before it keeps the scopes before it.
        let mut position = 0;
        let mut failed = false;
        for (offset, operation) in operations {
            let offset = offset.min(content.len());
            if offset > position {
                runs.push(line_start + position..line_start + offset, tone);
                position = offset;
            }
            if stack.apply(&operation).is_err() {
                failed = true;
                break;
            }
            tone = tones.of(&stack);
        }
        if failed {
            break;
        }
        if content.len() > position {
            runs.push(line_start + position..line_start + content.len(), tone);
        }
        line_start += piece.len();
    }
    runs.into_vec()
}

/// The tones of scopes seen so far, as reading a scope's name allocates.
#[derive(Default)]
struct ToneCache {
    tones: HashMap<Scope, Option<Tone>>,
}

impl ToneCache {
    /// The tone of the innermost scope of `stack` that has one.
    fn of(&mut self, stack: &ScopeStack) -> Tone {
        stack
            .as_slice()
            .iter()
            .rev()
            .find_map(|scope| {
                *self
                    .tones
                    .entry(*scope)
                    .or_insert_with(|| scope_tone(&scope.build_string()))
            })
            .unwrap_or(Tone::Plain)
    }
}
