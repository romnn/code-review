//! Highlighting both tiers: queries that compile, exact tokens per language, detection, the
//! bridge from tree-house's injections to bat, and the budget.
//!
//! Tokens are asserted as Helix's queries and bat's syntaxes classify them, plain ones
//! included, so a query that stops matching or matches too much fails here.

use std::time::Duration;

use std::fmt::Write as _;

use indoc::{formatdoc, indoc, writedoc};

use super::registry::{self, Row};
use super::*;

mod templates;

/// The tones over the token `marked` names in `text`.
///
/// `marked` is a stretch of `text` with the token between `⟨` and `⟩`, found where the stretch
/// first occurs, as `apply(⟨x⟩` names the `x` right after `apply(`; without marks the whole
/// stretch is the token.
fn tones_of(syntax: &SideSyntax, text: &str, marked: &str) -> Vec<Tone> {
    let (before, token, after) = match marked.split_once('⟨') {
        Some((before, rest)) => {
            let (token, after) = rest.split_once('⟩').unwrap();
            (before, token, after)
        }
        None => ("", marked, ""),
    };
    let stretch = format!("{before}{token}{after}");
    let start = text
        .find(&stretch)
        .unwrap_or_else(|| panic!("{stretch:?} is not in the text"))
        + before.len();
    let line = text[..start].matches('\n').count();
    let line_start = text[..start].rfind('\n').map_or(0, |newline| newline + 1);
    let mut tones: Vec<Tone> = (start - line_start..start - line_start + token.len())
        .map(|column| tone_at(syntax, line, column))
        .collect();
    tones.dedup();
    tones
}

/// The tone at `column` of the zero-based `line`.
fn tone_at(syntax: &SideSyntax, line: usize, column: usize) -> Tone {
    syntax
        .line(line)
        .iter()
        .find(|(range, _)| range.contains(&column))
        .map_or(Tone::Plain, |(_, tone)| *tone)
}

/// Asserts that `text`, as the file at `path`, resolves to `syntax` and that each marked token
/// (see [`tones_of`]) takes exactly its tone over every byte.
fn expect_tones(path: &str, text: &str, syntax: &Syntax, expected: &[(&str, Tone)]) {
    let resolved = resolve(path, text);
    assert_eq!(&resolved.syntax, syntax, "{path} resolves");
    let highlighted = highlight(&resolved, text);
    for &(marked, tone) in expected {
        assert_eq!(
            tones_of(&highlighted, text, marked),
            vec![tone],
            "{path}: {marked:?}"
        );
    }
}

/// The tree-house-tier syntax of `name`.
fn tree(name: &'static str) -> Syntax {
    Syntax::TreeHouse(name)
}

/// The bat syntax of `name`.
fn bat(name: &str) -> Syntax {
    Syntax::Bat(name.to_owned())
}

/// A deadline no test reaches, so debug builds' slowness never turns highlights plain.
fn far() -> Deadline {
    Deadline::after(Duration::from_secs(600))
}

/// Every tree-house-tier row's queries compile against its grammar through
/// `LanguageConfig::new`, `; inherits:` resolved, so no injection or highlight silently loses
/// its language.
#[test]
fn every_tree_house_row_compiles() {
    for row in &registry::rows() {
        if matches!(row.engine, Engine::TreeHouse { .. })
            && let Err(err) = languages::build_config(row)
        {
            panic!(
                "{err}: {}",
                std::error::Error::source(&err).map_or_else(String::new, ToString::to_string)
            );
        }
    }
}

/// The registry is one table without contradictions: names, aliases, file names, globs,
/// extensions and interpreters each belong to one row, every injection regex compiles, and
/// every bat syntax a row names exists.
#[test]
fn the_registry_has_no_conflicts() {
    let rows = registry::rows();
    // Every row of the manifest's table is a row of the registry.
    assert_eq!(rows.len(), review_languages::LANGUAGES.len());
    let mut seen = std::collections::HashMap::new();
    for row in &rows {
        let claims = std::iter::once(format!("name {}", row.name))
            .chain(row.aliases.iter().map(|alias| format!("name {alias}")))
            .chain(
                row.detection
                    .filenames
                    .iter()
                    .map(|name| format!("file {name}")),
            )
            .chain(
                row.detection
                    .globs
                    .iter()
                    .map(|glob| format!("glob {glob}")),
            )
            .chain(
                row.detection
                    .extensions
                    .iter()
                    .map(|ext| format!("extension {ext}")),
            )
            .chain(
                row.detection
                    .shebangs
                    .iter()
                    .map(|name| format!("shebang {name}")),
            );
        for claim in claims {
            if let Some(other) = seen.insert(claim.clone(), row.name) {
                panic!("{claim} belongs to {other} and {}", row.name);
            }
        }
    }
    for row in &rows {
        if let Some(regex) = row.injection_regex {
            assert!(
                regex::Regex::new(&format!("(?i)^(?:{regex})$")).is_ok(),
                "{}'s injection regex compiles",
                row.name
            );
        }
        if let Engine::Bat { syntax } = row.engine {
            assert!(bat::by_name(syntax).is_some(), "bat has {syntax}");
        }
    }
}

/// Every tree-house row's queries are embedded: its directory has Helix's highlights at least.
#[test]
fn every_tree_house_rows_queries_are_embedded() {
    for row in &registry::rows() {
        let Engine::TreeHouse { queries, .. } = row.engine else {
            continue;
        };
        assert!(
            !registry::query_text(queries, registry::QueryKind::Highlights).is_empty(),
            "{}'s queries {queries} have highlights",
            row.name
        );
    }
}

/// Every `; inherits:` line of an embedded query names a directory that is embedded too, as
/// `tree_house::read_query` asks for it, so no query silently loses what it inherits.
#[test]
fn every_inherits_line_resolves() {
    let embedded = |directory: &str| {
        review_languages::QUERIES
            .iter()
            .any(|query| query.directory == directory)
    };
    let mut inherited = 0;
    for query in review_languages::QUERIES {
        tree_house::read_query(query.directory, |directory| {
            if directory == query.directory {
                return query.text.to_owned();
            }
            inherited += 1;
            assert!(
                embedded(directory),
                "{}'s {:?} inherits {directory}, which is not embedded",
                query.directory,
                query.kind
            );
            registry::query_text(directory, query.kind).to_owned()
        });
    }
    // TypeScript, TSX and JavaScript inherit from `ecma`, `_typescript` and `_jsx`, so a reader
    // that saw no `; inherits:` line at all would be the one broken here.
    assert!(inherited > 0, "some query inherits");
}

/// Rust: keywords, functions, types, strings with escapes, numbers, comments, attributes and
/// macros, and a parameter used in the body takes the parameter's tone through locals.
#[test]
fn rust_tokens() {
    let text = indoc! {r#"
        // note
        #[derive(Debug)]
        pub fn apply(x: u32, name: &str) -> String {
            let s = "a\n";
            let c = 'c';
            println!("{name}");
            self.total + x + 42
        }
    "#};
    expect_tones(
        "lib.rs",
        text,
        &tree("rust"),
        &[
            ("// note", Tone::Comment),
            ("#[⟨derive⟩", Tone::Special),
            ("derive(⟨Debug⟩", Tone::Type),
            ("pub", Tone::Keyword),
            ("pub ⟨fn⟩", Tone::Keyword),
            ("apply", Tone::Function),
            ("apply(⟨x⟩", Tone::Parameter),
            ("u32", Tone::Type),
            ("String", Tone::Type),
            ("let ⟨s⟩", Tone::Plain),
            ("\"a", Tone::String),
            ("\\n", Tone::Escape),
            ("'c'", Tone::String),
            ("println!", Tone::Function),
            ("self", Tone::Special),
            ("self.⟨total⟩", Tone::Property),
            ("total + ⟨x⟩", Tone::Parameter),
            ("42", Tone::Constant),
            ("->", Tone::Operator),
        ],
    );
}

/// Python: a parameter used in the body is a parameter, a class name a type.
#[test]
fn python_tokens() {
    let text = indoc! {r#"
        import os
        def scale(value, by=2):
            """Doc."""
            return value * by  # times
        class Parser:
            pass
    "#};
    expect_tones(
        "tools.py",
        text,
        &tree("python"),
        &[
            ("import", Tone::Keyword),
            ("import ⟨os⟩", Tone::Plain),
            ("def", Tone::Keyword),
            ("scale", Tone::Function),
            ("scale(⟨value⟩", Tone::Parameter),
            ("by=⟨2⟩", Tone::Constant),
            ("\"\"\"Doc.\"\"\"", Tone::String),
            ("return ⟨value⟩", Tone::Parameter),
            ("value * ⟨by⟩", Tone::Parameter),
            ("# times", Tone::Comment),
            ("class ⟨Parser⟩", Tone::Type),
        ],
    );
}

/// JavaScript, parsed with the TSX grammar and highlighted with TSX's queries: JSX tags and
/// attributes, a template's interpolation, and a parameter in the body.
#[test]
fn javascript_tokens() {
    let text = indoc! {r#"
        import x from 'y';
        function inc(a) { return a + 1; }
        const el = <div className="a">{a}</div>;
        this.b = `t${a}`;
    "#};
    expect_tones(
        "app.jsx",
        text,
        &tree("javascript"),
        &[
            ("import", Tone::Keyword),
            ("'y'", Tone::String),
            ("inc", Tone::Function),
            ("inc(⟨a⟩", Tone::Parameter),
            ("return ⟨a⟩", Tone::Parameter),
            ("<⟨div⟩", Tone::Tag),
            ("className", Tone::Attribute),
            ("this", Tone::Special),
            ("`t", Tone::String),
            ("`t⟨${⟩", Tone::Punctuation),
        ],
    );
}

/// TypeScript: interfaces, built-in types and a parameter in the body.
#[test]
fn typescript_tokens() {
    let text = indoc! {"
        interface Shape { area: number }
        function show(p: string): void { console.log(p); }
    "};
    expect_tones(
        "shape.ts",
        text,
        &tree("typescript"),
        &[
            ("interface", Tone::Keyword),
            ("Shape", Tone::Type),
            ("area", Tone::Property),
            ("number", Tone::Type),
            ("show", Tone::Function),
            ("show(⟨p⟩", Tone::Parameter),
            ("log(⟨p⟩", Tone::Parameter),
            ("log", Tone::Function),
        ],
    );
}

/// TSX: a component's parameter in the body and its JSX.
#[test]
fn tsx_tokens() {
    let text = "function Card(props: Props) { return <div>{props.title}</div>; }\n";
    expect_tones(
        "card.tsx",
        text,
        &tree("tsx"),
        &[
            ("function", Tone::Keyword),
            ("Props", Tone::Type),
            ("Card(⟨props⟩", Tone::Parameter),
            ("{⟨props⟩", Tone::Parameter),
            ("title", Tone::Property),
            ("<⟨div⟩", Tone::Tag),
        ],
    );
}

/// Go: a parameter in the body, a package name left plain.
#[test]
fn go_tokens() {
    let text = indoc! {r#"
        package main
        import "fmt"
        func show(count int) string { return fmt.Sprint(count) }
    "#};
    expect_tones(
        "main.go",
        text,
        &tree("go"),
        &[
            ("package", Tone::Keyword),
            ("\"fmt\"", Tone::String),
            ("func", Tone::Keyword),
            ("show", Tone::Function),
            ("show(⟨count⟩", Tone::Parameter),
            ("int", Tone::Type),
            ("return ⟨fmt⟩", Tone::Plain),
            ("Sprint", Tone::Function),
            ("Sprint(⟨count⟩", Tone::Parameter),
        ],
    );
}

/// JSON, which Helix gives no locals: keys, values and constants.
#[test]
fn json_tokens() {
    let text = "{\"name\": \"a\", \"n\": 1, \"ok\": [true, null]}\n";
    expect_tones(
        "package.json",
        text,
        &tree("json"),
        &[
            ("\"name\"", Tone::Property),
            (": ⟨\"a\"⟩", Tone::String),
            ("1", Tone::Constant),
            ("true", Tone::Constant),
            ("null", Tone::Constant),
            ("{", Tone::Punctuation),
        ],
    );
}

/// TOML, which Helix gives no locals: tables, keys and values.
#[test]
fn toml_tokens() {
    let text = indoc! {r#"
        [package]
        name = "a" # the name
        version = 1
    "#};
    expect_tones(
        "Cargo.toml",
        text,
        &tree("toml"),
        &[
            ("[⟨package⟩", Tone::Type),
            ("name", Tone::Property),
            ("\"a\"", Tone::String),
            ("# the name", Tone::Comment),
            ("version = ⟨1⟩", Tone::Constant),
        ],
    );
}

/// Bash: an expansion inside a string resets to plain, as Helix's `@none` asks.
#[test]
fn bash_tokens() {
    let text = indoc! {r#"
        #!/bin/bash
        name="$1"
        if [ -z "$name" ]; then echo "hi ${name}"; fi
        greet() { local who=1; return 0; }
    "#};
    expect_tones(
        "run.sh",
        text,
        &tree("bash"),
        &[
            ("#!/bin/bash", Tone::Comment),
            ("if", Tone::Keyword),
            ("-z", Tone::Operator),
            ("echo", Tone::Function),
            ("\"hi ", Tone::String),
            ("hi ⟨${⟩", Tone::Punctuation),
            ("${⟨name⟩", Tone::Plain),
            ("greet", Tone::Function),
            ("local", Tone::Keyword),
            ("return ⟨0⟩", Tone::Constant),
        ],
    );
}

/// Markdown and its inline grammar: headings, emphasis, strong text, code, links, lists and
/// quotes, and a fence in a tree-house-tier language.
#[test]
fn markdown_tokens() {
    let text = indoc! {"
        # Title

        Some *soft* and **loud** and `code` and [docs](http://x).

        - item
        > quoted

        ```rust
        fn f() {}
        ```
    "};
    expect_tones(
        "README.md",
        text,
        &tree("markdown"),
        &[
            ("# Title", Tone::Heading),
            ("Some", Tone::Plain),
            ("soft", Tone::Emphasis),
            ("loud", Tone::Strong),
            ("`⟨code⟩", Tone::Raw),
            ("[⟨docs⟩", Tone::Link),
            ("http://x", Tone::Link),
            ("- ", Tone::List),
            ("> ⟨quoted⟩", Tone::Quote),
            ("```⟨rust⟩", Tone::Special),
            (
                indoc! {"
                    ```rust
                    ⟨fn⟩"},
                Tone::Keyword,
            ),
            ("fn ⟨f⟩", Tone::Function),
        ],
    );
}

/// YAML: keys, scalars, comments, and a `run:` block scalar injected as Bash.
#[test]
fn yaml_tokens() {
    let text = indoc! {r#"
        # c
        key: value
        list:
          - 1
          - "s"
        run: |
          echo hi
    "#};
    expect_tones(
        "ci.yml",
        text,
        &tree("yaml"),
        &[
            ("# c", Tone::Comment),
            ("key", Tone::Property),
            ("key: ⟨value⟩", Tone::String),
            ("- ⟨1⟩", Tone::Constant),
            ("\"s\"", Tone::String),
            (
                indoc! {"
                    run: |
                      ⟨echo⟩"},
                Tone::Function,
            ),
        ],
    );
}

/// C, in a header, which is C by default.
#[test]
fn c_tokens() {
    let text = indoc! {"
        #include <stdio.h>
        #define LIMIT 1
        struct Point { int x; };
        int main(int argc, char **argv) {
          return argc + LIMIT; // c
        }
    "};
    expect_tones(
        "point.h",
        text,
        &tree("c"),
        &[
            ("#include", Tone::Keyword),
            ("<stdio.h>", Tone::String),
            ("#define ⟨LIMIT⟩", Tone::Constant),
            ("struct", Tone::Keyword),
            ("Point", Tone::Type),
            ("{ int ⟨x⟩", Tone::Property),
            ("main", Tone::Function),
            ("main(int ⟨argc⟩", Tone::Parameter),
            ("return ⟨argc⟩", Tone::Parameter),
            ("// c", Tone::Comment),
        ],
    );
}

/// A normal C++ file: namespaces, classes, templates and a parameter in the body.
#[test]
fn cpp_tokens() {
    let text = indoc! {"
        #include <vector>
        namespace n { class Box { public: int get(int p) const { return p; } }; }
        template <typename T> T same(T t) { return t; }
    "};
    expect_tones(
        "box.cpp",
        text,
        &tree("cpp"),
        &[
            ("#include", Tone::Keyword),
            ("namespace", Tone::Keyword),
            ("class ⟨Box⟩", Tone::Type),
            ("public", Tone::Keyword),
            ("get", Tone::Function),
            ("get(int ⟨p⟩", Tone::Parameter),
            ("{ return ⟨p⟩", Tone::Parameter),
            (") ⟨const⟩", Tone::Keyword),
            ("template", Tone::Keyword),
            ("typename", Tone::Keyword),
            ("same(T ⟨t⟩", Tone::Parameter),
        ],
    );
}

/// C++26 reflection, which the grammar at Helix's revision parses and Helix's query colors: the
/// `^^` operator and the splice brackets.
#[test]
fn cpp_reflection_is_highlighted() {
    let text = indoc! {"
        constexpr auto r = ^^int;
        using T = [: r :];
        int main() { return 0; }
    "};
    expect_tones(
        "reflect.cpp",
        text,
        &tree("cpp"),
        &[
            ("constexpr", Tone::Keyword),
            ("^^", Tone::Operator),
            ("^^⟨int⟩", Tone::Type),
            ("[:", Tone::Punctuation),
            ("main", Tone::Function),
            ("main() { ⟨return⟩", Tone::Keyword),
        ],
    );
}

/// CSS: selectors, properties and values.
#[test]
fn css_tokens() {
    let text = ".card { color: #fff; margin: 0 auto; }\n";
    expect_tones(
        "site.css",
        text,
        &tree("css"),
        &[
            (".⟨card⟩", Tone::Special),
            ("color", Tone::Property),
            ("#fff", Tone::Escape),
            ("margin: ⟨0⟩", Tone::Constant),
            ("auto", Tone::Constant),
        ],
    );
}

/// HTML, with JavaScript and CSS injected through tree-house-tier rows.
#[test]
fn html_tokens() {
    let text = indoc! {r#"
        <p class="x">Hi</p>
        <script>let a = 1;</script>
        <style>p { color: red; }</style>
    "#};
    expect_tones(
        "index.html",
        text,
        &tree("html"),
        &[
            ("<⟨p⟩", Tone::Tag),
            ("class", Tone::Attribute),
            ("\"x\"", Tone::String),
            (">⟨Hi⟩", Tone::Plain),
            ("<script>⟨let⟩", Tone::Keyword),
            ("color", Tone::Property),
        ],
    );
}

/// A patch: added and removed lines, hunk headers.
#[test]
fn diff_tokens() {
    let text = indoc! {"
        --- a/x
        +++ b/x
        @@ -1 +1 @@
        -old
        +new
    "};
    expect_tones(
        "fix.patch",
        text,
        &tree("diff"),
        &[
            ("--- a/x", Tone::Deleted),
            ("+++ b/x", Tone::Inserted),
            ("@@ -1 +1 @@", Tone::Attribute),
            ("-old", Tone::Deleted),
            ("+new", Tone::Inserted),
        ],
    );
}

/// A Dockerfile under a glob's name, with its shell commands injected as Bash.
#[test]
fn dockerfile_tokens() {
    let text = indoc! {r#"
        FROM alpine:3.20 AS base
        # c
        RUN apk add curl && echo "x"
    "#};
    expect_tones(
        "deploy/Dockerfile.production",
        text,
        &tree("dockerfile"),
        &[
            ("FROM", Tone::Keyword),
            ("AS", Tone::Keyword),
            ("# c", Tone::Comment),
            ("RUN", Tone::Keyword),
            ("RUN ⟨apk⟩", Tone::Function),
            ("&&", Tone::Operator),
            ("\"x\"", Tone::String),
        ],
    );
}

/// A Makefile: variables, targets and recipes.
#[test]
fn make_tokens() {
    let text = indoc! {"
        CC = gcc
        all: main.o
        \t$(CC) -o app main.o
    "};
    expect_tones(
        "Makefile",
        text,
        &tree("make"),
        &[
            ("CC", Tone::Constant),
            ("CC ⟨=⟩", Tone::Operator),
            ("all", Tone::Constant),
            ("$(", Tone::Punctuation),
        ],
    );
}

/// Lua: a parameter in the body.
#[test]
fn lua_tokens() {
    let text = indoc! {r#"
        local function inc(x) return x + 1 end -- c
        print("s")
    "#};
    expect_tones(
        "init.lua",
        text,
        &tree("lua"),
        &[
            ("local", Tone::Keyword),
            ("inc", Tone::Function),
            ("inc(⟨x⟩", Tone::Parameter),
            ("return ⟨x⟩", Tone::Parameter),
            ("1 ⟨end⟩", Tone::Keyword),
            ("-- c", Tone::Comment),
            ("\"s\"", Tone::String),
        ],
    );
}

/// Ruby: interpolation, symbols and a parameter in the body.
#[test]
fn ruby_tokens() {
    let text = indoc! {r##"
        class Greeter
          def greet(who)
            puts "#{who}"
            :done
          end
        end
    "##};
    expect_tones(
        "Rakefile",
        text,
        &tree("ruby"),
        &[
            ("class", Tone::Keyword),
            ("def", Tone::Keyword),
            ("greet", Tone::Function),
            ("greet(⟨who⟩", Tone::Parameter),
            ("puts", Tone::Function),
            ("#{⟨who⟩", Tone::Parameter),
            (":done", Tone::Constant),
        ],
    );
}

/// Java: a parameter in the body; the declaration itself has no pattern in Helix's query.
#[test]
fn java_tokens() {
    let text = indoc! {"
        public class Counter { int inc(int p) { return p + 1; } // c
        }
    "};
    expect_tones(
        "Counter.java",
        text,
        &tree("java"),
        &[
            ("public", Tone::Keyword),
            ("class ⟨Counter⟩", Tone::Type),
            ("inc", Tone::Function),
            ("int", Tone::Type),
            ("inc(int ⟨p⟩", Tone::Plain),
            ("return ⟨p⟩", Tone::Parameter),
            ("// c", Tone::Comment),
        ],
    );
}

/// PowerShell, class and enum declarations included: at the revision Helix pins, the grammar
/// has the `class` and `enum` keywords Helix's query colors.
#[test]
fn powershell_tokens() {
    let text = indoc! {"
        function Get-Item($p) { return $p }
        class Point { [int]$X }
        enum Color { Red }
        # c
    "};
    expect_tones(
        "build.ps1",
        text,
        &tree("powershell"),
        &[
            ("function", Tone::Keyword),
            ("Get-Item", Tone::Function),
            ("return", Tone::Keyword),
            ("class", Tone::Keyword),
            ("class ⟨Point⟩", Tone::Plain),
            ("[⟨int⟩", Tone::Type),
            ("enum", Tone::Keyword),
            ("enum ⟨Color⟩", Tone::Plain),
            ("# c", Tone::Comment),
        ],
    );
}

/// SQL starts in the bat tier through its registry row.
#[test]
fn sql_tokens() {
    let text = "SELECT a, 'x' FROM t WHERE b = 1; -- c\n";
    expect_tones(
        "query.sql",
        text,
        &bat("SQL"),
        &[
            ("SELECT", Tone::Keyword),
            ("SELECT ⟨a⟩", Tone::Plain),
            ("'x'", Tone::String),
            ("WHERE", Tone::Keyword),
            ("b = ⟨1⟩", Tone::Constant),
            ("-- c", Tone::Comment),
        ],
    );
}

/// Kotlin, which only bat knows, found by its extension.
#[test]
fn kotlin_tokens() {
    let text = indoc! {r#"
        fun main() {
          val s = "x" // c
        }
    "#};
    expect_tones(
        "Main.kt",
        text,
        &bat("Kotlin"),
        &[
            ("fun", Tone::Keyword),
            ("main", Tone::Function),
            ("val", Tone::Keyword),
            ("val ⟨s⟩", Tone::Plain),
            ("\"x\"", Tone::String),
            ("// c", Tone::Comment),
        ],
    );
}

/// A `CMakeLists.txt`, a Makefile alternative only bat knows, found by its whole file name.
#[test]
fn cmake_tokens() {
    let text = indoc! {"
        cmake_minimum_required(VERSION 3.10)
        project(App)
    "};
    expect_tones(
        "CMakeLists.txt",
        text,
        &bat("CMake"),
        &[
            ("cmake_minimum_required", Tone::Function),
            ("VERSION", Tone::Parameter),
            ("project", Tone::Function),
            ("(", Tone::Punctuation),
        ],
    );
}

/// Detection by precedence: a file name beats a glob and a glob an extension, in the registry
/// and then in bat, and only a path neither knows looks at a `#!` line, the registry's
/// interpreters before bat's first lines.
#[test]
fn detection_follows_its_precedence() {
    let syntax = |path: &str, text: &str| resolve(path, text).syntax;
    // A glob over an extension, at any depth
    assert_eq!(syntax("Dockerfile.production", ""), tree("dockerfile"));
    assert_eq!(syntax("ops/Dockerfile.json", "{}"), tree("dockerfile"));
    // File names over extensions
    assert_eq!(syntax("Cargo.lock", ""), tree("toml"));
    assert_eq!(syntax("web/yarn.lock", ""), tree("yaml"));
    assert_eq!(syntax("Berksfile.lock", ""), tree("ruby"));
    // `.h` is C, `.hpp` and `.H` C++: extensions are case-sensitive
    assert_eq!(syntax("src/point.h", ""), tree("c"));
    assert_eq!(syntax("src/point.hpp", ""), tree("cpp"));
    assert_eq!(syntax("src/point.H", ""), tree("cpp"));
    // The longest extension first
    assert_eq!(syntax("app.js.map", ""), tree("json"));
    assert_eq!(syntax("types.d.ts", ""), tree("typescript"));
    // An extension over a contradicting `#!` line, in either tier: a launcher's shell does not
    // make a Ruby or Perl script Bash
    assert_eq!(syntax("build.sh", "#!/usr/bin/env python3\n"), tree("bash"));
    assert_eq!(syntax("tools/notify.rb", "#!/bin/sh\n"), tree("ruby"));
    assert_eq!(syntax("tools/report.pl", "#!/bin/sh\n"), bat("Perl"));
    // `#!` lines, `env -S` and versions included
    assert_eq!(syntax("tools/run", "#!/bin/sh\n"), tree("bash"));
    assert_eq!(
        syntax("tools/serve", "#!/usr/bin/env -S deno run -A\n"),
        tree("typescript")
    );
    assert_eq!(
        syntax("tools/check", "#!/usr/bin/python3.12\n"),
        tree("python")
    );
    // bat's own detection for what no row covers: names, extensions, then first lines
    assert_eq!(syntax("Main.kt", ""), bat("Kotlin"));
    assert_eq!(syntax("tools/report", "#!/usr/bin/env perl\n"), bat("Perl"));
    // Plain text is no highlighting
    assert_eq!(syntax("notes.txt", "fn f() {}\n"), Syntax::Plain);
    assert_eq!(syntax("LICENSE", "MIT\n"), Syntax::Plain);
}

/// The same content under two names resolves, and highlights, as each name's language.
#[test]
fn identical_content_highlights_by_its_name() {
    let text = indoc! {"
        # heading
        echo hi
    "};
    let as_script = analyze("notes.sh", text);
    let as_markdown = analyze("notes.md", text);
    assert_eq!(tone_at(&as_script, 0, 0), Tone::Comment);
    assert_eq!(tone_at(&as_script, 1, 0), Tone::Function);
    assert_eq!(tone_at(&as_markdown, 0, 0), Tone::Heading);
    assert_eq!(tone_at(&as_markdown, 1, 0), Tone::Plain);
}

/// Offsets are bytes through multibyte text and CRLF line ends, which never get a tone.
#[test]
fn multibyte_text_and_crlf_keep_their_offsets() {
    // CRLF line ends, which indoc keeps.
    let text = indoc! {"
        fn a() {}\r
        // é ünï\r
        let s = \"ß\";\r
    "};
    let syntax = analyze("lib.rs", text);
    assert_eq!(syntax.lines.len(), 3);
    assert_eq!(
        syntax.line(1),
        &[(0.."// é ünï".len(), Tone::Comment)],
        "the comment ends before its \\r"
    );
    let string = "let s = ".len();
    assert_eq!(
        syntax
            .line(2)
            .iter()
            .find(|(range, _)| range.start == string),
        Some(&(string..string + "\"ß\"".len(), Tone::String))
    );
}

/// A Kotlin fence in Markdown goes to bat, whose tones, plain ones included, replace the
/// fence's raw-code tone over exactly its content.
#[test]
fn a_bat_fence_is_highlighted_by_bat() {
    let text = indoc! {r#"
        ```kotlin
        fun main() { val s = "x" }
        ```
    "#};
    expect_tones(
        "README.md",
        text,
        &tree("markdown"),
        &[
            ("```", Tone::Punctuation),
            ("fun", Tone::Keyword),
            ("fun ⟨main⟩", Tone::Function),
            ("main⟨()⟩", Tone::Plain),
            ("\"x\"", Tone::String),
        ],
    );
}

/// Two fences are two inputs to bat: a string left open in the first ends with it, and the
/// second starts fresh.
#[test]
fn independent_fences_do_not_leak_into_each_other() {
    let text = indoc! {r#"
        ```kotlin
        val a = "open
        ```

        ```kotlin
        val b = 1
        ```
    "#};
    expect_tones(
        "two.md",
        text,
        &tree("markdown"),
        &[
            ("\"open", Tone::String),
            (
                indoc! {"
                    open
                    ⟨```⟩"},
                Tone::Punctuation,
            ),
            ("⟨val⟩ b", Tone::Keyword),
            ("val ⟨b⟩", Tone::Plain),
            ("b = ⟨1⟩", Tone::Constant),
        ],
    );
}

/// An injection inside an injected host reaches bat: GraphQL in a Ruby heredoc in a Markdown
/// fence, and SQL through its bat-tier row.
#[test]
fn an_injected_host_bridges_to_bat() {
    let text = indoc! {"
        ```ruby
        q = <<~GQL
          query { user(id: 1) { name } }
        GQL
        s = <<~SQL
          SELECT 1
        SQL
        ```
    "};
    expect_tones(
        "doc.md",
        text,
        &tree("markdown"),
        &[
            ("<<~GQL", Tone::String),
            ("query", Tone::Keyword),
            ("query { ⟨user⟩", Tone::Plain),
            ("id: ⟨1⟩", Tone::Constant),
            ("SELECT", Tone::Keyword),
        ],
    );
}

/// The rows, with `name` moved to the bat tier as the bat syntax `syntax`.
fn demoted(name: &str, syntax: &'static str) -> Languages {
    let rows: Vec<Row> = registry::rows()
        .into_iter()
        .map(|mut row| {
            if row.name == name {
                row.engine = Engine::Bat { syntax };
            }
            row
        })
        .collect();
    Languages::new(rows)
}

/// Demoting a language keeps both its files and its fences highlighted: files by its row's
/// detection, now naming a bat syntax, and fences through the bridge.
#[test]
fn a_demoted_language_stays_highlighted_in_files_and_fences() {
    let fence = indoc! {r#"
        ```ruby
        def greet(who) = puts("hi")
        ```
    "#};
    let file = "def greet(who) = puts(\"hi\")\n";
    let demoted = demoted("ruby", "Ruby");
    for (languages, expected) in [
        (Languages::standard(), tree("ruby")),
        (&demoted, bat("Ruby")),
    ] {
        // Detection stays the row's, by extension and by file name.
        let resolved = resolve_in(languages, "lib/greeter.rb", file);
        assert_eq!(resolved.syntax, expected);
        assert_eq!(resolve_in(languages, "Gemfile", file).syntax, expected);
        // The file and the fence both highlight.
        let highlighted = highlight_in(languages, &resolved, file, far());
        assert_eq!(tone_at(&highlighted, 0, 0), Tone::Keyword, "{expected:?}");
        let markdown = resolve_in(languages, "README.md", fence);
        let highlighted = highlight_in(languages, &markdown, fence, far());
        let fenced = indoc! {"
            ```ruby
            ⟨def⟩"};
        for (marked, tone) in [(fenced, Tone::Keyword), ("\"hi\"", Tone::String)] {
            assert_eq!(
                tones_of(&highlighted, fence, marked),
                vec![tone],
                "{expected:?}: {marked}"
            );
        }
    }
}

/// A line up to the limit goes to syntect; one byte more makes the whole input plain, even
/// where an earlier line opened a string that a skip would have carried past it.
#[test]
fn bat_keeps_to_its_line_limit() {
    let near = format!("val s = \"{}\"\n", "x".repeat(bat::MAX_LINE_BYTES - 10));
    assert_eq!(near.trim_end().len(), bat::MAX_LINE_BYTES);
    let syntax = analyze("Long.kt", &near);
    assert_eq!(tone_at(&syntax, 0, 0), Tone::Keyword, "a line at the limit");

    let over = formatdoc! {r#"
        val a = 1
        val text = """{}
        val b = 2
    "#, "x".repeat(bat::MAX_LINE_BYTES)};
    let syntax = analyze("Over.kt", &over);
    assert!(
        syntax.lines.iter().all(Vec::is_empty),
        "the whole input stays plain"
    );

    // The same in a fence: that fence stays plain, and the next one does not.
    let fenced = formatdoc! {"
        ```kotlin
        {over}```

        ```kotlin
        val c = 3
        ```
    "};
    let syntax = analyze("doc.md", &fenced);
    assert_eq!(
        tone_at(&syntax, 1, 0),
        Tone::Plain,
        "the long fence is plain"
    );
    assert_eq!(tone_at(&syntax, 7, 0), Tone::Keyword, "the next fence");
}

/// Many fences, in both tiers, all highlight, each from its own start.
#[test]
fn many_fences_highlight() {
    let text: String = (0..150)
        .map(|index| {
            formatdoc! {r#"
                ```kotlin
                val k{index} = "a"
                ```

                ```rust
                let r{index} = 1;
                ```

            "#}
        })
        .collect();
    let resolved = resolve("many.md", &text);
    let syntax = highlight_in(Languages::standard(), &resolved, &text, far());
    for (number, line) in text.lines().enumerate() {
        if line.starts_with("val ") || line.starts_with("let ") {
            assert_eq!(tone_at(&syntax, number, 0), Tone::Keyword, "line {number}");
        }
    }
}

/// Malformed input never fails: what parses keeps its tones.
#[test]
fn malformed_input_still_highlights() {
    let broken = indoc! {r#"
        fn f( {
          let = "
        }}}
    "#};
    let syntax = analyze("broken.rs", broken);
    assert_eq!(tone_at(&syntax, 0, 0), Tone::Keyword);
    let fences = indoc! {r#"
        ```rust
        fn (
        ```kotlin
        """
        ~~~
        ```
    "#};
    let syntax = analyze("broken.md", fences);
    assert_eq!(syntax.lines.len(), fences.lines().count());
    let controls = "\u{1}fn\u{7f}\n";
    assert_eq!(analyze("odd.rs", controls).lines.len(), 1);
}

/// The scopes share the file's deadline: one that has passed leaves the breadcrumbs empty,
/// while the same file within its budget has them.
#[test]
fn a_spent_budget_leaves_the_breadcrumbs_empty() {
    let languages = Languages::standard();
    let text = indoc! {"
        impl Ledger {
            fn apply(&mut self) {
                self.n += 1;
            }
        }
    "};
    let resolved = resolve_in(languages, "lib.rs", text);
    let spent = highlight_in(languages, &resolved, text, Deadline::after(Duration::ZERO));
    assert!(spent.scopes.scopes().is_empty(), "{:?}", spent.scopes);
    let whole = highlight_in(languages, &resolved, text, far());
    assert_eq!(whole.scopes.scopes().len(), 2);
}

/// A deeply nested file, such as a notebook of 100,000 nested arrays, highlights on a worker
/// with a 2 MiB stack without overflowing it, scopes included.
#[test]
fn deep_nesting_highlights_without_overflowing_the_stack() {
    let depth = 100_000;
    let text = format!("{{\"cells\": {}{}}}", "[".repeat(depth), "]".repeat(depth));
    let syntax = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || analyze("deep.ipynb", &text))
        .unwrap()
        .join()
        .expect("highlighting returns instead of overflowing the stack");
    let labels: Vec<&str> = syntax
        .scopes
        .scopes()
        .iter()
        .map(|scope| scope.label.as_str())
        .collect();
    assert_eq!(labels, ["cells"]);
    assert_eq!(tone_at(&syntax, 0, 1), Tone::Property);
}

/// A deadline that has passed leaves every tier plain.
#[test]
fn a_spent_budget_leaves_the_text_plain() {
    let spent = Deadline::after(Duration::ZERO);
    let languages = Languages::standard();
    for (path, text) in [
        ("lib.rs", "fn f() {}\n"),
        ("Main.kt", "fun main() {}\n"),
        (
            "README.md",
            indoc! {"
                # Title
                ```kotlin
                fun f() {}
                ```
            "},
        ),
    ] {
        let resolved = resolve_in(languages, path, text);
        let syntax = highlight_in(languages, &resolved, text, spent);
        assert!(
            syntax.lines.iter().all(Vec::is_empty),
            "{path} stays plain: {:?}",
            syntax.lines
        );
    }
}

/// A near-8 MiB file, the most the engine reads as text, stays within the budget; run in
/// release with `--run-ignored only`.
#[test]
#[ignore = "release only: a debug build says nothing about the budget"]
fn a_near_limit_file_keeps_to_the_budget() {
    let unit = indoc! {r#"
        /// Applies one entry.
        pub fn apply(ledger: &mut Ledger, entry: Entry) -> Result<(), Error> {
            let amount = entry.amount.checked_mul(100).ok_or(Error::Overflow)?;
            ledger.total += amount; // "text" 'c' 42
            Ok(())
        }
    "#};
    let limit = usize::try_from(review_core::git::MAX_TEXT_BLOB_BYTES).unwrap();
    let text = unit.repeat((limit - 1024) / unit.len());
    let started = std::time::Instant::now();
    let syntax = analyze("big.rs", &text);
    let elapsed = started.elapsed();
    eprintln!(
        "{} bytes in {elapsed:?}, {} lines highlighted",
        text.len(),
        syntax.lines.iter().filter(|line| !line.is_empty()).count()
    );
    assert!(
        elapsed < BUDGET + Duration::from_secs(1),
        "took {elapsed:?}"
    );
}

/// The repository's `THIRD-PARTY-SYNTAXES`, which release archives ship next to `cr`.
const THIRD_PARTY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../THIRD-PARTY-SYNTAXES");

/// `THIRD-PARTY-SYNTAXES` as review-languages' licenses and two-face's acknowledgements make it:
/// where Helix's queries come from and their license, then the notices of the grammars
/// review-languages compiles, then those of the bat syntaxes `cr` embeds.
fn third_party_syntaxes() -> String {
    let mut text = formatdoc! {"
        Third-party syntax definitions in cr

        cr highlights a curated set of languages with tree-sitter grammars and the highlighting
        queries of Helix (https://github.com/helix-editor/helix), which are subject to the terms
        of the Mozilla Public License, v. 2.0 (https://mozilla.org/MPL/2.0/). cr embeds them
        unmodified from runtime/queries/ in Helix's repository at commit
        {helix}, which is their source. Their license follows.
    ",
        helix = review_languages::HELIX,
    };
    let acknowledge = |text: &mut String, license: &review_languages::License| {
        let _ = writedoc! {text, "

            {name}: {git} at {rev} ({spdx})

            {body}
        ",
            name = license.name,
            git = license.git,
            rev = license.rev,
            spdx = license.spdx,
            body = license.text.trim_end(),
        };
    };
    acknowledge(&mut text, &review_languages::QUERIES_LICENSE);
    let _ = writedoc! {text, "

        cr compiles these grammars from the revisions Helix pins, apart from jinja's, which is
        cr's fork of cathaysia/tree-sitter-jinja without Jinja's optional line syntax, under
        upstream's license. cr highlights Jinja with queries of its own, derived from the jinja
        grammar's and under its license, the Apache License, Version 2.0. Their licenses follow,
        each under the grammar's repository and revision.
    "};
    for license in review_languages::LICENSES {
        acknowledge(&mut text, license);
    }
    let _ = writedoc! {text, "

        Every other file is highlighted with the syntax definitions the bat project curates
        (https://github.com/sharkdp/bat), as two-face bundles them. Their licenses follow, each
        under the path of its syntax in bat's repository.
    "};
    for license in two_face::acknowledgement::listing().for_syntaxes() {
        let _ = writedoc! {text, "

            {path}

            {body}
        ",
            path = license.rel_path.display(),
            body = license.text.trim_end(),
        };
    }
    text
}

/// The committed `THIRD-PARTY-SYNTAXES` matches the Helix queries and grammars
/// review-languages embeds and compiles and the bat syntaxes two-face embeds; run
/// `task third-party` to write it again after any of them changes.
///
/// The queries' and grammars' licenses are compiled into review-languages, so whenever this
/// test builds it has all of them.
#[test]
fn third_party_syntaxes_are_acknowledged() {
    let expected = third_party_syntaxes();
    // Written only when asked, from the task.
    if std::env::var_os("REVIEW_WRITE_THIRD_PARTY").is_some() {
        std::fs::write(THIRD_PARTY, &expected).unwrap();
    }
    let committed = std::fs::read_to_string(THIRD_PARTY).unwrap_or_default();
    assert!(
        committed == expected,
        "THIRD-PARTY-SYNTAXES is stale; run `task third-party`"
    );
}

/// The parse of `text` as the tree-house-tier row `name`, with all the time it needs.
fn parsed(name: &str, text: &str) -> tree_house::Syntax {
    let languages = Languages::standard();
    let index = languages.by_name(name).unwrap();
    tree_house::Syntax::new(
        ropey::RopeSlice::from(text),
        tree_house::Language::new(u32::try_from(index).unwrap()),
        Duration::from_secs(600),
        languages,
    )
    .unwrap()
}

/// A highlighter cancelled before it starts highlights nothing, and says it stopped.
#[test]
fn a_spent_highlighter_budget_leaves_the_file_plain() {
    let text = "fn apply(x: u32) -> u32 { x + 1 } // done\n";
    let syntax = parsed("rust", text);
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let budget = tree_house::Budget::new(Duration::from_secs(600), Some(&cancel));
    let source = ropey::RopeSlice::from(text);
    let host = tree::host_runs(&syntax, source, Languages::standard(), &budget);
    assert!(host.halted);
    assert_eq!(host.runs, Vec::new());
}

/// A highlighter whose budget runs out part of the way through keeps exactly what it had
/// finished: a prefix of the full highlights, the last run perhaps cut short, and nothing
/// after it.
#[test]
fn a_highlighter_out_of_budget_keeps_only_what_it_finished() {
    let unit = indoc! {r#"
        /// Applies one entry.
        pub fn apply(ledger: &mut Ledger, entry: Entry) -> Result<(), Error> {
            let amount = entry.amount.checked_mul(100).ok_or(Error::Overflow)?;
            ledger.total += amount; // "text" 'c' 42
            Ok(())
        }
    "#};
    let text = unit.repeat(3000);
    let syntax = parsed("rust", &text);
    let source = ropey::RopeSlice::from(text.as_str());
    let languages = Languages::standard();
    // The time the whole highlight takes, of which a quarter cannot be enough.
    let started = std::time::Instant::now();
    let full = tree::host_runs(
        &syntax,
        source,
        languages,
        &tree_house::Budget::new(far().remaining(), None),
    );
    let whole = started.elapsed();
    assert!(!full.halted);
    let budget = tree_house::Budget::new(whole / 4, None);
    let partial = tree::host_runs(&syntax, source, languages, &budget);
    assert!(partial.halted, "a quarter of {whole:?} is not enough");
    assert!(partial.runs.len() < full.runs.len());
    let Some((last, finished)) = partial.runs.split_last() else {
        return;
    };
    assert_eq!(finished, &full.runs[..finished.len()]);
    let cut = &full.runs[finished.len()];
    assert_eq!((last.range.start, last.tone), (cut.range.start, cut.tone));
    assert!(last.range.end <= cut.range.end);
}
