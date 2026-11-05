//! Syntax highlighting with Zed's highlight queries, on the tree-sitter `review-core` links, and
//! the named scopes of each file, computed once per file side off the UI thread.
//!
//! The queries under `queries/` are copied from Zed (GPL-3.0-or-later, like this crate); TOML's
//! comes with its grammar crate. As in Zed, a later pattern overrides an earlier one for the same
//! node, and an inner node overrides the node around it.

use std::ops::Range;
use std::sync::OnceLock;

use review_core::scope::ScopeMap;
use review_core::syntax::Language;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator as _};

/// A highlight class; the palette gives each a color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    /// Plain code, such as variables: the default text color.
    Plain,
    Keyword,
    Function,
    Type,
    String,
    /// Numbers, booleans and other constants.
    Constant,
    Comment,
    Parameter,
    Attribute,
    Operator,
    Punctuation,
    Property,
    /// `self`, `this` and other special variables.
    Special,
    Tag,
    /// Escapes, regexes and other special string parts.
    Escape,
}

/// Maps a capture name to its tone by its most specific known prefix; `None` for captures that
/// only help other patterns, such as `@_name`.
fn tone_of(capture: &str) -> Option<Tone> {
    if capture.starts_with('_') {
        return None;
    }
    let tones: &[(&str, Tone)] = &[
        ("comment", Tone::Comment),
        ("keyword", Tone::Keyword),
        ("operator", Tone::Operator),
        ("string.escape", Tone::Escape),
        ("string.regex", Tone::Escape),
        ("string.special", Tone::Escape),
        ("string", Tone::String),
        ("character", Tone::String),
        ("number", Tone::Constant),
        ("boolean", Tone::Constant),
        ("constant.builtin", Tone::Constant),
        ("constant", Tone::Constant),
        ("function", Tone::Function),
        ("constructor", Tone::Type),
        ("type", Tone::Type),
        ("attribute", Tone::Attribute),
        ("variable.parameter", Tone::Parameter),
        ("variable.special", Tone::Special),
        ("property", Tone::Property),
        ("tag", Tone::Tag),
        ("label", Tone::Special),
        ("lifetime", Tone::Special),
        ("punctuation", Tone::Punctuation),
        ("embedded", Tone::Plain),
        ("variable", Tone::Plain),
    ];
    let known = tones.iter().find(|(prefix, _)| {
        capture == *prefix
            || capture
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    });
    Some(known.map_or(Tone::Plain, |&(_, tone)| tone))
}

/// One highlighted run within a line, in bytes of the line.
pub type Span = (Range<usize>, Tone);

/// The highlights and scopes of one version of a file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideSyntax {
    /// The named scopes, for breadcrumbs.
    pub scopes: ScopeMap,
    /// Each line's highlighted runs, without plain ones.
    pub lines: Vec<Vec<Span>>,
}

impl SideSyntax {
    /// The highlighted runs of the zero-based `line`.
    #[must_use]
    pub fn line(&self, line: usize) -> &[Span] {
        self.lines.get(line).map_or(&[], Vec::as_slice)
    }
}

/// The highlights and scopes of both versions of a changed file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSyntax {
    /// The head version.
    pub head: Option<std::sync::Arc<SideSyntax>>,
    /// The base version.
    pub base: Option<std::sync::Arc<SideSyntax>>,
}

/// The compiled highlight query of `language`, built once.
fn query(language: Language) -> Option<&'static Query> {
    static QUERIES: [OnceLock<Option<Query>>; 8] = [const { OnceLock::new() }; 8];
    let (slot, source) = match language {
        Language::Rust => (0, include_str!("../queries/rust/highlights.scm")),
        Language::Python => (1, include_str!("../queries/python/highlights.scm")),
        Language::JavaScript => (2, include_str!("../queries/javascript/highlights.scm")),
        Language::TypeScript => (3, include_str!("../queries/typescript/highlights.scm")),
        Language::Tsx => (4, include_str!("../queries/tsx/highlights.scm")),
        Language::Go => (5, include_str!("../queries/go/highlights.scm")),
        Language::Json => (6, include_str!("../queries/json/highlights.scm")),
        Language::Toml => (7, tree_sitter_toml_ng::HIGHLIGHTS_QUERY),
    };
    QUERIES
        .get(slot)?
        .get_or_init(|| Query::new(&language.grammar(), source).ok())
        .as_ref()
}

/// Highlights `text` as the language of `path` and reads its scopes; plain for unsupported
/// languages.
#[must_use]
pub fn analyze(path: &str, text: &str) -> SideSyntax {
    let Some(language) = Language::from_path(path) else {
        return SideSyntax::default();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language.grammar()).is_err() {
        return SideSyntax::default();
    }
    let Some(tree) = parser.parse(text, None) else {
        return SideSyntax::default();
    };
    let scopes = ScopeMap::from_tree(language, &tree, text);
    let Some(query) = query(language) else {
        return SideSyntax {
            scopes,
            lines: Vec::new(),
        };
    };
    // Every capture with its tone; painted outer before inner, earlier pattern before later,
    // so the innermost, latest pattern wins.
    let names = query.capture_names();
    let mut captures: Vec<(Range<usize>, usize, Tone)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
    while let Some(found) = matches.next() {
        for capture in found.captures() {
            let name = names.get(capture.index as usize).copied().unwrap_or("_");
            if let Some(tone) = tone_of(name) {
                captures.push((capture.node.byte_range(), found.pattern_index, tone));
            }
        }
    }
    captures.sort_by_key(|(range, pattern, _)| (std::cmp::Reverse(range.len()), *pattern));
    let mut painted = vec![Tone::Plain; text.len()];
    for (range, _, tone) in captures {
        if let Some(bytes) = painted.get_mut(range) {
            bytes.fill(tone);
        }
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']).len();
        lines.push(runs(
            painted.get(start..start + content).unwrap_or_default(),
        ));
        start += line.len();
    }
    SideSyntax { scopes, lines }
}

/// Collapses per-byte tones into runs, leaving out plain ones.
fn runs(tones: &[Tone]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut start = 0;
    for (index, tone) in tones.iter().enumerate() {
        let next = tones.get(index + 1);
        if next != Some(tone) {
            if *tone != Tone::Plain {
                spans.push((start..index + 1, *tone));
            }
            start = index + 1;
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every query compiles against the grammar it is used with.
    #[test]
    fn queries_compile() {
        for language in [
            Language::Rust,
            Language::Python,
            Language::JavaScript,
            Language::TypeScript,
            Language::Tsx,
            Language::Go,
            Language::Json,
            Language::Toml,
        ] {
            assert!(
                query(language).is_some(),
                "{language:?}'s query does not compile"
            );
        }
    }

    /// The tone of `needle`'s first byte on the zero-based `line`.
    fn tone_at(syntax: &SideSyntax, line: usize, column: usize) -> Tone {
        syntax
            .line(line)
            .iter()
            .find(|(range, _)| range.contains(&column))
            .map_or(Tone::Plain, |(_, tone)| *tone)
    }

    /// Keywords, functions, types, strings, numbers and comments get their tones, and a later,
    /// more specific pattern overrides a generic one.
    #[test]
    fn rust_is_highlighted() {
        let text = "// note\npub fn apply(x: u32) -> String {\n    let s = \"a\\n\";\n    42\n}\n";
        let syntax = analyze("lib.rs", text);
        assert_eq!(tone_at(&syntax, 0, 0), Tone::Comment);
        assert_eq!(tone_at(&syntax, 1, 0), Tone::Keyword);
        assert_eq!(tone_at(&syntax, 1, 7), Tone::Function);
        assert_eq!(tone_at(&syntax, 1, 16), Tone::Type);
        assert_eq!(tone_at(&syntax, 2, 12), Tone::String);
        assert_eq!(tone_at(&syntax, 2, 14), Tone::Escape);
        assert_eq!(tone_at(&syntax, 3, 4), Tone::Constant);
    }

    #[test]
    fn other_languages_are_highlighted() {
        let python = analyze("a.py", "def f():\n    return 'x'\n");
        assert_eq!(tone_at(&python, 0, 0), Tone::Keyword);
        assert_eq!(tone_at(&python, 1, 11), Tone::String);
        let toml = analyze("Cargo.toml", "[package]\nname = \"a\"\n");
        assert_eq!(tone_at(&toml, 1, 8), Tone::String);
        let tsx = analyze("a.tsx", "const x = <div className=\"a\" />;\n");
        assert_eq!(tone_at(&tsx, 0, 0), Tone::Keyword);
        assert_eq!(analyze("notes.txt", "fn f() {}\n"), SideSyntax::default());
    }

    #[test]
    fn runs_merge_equal_tones_and_skip_plain() {
        let tones = [Tone::Keyword, Tone::Keyword, Tone::Plain, Tone::String];
        assert_eq!(
            runs(&tones),
            vec![(0..2, Tone::Keyword), (3..4, Tone::String)]
        );
    }
}
