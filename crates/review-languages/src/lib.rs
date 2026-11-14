//! The languages review-ui highlights, from one manifest, `languages.toml` (ADR 3): [`LANGUAGES`],
//! one [`LanguageSpec`] per row, the tree-sitter grammars the tree-house tier fetches and
//! compiles, and the queries it highlights with, [`QUERIES`]: Helix's, and cr's own for the
//! rows whose queries are local, under `queries/`.
//!
//! build.rs checks the manifest, finds each fetched grammar's sources and Helix's queries
//! (prepared, cached under `_cache/` or fetched with `git`) at the revisions the manifest pins,
//! compiles the grammars, and generates one [`LanguageFn`] constant per grammar, such as `BASH`
//! and `MARKDOWN_INLINE`, the table, [`LICENSES`], [`HELIX`], the commit the rows were last
//! synced to, [`QUERIES`], and [`QUERIES_LICENSE`].
//! `task grammars:fetch` fills the cache ahead of an offline build.
//!
//! The table is data: review-ui maps each row onto its queries and, for the languages of
//! review-core's formatting proof, onto review-core's grammar crates.

use tree_sitter_language::LanguageFn;

/// One language: its names, its detection, and how it is highlighted.
#[derive(Clone, Copy)]
pub struct LanguageSpec {
    /// Helix's name for it, which injections use, such as `markdown.inline`.
    pub name: &'static str,
    /// Which engine highlights it.
    pub tier: Tier,
    /// Its grammar: always for the tree-house tier, and a proof language's in the bat tier too.
    pub grammar: Option<GrammarSpec>,
    /// The directory of its queries in [`QUERIES`], which `; inherits:` lines name too; set
    /// for the tree-house tier.
    pub queries: Option<&'static str>,
    /// The bat syntax that highlights it in the bat tier, such as `SQL`; set for the bat tier.
    pub bat: Option<&'static str>,
    /// Other names documents call it by, such as a Markdown fence's `c++`, matched whole and
    /// ignoring case.
    pub aliases: &'static [&'static str],
    /// Helix's injection regex, matched against a whole name and ignoring case.
    pub injection_regex: Option<&'static str>,
    /// For a template language, the kinds of its grammar's nodes that hold the template's
    /// literal text, such as `text`; empty for every other language.
    pub data: &'static [&'static str],
    /// Which files are this language.
    pub detection: Detection,
}

/// The engine that highlights a language, named for the project whose work it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// tree-house, with the language's grammar and Helix's queries.
    TreeHouse,
    /// syntect, with the language's bat syntax.
    Bat,
}

/// A language's grammar.
#[derive(Clone, Copy)]
pub enum GrammarSpec {
    /// The grammar of a language of review-core's formatting proof, by its manifest name, such
    /// as `rust` or `javascript`, which review-core parses with the TSX grammar.
    Proof(&'static str),
    /// A grammar this crate compiled.
    Compiled(LanguageFn),
}

/// Which files are a language, by precedence: an exact file name, then a glob over the path, an
/// extension, and last the interpreter a `#!` line names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Detection {
    /// Whole file names, such as `Makefile`.
    pub filenames: &'static [&'static str],
    /// Globs over the whole path, where `*` also crosses `/`, `?` matches one character, and a
    /// glob without a leading `/` matches at any depth, as in Helix: `Dockerfile.*`, `i3/config`.
    pub globs: &'static [&'static str],
    /// Extensions without their dot, matched case-sensitively; one may have dots of its own,
    /// such as `js.map`.
    pub extensions: &'static [&'static str],
    /// Interpreters a `#!` line names, without a version: `python` also covers `python3.12`.
    pub shebangs: &'static [&'static str],
}

/// Which query of a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// `highlights.scm`.
    Highlights,
    /// `injections.scm`.
    Injections,
    /// `locals.scm`.
    Locals,
}

/// One query file: Helix's, embedded unmodified, or cr's own of a row whose queries are local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Query {
    /// Its directory under Helix's `runtime/queries/` or review-languages' `queries/`, such as
    /// `rust` or `ecma`.
    pub directory: &'static str,
    /// Which query it is.
    pub kind: QueryKind,
    /// Its text, whose `; inherits:` lines are left for tree-house's `read_query` to resolve.
    pub text: &'static str,
}

/// The text of the query of `kind` in `directory` of [`QUERIES`], or `None` when there is no
/// such file.
#[must_use]
pub fn query(directory: &str, kind: QueryKind) -> Option<&'static str> {
    QUERIES
        .iter()
        .find(|query| query.directory == directory && query.kind == kind)
        .map(|query| query.text)
}

/// Where one compiled grammar or Helix's queries come from, and their license, for the notices
/// release archives ship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct License {
    /// What it covers: a grammar's name, its C entry point's suffix, or `helix-queries`.
    pub name: &'static str,
    /// Its repository.
    pub git: &'static str,
    /// The commit compiled or embedded.
    pub rev: &'static str,
    /// The SPDX identifier of its license.
    pub spdx: &'static str,
    /// The license's text.
    pub text: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/languages.rs"));

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    /// Every compiled grammar loads into the workspace's runtime and parses a sample without
    /// errors.
    #[test]
    fn every_grammar_parses() {
        let samples: [(&str, LanguageFn, &str); 19] = [
            ("bash", BASH, "echo hi\n"),
            ("c", C, "int main(void) { return 0; }\n"),
            ("cpp", CPP, "class A { int f(); };\n"),
            ("css", CSS, "a { color: red; }\n"),
            (
                "diff",
                DIFF,
                indoc! {"
                    @@ -1 +1 @@
                    -a
                    +b
                "},
            ),
            ("dockerfile", DOCKERFILE, "FROM alpine\n"),
            (
                "embedded_template",
                EMBEDDED_TEMPLATE,
                "<p><%= name %></p>\n",
            ),
            ("gotmpl", GOTMPL, "name: {{ .Values.name }}\n"),
            ("html", HTML, "<p>hi</p>\n"),
            ("java", JAVA, "class A {}\n"),
            (
                "jinja",
                JINJA,
                "{% if user %}<p>{{ user.name | e }}</p>{% endif %}\n",
            ),
            ("lua", LUA, "local x = 1\n"),
            // A recipe line starts with a tab.
            (
                "make",
                MAKE,
                indoc! {"
                    all:
                    \techo hi
                "},
            ),
            ("markdown", MARKDOWN, "# Title\n"),
            ("markdown_inline", MARKDOWN_INLINE, "*soft*"),
            ("powershell", POWERSHELL, "Write-Host hi\n"),
            ("ruby", RUBY, "puts 1\n"),
            ("twig", TWIG, "<p>{{ user.name|upper }}</p>\n"),
            ("yaml", YAML, "a: 1\n"),
        ];
        for (name, language, sample) in samples {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&language.into()).unwrap();
            let tree = parser.parse(sample, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "{name}: {}",
                tree.root_node().to_sexp()
            );
        }
        assert_eq!(LICENSES.len(), 19);
        assert!(
            LICENSES
                .iter()
                .all(|license| !license.text.trim().is_empty())
        );
    }

    /// The table is the manifest's: every tree-house-tier row has a grammar and queries, every
    /// bat-tier row a bat syntax, and the fetched grammars compiled are exactly the tree-house
    /// tier's, a grammar that rows share, such as ERB's and EJS's, once.
    #[test]
    fn the_table_is_the_manifests() {
        assert_eq!(LANGUAGES.len(), 30);
        let mut compiled = std::collections::HashSet::new();
        for language in LANGUAGES {
            match language.tier {
                Tier::TreeHouse => {
                    assert!(language.grammar.is_some(), "{}", language.name);
                    assert!(language.queries.is_some(), "{}", language.name);
                    if let Some(GrammarSpec::Compiled(grammar)) = language.grammar {
                        compiled.insert(grammar.into_raw() as usize);
                    }
                }
                Tier::Bat => assert!(language.bat.is_some(), "{}", language.name),
            }
        }
        assert_eq!(compiled.len(), LICENSES.len());
        let templates: Vec<&str> = LANGUAGES
            .iter()
            .filter(|language| !language.data.is_empty())
            .map(|language| language.name)
            .collect();
        assert_eq!(templates, ["gotmpl", "helm", "jinja", "erb", "ejs", "twig"]);
        let markdown = LANGUAGES
            .iter()
            .find(|language| language.name == "markdown.inline")
            .unwrap();
        assert_eq!(markdown.injection_regex, Some("markdown\\.inline"));
    }

    /// Helix's queries are embedded with the MPL-2.0 they are under, at the manifest's commit.
    #[test]
    fn the_queries_carry_their_license() {
        assert!(query("rust", QueryKind::Highlights).is_some());
        assert_eq!(QUERIES_LICENSE.rev, HELIX);
        assert_eq!(QUERIES_LICENSE.spdx, "MPL-2.0");
        assert!(
            QUERIES_LICENSE
                .text
                .contains("Mozilla Public License Version 2.0")
        );
    }
}
