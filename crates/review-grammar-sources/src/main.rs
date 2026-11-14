//! Fills review-languages' cache with every grammar its manifest pins and Helix's queries,
//! without building them, for offline work, CI caches and the release container
//! (`task grammars:fetch`).
//!
//! Usage: `review-grammar-sources [--into <dir>]`.
//! With `--into`, each grammar's sources are also published at `<dir>/<name>`, and the queries
//! at `<dir>/helix-queries`, a directory of prepared sources to point `CODE_REVIEW_GRAMMARS_DIR`
//! at.
//! Prepared sources never change once published, so builds may read them meanwhile: a run again
//! leaves what already matches as it is, and refuses a directory that holds other sources, such
//! as an older pin's, which goes into a fresh directory instead.
//! With `CARGO_NET_OFFLINE=true` it fetches nothing, and fails unless the cache holds every
//! grammar and the queries, as the build script would.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use review_grammar_sources::{Error, Manifest, Pinned, Sources};

/// review-languages, whose manifest this reads and whose cache it fills.
const LANGUAGES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../review-languages");

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let into = match (args.next().as_deref(), args.next(), args.next()) {
        (None, _, _) => None,
        (Some("--into"), Some(dir), None) => Some(PathBuf::from(dir)),
        _ => {
            eprintln!("usage: review-grammar-sources [--into <dir>]");
            eprintln!(
                "--into publishes prepared sources under <dir>, never changing them: a pin \
                 whose directory holds other sources needs a fresh <dir>"
            );
            return ExitCode::FAILURE;
        }
    };
    let offline =
        review_grammar_sources::offline(std::env::var("CARGO_NET_OFFLINE").ok().as_deref());
    match fetch(Path::new(LANGUAGES), offline, into.as_deref()) {
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

/// Fetches every grammar of the manifest under `root` and its queries that its cache lacks,
/// unless `offline`, and publishes each as prepared sources under `into` when given.
fn fetch(root: &Path, offline: bool, into: Option<&Path>) -> Result<(), Error> {
    let manifest = Manifest::read(&root.join("languages.toml"))?;
    let sources = Sources {
        cache: root.join("_cache"),
        fallback: None,
        prepared: None,
        offline,
    };
    let queries = manifest.queries();
    let pinned = manifest
        .grammars()
        .map(Pinned::Grammar)
        .chain([Pinned::Queries(&queries)]);
    for pinned in pinned {
        let entry = sources.locate(pinned)?;
        println!(
            "{} {} in {}",
            pinned.name(),
            pinned.rev(),
            entry.directory.display()
        );
        if let Some(into) = into {
            entry.prepare(&into.join(pinned.name()), pinned)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    /// Offline, a grammar the cache lacks fails at once, without trying the network: the
    /// repository named here does not exist, so only a fetch could report anything else.
    #[test]
    fn an_offline_fetch_fetches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("languages.toml"),
            indoc! {r#"
                helix = "a714a818a3a4f6dd5aad3b003a78692f90a1703f"

                [[language]]
                name = "bash"
                tier = "tree-house"
                queries = "bash"

                [language.grammar]
                git = "file:///nonexistent/tree-sitter-bash"
                rev = "a06c2e4415e9bc0346c6b86d401879ffb44058f7"
                sources = ["parser.c"]
                license = "MIT"
            "#},
        )
        .unwrap();
        let err = fetch(dir.path(), true, None).unwrap_err();
        assert!(matches!(err, Error::Offline { .. }), "{err:?}");
        assert!(!dir.path().join("_cache").exists(), "nothing is written");
        let err = fetch(dir.path(), false, None).unwrap_err();
        assert!(matches!(err, Error::Unavailable { .. }), "{err:?}");
    }
}
