//! Tree-sitter parsing for the formatting-equivalence proof and token highlighting.
//!
//! Formatting-only is a claim that two files are equivalent, so it is only made when their
//! canonical forms are equal and neither has a parse error. The canonical form is lossless by
//! default (PLAN.md, "Format-independent classification"): it records the tree's node kinds,
//! fields and nesting, and every byte of the source as part of exactly one item:
//!
//! - a **token**, a leaf the language's lexical table classifies as an identifier, keyword or
//!   punctuator, with its exact text;
//! - an **opaque literal** or **comment**, with its exact bytes, hidden children included;
//! - the **prefix** before the first item, exactly;
//! - a **gap** between two items. A gap normalizes only when both neighbours are classified and
//!   every byte is in the language's whitespace alphabet; it then keeps whether it holds a line
//!   break, unless it is one of the language's wrap sites, and Rust keeps whether punctuation
//!   touches the next token. Any other gap is recorded byte for byte.
//!
//! Python also records the exact indentation of every logical line. Grammar acceptance is only
//! evidence: the rules for which bytes may change come from each language's specification.

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

    /// The lexical class of a leaf `node`, or of a node compared whole.
    fn class(self, node: Node<'_>) -> Class {
        let kind = node.kind();
        let table = self.table();
        if table.literals.contains(&kind) {
            return Class::Literal;
        }
        if table.comments.contains(&kind) {
            return Class::Comment;
        }
        if node.child_count() > 0 {
            return Class::Structure;
        }
        if node.is_named() {
            if table.named_tokens.contains(&kind) {
                Class::Token
            } else {
                Class::Unclassified
            }
        } else if table.unclassified_anonymous.contains(&kind) {
            Class::Unclassified
        } else if is_lexical_token(kind) || table.odd_tokens.contains(&kind) {
            Class::Token
        } else {
            Class::Unclassified
        }
    }

    /// The language's lexical table.
    fn table(self) -> &'static Table {
        match self {
            Self::Rust => &RUST,
            Self::Python => &PYTHON,
            Self::JavaScript | Self::TypeScript | Self::Tsx => &ECMASCRIPT,
            Self::Go => &GO,
            Self::Json => &JSON,
            Self::Toml => &TOML,
        }
    }

    /// Whether `character` is whitespace between tokens, a line break included.
    fn is_trivia(self, character: char) -> bool {
        self.is_break(character)
            || match self {
                // The Rust Reference, "Whitespace": Pattern_White_Space.
                Self::Rust => matches!(
                    character,
                    '\t' | '\u{b}'
                        | '\u{c}'
                        | '\r'
                        | ' '
                        | '\u{85}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{2028}'
                        | '\u{2029}'
                ),
                // The Python Language Reference, "Whitespace between tokens".
                Self::Python => matches!(character, ' ' | '\t' | '\u{c}'),
                // ECMA-262, "White Space": TAB, VT, FF, ZWNBSP and the Zs category.
                Self::JavaScript | Self::TypeScript | Self::Tsx => matches!(
                    character,
                    '\t' | '\u{b}' | '\u{c}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                        ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
                ),
                // The Go specification, "Tokens": spaces, tabs and carriage returns besides
                // newlines.
                Self::Go => matches!(character, ' ' | '\t' | '\r'),
                // RFC 8259, section 2.
                Self::Json => matches!(character, ' ' | '\t'),
                // TOML 1.0.0: tab and space.
                Self::Toml => matches!(character, ' ' | '\t'),
            }
    }

    /// Whether `character` is (part of) a line break.
    fn is_break(self, character: char) -> bool {
        match self {
            Self::Rust | Self::Go => character == '\n',
            Self::Python | Self::Json | Self::Toml => matches!(character, '\n' | '\r'),
            Self::JavaScript | Self::TypeScript | Self::Tsx => {
                matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
            }
        }
    }
}

/// What a leaf of the tree is to the proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// An identifier, keyword, punctuator or operator, compared by its text.
    Token,
    /// A literal, compared whole and exactly.
    Literal,
    /// A comment, compared whole and exactly; the gaps next to it keep their line breaks.
    Comment,
    /// A node with children, which the serializer descends into.
    Structure,
    /// Anything else, compared exactly with the gaps on both sides.
    Unclassified,
}

/// A language's lexical classification of the node kinds its grammar produces. Every kind a
/// leaf can have is listed somewhere or has the shape of a keyword or punctuator; the
/// completeness test holds each pinned grammar to that.
struct Table {
    /// Nodes compared whole and exactly: strings with their interpolations, numbers, regular
    /// expressions, characters, dates, quoted keys.
    literals: &'static [&'static str],
    comments: &'static [&'static str],
    /// Named leaves that are complete tokens.
    named_tokens: &'static [&'static str],
    /// Anonymous kinds that are tokens although they are not shaped like one.
    odd_tokens: &'static [&'static str],
    /// Anonymous kinds that are no token of the language, such as quotes outside a literal.
    unclassified_anonymous: &'static [&'static str],
}

/// Whether an anonymous kind is shaped like a keyword (letters, digits and `_`) or a punctuator
/// (ASCII punctuation other than quotes and backslashes).
fn is_lexical_token(kind: &str) -> bool {
    let keyword = kind
        .starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
        && kind
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_');
    let punctuator = !kind.is_empty()
        && kind.chars().all(|character| {
            character.is_ascii_punctuation() && !matches!(character, '"' | '`' | '\\')
        });
    keyword || punctuator
}

const RUST: Table = Table {
    literals: &[
        "string_literal",
        "raw_string_literal",
        "char_literal",
        "integer_literal",
        "float_literal",
        "shebang",
    ],
    comments: &["line_comment", "block_comment"],
    named_tokens: &[
        "identifier",
        "field_identifier",
        "type_identifier",
        "primitive_type",
        "shorthand_field_identifier",
        "crate",
        "self",
        "super",
        "metavariable",
        "fragment_specifier",
        "mutable_specifier",
        "boolean_literal",
    ],
    odd_tokens: &["macro_rules!"],
    unclassified_anonymous: &["\""],
};

const PYTHON: Table = Table {
    literals: &["string", "integer", "float"],
    comments: &["comment"],
    named_tokens: &[
        "identifier",
        "true",
        "false",
        "none",
        "ellipsis",
        "keyword_separator",
        "positional_separator",
        "wildcard_import",
        "import_prefix",
    ],
    // `is not` and `not in` are two tokens each, compared with their spacing.
    odd_tokens: &["is not", "not in"],
    unclassified_anonymous: &["\\"],
};

const ECMASCRIPT: Table = Table {
    literals: &[
        "string",
        "template_string",
        "template_literal_type",
        "regex",
        "number",
        "jsx_text",
        "html_character_reference",
        "hash_bang_line",
    ],
    comments: &["comment", "html_comment"],
    named_tokens: &[
        "identifier",
        "property_identifier",
        "shorthand_property_identifier",
        "shorthand_property_identifier_pattern",
        "private_property_identifier",
        "statement_identifier",
        "type_identifier",
        "this",
        "super",
        "true",
        "false",
        "null",
        "undefined",
        "import",
        "optional_chain",
        "this_type",
        "existential_type",
        "predefined_type",
        "accessibility_modifier",
        "override_modifier",
        "meta_property",
    ],
    odd_tokens: &["unique symbol"],
    unclassified_anonymous: &["\"", "'", "`"],
};

const GO: Table = Table {
    literals: &[
        "interpreted_string_literal",
        "raw_string_literal",
        "rune_literal",
        "int_literal",
        "float_literal",
        "imaginary_literal",
    ],
    comments: &["comment"],
    named_tokens: &[
        "identifier",
        "field_identifier",
        "type_identifier",
        "package_identifier",
        "label_name",
        "blank_identifier",
        "nil",
        "true",
        "false",
        "iota",
        "dot",
    ],
    odd_tokens: &[],
    unclassified_anonymous: &["\"", "`", "\u{0}"],
};

const JSON: Table = Table {
    literals: &["string", "number"],
    comments: &["comment"],
    named_tokens: &["true", "false", "null"],
    odd_tokens: &[],
    unclassified_anonymous: &["\""],
};

const TOML: Table = Table {
    literals: &[
        "string",
        "quoted_key",
        "bare_key",
        "integer",
        "float",
        "boolean",
        "offset_date_time",
        "local_date_time",
        "local_date",
        "local_time",
    ],
    comments: &["comment"],
    named_tokens: &[],
    odd_tokens: &[],
    unclassified_anonymous: &["\"", "\"\"\"", "'", "\'\'\'"],
};

/// A parsed file: its canonical form for the equivalence proof, and its leaf tokens.
pub(crate) struct ParsedFile {
    /// The canonical serialization, or `None` when the file has a parse error or holds
    /// something the language does not allow, such as a comment in JSON.
    pub canonical: Option<Vec<u8>>,
    /// Byte ranges of leaf tokens (literals and comments count as one token), in source order.
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
    /// The last leaf, with its class.
    previous: Option<(Node<'a>, Class)>,
    /// Python only: bracket nesting depth, inside which line breaks are insignificant.
    bracket_depth: usize,
    /// Whether the source holds something the proof refuses outright.
    refused: bool,
    /// The nodes open around the current one, outermost first: the context a gap is judged in,
    /// carried down the walk so no gap looks up through its ancestors.
    frames: Vec<Frame<'a>>,
    /// Where the gap before the next leaf lies: the shallowest frame whose children it separates
    /// since the last leaf, with the kind of the child before it.
    split: Option<(usize, &'a str)>,
}

/// One open node during the walk.
#[derive(Debug, Clone, Copy)]
struct Frame<'a> {
    kind: &'a str,
    /// Whether the node is, or is inside, a JSX element's name.
    jsx_name: bool,
}

fn serialize<'a>(language: Language, tree: &'a Tree, source: &'a str) -> ParsedFile {
    let root = tree.root_node();
    let mut serializer = Serializer {
        language,
        source,
        canonical: Vec::with_capacity(source.len() * 2),
        tokens: Vec::new(),
        previous: None,
        bracket_depth: 0,
        refused: false,
        frames: Vec::new(),
        split: None,
    };
    serializer.node(root);
    serializer.end();
    let refused = serializer.refused || root.has_error();
    ParsedFile {
        canonical: (!refused).then_some(serializer.canonical),
        tokens: serializer.tokens,
    }
}

impl<'a> Serializer<'a> {
    fn text(&self, range: Range<usize>) -> &'a str {
        self.source.get(range).unwrap_or("")
    }

    /// Serializes `root` and its subtree iteratively, so deep trees cannot overflow the stack.
    /// Each node and gap costs constant work, whatever the tree's depth.
    fn node(&mut self, root: Node<'a>) {
        let mut cursor = root.walk();
        let mut entered = true;
        loop {
            if entered {
                let node = cursor.node();
                let field = cursor.field_name();
                push_field(&mut self.canonical, b'f', field.unwrap_or("").as_bytes());
                let kind = node.kind();
                let tag = if node.is_named() { b'n' } else { b'a' };
                push_field(&mut self.canonical, tag, kind.as_bytes());
                if node.is_missing() {
                    self.canonical.push(b'm');
                }
                let class = self.language.class(node);
                if class != Class::Structure {
                    self.leaf(node, class);
                } else if cursor.goto_first_child() {
                    let parent = self.frames.last().copied();
                    let jsx_name = parent.is_some_and(|parent| {
                        parent.jsx_name
                            || (field == Some("name") && parent.kind.starts_with("jsx_"))
                    });
                    self.frames.push(Frame { kind, jsx_name });
                    self.canonical.push(b'(');
                    continue;
                }
            }
            let child = cursor.node().kind();
            if cursor.goto_next_sibling() {
                // The next leaf's gap separates two children of the innermost open node, or of
                // a shallower one when an earlier step since the last leaf was shallower.
                let depth = self.frames.len().saturating_sub(1);
                if self.split.is_none_or(|(split, _)| depth < split) {
                    self.split = Some((depth, child));
                }
                entered = true;
                continue;
            }
            if !cursor.goto_parent() {
                break;
            }
            self.frames.pop();
            self.canonical.push(b')');
            if cursor.node() == root {
                break;
            }
            entered = false;
        }
    }

    /// Records one leaf: the gap before it, Python's indentation, and its exact text.
    fn leaf(&mut self, node: Node<'a>, class: Class) {
        let range = node.byte_range();
        push_field(&mut self.canonical, b'c', &[class as u8]);
        if range.is_empty() {
            // A zero-width node, such as an automatic semicolon, has no bytes or gaps.
            return;
        }
        if self.language == Language::Json && class == Class::Comment {
            // A comment is not JSON.
            self.refused = true;
        }
        match self.previous {
            None => {
                let prefix = self.text(0..range.start).as_bytes().to_vec();
                push_field(&mut self.canonical, b'p', &prefix);
            }
            Some((previous, previous_class)) => self.gap(previous, previous_class, node, class),
        }
        if self.language == Language::Python {
            self.python_indentation(&range);
        }
        let text = self.text(range.clone()).as_bytes().to_vec();
        push_field(&mut self.canonical, b't', &text);
        self.tokens.push(range);
        self.previous = Some((node, class));
        self.split = None;
    }

    /// Records the gap between the leaves `previous` and `next`.
    fn gap(
        &mut self,
        previous: Node<'a>,
        previous_class: Class,
        next: Node<'a>,
        next_class: Class,
    ) {
        let gap = self.text(previous.end_byte()..next.start_byte());
        let before = self.text(previous.byte_range());
        let after = self.text(next.byte_range());
        let exact = previous_class == Class::Unclassified
            || next_class == Class::Unclassified
            || !gap
                .chars()
                .all(|character| self.language.is_trivia(character))
            || self.exact_gap()
            || (next_class == Class::Comment && self.is_directive(after));
        if exact {
            push_field(&mut self.canonical, b'x', gap.as_bytes());
            return;
        }
        let breaks = self.breaks(gap);
        if previous_class == Class::Comment || next_class == Class::Comment {
            // Next to a comment, a line break and a blank line are kept.
            push_field(&mut self.canonical, b'k', &[breaks.min(2)]);
            return;
        }
        let line = if self.wrap_site(previous, before, after) {
            2
        } else {
            u8::from(breaks > 0)
        };
        // Whether the two touch is kept unless the language shows it cannot matter.
        let touch = if self.separable(previous, previous_class, before, next_class, after) {
            2
        } else {
            u8::from(gap.is_empty())
        };
        push_field(&mut self.canonical, b'w', &[line, touch]);
    }

    /// Whether touching or separating the leaves `before` and `after` provably changes nothing,
    /// so the gap between them may become empty or not. Only two classes qualify; a gap next to
    /// any other punctuation keeps whether it is empty, because the real lexers merge or reject
    /// sequences the grammars split (Go `<-`, JavaScript `<!--`, Rust `0x1.0` and `foo#bar`).
    ///
    /// - **A bracket `( ) [ ] { }` on either side**, when the tokens are unchanged: a bracket is a
    ///   token of one character in every supported language, no longer token contains one
    ///   outside a literal (which is opaque), so whitespace next to it can neither join it to a
    ///   neighbour nor split a neighbour.
    ///   - Rust (Reference, "Tokens", "Delimiters"): brackets are delimiters, which a macro
    ///     receives as a `Group` without spacing. Punctuation before a bracket still keeps its
    ///     touching, since its own `Spacing` reaches the macro.
    ///   - Python (Language Reference, "Delimiters"): brackets are delimiters; inside an f-string
    ///     they belong to the opaque literal.
    ///   - JavaScript and TypeScript (ECMA-262, "Punctuators"): brackets are punctuators of one
    ///     character; a template's `${` and `}` belong to the opaque template literal, and
    ///     `<!--` and `-->` contain no bracket, so a gap next to a bracket cannot create or break
    ///     one.
    ///   - Go (specification, "Operators and punctuation"): brackets are single-character
    ///     tokens; whether a line ends after `)`, `]` or `}` is the line-break rule's concern.
    ///   - TOML 1.0.0: whitespace inside arrays and inline tables and around a table header's key
    ///     "is ignored"; the `[[` and `]]` of array tables are tokens of their own, not brackets
    ///     here, so their gaps keep touching.
    /// - **JSON, every gap** (RFC 8259, section 2): insignificant whitespace is allowed before or
    ///   after any structural character, every gap is next to one, and JSON has no comments or
    ///   operators that touching could form.
    fn separable(
        &self,
        previous: Node<'a>,
        previous_class: Class,
        before: &str,
        next_class: Class,
        after: &str,
    ) -> bool {
        if self.language == Language::Json {
            return true;
        }
        if self.language == Language::Rust && is_rust_punctuation(before) {
            // Punctuation's spacing to whatever follows reaches procedural macros.
            return false;
        }
        let bracket = |class: Class, text: &str| {
            class == Class::Token && matches!(text, "(" | ")" | "[" | "]" | "{" | "}")
        };
        bracket(previous_class, before)
            || bracket(next_class, after)
            || self.chain_link(previous, previous_class, before, after)
    }

    /// The one token pair besides brackets: a member access `.` (and `?.` in JavaScript and
    /// TypeScript) right after an identifier, which is where rustfmt and prettier break a
    /// chain. An identifier is a maximal token, so a following `.` cannot join it:
    ///
    /// - Rust (Reference, "Identifiers" and "Reserved prefixes"): an identifier ends at the
    ///   first character that cannot continue it, the only characters that make a touching
    ///   identifier lex differently are `#`, `"` and `'`, never `.`, and `proc_macro::Ident`
    ///   carries no spacing to a macro. `self`, `Self`, `super` and `crate` count as
    ///   identifiers here; a literal does not, since `0x1.0` lexes differently from `0x1 .0`.
    /// - JavaScript and TypeScript (ECMA-262, "Punctuators", "Names and Keywords"): after an
    ///   `IdentifierName`, `.` is always the punctuator `.`, and `?.` is the
    ///   `OptionalChainingPunctuator` whenever it is not followed by a digit, which the
    ///   unchanged tokens show; only a numeric literal followed by `.` lexes differently, and it
    ///   is excluded. `this` and `super` count as identifiers here.
    ///
    /// Python and Go do not get it: black wraps inside brackets, and gofmt keeps the dot at the
    /// end of the line.
    fn chain_link(
        &self,
        previous: Node<'a>,
        previous_class: Class,
        before: &str,
        after: &str,
    ) -> bool {
        if previous_class != Class::Token {
            return false;
        }
        match self.language {
            Language::Rust => {
                after == "."
                    && (matches!(
                        previous.kind(),
                        "identifier"
                            | "field_identifier"
                            | "type_identifier"
                            | "shorthand_field_identifier"
                    ) || matches!(before, "self" | "Self" | "super" | "crate"))
            }
            Language::JavaScript | Language::TypeScript | Language::Tsx => {
                matches!(after, "." | "?.")
                    && (matches!(
                        previous.kind(),
                        "identifier"
                            | "property_identifier"
                            | "shorthand_property_identifier"
                            | "type_identifier"
                    ) || matches!(before, "this" | "super"))
            }
            Language::Python | Language::Go | Language::Json | Language::Toml => false,
        }
    }

    /// Go comments that are directives, whose place is part of their meaning (`//line` and
    /// `/*line` must start a line; `//go:`, `//export`, `//extern` and `// +build` are found by
    /// their position): the gap before one is exact. A cgo preamble sits right before
    /// `import "C"`, and comment gaps already keep whether a blank line separates them.
    fn is_directive(&self, comment: &str) -> bool {
        const DIRECTIVES: [&str; 6] = [
            "//line ",
            "/*line ",
            "//go:",
            "//export ",
            "//extern ",
            "// +build",
        ];
        self.language == Language::Go && DIRECTIVES.iter().any(|prefix| comment.starts_with(prefix))
    }

    /// The end of the source: trailing whitespace is trivia, anything else is exact, and so is
    /// everything after an unclassified item, even nothing.
    fn end(&mut self) {
        let start = self.previous.map_or(0, |(node, _)| node.end_byte());
        let rest = self.text(start..self.source.len());
        let unclassified = self
            .previous
            .is_some_and(|(_, class)| class == Class::Unclassified);
        if self.previous.is_none()
            || unclassified
            || !rest
                .chars()
                .all(|character| self.language.is_trivia(character))
        {
            push_field(&mut self.canonical, b'e', rest.as_bytes());
        }
    }

    /// How many line breaks `gap` holds, a CRLF counting once, up to 255.
    fn breaks(&self, gap: &str) -> u8 {
        let mut count = 0_u8;
        let mut characters = gap.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '\r' && characters.peek() == Some(&'\n') {
                continue;
            }
            if self.language.is_break(character) {
                count = count.saturating_add(1);
            }
        }
        count
    }

    /// Gaps a language keeps byte for byte although they hold only whitespace: JSX text
    /// between elements, and inside a JSX element name.
    fn exact_gap(&self) -> bool {
        if !matches!(self.language, Language::JavaScript | Language::Tsx) {
            return false;
        }
        self.split_frame().is_some_and(|(frame, _)| {
            frame.jsx_name || matches!(frame.kind, "jsx_element" | "jsx_fragment")
        })
    }

    /// The node whose children the current gap separates, with the kind of the child before it.
    fn split_frame(&self) -> Option<(Frame<'a>, &'a str)> {
        let (depth, child) = self.split?;
        self.frames.get(depth).map(|&frame| (frame, child))
    }

    /// Whether a line break may come or go in the gap between `previous` (with text `before`)
    /// and the next leaf (with text `after`); each rule is justified in PLAN.md, "Per
    /// language".
    fn wrap_site(&self, previous: Node<'a>, before: &str, after: &str) -> bool {
        let parent = self
            .frames
            .last()
            .map(|frame| frame.kind)
            .unwrap_or_default();
        let split = self.split_frame();
        match self.language {
            // Free-form: whitespace only separates tokens.
            Language::Rust | Language::Json => true,
            // Implicit line joining inside brackets.
            Language::Python => self.bracket_depth > 0,
            // Semicolons are inserted only after these tokens when a line ends, and one before
            // a list's closing brace changes nothing.
            Language::Go => {
                !go_inserts_semicolon_after(previous, before) || go_closes_a_list(parent, after)
            }
            Language::JavaScript | Language::TypeScript | Language::Tsx => {
                ecmascript_wrap_site(previous, before, parent, after, split)
            }
            // Arrays span lines between their own elements, commas and brackets.
            Language::Toml => split.is_some_and(|(frame, _)| frame.kind == "array"),
        }
    }

    /// Records the indentation of a Python leaf that starts a logical line.
    ///
    /// A line starts a logical line unless it is inside brackets or follows an explicit line
    /// continuation, which is a leaf of its own whose gaps are exact.
    fn python_indentation(&mut self, range: &Range<usize>) {
        let first_leaf = self.previous.is_none();
        let gap_start = self.previous.map_or(0, |(node, _)| node.end_byte());
        let gap = self.text(gap_start..range.start);
        let starts_line = first_leaf || gap.contains(['\n', '\r']);
        if starts_line && self.bracket_depth == 0 {
            let line_start = self
                .text(0..range.start)
                .rfind(['\n', '\r'])
                .map_or(0, |newline| newline + 1);
            let indentation = self.text(line_start..range.start).as_bytes().to_vec();
            push_field(&mut self.canonical, b'i', &indentation);
        }
        match self.text(range.clone()) {
            "(" | "[" | "{" => self.bracket_depth += 1,
            ")" | "]" | "}" => self.bracket_depth = self.bracket_depth.saturating_sub(1),
            _ => {}
        }
    }
}

/// Whether a Rust token is punctuation that a procedural macro sees with its spacing: any
/// non-delimiter punctuator, the `'` of lifetimes and labels, and the `$` of metavariables.
fn is_rust_punctuation(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| "!#$%&*+,-./:;<=>?@^|~'".contains(character))
}

/// The Go specification, "Semicolons": a line's final token gets a semicolon after it when it
/// is an identifier, a basic literal, `break`, `continue`, `fallthrough`, `return`, `++`, `--`,
/// `)`, `]` or `}`. Identifiers are lexical: `nil`, `true`, `false` and `iota` are identifiers
/// whatever node kind the grammar gives them.
fn go_inserts_semicolon_after(previous: Node<'_>, text: &str) -> bool {
    const KEYWORDS: [&str; 25] = [
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
    ];
    if GO.literals.contains(&previous.kind()) {
        return true;
    }
    let identifier = text
        .starts_with(|character: char| character.is_alphabetic() || character == '_')
        && text
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_');
    if identifier {
        return !KEYWORDS.contains(&text)
            || matches!(text, "break" | "continue" | "fallthrough" | "return");
    }
    matches!(text, "++" | "--" | ")" | "]" | "}")
}

/// The Go specification, "Semicolons": "a semicolon may be omitted before a closing `}`" of a
/// statement or declaration list, so one inserted before it changes nothing. A composite
/// literal's `}` closes elements separated by commas instead, and is not one of these.
fn go_closes_a_list(parent: &str, text: &str) -> bool {
    const LISTS: [&str; 6] = [
        "block",
        "field_declaration_list",
        "interface_type",
        "expression_switch_statement",
        "type_switch_statement",
        "select_statement",
    ];
    text == "}" && LISTS.contains(&parent)
}

/// The JavaScript and TypeScript wrap sites of PLAN.md, "Per language".
fn ecmascript_wrap_site(
    previous: Node<'_>,
    before: &str,
    parent: &str,
    after: &str,
    split: Option<(Frame<'_>, &str)>,
) -> bool {
    const OPENERS: [&str; 5] = ["(", "[", "{", ",", ";"];
    const CLOSERS: [&str; 3] = [")", "]", "}"];
    const OPERATORS: [&str; 39] = [
        "=", "+=", "-=", "*=", "/=", "%=", "**=", "<<=", ">>=", ">>>=", "&=", "|=", "^=", "&&=",
        "||=", "??=", "+", "-", "*", "/", "%", "**", "==", "!=", "===", "!==", "<=", ">=", "<<",
        ">>", ">>>", "&", "|", "^", "&&", "||", "??", "=>", "...",
    ];
    const OPERANDS: [&str; 7] = [
        "identifier",
        "property_identifier",
        "private_property_identifier",
        "shorthand_property_identifier",
        "type_identifier",
        "this",
        "super",
    ];
    // The spread operator `...` is followed by its operand; a break after it is harmless.
    if OPENERS.contains(&before) || CLOSERS.contains(&after) || OPERATORS.contains(&before) {
        return true;
    }
    if matches!(after, "." | "?.")
        && (OPERANDS.contains(&previous.kind()) || matches!(before, ")" | "]"))
    {
        return true;
    }
    if matches!(after, "?" | ":") && matches!(parent, "ternary_expression" | "conditional_type") {
        return true;
    }
    if matches!(after, "|" | "&") && matches!(parent, "union_type" | "intersection_type") {
        return true;
    }
    // Between the name, attributes and closing `>` or `/>` of a JSX tag, never after `<`.
    split.is_some_and(|(frame, child)| {
        matches!(
            frame.kind,
            "jsx_opening_element" | "jsx_self_closing_element"
        ) && !matches!(child, "<" | "</")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn equivalent(language: Language, before: &str, after: &str) -> bool {
        let before = parse(language, before).unwrap().canonical;
        let after = parse(language, after).unwrap().canonical;
        before.is_some() && before == after
    }

    /// A comment ending in a backslash does not continue the line, so the next line's
    /// indentation still counts; a real continuation's indentation does not.
    #[test]
    fn python_continuation_is_lexical() {
        assert!(!equivalent(
            Language::Python,
            "if ready:\n    a()\n    # \\\n    b()\n",
            "if ready:\n    a()\n    # \\\n      b()\n",
        ));
        // An explicit continuation is exact, with the gaps around it.
        assert!(!equivalent(
            Language::Python,
            "x = 1 + \\\n    2\n",
            "x = 1 + \\\n        2\n",
        ));
        // A bare carriage return ends a line too.
        assert!(!equivalent(
            Language::Python,
            "if a:\r    b()\r    c()\r",
            "if a:\r    b()\r      c()\r",
        ));
    }

    /// Every restricted production of ECMAScript, and TypeScript's own, records whether a line
    /// terminator separates its tokens; any line terminator counts.
    #[test]
    fn ecmascript_restricted_line_breaks_are_significant() {
        let cases = [
            (
                "throw new Error(\"denied\");",
                "throw\nnew Error(\"denied\");",
            ),
            (
                "function f(x) { return x; }",
                "function f(x) { return\nx; }",
            ),
            ("a: for (;;) { break a; }", "a: for (;;) { break\na; }"),
            (
                "a: for (;;) { continue a; }",
                "a: for (;;) { continue\na; }",
            ),
            ("function* g() { yield x; }", "function* g() { yield\nx; }"),
            ("const f = async (x) => x;", "const f = async\n(x) => x;"),
            ("async function f() {}", "async\nfunction f() {}"),
            ("const f = (x) => x;", "const f = (x)\n=> x;"),
            ("x++;", "x\n++;"),
            ("throw new Error();", "throw\u{2028}new Error();"),
            ("throw new Error();", "throw\u{2029}new Error();"),
            ("throw new Error();", "throw\rnew Error();"),
            ("throw new Error();", "throw /*\n*/ new Error();"),
        ];
        for (before, after) in cases {
            for language in [Language::JavaScript, Language::TypeScript] {
                assert!(
                    !equivalent(language, before, after),
                    "{language:?}: {before:?} against {after:?}"
                );
            }
        }
        let typescript = [
            ("type A = B;", "type\nA = B;"),
            ("declare const x: number;", "declare\nconst x: number;"),
            ("const y = x as T;", "const y = x\nas T;"),
            (
                "function f(x): x is T { return true; }",
                "function f(x): x\nis T { return true; }",
            ),
            ("let a = b!;", "let a = b\n!;"),
        ];
        for (before, after) in typescript {
            assert!(
                !equivalent(Language::TypeScript, before, after),
                "{before:?} against {after:?}"
            );
        }
        // Line breaks elsewhere stay formatting.
        assert!(equivalent(
            Language::JavaScript,
            "f(a, b);\nconst x = [1, 2];\n",
            "f(\n  a,\n  b,\n);\nconst x =\n  [1, 2];\n"
                .replace(",\n);", "\n);")
                .as_str(),
        ));
        assert!(equivalent(
            Language::TypeScript,
            "function f(x: number) { return g(x); }\n",
            "function f(x: number) {\n  return g(x);\n}\n",
        ));
    }

    /// Inside a macro, adjacent punctuation's spacing reaches the macro; elsewhere it does not.
    #[test]
    fn rust_macro_punctuation_spacing_is_significant() {
        assert!(!equivalent(
            Language::Rust,
            "const FLAG: bool = first_spacing!(++);\n",
            "const FLAG: bool = first_spacing!(+ +);\n",
        ));
        assert!(!equivalent(
            Language::Rust,
            "#[attr(a = -1)]\nfn f() {}\n",
            "#[attr(a =- 1)]\nfn f() {}\n",
        ));
        assert!(!equivalent(
            Language::Rust,
            "macro_rules! m { ($a:expr) => { $a + -1 }; }\n",
            "macro_rules! m { ($a:expr) => { $a +-1 }; }\n",
        ));
        assert!(equivalent(
            Language::Rust,
            "fn f() { vec![1, 2]; }\n",
            "fn f() {\n    vec![\n        1,\n        2\n    ];\n}\n",
        ));
        // Outside macros too: an attribute macro receives the whole item.
        assert!(!equivalent(
            Language::Rust,
            "#[policy]\nfn f() -> i32 { a + -b }\n",
            "#[policy]\nfn f() -> i32 { a +-b }\n",
        ));
    }

    /// Every terminal kind of every pinned grammar is classified: a token, a literal, a
    /// comment, a node with children, or deliberately unclassified. A grammar upgrade that adds
    /// a kind fails here instead of having its bytes normalized.
    #[test]
    fn every_terminal_kind_is_classified() {
        let grammars = [
            (Language::Rust, tree_sitter_rust::NODE_TYPES),
            (Language::Python, tree_sitter_python::NODE_TYPES),
            (Language::Tsx, tree_sitter_typescript::TSX_NODE_TYPES),
            (
                Language::TypeScript,
                tree_sitter_typescript::TYPESCRIPT_NODE_TYPES,
            ),
            (Language::Go, tree_sitter_go::NODE_TYPES),
            (Language::Json, tree_sitter_json::NODE_TYPES),
            (Language::Toml, tree_sitter_toml_ng::NODE_TYPES),
        ];
        for (language, node_types) in grammars {
            let table = language.table();
            let structural = structural(language);
            let kinds: Vec<serde_json::Value> = serde_json::from_str(node_types).unwrap();
            for kind in kinds {
                let name = kind["type"].as_str().unwrap();
                let named = kind["named"].as_bool().unwrap();
                let terminal = kind.get("fields").is_none()
                    && kind.get("children").is_none()
                    && kind.get("subtypes").is_none();
                let classified = if named {
                    !terminal
                        || table.literals.contains(&name)
                        || table.comments.contains(&name)
                        || table.named_tokens.contains(&name)
                        || structural.contains(&name)
                } else {
                    is_lexical_token(name)
                        || table.odd_tokens.contains(&name)
                        || table.unclassified_anonymous.contains(&name)
                };
                assert!(
                    classified,
                    "{language:?}: {name:?} (named: {named}) is unclassified"
                );
            }
        }
    }

    /// Named kinds that only occur with children, or only inside literals and comments, so the
    /// serializer never meets them as a leaf of its own; a leaf of one would be unclassified.
    fn structural(language: Language) -> &'static [&'static str] {
        match language {
            Language::Rust => &[
                "empty_statement",
                "never_type",
                "remaining_field_pattern",
                "unit_expression",
                "unit_type",
                "doc_comment",
                "escape_sequence",
                "inner_doc_comment_marker",
                "outer_doc_comment_marker",
                "string_content",
            ],
            Language::Python => &[
                "break_statement",
                "continue_statement",
                "pass_statement",
                "escape_interpolation",
                "escape_sequence",
                "string_start",
                "string_end",
                "type_conversion",
                "line_continuation",
            ],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &[
                "debugger_statement",
                "empty_statement",
                "escape_sequence",
                "string_fragment",
                "regex_pattern",
                "regex_flags",
            ],
            Language::Go => &[
                "empty_statement",
                "fallthrough_statement",
                "escape_sequence",
                "interpreted_string_literal_content",
                "raw_string_literal_content",
            ],
            Language::Json => &["escape_sequence", "string_content"],
            Language::Toml => &["escape_sequence"],
        }
    }

    /// Every counterexample from the reviews of the proof: each pair has the same tree in the
    /// pinned grammar, and the language accepts one and rejects the other or reads them
    /// differently.
    #[test]
    fn review_counterexamples_are_not_equivalent() {
        use Language::{Go, JavaScript, Json, Python, Rust, Toml, Tsx, TypeScript};
        let cases = [
            // Python: a vertical tab is no whitespace; a comment's backslash continues nothing.
            (Python, "x = (1 + 2)\n", "x = (1\u{b}+ 2)\n"),
            (
                Python,
                "if ready:\n    a()\n    # \\\n    b()\n",
                "if ready:\n    a()\n    # \\\n      b()\n",
            ),
            (
                Python,
                "if a:\n    b()\n    c()\n",
                "if a:\n    b()\n      c()\n",
            ),
            // JavaScript: restricted productions, the hashbang's position, `-->` comments.
            (
                JavaScript,
                "throw new Error(\"denied\");\n",
                "throw\nnew Error(\"denied\");\n",
            ),
            (
                JavaScript,
                "#!/usr/bin/env node\nf();\n",
                " #!/usr/bin/env node\nf();\n",
            ),
            (
                JavaScript,
                "const x = 1;\n--> comment\n",
                "const x = 1; --> comment\n",
            ),
            // TypeScript: `interface` before a break; angle-list commas are tokens.
            (TypeScript, "interface I {}\n", "interface\nI {}\n"),
            (TypeScript, "f<string>();\n", "f<string,>();\n"),
            (
                Tsx,
                "const id = <T,>(x: T) => x;\n",
                "const id = <T>(x: T) => x;\n",
            ),
            // Go: a break after an operand inserts a semicolon.
            (
                Go,
                "package p\nvar a = g(1 + 2)\n",
                "package p\nvar a = g(1\n+ 2)\n",
            ),
            (
                Go,
                "package p\nfunc f(x int) {}\n",
                "package p\nfunc f(x\nint) {}\n",
            ),
            (
                Go,
                "package p\nvar a = []int{1}\n",
                "package p\nvar a = []int{1\n}\n",
            ),
            // JSON: a vertical tab is no whitespace; a comment is no JSON.
            (Json, "{\"a\": 1}", "{\"a\":\u{b}1}"),
            (Json, "{\"a\": 1}", "{\"a\": 1 // one\n}"),
            // TOML: quoted keys are exact.
            (Toml, "\" \" = 1\n", "\"  \" = 1\n"),
            (Toml, "' ' = 1\n", "'  ' = 1\n"),
            // Rust: punctuation touching the next token reaches macros, and commas are tokens.
            (
                Rust,
                "const F: bool = first_spacing!(+'a);\n",
                "const F: bool = first_spacing!(+ 'a);\n",
            ),
            (
                Rust,
                "#[policy]\nfn f() -> i32 { a + -b }\n",
                "#[policy]\nfn f() -> i32 { a +-b }\n",
            ),
            (
                Rust,
                "#[probe]\nfn f(x: u8) {}\n",
                "#[probe]\nfn f(x: u8,) {}\n",
            ),
        ];
        assert_never_equivalent(&cases);
    }

    /// The counterexamples of the fifth review: touching tokens that are one lexeme or that a
    /// language forbids, the end of a file after an unclassified item, and comments whose place
    /// is their meaning.
    #[test]
    fn touching_and_placement_counterexamples_are_not_equivalent() {
        use Language::{Go, JavaScript, Python, Rust, TypeScript};
        let cases = [
            // Round 5: touching tokens are one lexeme, or the language forbids them touching.
            (
                Rust,
                "const A: u8 = probe!(1custom);\n",
                "const A: u8 = probe!(1 custom);\n",
            ),
            (
                Rust,
                "const A: u8 = probe!(\"a\"x);\n",
                "const A: u8 = probe!(\"a\" x);\n",
            ),
            (
                Rust,
                "const A: u8 = probe!(r\"a\"x);\n",
                "const A: u8 = probe!(r\"a\" x);\n",
            ),
            (
                Rust,
                "const A: u8 = probe!('a'x);\n",
                "const A: u8 = probe!('a' x);\n",
            ),
            (
                TypeScript,
                "const a = 1 as number;\n",
                "const a = 1as number;\n",
            ),
            (JavaScript, "const a = 1 in obj;\n", "const a = 1in obj;\n"),
            // Python's encoding declaration counts only on line 1 or 2.
            (
                Python,
                "# -*- coding: latin-1 -*-\nx = 1\n",
                "\n\n# -*- coding: latin-1 -*-\nx = 1\n",
            ),
            (
                Python,
                "#!/usr/bin/env python\n# coding: latin-1\nx = 1\n",
                "#!/usr/bin/env python\n\n# coding: latin-1\nx = 1\n",
            ),
            // Round 6: the real lexers merge or reject what the grammars split.
            (
                Go,
                "package p\n\nvar x = a< -b\n",
                "package p\n\nvar x = a<-b\n",
            ),
            (
                Go,
                "package p\n\nvar x = a+ +b\n",
                "package p\n\nvar x = a++b\n",
            ),
            (
                Go,
                "package p\n\nvar x = a- -b\n",
                "package p\n\nvar x = a--b\n",
            ),
            (
                Go,
                "package p\n\nvar x = a/ *b\n",
                "package p\n\nvar x = a/*b\n",
            ),
            (JavaScript, "f(a< !--b);\n", "f(a<!--b);\n"),
            (
                Rust,
                "const A: u8 = m!(0x1 .0);\n",
                "const A: u8 = m!(0x1.0);\n",
            ),
            (
                Rust,
                "const A: u8 = m!(foo #bar);\n",
                "const A: u8 = m!(foo#bar);\n",
            ),
            // An explicit continuation at the end of the file.
            (Python, "x=1\\\n", "x=1\\\n\n"),
            // A Go `//line` directive must start its line.
            (
                Go,
                "package p\n\n//line x:notanumber\nvar a = 1\n",
                "package p\n\n //line x:notanumber\nvar a = 1\n",
            ),
            (
                Go,
                "package p\n\n//go:noinline\nfunc f() {}\n",
                "package p\n\n  //go:noinline\nfunc f() {}\n",
            ),
        ];
        assert_never_equivalent(&cases);
    }

    /// Asserts that each pair is not equivalent, either way round.
    fn assert_never_equivalent(cases: &[(Language, &str, &str)]) {
        for &(language, before, after) in cases {
            assert!(
                !equivalent(language, before, after),
                "{language:?}: {before:?} against {after:?}"
            );
            assert!(
                !equivalent(language, after, before),
                "{language:?}: {after:?} against {before:?}"
            );
        }
    }

    /// Spacing next to punctuation other than brackets stays unproven: the real lexers merge or
    /// reject sequences the grammars split, so these reflows, though harmless here, are never
    /// claimed. A `.` after a literal or other punctuation is one of them.
    #[test]
    fn spacing_next_to_punctuation_stays_unproven() {
        use Language::{Go, Python, Rust, TypeScript};
        let cases = [
            (Python, "f(a,b)\n", "f(a, b)\n"),
            (Python, "x=y+1\n", "x = y + 1\n"),
            (TypeScript, "f(a,b);\n", "f(a, b);\n"),
            (TypeScript, "const x=y+1;\n", "const x = y + 1;\n"),
            (
                Go,
                "package p\n\nvar x = f(a,b)\n",
                "package p\n\nvar x = f(a, b)\n",
            ),
            (Rust, "fn f() { g(a,b); }\n", "fn f() { g(a, b); }\n"),
            // Not after a literal or other punctuation.
            (
                Rust,
                "const A: u8 = m!(0x1 .0);\n",
                "const A: u8 = m!(0x1.0);\n",
            ),
            (
                Rust,
                "const A: u8 = m!(foo #bar);\n",
                "const A: u8 = m!(foo#bar);\n",
            ),
            (
                TypeScript,
                "const s = 1 .toString();\n",
                "const s = 1.toString();\n",
            ),
        ];
        assert_never_equivalent(&cases);
    }

    /// A chain may break right after an identifier, the one token pair besides brackets whose
    /// touching is not kept, in Rust, JavaScript and TypeScript.
    #[test]
    fn chains_break_after_an_identifier() {
        use Language::{Rust, TypeScript};
        let cases = [
            (
                TypeScript,
                "const total = items.filter(f);\n",
                "const total = items\n  .filter(f);\n",
            ),
            (TypeScript, "const y = x?.y;\n", "const y = x ?.y;\n"),
            (TypeScript, "const y = this.y;\n", "const y = this\n  .y;\n"),
            (
                Rust,
                "fn f() -> u32 { fraction.chars().count() }\n",
                "fn f() -> u32 {\n    fraction\n        .chars()\n        .count()\n}\n",
            ),
            (
                Rust,
                "fn f(&self) -> u32 { self.a.len() }\n",
                "fn f(&self) -> u32 {\n    self\n        .a\n        .len()\n}\n",
            ),
        ];
        for (language, before, after) in cases {
            assert!(
                equivalent(language, before, after),
                "{language:?}: {before:?} against {after:?}"
            );
        }
    }

    /// The work per gap does not grow with the tree's depth: a long left-deep sum serializes in
    /// time linear in its length, where looking up through ancestors for every gap would take
    /// over a billion steps.
    #[test]
    fn deep_trees_serialize_in_linear_time() {
        let terms = 25_000;
        for (language, prefix, suffix) in [
            (Language::Rust, "const A: u64 = ", ";\n"),
            (Language::Tsx, "const a = ", ";\n"),
            (Language::Go, "package p\n\nvar a = ", "\n"),
        ] {
            let source = format!("{prefix}{}{suffix}", vec!["1"; terms].join(" + "));
            let started = std::time::Instant::now();
            let parsed = parse(language, &source).unwrap();
            assert!(parsed.canonical.is_some(), "{language:?}");
            assert!(parsed.tokens.len() > terms);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "{language:?} took {:?}",
                started.elapsed()
            );
        }
    }

    /// The reflows formatters make of formatted code stay proven when no token changes.
    #[test]
    fn common_reflows_stay_equivalent() {
        use Language::{Go, Json, Python, Rust, TypeScript};
        let cases = [
            (
                Rust,
                "fn f(&mut self) {\n    for entry in &entries[..take] { self.apply(entry); }\n}\n",
                "fn f(&mut self) {\n    for entry in &entries[..take] {\n        self.apply(entry);\n    }\n}\n",
            ),
            (
                Rust,
                "fn f() -> u32 { a.iter().map(g).sum() }\n",
                "fn f() -> u32 {\n    a.iter()\n        .map(g)\n        .sum()\n}\n",
            ),
            (
                Rust,
                "fn f() -> bool { alpha && beta || gamma }\n",
                "fn f() -> bool {\n    alpha\n        && beta\n        || gamma\n}\n",
            ),
            (
                TypeScript,
                "const total = items.filter((x) => x.ok).map((x) => x.value);\n",
                "const total = items.filter((x) => x.ok)\n  .map((x) => x.value);\n",
            ),
            (
                TypeScript,
                "const ok = alpha && beta;\nconst v = cond ? a : b;\n",
                "const ok =\n  alpha &&\n  beta;\nconst v = cond\n  ? a\n  : b;\n",
            ),
            (
                Go,
                "package p\n\ntype S struct {\n\ta int // a\n\tlonger string // b\n}\n\nfunc f() { g(1, 2) }\n",
                "package p\n\ntype S struct {\n\ta      int    // a\n\tlonger string // b\n}\n\nfunc f() {\n\tg(1,\n\t\t2)\n}\n",
            ),
            (
                Python,
                "result = compute(alpha, beta, gamma)\n",
                "result = compute(\n    alpha, beta,\n    gamma\n)\n",
            ),
            (
                Python,
                "x = 1\ny = [a, b]\n\n\n\nz = 2\n",
                "x = 1\ny = [a, b]\n\nz = 2\n",
            ),
            (Python, "f( a, b )\n", "f(a, b)\n"),
            (
                Json,
                "{\"a\": [1, 2], \"b\": {\"c\": null}}",
                "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {\n    \"c\": null\n  }\n}\n",
            ),
        ];
        for (language, before, after) in cases {
            assert!(
                equivalent(language, before, after),
                "{language:?}: {before:?} against {after:?}"
            );
        }
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
