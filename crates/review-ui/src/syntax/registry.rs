//! The registry of highlighted languages: review-languages' table, the one manifest
//! `languages.toml` turned into rows, mapped onto engines.
//!
//! A row in the tree-house tier is highlighted by tree-house with its grammar and Helix's
//! queries; a row in the bat tier by syntect with the bat syntax it names.
//! Moving a language between tiers changes only its manifest row's `tier`: its names and
//! detection stay, so files and injections find it the same way in either tier.
//! Files no row covers fall to syntect's own detection.
//!
//! The grammars are review-core's crates for the formatting proof's languages, so the binary
//! links one grammar per language, and review-languages' for the rest, fetched at the revisions
//! Helix pins.

use review_core::syntax::Language as Proof;
pub(super) use review_languages::{Detection, QueryKind};
use review_languages::{GrammarSpec, LANGUAGES, LanguageSpec, Tier};
use tree_sitter_language::LanguageFn;

/// One language and how to find and highlight it.
#[derive(Clone, Copy)]
pub(super) struct Row {
    /// Helix's name for the language, which injection queries use, such as `markdown.inline`.
    pub(super) name: &'static str,
    /// Other names documents call it by, such as a Markdown fence's `c++`, matched whole and
    /// ignoring case.
    pub(super) aliases: &'static [&'static str],
    /// Helix's injection regex, matched against a whole name and ignoring case.
    pub(super) injection_regex: Option<&'static str>,
    /// For a template language, the kinds of its grammar's nodes that hold the template's
    /// literal text; empty for every other language.
    pub(super) data: &'static [&'static str],
    /// Which files are this language.
    pub(super) detection: Detection,
    /// The language whose named scopes review-core finds, for breadcrumbs.
    pub(super) scopes: Option<Proof>,
    /// Which engine highlights it.
    pub(super) engine: Engine,
}

impl Row {
    /// Whether the row is a template language in the tree-house tier, whose literal text takes
    /// the language of the file the template produces.
    pub(super) fn is_template(&self) -> bool {
        !self.data.is_empty() && matches!(self.engine, Engine::TreeHouse { .. })
    }
}

/// The engine that highlights a language.
#[derive(Clone, Copy)]
pub(super) enum Engine {
    /// tree-house, with the grammar and Helix's queries in the directory `queries`.
    TreeHouse {
        /// The grammar, from its crate.
        grammar: LanguageFn,
        /// The directory of its queries, which `; inherits:` lines name too.
        queries: &'static str,
    },
    /// syntect, with the bat syntax of this name.
    Bat {
        /// The syntax's name in bat's syntax set, such as `SQL`.
        syntax: &'static str,
    },
}

/// The rows of review-languages' table, in its order.
///
/// review-languages' build script checks that every tree-house-tier row has a grammar and
/// queries and every bat-tier row a syntax, so no row is left out; a test holds that.
pub(super) fn rows() -> Vec<Row> {
    LANGUAGES.iter().filter_map(row).collect()
}

/// The row of `spec`, or `None` when it lacks what its tier needs.
fn row(spec: &LanguageSpec) -> Option<Row> {
    let scopes = match spec.grammar {
        Some(GrammarSpec::Proof(name)) => Some(proof(name)?),
        Some(GrammarSpec::Compiled(_)) | None => None,
    };
    let engine = match spec.tier {
        Tier::TreeHouse => Engine::TreeHouse {
            grammar: match spec.grammar? {
                GrammarSpec::Proof(_) => proof_grammar(scopes?),
                GrammarSpec::Compiled(grammar) => grammar,
            },
            queries: spec.queries?,
        },
        Tier::Bat => Engine::Bat { syntax: spec.bat? },
    };
    Some(Row {
        name: spec.name,
        aliases: spec.aliases,
        injection_regex: spec.injection_regex,
        data: spec.data,
        detection: spec.detection,
        scopes,
        engine,
    })
}

/// The language of review-core's formatting proof that the manifest calls `name`.
fn proof(name: &str) -> Option<Proof> {
    Some(match name {
        "rust" => Proof::Rust,
        "python" => Proof::Python,
        "javascript" => Proof::JavaScript,
        "typescript" => Proof::TypeScript,
        "tsx" => Proof::Tsx,
        "go" => Proof::Go,
        "json" => Proof::Json,
        "toml" => Proof::Toml,
        _ => return None,
    })
}

/// The grammar review-core parses `language` with, from the same crate, so the binary links it
/// once.
fn proof_grammar(language: Proof) -> LanguageFn {
    match language {
        Proof::Rust => tree_sitter_rust::LANGUAGE,
        Proof::Python => tree_sitter_python::LANGUAGE,
        Proof::JavaScript | Proof::Tsx => tree_sitter_typescript::LANGUAGE_TSX,
        Proof::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        Proof::Go => tree_sitter_go::LANGUAGE,
        Proof::Json => tree_sitter_json::LANGUAGE,
        Proof::Toml => tree_sitter_toml_ng::LANGUAGE,
    }
}

/// The text of the query of `kind` in `directory` of review-languages' Helix queries, as Helix
/// ships it: its `; inherits:` lines are resolved by `tree_house::read_query`, which asks for
/// the inherited directories' files here too.
/// Empty when Helix has no such file.
pub(super) fn query_text(directory: &str, kind: QueryKind) -> &'static str {
    review_languages::query(directory, kind).unwrap_or_default()
}
