# 3. tree-house highlights a curated set of languages, and bat's syntaxes the rest

**Status:** accepted · **Date:** 2026-11-13 · **Supersedes:** highlighting only review-core's
formatting-proof languages, with Zed's queries on a plain tree-sitter query cursor

## Context

The apps highlighted only the eight languages of the formatting proof (Rust, Python, JavaScript,
TypeScript, TSX, Go, JSON and TOML), with Zed's `highlights.scm` run on a plain `QueryCursor`.
Shell scripts, Markdown, YAML, Dockerfiles, Makefiles and every other file showed plain, which is
not acceptable for a tool people read most of their diffs in.

The constraints were the release binary's size and build time, and the graph's one tree-sitter
runtime, which review-core's proof parses with.
The options considered were:

- **A tree-sitter grammar for every language**, such as all of the arborium crates.
  It is uniform, but about 120 MB of dense parse tables and 800 MB of generated C for 104
  grammars, compiled for six release targets, mostly for languages few reviews contain.
- **bat's syntaxes for everything**, through syntect.
  About 1 MB of data covers nearly every format, but TextMate grammars highlight worse than
  tree-sitter queries with locals and injections.
- **A curated tree-sitter tier and bat for the rest**, tunable language by language.
- **tree-sitter-highlight** as the tree-house tier's engine.
  It cannot hand an injection it has no grammar for to another engine, and Helix's tree-house
  highlights better (locals, combined injections, precedence across layers).
- **luup2's engine**: each language a sandboxed WASM module running tree-house, Neovim queries or
  nothing under wasmtime.
  It loads languages lazily and safely, but wasmtime is far heavier than this app needs today.

## Decision

Highlighting has two tiers, chosen per language in one manifest,
`crates/review-languages/languages.toml`, the registry's one source of truth, and each named for
the project whose work it uses: `tree-house` and `bat`.

- **The tree-house tier** is parsed by tree-house with each language's tree-sitter grammar and
  highlighted with Helix's queries, injections and locals included.
  It starts with 29 languages: review-core's eight, then Bash, Markdown and its inline grammar,
  YAML, C, C++, CSS, HTML, diff, Dockerfile, Make, Lua, Ruby, Java and PowerShell, and the
  template languages Go templates, Helm, Jinja, ERB, EJS and Twig.
- **The bat tier** is syntect with bat's syntaxes, through two-face on the pure-Rust fancy-regex
  engine, for everything else.
  PowerShell is in the tree-house tier because fancy-regex cannot run bat's PowerShell syntax.
  SQL starts in the bat tier: its grammar, tree-sitter-sequel, is 47 MB of C with about 10 MB of
  parse tables, too much for every `cr` until a comparison shows it highlights SQL better.
- **A row** of the manifest holds a language's Helix name (`markdown.inline`), its tier, its
  grammar (a proof language's, or one fetched at a pinned commit), its queries directory, its
  aliases and injection regex, its detection (extensions, file names, globs, interpreters, from
  Helix's `languages.toml`), the bat syntax that highlights it in the bat tier, for a template
  language the `data` nodes that hold its literal text, `local-queries` when its queries are
  cr's own, and `locked`, which keeps `task grammars:sync` from moving the row or some of its
  fields.
  Rows that pin the same grammar alike share it, as Go templates and Helm or ERB and EJS do, and
  it is compiled once.
  review-languages' build script checks the rows (duplicate names, a tree-house row without a
  grammar or queries, a bat row without a syntax, globs the matcher cannot match, regexes that do
  not compile) and generates a data-only table of them, which review-ui maps onto queries and
  review-core's grammar crates.
  Tuning a language is editing its row.
  Moving a language between tiers changes its `tier`, nothing else: a demoted language keeps its
  detection and aliases, and resolves to the bat syntax its row names, never to what bat would
  detect from the path; its fetched grammar stays pinned but is no longer compiled.
  The proof languages' grammars stay linked whatever their tier, since review-core needs them,
  and their dependencies never move.
  JavaScript (and `.jsx`, which Helix gives a language of its own) takes TSX's queries, and `.h`
  is C, so those rows lock their extensions.
- **Injections cross the tiers.**
  tree-house returns the injections it could not resolve, in host coordinates, through
  `Syntax::unresolved_injections` of the tree-house fork; each goes to bat as one input, its
  ranges joined, and bat's tones, plain ones included, replace the host's over exactly those
  ranges.
  A Kotlin fence in Markdown is highlighted, and a language demoted to bat stays highlighted in
  fences.
- **Templates take the language of the file they produce.**
  A template's own syntax is highlighted by its grammar, and its literal text, its data, as the
  language of the file it produces, in either tier.
  - The data's language comes from the path without the template's extension
    (`helmfile.yaml.gotmpl` is YAML), or the path itself for Helm's globs
    (`templates/deployment.yaml`), by the ordinary detection apart from template rows; then from
    chezmoi's source names, whose kind decides first (a `symlink_` file is plain, a `modify_`
    script is what its `#!` line runs, or its target's language only as a modify template, a
    `run_` script is what its `#!` line runs before what its name says), then from the target
    name (`dot_gitconfig.tmpl` is bat's Git Config); then from the first line.
    bat's plain text is plain data, as Helm's `NOTES.txt` is.
    A template whose data has no language is its plain row, whose own injections give the data
    Helix's default, HTML for most and YAML for Helm.
  - A template's last extension decides ahead of every file name and glob: `Dockerfile.j2` is
    Jinja producing a Dockerfile.
  - The ordinary fallback to chezmoi's target name also serves files whose path resolves to
    nothing, such as `dot_zshrc`.
  - A template file with data highlights as a variant of its row: a tree-house language of its
    own, built on first use, whose injections are the row's plus one pattern over the row's
    `data` nodes, unnamed children included, naming the data language, which wins over the
    row's own pattern for the same nodes.
    Data in a bat syntax is named `review-bat:<syntax>`, which no row resolves and the bridge to
    bat maps back, and plain data `review-plain`, which nothing resolves.
    The variants are bounded, one slot per template row and data language, and never move once
    built.
  - The resolved syntax, data included, is part of a highlight's identity, so one blob under
    `a.yaml.j2` and `a.json.j2` highlights as each, and templates have no breadcrumb scopes,
    since template syntax breaks the produced language's parse.
  - Jinja takes cr's fork of cathaysia/tree-sitter-jinja (romnn/tree-sitter-jinja), the grammar
    nvim-treesitter uses, rather than Helix's grammar, whose text is anonymous, and queries of
    cr's own derived from that grammar's (Apache-2.0), with Helix's capture names, only the
    `elif` keyword of an `elif` branch captured, raw blocks' bodies left as data, and an HTML
    default for its text.
    The fork is cathaysia's v0.13.1 without Jinja's line statements (`# if`) and line comments
    (`##`), which Jinja enables only when configured to and an editor cannot know of, so such
    lines are text: `## Heading` in `README.md.j2` is Markdown, and `## {{ title }}` holds a real
    expression.
    Rejected: treating the line syntax's nodes as data, which leaves the expressions it swallows
    opaque and a line-form `# raw` turning expressions into raw text.
  - Rejected: a loader per highlighted document that resolves a sentinel marker to the data
    language, which every injected template would share; tera, whose data cannot be told from
    its YAML front matter, and Handlebars and Liquid, for now.
- **Detection** is per file version, from its own path and text, by precedence: the path first,
  in both tiers (the registry's exact file names, globs and extensions, `.h` being C, then bat's
  file names and extensions), and only then the first line (the registry's `#!` interpreters,
  `env -S` and versions read as Helix reads them, then bat's first-line rules), then plain.
  A Perl script with a launcher's `#!/bin/sh` stays Perl.
  A version is read before its syntax is resolved, so any text blob under the 8 MiB limit is
  highlighted.
- **Identity:** highlights belong to a blob and the syntax it resolved to, not to the blob alone,
  so the same blob under two names, or renamed across languages, highlights as each name.
  Text stays cached by blob.
- **Budget:** one deadline per highlighted blob, two seconds (`syntax::BUDGET`), covers the
  breadcrumbs' scope parse, parsing, injections, queries and bat; what is left when it passes
  stays plain.
  The scopes come first, all or nothing and usually quick, within at most half the budget, and
  breadcrumbs stay empty when they do not finish; the highlighting, which keeps whatever it
  finishes, takes the rest, at least half.
  review-core's scope walk keeps its own stack, so a deeply nested file cannot overflow a
  worker's.
  tree-house enforces it for its parse, injections and locals, and for its highlighter's queries
  through a `tree_house::Budget` of the time left; syntect checks it between lines.
  syntect cannot stop inside a line, so an input with a line over 16 KiB stays plain, never
  skipped and resumed with the wrong state.
  Files are highlighted one at a time, each landing on its own, so one expensive file holds up
  only itself, and work for a revision no longer shown stops after its current file.
- **One tree-sitter runtime**, from the romnn/tree-sitter fork (v0.27.2 plus three commits),
  patched in for both `tree-sitter` and `tree-sitter-language` in the workspace's
  `[patch.crates-io]`, so review-core, tree-house and every grammar share one runtime and one
  `LanguageFn`.
  It is a fork because it carries the parser's explicit-frame stack fix, which stops ambiguous
  grammars from overflowing the native stack, `highlight_with_included_ranges`, and the query
  extension tree-house calls.
  tree-house's own fork (romnn/tree-house) compiles no C and links this runtime.
  Disabling a second runtime's build with `DISABLED_TS_BUILD` was rejected: it lives in
  `.cargo/config.toml`, which `cargo install --git` ignores.
  A test in review-ui (`tests/one_runtime.rs`) checks the locked graph: one `tree-sitter` and one
  `tree-sitter-language` by package ID, both from the fork, no other package with
  `links = "tree-sitter"`, and grammar crates that depend only on `tree-sitter-language`.
- **Grammar sources:** review-core's grammar crates from crates.io serve its eight languages, and
  every other tree-house-tier grammar is fetched at the exact revision Helix's `languages.toml`
  pins, a commit or an annotated tag of one, and compiled by review-languages' build script, one
  grammar per language in the binary; Jinja's, which is not Helix's, is cr's fork, pinned at a
  commit of its own, and its row is locked.
  The product owner chose fetching so that grammars and queries stay exactly as Helix ships them
  together.
  A fetched grammar's row lists its repository, commit, subdirectory, source files and
  license, and `task grammars:sync -- <commit>`, a small script run with `uv`, moves the
  manifest's Helix commit, and with it the queries, and every unlocked row's pin, detection and
  injection regex to another Helix commit, keeping the manifest's comments with tomlkit, and
  reports where review-core's crates drift from Helix's pins.
  - Sources come from `$CODE_REVIEW_GRAMMARS_DIR/<name>` (`helix-queries` for the queries)
    when it exists, else from the gitignored cache `crates/review-languages/_cache/`, else from
    a shallow fetch of the one pinned commit with `git` into that cache.
    The files are read from git's objects, never checked out, so no filter, hook or line-ending
    setting changes them; `cc` compiles them, YAML's C++ scanner apart.
  - Every entry, prepared or cached, carries a descriptor naming its repository, commit,
    subdirectory and files with their sizes and git blob ids, checked against the manifest on
    every use, each file's content hashed as git hashes a blob, so a cache restored with a
    changed file is caught even when the size matches.
    A cached entry that fails the check is fetched again into a new entry, a prepared one is an
    error, and a published entry is never changed, since compilers may be reading it.
  - A fetch clears the caller's git repository variables, which a git hook running cargo sets,
    runs no hooks, holds an advisory lock per grammar and publishes the entry with one rename
    once it is checked.
    A read-only checkout reads its cache without writing to it, and fetches into `OUT_DIR`.
  - The costs: a build without the cache needs network, or `task grammars:fetch` beforehand;
    `cargo install --git` fetches too; CI keeps the cache with actions/cache keyed by the
    manifest, and releases run `task grammars:fetch` before goreleaser; YAML's C++ scanner
    links the C++ standard library on every target.
  - The mitigations: `CARGO_NET_OFFLINE=true` stops at once instead of fetching, every missing
    source error names `task grammars:fetch` and `CODE_REVIEW_GRAMMARS_DIR`, the cache outlives
    `cargo clean`, and `task grammars:fetch -- --into <dir>` prepares sources for fully offline
    builds, such as a distribution's.
    Prepared sources are published whole, with one rename, and never changed afterwards, since
    builds may read them: a run again keeps what matches and refuses a directory of other
    sources.
  - Rejected: crates.io grammar crates with a test of their drift from Helix's pins, which leaves
    highlighting to whichever version each crate last published, as the Dockerfile crate still
    on tree-sitter 0.20 and the C++ and PowerShell crates that needed adapted queries showed;
    and Cargo git dependencies on the grammar repositories, which clone each repository's whole
    history, over 700 MB for 13 of them (ruby 317 MB, typescript 146 MB, cpp 100 MB).
- **Queries** are Helix's at the commit `languages.toml` names (a714a818), fetched by the same
  machinery as the grammars, into an entry of the same cache with its own descriptor, and
  embedded unmodified, apart from the rows with `local-queries`, Jinja's alone, whose files are
  cr's and live beside the manifest under `crates/review-languages/queries/`.
  The fetch takes `runtime/queries/<dir>/` `highlights.scm`, `injections.scm` and `locals.scm`
  for every directory a tree-house row names, where Helix has the file, and the same of every
  directory their `; inherits:` lines name, and Helix's license.
  review-languages' build script generates the table of them, `QUERIES`, with an `include_str!`
  of each cached file, and tree-house's `read_query` resolves `; inherits:` from it.
  The manifest's commit and `THIRD-PARTY-SYNTAXES` record where they come from.
  JavaScript takes TSX's queries, since review-core parses it with the TSX grammar.
  Tests compile every row's queries through `LanguageConfig::new`, check that every row's
  queries are embedded, and resolve every `; inherits:` line.
- **Licenses:** Helix's queries are MPL-2.0, which the GPL-3.0-or-later review-ui may use.
  `THIRD-PARTY-SYNTAXES` names their source, Helix's repository at the manifest's commit, and
  holds the MPL-2.0's text, then collects the fetched grammars' licenses, all of which
  review-languages compiles in from their entries, and bat's syntaxes' notices from two-face's
  acknowledgements; a test keeps it current, and release archives ship it.
  cr's Jinja queries carry a header naming the files they derive from and their Apache-2.0,
  whose text `THIRD-PARTY-SYNTAXES` holds with the Jinja grammar's.

## Consequences

- Every common file type is highlighted out of the box, and fenced code in any language bat
  knows.
- Helm charts, helmfiles, Ansible and chezmoi templates, ERB, EJS and Twig views highlight both
  their template syntax and the file they produce.
- Tuning is one manifest row per language; a tree-house-tier language is a grammar pinned in its
  row, Helix's queries at the manifest's commit and exact-token tests.
- Following Helix is one `task grammars:sync` and the token fixtures it changes.
- `cr` grows by the grammars of the new tree-house-tier languages and bat's syntax set; release
  targets compile about 25 grammars' C, and the first build of a checkout fetches 19 of them and
  Helix's queries.
- Tokens take the classes Helix's queries give them rather than Zed's, so some change tone.
- review-ui no longer contains code copied from Zed; it stays GPL-3.0-or-later as the shared model
  of GPL apps.
- A third tier, luup2's sandboxed WASM languages loaded on demand, could later cover languages
  neither tier serves well without growing `cr`.

## References

- `crates/review-ui/src/syntax/` (`registry`, `languages`, `tree`, `bat`, `budget`, `chezmoi`)
- `crates/review-ui/src/model/mod.rs` (`Blobs`, `request_highlights`, `highlight_sides`)
- `crates/review-ui/tests/one_runtime.rs`
- `crates/review-languages/` (`languages.toml`, `queries/`, `build.rs`),
  `crates/review-grammar-sources/`, `scripts/grammars/sync.py`
- `Cargo.toml` (`[patch.crates-io]`), `THIRD-PARTY-SYNTAXES`, `.goreleaser.yaml`
- PLAN.md, "One tree-sitter", "Syntax highlighting" and "License"
