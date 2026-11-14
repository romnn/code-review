//! The languages manifest, review-languages' `languages.toml`: one row per highlighted language,
//! the one source of truth of review-ui's registry (ADR 3).
//!
//! It is parsed and checked here alone, for review-languages' build script and for
//! `task grammars:fetch`; the build script turns it into the table review-ui reads.
//! Its `helix` commit pins Helix's queries ([`Queries`]), which the build script embeds.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use crate::Error;

/// The languages of review-core's formatting proof, whose grammar crates a row may share.
pub const PROOF: &[&str] = &[
    "rust",
    "python",
    "javascript",
    "typescript",
    "tsx",
    "go",
    "json",
    "toml",
];

/// Helix's repository, whose queries review-languages embeds at the manifest's `helix` commit.
pub const HELIX_GIT: &str = "https://github.com/helix-editor/helix";

/// The fields `locked` may name, which `task grammars:sync` then leaves alone; `grammar` stands
/// for the grammar's pin.
pub const LOCKABLE: &[&str] = &[
    "grammar",
    "extensions",
    "filenames",
    "globs",
    "shebangs",
    "injection-regex",
];

/// The manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The Helix commit the rows were last synced to, as a full object id, whose queries
    /// review-languages embeds.
    pub helix: String,
    /// The languages, in the order the registry lists them.
    #[serde(rename = "language")]
    pub languages: Vec<Language>,
}

/// One language: its names, its detection, and how it is highlighted.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Language {
    /// Helix's name for it, which injections use, such as `markdown.inline`.
    pub name: String,
    /// Which engine highlights it.
    pub tier: Tier,
    /// The directory of its queries under Helix's `runtime/queries/`, for the tree-house tier.
    #[serde(default)]
    pub queries: Option<String>,
    /// The language of review-core's formatting proof whose grammar it shares, one of
    /// [`PROOF`].
    #[serde(default)]
    pub proof: Option<String>,
    /// The grammar fetched and compiled for it, when it is not a proof language.
    #[serde(default)]
    pub grammar: Option<Grammar>,
    /// The bat syntax that highlights it in the bat tier.
    #[serde(default)]
    pub bat: Option<String>,
    /// Other names documents call it by, such as a fence's `c++`.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Helix's injection regex, matched against a whole name and ignoring case.
    #[serde(default)]
    pub injection_regex: Option<String>,
    /// Extensions without their dot, such as `rs` or `js.map`.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Whole file names, such as `Makefile`.
    #[serde(default)]
    pub filenames: Vec<String>,
    /// Globs over the path, where `*` also crosses `/` and `?` matches one character.
    #[serde(default)]
    pub globs: Vec<String>,
    /// The interpreters of a `#!` line, without a version.
    #[serde(default)]
    pub shebangs: Vec<String>,
    /// Whether its queries are cr's own, under review-languages' `queries/<queries>/`, rather
    /// than Helix's.
    #[serde(default)]
    pub local_queries: bool,
    /// The literal text of a template language, whose language is that of the file the
    /// template produces; `None` for every other language.
    #[serde(default)]
    pub data: Option<TemplateData>,
    /// What `task grammars:sync` leaves alone.
    #[serde(default)]
    pub locked: Locked,
}

/// Where a template language's grammar keeps the template's literal text.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateData {
    /// The kinds of the nodes that hold it, such as Go templates' `text`.
    pub nodes: Vec<String>,
}

/// The engine that highlights a language, named for the project whose work it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// tree-house, with a grammar and Helix's queries.
    TreeHouse,
    /// syntect, with a bat syntax.
    Bat,
}

/// What `task grammars:sync` leaves alone in a row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Locked {
    /// The whole row when `true`, nothing when `false`.
    All(bool),
    /// These fields, each one of [`LOCKABLE`].
    Fields(Vec<String>),
}

impl Default for Locked {
    fn default() -> Self {
        Self::All(false)
    }
}

/// A grammar fetched at a pinned commit and compiled.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Grammar {
    /// Its name, which its C entry point carries, `tree_sitter_<name>`, and its cache entries:
    /// the row's name unless the manifest names another.
    #[serde(default)]
    pub name: String,
    /// The repository.
    pub git: String,
    /// The commit, as a full object id.
    pub rev: String,
    /// The grammar's directory in the repository, when it is not the root.
    #[serde(default)]
    pub subpath: Option<String>,
    /// The files to compile, relative to the grammar's `src/`: `parser.c`, and a scanner when
    /// the grammar has one.
    pub sources: Vec<String>,
    /// The SPDX identifier of its license.
    pub license: String,
    /// Its license file, relative to the repository's root.
    #[serde(default = "default_license_file")]
    pub license_file: String,
}

fn default_license_file() -> String {
    "LICENSE".to_owned()
}

/// Helix's queries at the manifest's `helix` commit: the directories of `runtime/queries/` the
/// tree-house rows name, which are fetched with every directory their `; inherits:` lines name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queries {
    /// Helix's repository, [`HELIX_GIT`].
    pub git: String,
    /// The commit, the manifest's `helix`.
    pub rev: String,
    /// The directories the tree-house rows name, sorted and without duplicates.
    pub directories: Vec<String>,
}

impl Queries {
    /// The name of the queries' prepared directory, and of their entries and lock in a cache,
    /// which no grammar's name can be, since it is not a C identifier.
    pub const NAME: &str = "helix-queries";

    /// The directory of Helix's repository that holds one directory of queries per language.
    pub const UPSTREAM: &str = "runtime/queries";

    /// The directory of an entry that holds the query directories, such as
    /// `queries/rust/highlights.scm`.
    pub const DIRECTORY: &str = "queries";

    /// The query files highlighting reads, which are fetched from each directory that has them.
    pub const KINDS: [&str; 3] = ["highlights.scm", "injections.scm", "locals.scm"];

    /// Helix's license file, the MPL-2.0 its queries are under.
    pub const LICENSE_FILE: &str = "LICENSE";

    /// The SPDX identifier of the queries' license.
    pub const SPDX: &str = "MPL-2.0";
}

/// What is wrong with a row of the manifest.
#[derive(Debug, thiserror::Error)]
pub enum Problem {
    /// Another row has the same name.
    #[error("another language has this name")]
    Duplicate,
    /// A tree-house-tier row names no queries.
    #[error("a tree-house-tier language needs `queries`")]
    NoQueries,
    /// A tree-house-tier row has no grammar, or two.
    #[error("a tree-house-tier language needs exactly one of `proof` and `[language.grammar]`")]
    Grammar,
    /// A bat-tier row names no bat syntax.
    #[error("a bat-tier language needs `bat`")]
    NoBatSyntax,
    /// `proof` names a language the formatting proof does not have.
    #[error("`proof = {0:?}` is not one of review-core's languages")]
    UnknownProof(String),
    /// The grammar's name cannot be a C identifier or a cache entry's name.
    #[error("grammar name {0:?} is not a lowercase C identifier")]
    GrammarName(String),
    /// Another row compiles a grammar of the same name from other sources.
    #[error("another language compiles grammar {0} from other sources")]
    DuplicateGrammar(String),
    /// The grammar pins something other than one commit, which could move.
    #[error("`rev = {0:?}` is not a full commit id")]
    Revision(String),
    /// The grammar compiles no files.
    #[error("the grammar has no `sources`")]
    NoSources,
    /// A glob uses syntax the matcher does not have, or is a plain file name.
    #[error(
        "glob {0:?} is not one the registry matches: only `*` and `?` are wildcards, and a name \
         without either or `/` belongs in `filenames`"
    )]
    Glob(String),
    /// A file name is empty or holds a separator or wildcard.
    #[error("file name {0:?} is not a whole file name")]
    Filename(String),
    /// An extension is empty or holds a dot at its start or a separator.
    #[error("extension {0:?} is not an extension without its dot")]
    Extension(String),
    /// The injection regex does not compile.
    #[error("injection regex {regex:?} does not compile")]
    Regex {
        /// The regex.
        regex: String,
        /// Why.
        #[source]
        source: Box<regex::Error>,
    },
    /// `data` names no nodes, or a node kind that cannot be a grammar's.
    #[error("`data` needs node kinds, each a lowercase identifier, not {0:?}")]
    DataNodes(Vec<String>),
    /// `local-queries` is set on a row without queries.
    #[error("`local-queries` needs `queries`")]
    LocalQueries,
    /// `locked` names a field sync does not move.
    #[error("`locked` names {0:?}, which is not one of {LOCKABLE:?}")]
    LockedField(String),
}

impl Manifest {
    /// Reads and checks the manifest at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when it cannot be read, [`Error::Manifest`] when it is not TOML of
    /// the manifest's shape, [`Error::HelixRevision`] when its `helix` is not a full commit id,
    /// and [`Error::Language`] for the first row that is not valid.
    pub fn read(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            action: "read",
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text).map_err(|err| match err {
            ParseError::Toml(source) => Error::Manifest {
                path: path.to_owned(),
                source,
            },
            ParseError::Invalid(err) => err,
        })
    }

    /// The grammars to fetch and compile: those of the tree-house tier's rows that pin one, each
    /// once, though several rows may share it.
    pub fn grammars(&self) -> impl Iterator<Item = &Grammar> {
        let mut seen = HashSet::new();
        self.languages
            .iter()
            .filter(|language| language.tier == Tier::TreeHouse)
            .filter_map(|language| language.grammar.as_ref())
            .filter(move |grammar| seen.insert(grammar.name.as_str()))
    }

    /// The queries to fetch and embed: Helix's, at its `helix` commit, in the directories the
    /// tree-house tier's rows name, apart from the rows whose queries are local.
    #[must_use]
    pub fn queries(&self) -> Queries {
        let mut directories: Vec<String> = self
            .languages
            .iter()
            .filter(|language| language.tier == Tier::TreeHouse && !language.local_queries)
            .filter_map(|language| language.queries.clone())
            .collect();
        directories.sort();
        directories.dedup();
        Queries {
            git: HELIX_GIT.to_owned(),
            rev: self.helix.clone(),
            directories,
        }
    }

    /// The directories of the local queries to embed, under review-languages' `queries/`: those
    /// of the tree-house tier's rows whose queries are local, sorted and without duplicates.
    #[must_use]
    pub fn local_queries(&self) -> Vec<String> {
        let mut directories: Vec<String> = self
            .languages
            .iter()
            .filter(|language| language.tier == Tier::TreeHouse && language.local_queries)
            .filter_map(|language| language.queries.clone())
            .collect();
        directories.sort();
        directories.dedup();
        directories
    }

    pub(crate) fn parse(text: &str) -> Result<Self, ParseError> {
        let mut manifest: Self =
            toml::from_str(text).map_err(|source| ParseError::Toml(Box::new(source)))?;
        if !is_full_commit(&manifest.helix) {
            return Err(ParseError::Invalid(Error::HelixRevision(
                manifest.helix.clone(),
            )));
        }
        let mut names = HashSet::new();
        let mut grammars: HashMap<String, Grammar> = HashMap::new();
        for language in &mut manifest.languages {
            if let Some(grammar) = &mut language.grammar
                && grammar.name.is_empty()
            {
                grammar.name.clone_from(&language.name);
            }
            let invalid = |problem| {
                ParseError::Invalid(Error::Language {
                    name: language.name.clone(),
                    problem,
                })
            };
            if !names.insert(language.name.clone()) {
                return Err(invalid(Problem::Duplicate));
            }
            language.check().map_err(invalid)?;
            // Rows may share a grammar, such as ERB and EJS, when they pin it alike.
            if language.tier == Tier::TreeHouse
                && let Some(grammar) = &language.grammar
                && grammars
                    .entry(grammar.name.clone())
                    .or_insert_with(|| grammar.clone())
                    != grammar
            {
                return Err(invalid(Problem::DuplicateGrammar(grammar.name.clone())));
            }
        }
        Ok(manifest)
    }
}

impl Language {
    /// Checks the row on its own.
    fn check(&self) -> Result<(), Problem> {
        match self.tier {
            Tier::TreeHouse => {
                if self.queries.is_none() {
                    return Err(Problem::NoQueries);
                }
                if self.proof.is_some() == self.grammar.is_some() {
                    return Err(Problem::Grammar);
                }
            }
            Tier::Bat => {
                if self.bat.is_none() {
                    return Err(Problem::NoBatSyntax);
                }
            }
        }
        if let Some(proof) = &self.proof
            && !PROOF.contains(&proof.as_str())
        {
            return Err(Problem::UnknownProof(proof.clone()));
        }
        if let Some(grammar) = &self.grammar {
            grammar.check()?;
        }
        if let Some(data) = &self.data
            && (data.nodes.is_empty() || !data.nodes.iter().all(|kind| node_kind(kind)))
        {
            return Err(Problem::DataNodes(data.nodes.clone()));
        }
        if self.local_queries && self.queries.is_none() {
            return Err(Problem::LocalQueries);
        }
        for glob in &self.globs {
            let wildcard = glob.contains(['*', '?', '/']);
            if glob.is_empty() || !wildcard || glob.contains(['[', ']', '{', '}', '\\']) {
                return Err(Problem::Glob(glob.clone()));
            }
        }
        for name in &self.filenames {
            if name.is_empty() || name.contains(['/', '\\', '*', '?']) {
                return Err(Problem::Filename(name.clone()));
            }
        }
        for extension in &self.extensions {
            if extension.is_empty() || extension.starts_with('.') || extension.contains(['/', '\\'])
            {
                return Err(Problem::Extension(extension.clone()));
            }
        }
        if let Some(regex) = &self.injection_regex {
            // As the registry compiles it.
            regex::Regex::new(&format!("(?i)^(?:{regex})$")).map_err(|source| Problem::Regex {
                regex: regex.clone(),
                source: Box::new(source),
            })?;
        }
        if let Locked::Fields(fields) = &self.locked
            && let Some(field) = fields
                .iter()
                .find(|field| !LOCKABLE.contains(&field.as_str()))
        {
            return Err(Problem::LockedField(field.clone()));
        }
        Ok(())
    }
}

impl Grammar {
    /// Checks the grammar's name and pin.
    fn check(&self) -> Result<(), Problem> {
        let identifier = !self
            .name
            .starts_with(|letter: char| letter.is_ascii_digit())
            && self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        if self.name.is_empty() || !identifier {
            return Err(Problem::GrammarName(self.name.clone()));
        }
        if !is_full_commit(&self.rev) {
            return Err(Problem::Revision(self.rev.clone()));
        }
        if self.sources.is_empty() {
            return Err(Problem::NoSources);
        }
        Ok(())
    }
}

/// Whether `kind` can be a node kind, a lowercase identifier, which review-ui writes into a
/// query unescaped.
fn node_kind(kind: &str) -> bool {
    !kind.is_empty()
        && !kind.starts_with(|letter: char| letter.is_ascii_digit())
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Whether `rev` is a full SHA-1 object id.
///
/// Only a full object id names one commit; a branch, tag or abbreviation could fetch something
/// else.
fn is_full_commit(rev: &str) -> bool {
    rev.len() == 40
        && rev
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Why a manifest's text does not parse, before [`Manifest::read`] names its path.
pub(crate) enum ParseError {
    Toml(Box<toml::de::Error>),
    Invalid(Error),
}
