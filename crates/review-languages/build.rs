//! Reads `languages.toml`, finds the pinned sources of every tree-house-tier grammar it fetches
//! and of Helix's queries, compiles each grammar with `cc`, and generates `languages.rs`: one
//! `LanguageFn` constant per compiled grammar, the `LANGUAGES` table of every row, the
//! grammars' licenses, the Helix commit, and the `QUERIES` table of Helix's query files, with
//! their license, and of the local ones under `queries/`.
//!
//! Sources come from `$CODE_REVIEW_GRAMMARS_DIR/<name>` when it exists, a relative
//! `CODE_REVIEW_GRAMMARS_DIR` naming a directory under this package's, else from the cache
//! under `_cache/`, else from a fetch with `git` into that cache, or into one under `OUT_DIR`
//! when the checkout is read-only; see review-grammar-sources, which also checks the manifest.
//! `CARGO_NET_OFFLINE=true` forbids the fetch; `cargo --offline` sets nothing a build script
//! sees.
//! Every object and archive goes under `OUT_DIR`, and the script works there, so no compiler
//! probe leaves a file in the source tree.
//!
//! The module is built with `quote`, parsed with `syn`, so it is valid Rust before rustc sees
//! it, and formatted with `prettyplease`, so it reads well when a build is debugged.

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use indoc::formatdoc;
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use review_grammar_sources::{Entry, Grammar, Language, Manifest, Queries, Sources, Tier};

/// Why the grammars could not be built.
#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Sources(#[from] review_grammar_sources::Error),
    #[error("failed to compile grammar {name}")]
    Compile {
        name: String,
        #[source]
        source: cc::Error,
    },
    #[error("a thread finding sources or compiling a grammar panicked")]
    Panicked,
    #[error("the local queries {path} hold no query file")]
    LocalQueries { path: PathBuf },
    #[error("the query directory {0} is both local and Helix's")]
    DuplicateQueries(String),
    #[error("the generated languages.rs is not valid Rust")]
    Generate(#[source] syn::Error),
    #[error("failed to write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cargo did not set {0}")]
    Unset(&'static str),
    #[error("failed to work in {path}")]
    Directory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn main() -> ExitCode {
    println!("cargo:rerun-if-env-changed=CODE_REVIEW_GRAMMARS_DIR");
    println!("cargo:rerun-if-env-changed=CARGO_NET_OFFLINE");
    match build() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let mut message = err.to_string();
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                // Writing to a `String` cannot fail.
                let _ = write!(message, ": {cause}");
                source = cause.source();
            }
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn build() -> Result<(), Error> {
    let crate_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or(Error::Unset("CARGO_MANIFEST_DIR"))?,
    );
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or(Error::Unset("OUT_DIR"))?);
    // The paths cargo watches are absolute, since the script leaves the package's directory.
    let manifest_path = crate_dir.join("languages.toml");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!(
        "cargo:rerun-if-changed={}",
        crate_dir.join("build.rs").display()
    );
    // Resolved before the script leaves the package's directory, against which a relative
    // value is meant.
    let prepared = std::env::var_os("CODE_REVIEW_GRAMMARS_DIR")
        .map(|value| review_grammar_sources::prepared_dir(&value, &crate_dir));
    if let Some(root) = &prepared {
        // A grammar's prepared sources may appear in it later, under the same variable.
        println!("cargo:rerun-if-changed={}", root.display());
    }
    // A compiler probe may write its output to the working directory, as zig's `a.o`.
    std::env::set_current_dir(&out_dir).map_err(|source| Error::Directory {
        path: out_dir.clone(),
        source,
    })?;
    let manifest = Manifest::read(&manifest_path)?;
    let queries = manifest.queries();
    let sources = Sources {
        cache: crate_dir.join("_cache"),
        fallback: Some(out_dir.join("cache")),
        prepared,
        offline: review_grammar_sources::offline(
            std::env::var("CARGO_NET_OFFLINE").ok().as_deref(),
        ),
    };
    // The grammars are independent, and fetching one or compiling a big one such as C++ takes a
    // while, so each is found and compiled on a thread of its own, and the queries are found on
    // another.
    let (located, queries) = std::thread::scope(|scope| {
        let sources = &sources;
        let queries = &queries;
        let queries = scope.spawn(move || sources.locate(queries));
        let builds: Vec<_> = manifest
            .grammars()
            .map(|grammar| {
                let out_dir = &out_dir;
                scope.spawn(move || {
                    let entry = sources.locate(grammar)?;
                    compile(grammar, &entry.directory, &out_dir.join(&grammar.name))?;
                    Ok(Located { grammar, entry })
                })
            })
            .collect();
        let located = builds
            .into_iter()
            .map(|build| build.join().map_err(|_| Error::Panicked)?)
            .collect::<Result<Vec<Located<'_>>, Error>>();
        let queries = queries.join().map_err(|_| Error::Panicked)?;
        Ok::<_, Error>((located?, queries?))
    })?;
    // Exactly the files compiled, included and acknowledged, never the whole cache, whose locks
    // and staging change while other builds fetch.
    let entries = located.iter().map(|Located { entry, .. }| entry);
    for entry in entries.chain([&queries]) {
        for file in &entry.files {
            println!("cargo:rerun-if-changed={}", file.display());
        }
    }
    // The local queries' directory, which also catches a query file added to it.
    let local = crate_dir.join("queries");
    println!("cargo:rerun-if-changed={}", local.display());
    let mut query_files = query_files(&queries);
    let local_files = local_query_files(&local, &manifest.local_queries())?;
    if let Some(file) = local_files.iter().find(|file| {
        query_files
            .iter()
            .any(|helix| helix.directory == file.directory)
    }) {
        return Err(Error::DuplicateQueries(file.directory.clone()));
    }
    query_files.extend(local_files);
    let generated = out_dir.join("languages.rs");
    let module = module(&manifest, &located, &queries, query_files)?;
    std::fs::write(&generated, module).map_err(|source| Error::Write {
        path: generated,
        source,
    })
}

/// A grammar and its sources.
struct Located<'a> {
    grammar: &'a Grammar,
    entry: Entry,
}

/// Compiles `grammar`'s sources in `directory` into static libraries under `out_dir`: its C,
/// and its C++ scanner apart, which links the C++ standard library.
///
/// Each grammar includes the `tree_sitter/parser.h` its parser was generated with, so each
/// compiles with its own `src/` as the include path.
fn compile(grammar: &Grammar, directory: &Path, out_dir: &Path) -> Result<(), Error> {
    let src = directory.join("src");
    let (cpp, c): (Vec<&String>, Vec<&String>) = grammar.sources.iter().partition(|file| {
        Path::new(file).extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cc") || extension.eq_ignore_ascii_case("cpp")
        })
    });
    for (files, is_cpp) in [(c, false), (cpp, true)] {
        if files.is_empty() {
            continue;
        }
        let mut build = cc::Build::new();
        build
            .cpp(is_cpp)
            .include(&src)
            .out_dir(out_dir)
            .files(files.iter().map(|file| src.join(file)))
            // The generated code is not ours to fix, so its warnings stay quiet.
            .warnings(false)
            .cargo_warnings(false);
        if !is_cpp {
            build.std("c11");
        }
        let suffix = if is_cpp { "-scanner" } else { "" };
        build
            .try_compile(&format!("tree-sitter-{}{suffix}", grammar.name))
            .map_err(|source| Error::Compile {
                name: grammar.name.clone(),
                source,
            })?;
    }
    Ok(())
}

/// The generated module: the grammars' C entry points, a `LanguageFn` constant for each, the
/// table of every language, the grammars' licenses, the Helix commit, the table of
/// `query_files`, and the license of Helix's queries in `queries`, their entry.
fn module(
    manifest: &Manifest,
    located: &[Located<'_>],
    queries: &Entry,
    query_files: Vec<QueryFile>,
) -> Result<String, Error> {
    let entry_points: Vec<Ident> = located
        .iter()
        .map(|Located { grammar, .. }| format_ident!("tree_sitter_{}", grammar.name))
        .collect();
    let constants =
        located
            .iter()
            .zip(&entry_points)
            .map(|(Located { grammar, .. }, entry_point)| {
                let doc = format!(
                    " The `{}` grammar, from <{}> at `{}`.",
                    grammar.name, grammar.git, grammar.rev
                );
                let constant = constant(grammar);
                quote! {
                    #[doc = #doc]
                    #[expect(
                        unsafe_code,
                        reason = "the linked parser defines it and returns its static TSLanguage"
                    )]
                    pub const #constant: LanguageFn = unsafe { LanguageFn::from_raw(#entry_point) };
                }
            });
    let specs = manifest.languages.iter().map(spec);
    let helix = &manifest.helix;
    let licenses = located.iter().map(|Located { grammar, entry }| {
        let Grammar {
            name,
            git,
            rev,
            license,
            ..
        } = grammar;
        let text = path_literal(&entry.directory.join("LICENSE"));
        quote! {
            License {
                name: #name,
                git: #git,
                rev: #rev,
                spdx: #license,
                text: include_str!(#text),
            }
        }
    });
    let queries_name = Queries::NAME;
    let queries_git = review_grammar_sources::HELIX_GIT;
    let queries_spdx = Queries::SPDX;
    let queries_license = path_literal(&queries.directory.join("LICENSE"));
    let query_files = query_files.into_iter().map(
        |QueryFile {
             directory,
             kind,
             path,
         }| {
            let text = path_literal(&path);
            quote! {
                Query {
                    directory: #directory,
                    kind: QueryKind::#kind,
                    text: include_str!(#text),
                }
            }
        },
    );
    let tokens = quote! {
        #[expect(
            unsafe_code,
            reason = "declares the C entry points of the grammars build.rs compiles"
        )]
        unsafe extern "C" {
            #(fn #entry_points() -> *const ();)*
        }

        #(#constants)*

        #[doc = " Every language of `languages.toml`, in its order."]
        pub const LANGUAGES: &[LanguageSpec] = &[#(#specs),*];

        #[doc = " The Helix commit `languages.toml` was last synced to, whose queries [`QUERIES`]"]
        #[doc = " embeds."]
        pub const HELIX: &str = #helix;

        #[doc = " Where every compiled grammar comes from, and its license."]
        pub const LICENSES: &[License] = &[#(#licenses),*];

        #[doc = " Every query file highlighting reads: Helix's at [`HELIX`], unmodified, of the"]
        #[doc = " directories the tree-house tier's rows name and of the directories their"]
        #[doc = " `; inherits:` lines name, then cr's own of the rows whose queries are local."]
        pub const QUERIES: &[Query] = &[#(#query_files),*];

        #[doc = " Where Helix's queries come from, and their license."]
        pub const QUERIES_LICENSE: License = License {
            name: #queries_name,
            git: #queries_git,
            rev: HELIX,
            spdx: #queries_spdx,
            text: include_str!(#queries_license),
        };
    };
    let file = syn::parse_file(&tokens.to_string()).map_err(Error::Generate)?;
    let body = prettyplease::unparse(&file);
    Ok(formatdoc! {"
        // Generated by review-languages' build.rs from languages.toml.

        {body}"
    })
}

/// The name of the `LanguageFn` constant of `grammar`, such as `MARKDOWN_INLINE`.
fn constant(grammar: &Grammar) -> Ident {
    format_ident!("{}", grammar.name.to_uppercase())
}

/// `path` as the string literal `include_str!` takes.
fn path_literal(path: &Path) -> String {
    path.display().to_string()
}

/// The entry of `LANGUAGES` for `language`.
fn spec(language: &Language) -> TokenStream {
    let tier = match language.tier {
        Tier::TreeHouse => quote!(Tier::TreeHouse),
        Tier::Bat => quote!(Tier::Bat),
    };
    // A bat-tier row keeps its fetched grammar's pin, but the grammar is not compiled.
    let grammar = match (&language.proof, &language.grammar, language.tier) {
        (Some(proof), _, _) => quote!(Some(GrammarSpec::Proof(#proof))),
        (None, Some(grammar), Tier::TreeHouse) => {
            let constant = constant(grammar);
            quote!(Some(GrammarSpec::Compiled(#constant)))
        }
        _ => quote!(None),
    };
    let Language {
        name,
        queries,
        bat,
        aliases,
        injection_regex,
        extensions,
        filenames,
        globs,
        shebangs,
        data,
        ..
    } = language;
    let data: &[String] = data.as_ref().map_or(&[], |data| data.nodes.as_slice());
    let queries = optional(queries.as_deref());
    let bat = optional(bat.as_deref());
    let injection_regex = optional(injection_regex.as_deref());
    quote! {
        LanguageSpec {
            name: #name,
            tier: #tier,
            grammar: #grammar,
            queries: #queries,
            bat: #bat,
            aliases: &[#(#aliases),*],
            injection_regex: #injection_regex,
            data: &[#(#data),*],
            detection: Detection {
                filenames: &[#(#filenames),*],
                globs: &[#(#globs),*],
                extensions: &[#(#extensions),*],
                shebangs: &[#(#shebangs),*],
            },
        }
    }
}

/// `value` as an `Option<&str>` expression.
fn optional(value: Option<&str>) -> TokenStream {
    value.map_or_else(|| quote!(None), |value| quote!(Some(#value)))
}

/// One query file, of the queries' entry or local.
struct QueryFile {
    /// Its directory, such as `rust`, which `; inherits:` lines name.
    directory: String,
    /// Its variant of `QueryKind`, such as `Highlights`.
    kind: Ident,
    /// Where it is.
    path: PathBuf,
}

/// The query files of the queries' entry, `queries/<directory>/<kind>.scm`, in the entry's
/// order.
fn query_files(entry: &Entry) -> Vec<QueryFile> {
    entry
        .files
        .iter()
        .filter_map(|path| {
            let relative = path.strip_prefix(&entry.directory).ok()?;
            let mut components = relative.components().map(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            });
            let (Some(Some(Queries::DIRECTORY)), Some(Some(directory)), Some(Some(file)), None) = (
                components.next(),
                components.next(),
                components.next(),
                components.next(),
            ) else {
                return None;
            };
            Some(QueryFile {
                directory: directory.to_owned(),
                kind: query_kind(file)?,
                path: path.clone(),
            })
        })
        .collect()
}

/// The local query files of `directories` under `root`, review-languages' `queries/`.
fn local_query_files(root: &Path, directories: &[String]) -> Result<Vec<QueryFile>, Error> {
    let mut files = Vec::new();
    for directory in directories {
        let path = root.join(directory);
        let found = Queries::KINDS.iter().filter_map(|file| {
            let path = path.join(file);
            let kind = query_kind(file).filter(|_| path.is_file())?;
            Some(QueryFile {
                directory: directory.clone(),
                kind,
                path,
            })
        });
        let before = files.len();
        files.extend(found);
        if files.len() == before {
            return Err(Error::LocalQueries { path });
        }
    }
    Ok(files)
}

/// The variant of `QueryKind` of the query file named `file`, such as `Highlights`.
fn query_kind(file: &str) -> Option<Ident> {
    let kind = match file {
        "highlights.scm" => "Highlights",
        "injections.scm" => "Injections",
        "locals.scm" => "Locals",
        _ => return None,
    };
    Some(format_ident!("{kind}"))
}
