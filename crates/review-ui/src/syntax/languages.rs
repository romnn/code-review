//! The registry at run time: its rows with their injection regexes compiled and their
//! tree-house configurations built on first use, how files and injections find a row, and
//! tree-house's [`LanguageLoader`] over them.
//!
//! A template file whose data has a language highlights as a variant of its template row: a
//! [`tree_house::Language`] of its own, numbered after the rows, whose configuration is the
//! row's with one more injection, which gives the data that language.
//! Variants are built on first use and kept, one slot for each template row and data language,
//! so their number is bounded and a configuration, once built, never moves.

use std::borrow::Cow;
use std::sync::{LazyLock, OnceLock};

use regex::Regex;
use tree_house::highlighter::Highlight;
use tree_house::tree_sitter::Grammar;
use tree_house::{InjectionLanguageMarker, LanguageConfig, LanguageLoader, read_query};

use super::registry::{self, Engine, QueryKind, Row, query_text};
use super::tones::capture_tone;
use super::{Data, bat};

/// The injection language a variant gives data in a bat syntax: this, then the syntax's name,
/// which no row resolves, so the bridge to bat finds the syntax by its name.
pub(super) const BAT_MARKER: &str = "review-bat:";

/// The injection language a variant gives plain data, which nothing resolves.
pub(super) const PLAIN_MARKER: &str = "review-plain";

/// The rows and what is built from them.
pub(super) struct Languages {
    rows: Vec<Row>,
    /// Each row's injection regex, anchored at both ends and ignoring case.
    matchers: Vec<Option<Regex>>,
    /// Each tree-house-tier row's configuration, built when it is first asked for; `None` for a
    /// bat row or queries that do not compile.
    configs: Vec<OnceLock<Option<LanguageConfig>>>,
    /// The template rows, by index, in the order of their variants' slots.
    templates: Vec<usize>,
    /// The variants' configurations, built when first asked for: for each template row, one
    /// slot per row, then one per bat syntax, then one for plain data; `None` for queries that
    /// do not compile.
    ///
    /// Each is boxed so the empty slots stay small.
    variants: Vec<OnceLock<Option<Box<LanguageConfig>>>>,
}

/// The registry of review-languages' table.
static STANDARD: LazyLock<Languages> = LazyLock::new(|| Languages::new(registry::rows()));

/// Why a row's tree-house configuration could not be built.
#[derive(Debug, thiserror::Error)]
pub(super) enum ConfigError {
    /// The row is in the bat tier.
    #[error("{0} is not in the tree-house tier")]
    NotTreeHouse(&'static str),
    /// The grammar's ABI is one the runtime cannot load, the only way a grammar fails to load.
    #[error("the grammar of {0} has an ABI the tree-sitter runtime cannot load")]
    Grammar(&'static str),
    /// A query does not compile against the grammar.
    #[error("the queries of {name} do not compile")]
    Query {
        name: &'static str,
        #[source]
        source: Box<tree_house::tree_sitter::query::ParseError>,
    },
}

impl Languages {
    /// The registry of `rows`; tests build one with a row moved to another tier.
    pub(super) fn new(rows: Vec<Row>) -> Self {
        let matchers = rows
            .iter()
            .map(|row| {
                let regex = row.injection_regex?;
                Regex::new(&format!("(?i)^(?:{regex})$")).ok()
            })
            .collect();
        let configs = rows.iter().map(|_| OnceLock::new()).collect();
        let templates: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.is_template())
            .map(|(index, _)| index)
            .collect();
        let slots = templates.len() * Self::data_slots(rows.len());
        let variants = std::iter::repeat_with(OnceLock::new).take(slots).collect();
        Self {
            rows,
            matchers,
            configs,
            templates,
            variants,
        }
    }

    /// How many data languages each template row has a slot for: every row, every bat syntax,
    /// and plain data.
    fn data_slots(rows: usize) -> usize {
        rows + bat::count() + 1
    }

    /// The registry of the standard rows, shared by every highlight.
    pub(super) fn standard() -> &'static Self {
        &STANDARD
    }

    /// The row at `index`.
    pub(super) fn row(&self, index: usize) -> Option<&Row> {
        self.rows.get(index)
    }

    /// The tree-house configuration of the row at `index`, built and configured on first use;
    /// `None` for a bat row.
    pub(super) fn config(&self, index: usize) -> Option<&LanguageConfig> {
        let row = self.rows.get(index)?;
        self.configs
            .get(index)?
            .get_or_init(|| build_config(row).ok())
            .as_ref()
    }

    /// The variant of the template row at `index` whose data is `data`, built and configured on
    /// first use; `None` when the row is no template, the data is a template or has no slot,
    /// or the variant's queries do not compile.
    pub(super) fn variant(&self, index: usize, data: &Data) -> Option<tree_house::Language> {
        let template = self.templates.iter().position(|&row| row == index)?;
        let ordinal = match data {
            Data::TreeHouse(name) => self.by_name(name)?,
            Data::Bat(name) => self.rows.len() + bat::position(name)?,
            Data::Plain => self.rows.len() + bat::count(),
        };
        let slot = template * Self::data_slots(self.rows.len()) + ordinal;
        let row = self.rows.get(index)?;
        self.variants
            .get(slot)?
            .get_or_init(|| build_variant(row, data).ok().map(Box::new))
            .as_ref()?;
        let language = u32::try_from(self.rows.len() + slot).ok()?;
        Some(tree_house::Language::new(language))
    }

    /// The configuration of `language`, a row or a variant already built.
    pub(super) fn config_of(&self, language: tree_house::Language) -> Option<&LanguageConfig> {
        let index = language.idx();
        if index < self.rows.len() {
            return self.config(index);
        }
        self.variants
            .get(index - self.rows.len())?
            .get()?
            .as_deref()
    }

    /// The row of a language by its name or one of its aliases, ignoring case.
    pub(super) fn by_name(&self, name: &str) -> Option<usize> {
        self.rows.iter().position(|row| {
            row.name.eq_ignore_ascii_case(name)
                || row
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(name))
        })
    }

    /// The row a document's name for a language selects, such as a Markdown fence's `py`: by
    /// name or alias, then by the injection regex that matches it whole.
    ///
    /// Helix finds the longest match anywhere in the name, which works when every language has
    /// a row; with most languages in the bat tier it would read `csharp` as C, so the regexes
    /// must match the whole name and anything else is left to bat.
    pub(super) fn by_token(&self, token: &str) -> Option<usize> {
        let token = token.trim();
        self.by_name(token).or_else(|| {
            self.matchers
                .iter()
                .position(|matcher| matcher.as_ref().is_some_and(|regex| regex.is_match(token)))
        })
    }

    /// The template row a file's last extension names, ahead of every file name, glob and
    /// longer extension, and its path without that extension: `Dockerfile.j2` is Jinja
    /// producing `Dockerfile`.
    pub(super) fn by_template_suffix<'a>(&self, path: &'a str) -> Option<(usize, &'a str)> {
        let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let (stem, extension) = file_name.rsplit_once('.')?;
        if stem.is_empty() {
            return None;
        }
        let index = self
            .rows
            .iter()
            .position(|row| row.is_template() && row.detection.extensions.contains(&extension))?;
        let inner = path.get(..path.len() - extension.len() - 1)?;
        Some((index, inner))
    }

    /// The row of a file by its path, as [`Self::by_path`] finds it, apart from template rows:
    /// the language of a template's data.
    pub(super) fn data_by_path(&self, path: &str) -> Option<usize> {
        self.find_by_path(path, |row| !row.is_template())
    }

    /// The row of an interpreter a `#!` line names, apart from template rows.
    pub(super) fn data_by_interpreter(&self, interpreter: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| !row.is_template() && row.detection.shebangs.contains(&interpreter))
    }

    /// The row of a file by its path: an exact file name first, then a glob, then the longest
    /// extension.
    pub(super) fn by_path(&self, path: &str) -> Option<usize> {
        self.find_by_path(path, |_| true)
    }

    /// [`Self::by_path`] over the rows `eligible` accepts.
    fn find_by_path(&self, path: &str, eligible: impl Fn(&Row) -> bool) -> Option<usize> {
        let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if file_name.is_empty() {
            return None;
        }
        let rooted: Cow<'_, str> = if path.starts_with('/') {
            Cow::Borrowed(path)
        } else {
            Cow::Owned(format!("/{path}"))
        };
        self.rows
            .iter()
            .position(|row| eligible(row) && row.detection.filenames.contains(&file_name))
            .or_else(|| {
                self.rows.iter().position(|row| {
                    eligible(row)
                        && row
                            .detection
                            .globs
                            .iter()
                            .any(|glob| glob_matches(glob, &rooted))
                })
            })
            .or_else(|| {
                // `a.d.ts` tries `d.ts`, then `ts`.
                file_name
                    .match_indices('.')
                    .filter_map(|(dot, _)| file_name.get(dot + 1..))
                    .find_map(|extension| {
                        self.rows.iter().position(|row| {
                            eligible(row) && row.detection.extensions.contains(&extension)
                        })
                    })
            })
    }

    /// The row of an interpreter a `#!` line names, such as `python`.
    pub(super) fn by_interpreter(&self, interpreter: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.detection.shebangs.contains(&interpreter))
    }
}

impl LanguageLoader for Languages {
    /// Resolves an injection to a tree-house-tier row; a bat row or no row leaves it to
    /// [`tree_house::Syntax::unresolved_injections`], which the bridge to bat reads.
    fn language_for_marker(&self, marker: InjectionLanguageMarker) -> Option<tree_house::Language> {
        let index = match marker {
            // A variant's data in a bat syntax, or plain, is for the bridge to bat alone.
            InjectionLanguageMarker::Name(name)
                if name.starts_with(BAT_MARKER) || name == PLAIN_MARKER =>
            {
                return None;
            }
            InjectionLanguageMarker::Name(name) => self.by_name(name),
            InjectionLanguageMarker::Match(text) => self.by_token(&Cow::from(text)),
            InjectionLanguageMarker::Filename(text) => self.by_path(&Cow::from(text)),
            InjectionLanguageMarker::Shebang(text) => self.by_interpreter(&Cow::from(text)),
        }?;
        self.config(index)?;
        Some(tree_house::Language::new(u32::try_from(index).ok()?))
    }

    fn get_config(&self, language: tree_house::Language) -> Option<&LanguageConfig> {
        self.config_of(language)
    }
}

/// Builds `row`'s configuration from its grammar and queries, `; inherits:` resolved, and
/// configures it before any parse: every capture and local definition gets its tone's
/// highlight.
pub(super) fn build_config(row: &Row) -> Result<LanguageConfig, ConfigError> {
    configure(row, "")
}

/// Builds the configuration of the variant of the template row `row` whose data is `data`:
/// the row's, with one more injection pattern after the row's own, which gives the row's data
/// nodes `data`'s language.
///
/// tree-house keeps, of injections over the same node, the one of the last pattern, so this
/// pattern wins over the row's own pattern for its data, and the row's other injections, such
/// as ERB's Ruby code, stay.
/// The anonymous tokens inside a data node, such as a `#` in Jinja's text, are part of the data.
pub(super) fn build_variant(row: &Row, data: &Data) -> Result<LanguageConfig, ConfigError> {
    let language = match data {
        Data::TreeHouse(name) => (*name).to_owned(),
        Data::Bat(name) => format!("{BAT_MARKER}{name}"),
        Data::Plain => PLAIN_MARKER.to_owned(),
    };
    // A query string literal escapes its quotes and backslashes, which bat's names may hold.
    let language = language.replace('\\', "\\\\").replace('"', "\\\"");
    let nodes: Vec<String> = row.data.iter().map(|node| format!("({node})")).collect();
    let nodes = nodes.join(" ");
    // The row's query may end without a line break, so the pattern starts on a line of its own.
    let pattern = format!(
        "\n([{nodes}] @injection.content (#set! injection.language \"{language}\") \
         (#set! injection.combined) (#set! injection.include-unnamed-children))\n"
    );
    configure(row, &pattern)
}

/// Builds `row`'s configuration with `injections` after its own injection query.
fn configure(row: &Row, injections: &str) -> Result<LanguageConfig, ConfigError> {
    let Engine::TreeHouse { grammar, queries } = row.engine else {
        return Err(ConfigError::NotTreeHouse(row.name));
    };
    let grammar = Grammar::try_from(grammar).map_err(|_| ConfigError::Grammar(row.name))?;
    let read = |kind| read_query(queries, |directory| query_text(directory, kind).to_owned());
    let config = LanguageConfig::new(
        grammar,
        &read(QueryKind::Highlights),
        &(read(QueryKind::Injections) + injections),
        &read(QueryKind::Locals),
    )
    .map_err(|source| ConfigError::Query {
        name: row.name,
        source: Box::new(source),
    })?;
    config.configure(|name| capture_tone(name).map(|tone| Highlight::new(tone.index())));
    Ok(config)
}

/// The interpreter a `#!` first line names, without a version, as Helix reads it:
/// `#!/usr/bin/env -S python3.12 -u` names `python`.
pub(super) fn interpreter(first_line: &str) -> Option<&str> {
    let rest = first_line.strip_prefix("#!")?.trim_start();
    let mut words = rest.split_whitespace();
    let command = words.next()?;
    let program = match command.rfind(['/', '\\']) {
        // A path to `env` runs the next word that is not an option.
        Some(slash) if command.get(slash + 1..) == Some("env") => {
            words.find(|word| !word.starts_with('-'))?
        }
        Some(slash) => command.get(slash + 1..)?,
        None => command,
    };
    let end = program
        .find(|letter: char| letter == '.' || letter.is_ascii_digit())
        .unwrap_or(program.len());
    program.get(..end).filter(|name| !name.is_empty())
}

/// Whether `path`, which starts with `/`, matches a Helix file-type glob: `*` matches any run of
/// characters, `/` included, `?` any one, and a glob that does not start with `/` or `*/` gets
/// `*/` in front, so it matches at any depth.
fn glob_matches(glob: &str, path: &str) -> bool {
    let anchored: Cow<'_, str> = if glob.starts_with('/') || glob.starts_with("*/") {
        Cow::Borrowed(glob)
    } else {
        Cow::Owned(format!("*/{glob}"))
    };
    wildcard(anchored.as_bytes(), path.as_bytes())
}

/// Matches `text` against `pattern`'s `*` and `?`, backtracking only to the last `*`.
fn wildcard(pattern: &[u8], text: &[u8]) -> bool {
    let (mut at_pattern, mut at_text) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while at_text < text.len() {
        match pattern.get(at_pattern) {
            Some(b'*') => {
                star = Some((at_pattern, at_text));
                at_pattern += 1;
            }
            Some(&letter) if letter == b'?' || Some(&letter) == text.get(at_text) => {
                at_pattern += 1;
                at_text += 1;
            }
            _ => match star {
                // The last `*` takes one more character.
                Some((star_pattern, star_text)) => {
                    star = Some((star_pattern, star_text + 1));
                    at_pattern = star_pattern + 1;
                    at_text = star_text + 1;
                }
                None => return false,
            },
        }
    }
    pattern
        .get(at_pattern..)
        .is_some_and(|rest| rest.iter().all(|&letter| letter == b'*'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shebang lines name their interpreter without paths, `env`, its options or versions.
    #[test]
    fn interpreters_are_read_as_helix_reads_them() {
        assert_eq!(interpreter("#!/bin/sh"), Some("sh"));
        assert_eq!(interpreter("#! /usr/bin/python3.12 -u"), Some("python"));
        assert_eq!(interpreter("#!/usr/bin/env python3"), Some("python"));
        assert_eq!(interpreter("#!/usr/bin/env -S deno run -A"), Some("deno"));
        assert_eq!(interpreter("#!/usr/bin/env -S uv run --script"), Some("uv"));
        assert_eq!(interpreter("#!/usr/bin/env ts-node"), Some("ts-node"));
        assert_eq!(interpreter("#!node"), Some("node"));
        assert_eq!(interpreter("# not a shebang"), None);
        assert_eq!(interpreter("#!/usr/bin/env"), None);
        assert_eq!(interpreter("#!/usr/bin/3"), None);
    }

    /// Helix's globs match at any depth, and `*` crosses directories.
    #[test]
    fn globs_match_like_helix() {
        assert!(glob_matches("Dockerfile.*", "/Dockerfile.production"));
        assert!(glob_matches(
            "Dockerfile.*",
            "/deploy/Dockerfile.production"
        ));
        assert!(!glob_matches("Dockerfile.*", "/Dockerfile"));
        assert!(!glob_matches("Dockerfile.*", "/MyDockerfile.x"));
        assert!(glob_matches("i3/config", "/home/.config/i3/config"));
        assert!(!glob_matches("i3/config", "/xi3/config"));
        assert!(glob_matches("*.tfstate.backup", "/infra/a.tfstate.backup"));
        assert!(glob_matches(
            "bash-completion/completions/*",
            "/share/bash-completion/completions/git"
        ));
        assert!(glob_matches("*SConstruct", "/SConstruct"));
    }
}
