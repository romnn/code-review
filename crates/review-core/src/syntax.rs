//! Tree-sitter parsing for the formatting-equivalence proof and token highlighting.
//!
//! Formatting-only is a claim that two files are equivalent, so it is only made when their
//! canonical trees are equal and neither has a parse error.
//! The canonical tree records node kinds, named-ness, field names, nesting and leaf text, and
//! nothing about whitespace, except where whitespace is part of the program:
//!
//! - String, character, regex, template and comment nodes are recorded verbatim, including any
//!   bytes their grammar keeps in hidden tokens.
//! - Any non-whitespace byte between two sibling nodes is recorded, so content a grammar lexes into
//!   a hidden token can never vanish from the comparison.
//! - JSX elements record the whitespace between their children, which renders as text.
//! - Python records the indentation of every logical line, because indentation is syntax there and
//!   mixing tabs and spaces can fail even where the tree looks the same.

use std::ops::Range;

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser, Tree};

/// A language with a validated formatting-equivalence predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// Rust.
    Rust,
    /// Python.
    Python,
    /// JavaScript and JSX, parsed with the TSX grammar as Zed does.
    JavaScript,
    /// TypeScript.
    TypeScript,
    /// TypeScript with JSX.
    Tsx,
    /// Go.
    Go,
    /// JSON.
    Json,
    /// TOML.
    Toml,
}

impl Language {
    /// Detects the language from a path's extension.
    #[must_use]
    pub fn from_path(path: &str) -> Option<Self> {
        let file_name = path.rsplit('/').next().unwrap_or(path);
        let extension = file_name.rsplit_once('.').map(|(_, extension)| extension)?;
        Some(match extension {
            "rs" => Self::Rust,
            "py" | "pyi" => Self::Python,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "go" => Self::Go,
            "json" => Self::Json,
            "toml" => Self::Toml,
            _ => return None,
        })
    }

    /// The tree-sitter grammar this crate parses the language with, so front ends that
    /// highlight code share it and link one tree-sitter.
    #[must_use]
    pub fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript | Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
        }
    }

    /// Returns whether a node of `kind` is compared verbatim, whitespace included.
    fn is_verbatim(self, kind: &str) -> bool {
        const VERBATIM_PARTS: [&str; 7] = [
            "string", "comment", "char", "rune", "regex", "template", "heredoc",
        ];
        if VERBATIM_PARTS.iter().any(|part| kind.contains(part)) {
            return true;
        }
        match self {
            Self::JavaScript | Self::Tsx => kind == "jsx_text",
            _ => false,
        }
    }

    fn is_jsx(self) -> bool {
        matches!(self, Self::JavaScript | Self::Tsx)
    }

    /// Returns whether whitespace between the children of `node` is significant.
    ///
    /// - JSX element children render whitespace as text.
    /// - Rust macro fragment bindings (`$x:expr`) and tuple-index chains (`x.0.1`, where `0. 1`
    ///   would lex as a float) are single lexemes that the grammar splits into tokens.
    fn has_significant_gaps(self, node: Node<'_>) -> bool {
        match self {
            Self::JavaScript | Self::Tsx => matches!(node.kind(), "jsx_element" | "jsx_fragment"),
            Self::Rust => match node.kind() {
                "token_binding_pattern" => true,
                "field_expression" => node
                    .child_by_field_name("field")
                    .is_some_and(|field| field.kind() == "integer_literal"),
                _ => false,
            },
            _ => false,
        }
    }

    /// Returns whether a leaf with `text` must touch the next token: Rust's `'` of lifetimes and
    /// labels and `$` of macro metavariables, which the grammar splits from their identifier.
    fn glues_to_next(self, text: &[u8]) -> bool {
        self == Self::Rust && matches!(text, b"'" | b"$")
    }
}

/// A parsed file: its canonical form for the equivalence proof, and its leaf tokens.
pub(crate) struct ParsedFile {
    /// The canonical serialization, or `None` when the file has a parse error.
    pub canonical: Option<Vec<u8>>,
    /// Byte ranges of leaf tokens (verbatim nodes count as one token), in source order.
    pub tokens: Vec<Range<usize>>,
}

/// Parses `source` as `language`.
///
/// Returns `None` when the parser fails entirely.
pub(crate) fn parse(language: Language, source: &str) -> Option<ParsedFile> {
    let mut parser = Parser::new();
    parser.set_language(&language.grammar()).ok()?;
    let tree = parser.parse(source, None)?;
    Some(serialize(language, &tree, source))
}

/// Appends a length-prefixed byte string, so field boundaries are unambiguous.
fn push_field(out: &mut Vec<u8>, tag: u8, bytes: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Walks the tree in source order, emitting the canonical form and collecting leaf tokens.
struct Serializer<'a> {
    language: Language,
    source: &'a str,
    canonical: Vec<u8>,
    tokens: Vec<Range<usize>>,
    /// Python only: bracket nesting depth, inside which line breaks are insignificant.
    bracket_depth: usize,
    /// The end of the last emitted leaf.
    last_leaf_end: usize,
    /// Whether the last leaf must touch the next one, so whitespace after it is significant.
    after_glue: bool,
}

/// One open node during serialization.
#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Where the previous child ended, or the node's start before its first child.
    previous_end: usize,
    /// Whether whitespace between the node's children is significant.
    significant_gaps: bool,
    /// Whether the node is (part of) a JSX element name, where `a.b` and `a. b` differ.
    jsx_name: bool,
}

fn serialize(language: Language, tree: &Tree, source: &str) -> ParsedFile {
    let root = tree.root_node();
    let mut serializer = Serializer {
        language,
        source,
        canonical: Vec::with_capacity(source.len() * 2),
        tokens: Vec::new(),
        bracket_depth: 0,
        last_leaf_end: 0,
        after_glue: false,
    };
    serializer.gap(0, root.start_byte(), false);
    serializer.node(root);
    serializer.gap(root.end_byte(), source.len(), false);
    let has_error = root.has_error();
    ParsedFile {
        canonical: (!has_error).then_some(serializer.canonical),
        tokens: serializer.tokens,
    }
}

impl Serializer<'_> {
    fn text(&self, range: Range<usize>) -> &str {
        self.source.get(range).unwrap_or("")
    }

    /// Emits the bytes between two siblings when they are significant.
    fn gap(&mut self, start: usize, end: usize, significant_whitespace: bool) {
        if start >= end {
            return;
        }
        let gap = self.text(start..end);
        let after_glue = self.after_glue && start == self.last_leaf_end;
        if significant_whitespace
            || after_glue
            || gap.chars().any(|character| !character.is_whitespace())
        {
            let gap = gap.as_bytes().to_vec();
            push_field(&mut self.canonical, b'g', &gap);
        }
    }

    /// Serializes `root` and its subtree iteratively, so deep trees cannot overflow the stack.
    fn node(&mut self, root: Node<'_>) {
        let mut cursor = root.walk();
        let mut entered = true;
        let mut frames: Vec<Frame> = Vec::new();
        loop {
            if entered {
                let node = cursor.node();
                if let Some(frame) = frames.last().copied() {
                    self.gap(
                        frame.previous_end,
                        node.start_byte(),
                        frame.significant_gaps,
                    );
                }
                let field = cursor.field_name().unwrap_or("").as_bytes().to_vec();
                push_field(&mut self.canonical, b'f', &field);
                let kind = node.kind();
                let tag = if node.is_named() { b'n' } else { b'a' };
                push_field(&mut self.canonical, tag, kind.as_bytes());
                if node.is_missing() {
                    self.canonical.push(b'm');
                }
                let leaf = node.child_count() == 0 || self.language.is_verbatim(kind);
                if leaf {
                    self.leaf(node);
                } else if cursor.goto_first_child() {
                    let parent_kind = node.parent().map_or("", |parent| parent.kind());
                    let jsx_name = frames.last().is_some_and(|frame| frame.jsx_name)
                        || (field == b"name" && parent_kind.starts_with("jsx_"));
                    let jsx_name = jsx_name && self.language.is_jsx();
                    frames.push(Frame {
                        previous_end: node.start_byte(),
                        significant_gaps: jsx_name || self.language.has_significant_gaps(node),
                        jsx_name,
                    });
                    self.canonical.push(b'(');
                    continue;
                }
            }
            // Leave the current node: record its end for the parent's next gap.
            let end = cursor.node().end_byte();
            if let Some(frame) = frames.last_mut() {
                frame.previous_end = end;
            }
            if cursor.goto_next_sibling() {
                entered = true;
                continue;
            }
            if !cursor.goto_parent() {
                break;
            }
            let parent_end = cursor.node().end_byte();
            if let Some(frame) = frames.pop() {
                self.gap(frame.previous_end, parent_end, frame.significant_gaps);
            }
            self.canonical.push(b')');
            if cursor.node() == root {
                break;
            }
            entered = false;
        }
    }

    fn leaf(&mut self, node: Node<'_>) {
        let range = node.byte_range();
        if self.language == Language::Python {
            self.python_indentation(&range);
        }
        let text = self.text(range.clone()).as_bytes().to_vec();
        self.after_glue = self.language.glues_to_next(&text);
        push_field(&mut self.canonical, b't', &text);
        if !range.is_empty() {
            self.tokens.push(range.clone());
            self.last_leaf_end = range.end;
        }
    }

    /// Records the indentation of a Python leaf that starts a logical line.
    ///
    /// A line starts a logical line unless it is inside brackets or continues the previous line
    /// with a backslash.
    fn python_indentation(&mut self, range: &Range<usize>) {
        let first_leaf = self.tokens.is_empty();
        let starts_line = first_leaf || self.text(self.last_leaf_end..range.start).contains('\n');
        if starts_line && self.bracket_depth == 0 && !range.is_empty() {
            let line_start = self
                .text(0..range.start)
                .rfind('\n')
                .map_or(0, |newline| newline + 1);
            let continued = line_start > 0
                && self
                    .text(0..line_start - 1)
                    .trim_end_matches('\r')
                    .ends_with('\\');
            if !continued {
                let indentation = self.text(line_start..range.start).as_bytes().to_vec();
                push_field(&mut self.canonical, b'i', &indentation);
            }
        }
        match self.text(range.clone()) {
            "(" | "[" | "{" => self.bracket_depth += 1,
            ")" | "]" | "}" => self.bracket_depth = self.bracket_depth.saturating_sub(1),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn equivalent(language: Language, before: &str, after: &str) -> bool {
        let before = parse(language, before).unwrap().canonical;
        let after = parse(language, after).unwrap().canonical;
        before.is_some() && before == after
    }

    #[test]
    fn rust_rewrap_is_equivalent() {
        assert!(equivalent(
            Language::Rust,
            "fn main() {\n    call(alpha, beta, gamma);\n}\n",
            "fn main() {\n    call(\n        alpha,\n        beta,\n        gamma\n    );\n}\n",
        ));
    }

    #[test]
    fn rust_literals_and_comments_are_verbatim() {
        assert!(!equivalent(
            Language::Rust,
            "const S: &str = \"a b\";",
            "const S: &str = \"a  b\";"
        ));
        assert!(!equivalent(
            Language::Rust,
            "const S: &str = r#\"a\nb\"#;",
            "const S: &str = r#\"a\n  b\"#;"
        ));
        assert!(!equivalent(
            Language::Rust,
            "// a b\nfn f() {}",
            "// a  b\nfn f() {}"
        ));
        assert!(!equivalent(
            Language::Rust,
            "const C: char = ' ';",
            "const C: char = '\t';"
        ));
        assert!(!equivalent(
            Language::Rust,
            "fn f() { let x = 1; }",
            "fn f() { let x = 2; }"
        ));
    }

    #[test]
    fn parse_errors_are_never_equivalent() {
        assert!(!equivalent(Language::Rust, "fn f( {", "fn f( {"));
    }

    #[test]
    fn javascript_return_line_break_is_not_equivalent() {
        assert!(!equivalent(
            Language::JavaScript,
            "function f() {\n  return isAdmin;\n}\n",
            "function f() {\n  return\n  isAdmin;\n}\n",
        ));
        assert!(equivalent(
            Language::JavaScript,
            "function f() {\n  return isAdmin;\n}\n",
            "function f() {\n    return isAdmin;\n}\n",
        ));
    }

    #[test]
    fn javascript_template_and_jsx_whitespace_is_significant() {
        assert!(!equivalent(
            Language::JavaScript,
            "const a = `x ${b} y`;",
            "const a = `x  ${b} y`;"
        ));
        // Template literals are compared verbatim, substitutions included.
        assert!(!equivalent(
            Language::JavaScript,
            "const a = `x ${b} y`;",
            "const a = `x ${ b } y`;"
        ));
        assert!(!equivalent(
            Language::Tsx,
            "const a = <p><b>x</b> <i>y</i></p>;",
            "const a = <p><b>x</b><i>y</i></p>;",
        ));
        assert!(equivalent(
            Language::Tsx,
            "const a = <p className=\"c\">x</p>;",
            "const a = <p   className=\"c\">x</p>;",
        ));
    }

    #[test]
    fn python_indentation_is_significant_but_bracket_wrapping_is_not() {
        let original = "if a:\n    x()\n    y()\n";
        assert!(!equivalent(
            Language::Python,
            original,
            "if a:\n    x()\ny()\n"
        ));
        assert!(!equivalent(
            Language::Python,
            original,
            "if a:\n  x()\n  y()\n"
        ));
        assert!(!equivalent(
            Language::Python,
            "if a:\n    x()\nfor i in b:\n    c(i)\n",
            "if a:\n    x()\n    for i in b:\n        c(i)\n"
        ));
        assert!(equivalent(
            Language::Python,
            "f(a, b)\n",
            "f(\n    a,\n    b)\n"
        ));
        assert!(!equivalent(
            Language::Python,
            "s = \"\"\"a\n b\"\"\"\n",
            "s = \"\"\"a\n  b\"\"\"\n"
        ));
    }

    #[test]
    fn go_json_and_toml_follow_the_same_rules() {
        assert!(!equivalent(
            Language::Go,
            "package p\nfunc f() int {\n\treturn 1\n}\n",
            "package p\nfunc f() int {\n\treturn\n\t1\n}\n"
        ));
        assert!(equivalent(
            Language::Go,
            "package p\nfunc f() { g(1, 2) }\n",
            "package p\nfunc f() {\n\tg(1,\n\t\t2)\n}\n"
        ));
        assert!(equivalent(
            Language::Json,
            "{\"a\": [1, 2]}",
            "{\n  \"a\": [\n    1,\n    2\n  ]\n}"
        ));
        assert!(!equivalent(Language::Json, "{\"a b\": 1}", "{\"a  b\": 1}"));
        // A trailing comma is an extra token, not formatting.
        assert!(!equivalent(
            Language::Toml,
            "a = [1, 2]\n",
            "a = [\n  1,\n  2,\n]\n"
        ));
        assert!(!equivalent(
            Language::Toml,
            "\"a b\" = 1\n",
            "\"a  b\" = 1\n"
        ));
        assert!(equivalent(
            Language::Toml,
            "a = [1, 2]\n",
            "a = [\n  1,\n  2\n]\n"
        ));
        assert!(!equivalent(
            Language::Toml,
            "a = \"x y\"\n",
            "a = \"x  y\"\n"
        ));
    }

    /// Lexemes a grammar splits into several tokens must stay contiguous: inserting whitespace
    /// inside one changes or breaks the program.
    #[test]
    fn split_lexemes_are_not_equivalent_with_inserted_whitespace() {
        use Language::{JavaScript, Rust, Tsx};
        let cases = [
            (Rust, "fn f<'a>(x: &'a u8) {}", "fn f<' a>(x: &' a u8) {}"),
            (
                Rust,
                "fn f() { 'outer: loop { break 'outer; } }",
                "fn f() { ' outer: loop { break ' outer; } }",
            ),
            (Rust, "fn f() { m!('a); }", "fn f() { m!(' a); }"),
            (Rust, "fn f() { m!($x); }", "fn f() { m!($ x); }"),
            (Rust, "fn f() { x.0.1; }", "fn f() { x.0. 1; }"),
            (
                Rust,
                "macro_rules! m { ($x:expr) => {}; }",
                "macro_rules! m { ($x: expr) => {}; }",
            ),
            (JavaScript, "x = <a.b/>;", "x = <a. b/>;"),
            (Tsx, "x = <a.b></a.b>;", "x = <a.b></a. b>;"),
        ];
        for (language, before, after) in cases {
            assert!(
                !equivalent(language, before, after),
                "{language:?}: {before:?} vs {after:?}"
            );
        }
        // Ordinary re-wrapping around those constructs stays formatting-only.
        assert!(equivalent(
            Rust,
            "fn f<'a>(x: &'a u8, y: u8) {}",
            "fn f<'a>(\n    x: &'a u8,\n    y: u8\n) {}"
        ));
        assert!(equivalent(
            Rust,
            "fn f() { a.b().c(); }",
            "fn f() {\n    a.b()\n        .c();\n}"
        ));
    }
}
