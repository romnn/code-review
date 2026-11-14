//! Syntax highlighting of every file side, and the named scopes of each, computed once per blob
//! and syntax off the UI thread (ADR 3).
//!
//! Highlighting has two tiers, chosen per language in one manifest, review-languages'
//! `languages.toml`, which [`registry`] maps onto engines:
//!
//! - **The tree-house tier**: a curated set of languages, parsed by tree-house with their
//!   tree-sitter grammars and highlighted with Helix's queries, injections and locals included.
//! - **The bat tier**: every other file, highlighted by syntect with bat's syntaxes.
//!   An injection the tree-house tier has no row for, such as a Kotlin fence in Markdown, is
//!   handed to bat over exactly its ranges.
//!
//! A text first resolves to its syntax ([`resolve`]): by its path, the registry's detection and
//! then bat's, and only then by its first line, the registry's interpreters and then bat's.
//! The highlights of a blob are those of its object ID and resolved syntax: the same blob under
//! two names may highlight differently.
//!
//! A template, such as a Helm chart's `templates/deployment.yaml` or `nginx.conf.j2`, is
//! highlighted by its template language, and its literal text, its [`Data`], as the language of
//! the file it produces, in either tier.
//!
//! The queries are Helix's (MPL-2.0), which review-languages fetches and embeds unmodified at
//! the commit whose pins it compiles, apart from Jinja's, which are cr's own.
//! Highlighting one text is bounded by [`BUDGET`]; what is not highlighted in time stays plain.

use std::ops::Range;
use std::sync::Arc;

use review_core::scope::ScopeMap;
use review_core::syntax::Language as Proof;

mod bat;
mod budget;
mod chezmoi;
mod languages;
mod registry;
mod tones;
mod tree;

#[cfg(test)]
mod tests;

pub use budget::BUDGET;
use budget::Deadline;
use languages::Languages;
use registry::{Engine, Row};
pub use tones::Tone;

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
    pub head: Option<Arc<SideSyntax>>,
    /// The base version.
    pub base: Option<Arc<SideSyntax>>,
}

/// What highlights a text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Syntax {
    /// tree-house, as the tree-house-tier language of this name, such as `rust`.
    TreeHouse(&'static str),
    /// tree-house, as the template language of this name, such as `helm`, whose literal text
    /// is `data`.
    ///
    /// A template whose data has no known language is [`Syntax::TreeHouse`], which gives its
    /// text the language its template language's own injections give it, such as Helm's YAML.
    Template {
        /// The template language.
        name: &'static str,
        /// The language of its literal text.
        data: Data,
    },
    /// syntect, as the bat syntax of this name, such as `Kotlin`.
    Bat(String),
    /// Nothing: the text stays plain.
    Plain,
}

/// The language of a template's literal text: that of the file the template produces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Data {
    /// The tree-house-tier language of this name, such as `yaml`.
    TreeHouse(&'static str),
    /// The bat syntax of this name, such as `nginx`.
    Bat(String),
    /// Plain text, such as Helm's `NOTES.txt`: only the template's own syntax is highlighted.
    Plain,
}

/// The syntax a file's text resolves to, which with its blob identifies its highlights.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Resolved {
    /// What highlights it.
    pub syntax: Syntax,
    /// The language whose named scopes review-core finds in it, for breadcrumbs.
    pub scopes: Option<Proof>,
}

impl Resolved {
    /// A text without highlights or scopes.
    pub const PLAIN: Self = Self {
        syntax: Syntax::Plain,
        scopes: None,
    };
}

/// Resolves the syntax of the file at `path` holding `text`, by precedence:
///
/// 1. a template row's extension as the file's last, such as `.j2`;
/// 2. the registry's exact file names, globs, then extensions (`.h` is C);
/// 3. bat's syntaxes by file name and extension;
/// 4. chezmoi's target name, for a chezmoi source such as `dot_zshrc`;
/// 5. the registry's interpreters, from a `#!` first line, `/usr/bin/env -S` and versions such as
///    `python3.12` read as Helix reads them;
/// 6. bat's syntaxes by first line, `#!` lines included;
/// 7. plain.
///
/// The path decides first in both tiers, so a script whose extension says what it is keeps
/// its language under a launcher's `#!/bin/sh`.
///
/// A row in the bat tier resolves to the bat syntax it names, never to what bat would detect
/// from the path.
///
/// A template row resolves to [`Syntax::Template`] with the language of the file the template
/// produces, found from its name, chezmoi's names and its first line, and without scopes, since
/// template syntax breaks the produced language's parse.
#[must_use]
pub fn resolve(path: &str, text: &str) -> Resolved {
    resolve_in(Languages::standard(), path, text)
}

/// Highlights `text` as `resolved` within [`BUDGET`], and reads its scopes.
///
/// A text larger than the blobs the engine reads as text stays plain.
#[must_use]
pub fn highlight(resolved: &Resolved, text: &str) -> SideSyntax {
    highlight_in(
        Languages::standard(),
        resolved,
        text,
        Deadline::after(BUDGET),
    )
}

/// Resolves and highlights `text` as the file at `path`, for a file shown on its own.
#[must_use]
pub fn analyze(path: &str, text: &str) -> SideSyntax {
    highlight(&resolve(path, text), text)
}

/// [`resolve`] over `languages`.
fn resolve_in(languages: &Languages, path: &str, text: &str) -> Resolved {
    let of_row = |index: usize| {
        let row = languages.row(index)?;
        let syntax = match row.engine {
            Engine::TreeHouse { .. } => Syntax::TreeHouse(row.name),
            Engine::Bat { syntax } => {
                bat::by_name(syntax).map_or(Syntax::Plain, |found| Syntax::Bat(found.name.clone()))
            }
        };
        Some(Resolved {
            syntax,
            scopes: row.scopes,
        })
    };
    let of_bat = |found: &syntect::parsing::SyntaxReference| Resolved {
        syntax: Syntax::Bat(found.name.clone()),
        scopes: None,
    };
    // A template suffix
    if let Some((index, inner)) = languages.by_template_suffix(path) {
        let chezmoi = path.ends_with(chezmoi::TEMPLATE_SUFFIX);
        return template(languages, index, inner, text, chezmoi);
    }
    // By path
    if let Some(index) = languages.by_path(path) {
        if languages.row(index).is_some_and(Row::is_template) {
            return template(languages, index, path, text, false);
        }
        if let Some(resolved) = of_row(index) {
            return resolved;
        }
    }
    if let Some(found) = bat::by_path(path) {
        return of_bat(found);
    }
    // By chezmoi's target name
    if let Some(data) =
        chezmoi::source(path).and_then(|source| chezmoi_data(languages, &source, text))
    {
        return match data {
            Data::TreeHouse(name) => languages.by_name(name).and_then(of_row),
            Data::Bat(name) => bat::by_name(&name).map(of_bat),
            Data::Plain => None,
        }
        .unwrap_or(Resolved::PLAIN);
    }
    // By first line
    let interpreter = text.lines().next().and_then(languages::interpreter);
    if let Some(resolved) = interpreter
        .and_then(|interpreter| languages.by_interpreter(interpreter))
        .and_then(of_row)
    {
        return resolved;
    }
    bat::by_first_line(text).map_or(Resolved::PLAIN, of_bat)
}

/// The resolution of a file of the template row at `index` whose path without the template's
/// extension is `inner`, its data's language found, in order, by:
///
/// 1. chezmoi's source kind, for a symbolic link, a modify script or a script it runs (see
///    [`chezmoi_data`]);
/// 2. `inner`, by the registry's path detection and then bat's, apart from template rows, where
///    bat's plain text is plain data;
/// 3. chezmoi's target name;
/// 4. the first line, by the registry's interpreters and then bat's.
///
/// chezmoi's names count only for `chezmoi`, a template with chezmoi's suffix, so a view such as
/// `modify_user.html.erb` is HTML, not a modify script.
/// A template whose data has no language is the template row itself.
fn template(
    languages: &Languages,
    index: usize,
    inner: &str,
    text: &str,
    chezmoi: bool,
) -> Resolved {
    let Some(row) = languages.row(index) else {
        return Resolved::PLAIN;
    };
    let source = chezmoi.then(|| chezmoi::source(inner)).flatten();
    let by_kind = source
        .as_ref()
        .filter(|source| source.kind != chezmoi::Kind::Target)
        .and_then(|source| chezmoi_data(languages, source, text));
    let data = by_kind
        .or_else(|| path_data(languages, inner))
        .or_else(|| {
            let source = source.as_ref()?;
            path_data(languages, &source.target)
        })
        .or_else(|| first_line_data(languages, text));
    let syntax = match data {
        Some(data) => Syntax::Template {
            name: row.name,
            data,
        },
        None => Syntax::TreeHouse(row.name),
    };
    Resolved {
        syntax,
        scopes: None,
    }
}

/// The language a chezmoi source holds by its kind, or by its target's name:
///
/// - a symbolic link is plain, its content a path;
/// - a modify script is what its `#!` line runs, else, when it says `chezmoi:modify-template`,
///   a template of its target, and else plain;
/// - a script chezmoi runs is what its `#!` line runs, else what its name says;
/// - any other file is its target's language.
fn chezmoi_data(languages: &Languages, source: &chezmoi::Source, text: &str) -> Option<Data> {
    let target = || path_data(languages, &source.target);
    match source.kind {
        chezmoi::Kind::Symlink => Some(Data::Plain),
        chezmoi::Kind::Modify => interpreter_data(languages, text)
            .or_else(|| {
                text.contains("chezmoi:modify-template")
                    .then(target)
                    .flatten()
            })
            .or(Some(Data::Plain)),
        chezmoi::Kind::Run => interpreter_data(languages, text).or_else(target),
        chezmoi::Kind::Target => target(),
    }
}

/// The data language of a file at `path`, apart from template rows: a registry row's, else
/// bat's, where bat's plain text is plain data.
fn path_data(languages: &Languages, path: &str) -> Option<Data> {
    if let Some(data) = languages
        .data_by_path(path)
        .and_then(|index| row_data(languages, index))
    {
        return Some(data);
    }
    bat::by_path_or_plain(path).map(|found| {
        if bat::is_plain(found) {
            Data::Plain
        } else {
            Data::Bat(found.name.clone())
        }
    })
}

/// The data language the interpreter of `text`'s `#!` line names, apart from template rows.
fn interpreter_data(languages: &Languages, text: &str) -> Option<Data> {
    let interpreter = text.lines().next().and_then(languages::interpreter)?;
    languages
        .data_by_interpreter(interpreter)
        .and_then(|index| row_data(languages, index))
        .or_else(|| bat::by_interpreter(interpreter).map(|found| Data::Bat(found.name.clone())))
}

/// The data language `text`'s first line selects: an interpreter, then bat's first-line rules.
fn first_line_data(languages: &Languages, text: &str) -> Option<Data> {
    interpreter_data(languages, text)
        .or_else(|| bat::by_first_line(text).map(|found| Data::Bat(found.name.clone())))
}

/// The data language of the row at `index`: its own in the tree-house tier, its bat syntax in
/// the bat tier.
fn row_data(languages: &Languages, index: usize) -> Option<Data> {
    let row = languages.row(index)?;
    match row.engine {
        Engine::TreeHouse { .. } => Some(Data::TreeHouse(row.name)),
        Engine::Bat { syntax } => bat::by_name(syntax).map(|found| Data::Bat(found.name.clone())),
    }
}

/// [`highlight`] over `languages`, until `deadline`.
///
/// The scopes come first, from review-core's own parse, within at most half of [`BUDGET`]:
/// they are all or nothing and usually take a small part of the time, while the highlighting
/// keeps whatever it finishes, so it takes all the time left, at least the other half.
/// Scopes that do not finish in time leave the breadcrumbs empty.
fn highlight_in(
    languages: &Languages,
    resolved: &Resolved,
    text: &str,
    deadline: Deadline,
) -> SideSyntax {
    let too_large =
        u64::try_from(text.len()).map_or(true, |len| len > review_core::git::MAX_TEXT_BLOB_BYTES);
    if too_large {
        return SideSyntax::default();
    }
    let scopes_deadline = deadline.capped(BUDGET / 2).instant();
    let scopes = resolved
        .scopes
        .and_then(|language| ScopeMap::until(language, text, Some(scopes_deadline)))
        .unwrap_or_default();
    let row = |name: &str| {
        let index = languages.by_name(name)?;
        Some((index, tree_house::Language::new(u32::try_from(index).ok()?)))
    };
    let language = match &resolved.syntax {
        Syntax::TreeHouse(name) => row(name).map(|(_, language)| language),
        // A variant that cannot be built leaves the data to the row's own injections.
        Syntax::Template { name, data } => {
            row(name).map(|(index, language)| languages.variant(index, data).unwrap_or(language))
        }
        Syntax::Bat(_) | Syntax::Plain => None,
    };
    let runs = match &resolved.syntax {
        Syntax::TreeHouse(_) | Syntax::Template { .. } => language
            .map(|language| tree::highlight(languages, language, text, deadline))
            .unwrap_or_default(),
        Syntax::Bat(name) => bat::by_name(name)
            .map(|syntax| bat::highlight(syntax, text, deadline))
            .unwrap_or_default(),
        Syntax::Plain => Vec::new(),
    };
    SideSyntax {
        scopes,
        lines: into_lines(text, &runs),
    }
}

/// A highlighted stretch of a whole text, in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    range: Range<usize>,
    tone: Tone,
}

/// Runs in order, adjacent ones of the same tone merged and plain ones left out.
#[derive(Default)]
struct Runs {
    runs: Vec<Run>,
}

impl Runs {
    /// Adds `range` in `tone` after the runs so far.
    fn push(&mut self, range: Range<usize>, tone: Tone) {
        if tone == Tone::Plain || range.is_empty() {
            return;
        }
        if let Some(last) = self.runs.last_mut()
            && last.tone == tone
            && last.range.end == range.start
        {
            last.range.end = range.end;
            return;
        }
        self.runs.push(Run { range, tone });
    }

    fn into_vec(self) -> Vec<Run> {
        self.runs
    }
}

/// Cuts ordered, non-overlapping `runs` over `text` into each line's spans, in bytes of the
/// line, leaving out line breaks.
fn into_lines(text: &str, runs: &[Run]) -> Vec<Vec<Span>> {
    let mut lines = Vec::new();
    let mut first = 0;
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.trim_end_matches(['\n', '\r']).len();
        // Runs that end before this line end before every later one.
        while runs.get(first).is_some_and(|run| run.range.end <= start) {
            first += 1;
        }
        let spans = runs
            .get(first..)
            .unwrap_or_default()
            .iter()
            .take_while(|run| run.range.start < end)
            .filter_map(|run| {
                let from = run.range.start.max(start);
                let to = run.range.end.min(end);
                (from < to).then(|| (from - start..to - start, run.tone))
            })
            .collect();
        lines.push(spans);
        start += line.len();
    }
    lines
}
