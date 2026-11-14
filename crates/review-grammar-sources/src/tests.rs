use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use indoc::indoc;

use super::*;
use crate::git::Git;
use crate::manifest::ParseError;

/// Runs git in `directory` as a test's own setup, apart from the user's configuration.
fn run_git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A grammar of a local repository with one commit, whose sources are `src/parser.c` and the
/// header it includes, beside files an entry leaves out.
fn local_grammar(dir: &Path) -> Grammar {
    let repository = dir.join("upstream");
    std::fs::create_dir_all(repository.join("src/tree_sitter")).unwrap();
    std::fs::write(
        repository.join("src/parser.c"),
        "#include \"tree_sitter/parser.h\"\n",
    )
    .unwrap();
    std::fs::write(repository.join("src/tree_sitter/parser.h"), "int x;\n").unwrap();
    std::fs::write(repository.join("README.md"), "# Grammar\n").unwrap();
    std::fs::write(repository.join("LICENSE"), "MIT\n").unwrap();
    run_git(&repository, &["init", "--quiet"]);
    run_git(&repository, &["add", "--all"]);
    run_git(&repository, &["commit", "--quiet", "-m", "grammar"]);
    Grammar {
        name: "local".to_owned(),
        git: format!("file://{}", repository.display()),
        rev: run_git(&repository, &["rev-parse", "HEAD"]),
        subpath: None,
        sources: vec!["parser.c".to_owned()],
        license: "MIT".to_owned(),
        license_file: "LICENSE".to_owned(),
    }
}

/// The locals query of [`local_queries`]' directory `b`.
const B_LOCALS: &str = indoc! {"
    ;; inherits: c
    (b) @local.scope
"};

/// The queries of a local repository laid out as Helix's, with one commit: `a` inherits `b`,
/// `b` inherits `c` from its locals, `unused` is named by nothing, and the license is at the
/// root.
fn local_queries(dir: &Path) -> Queries {
    let repository = dir.join("helix");
    let queries = repository.join("runtime/queries");
    let files = [
        (
            "a/highlights.scm",
            indoc! {"
                ; inherits: b
                (a) @keyword
            "},
        ),
        ("a/injections.scm", "(a) @injection.content\n"),
        ("b/highlights.scm", "(b) @type\n"),
        ("b/locals.scm", B_LOCALS),
        ("c/locals.scm", "(c) @local.definition\n"),
        ("c/README.md", "Not a query.\n"),
        ("unused/highlights.scm", "(unused) @comment\n"),
    ];
    for (path, text) in files {
        let path = queries.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::write(
        repository.join("LICENSE"),
        "Mozilla Public License Version 2.0\n",
    )
    .unwrap();
    run_git(&repository, &["init", "--quiet"]);
    run_git(&repository, &["add", "--all"]);
    run_git(&repository, &["commit", "--quiet", "-m", "queries"]);
    Queries {
        git: format!("file://{}", repository.display()),
        rev: run_git(&repository, &["rev-parse", "HEAD"]),
        directories: vec!["a".to_owned()],
    }
}

fn cache_of(dir: &Path) -> Sources {
    Sources {
        cache: dir.join("cache"),
        fallback: None,
        prepared: None,
        offline: false,
    }
}

/// Every path under `directory`, to show that a lookup wrote nothing.
fn listing(directory: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![directory.to_owned()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

/// A fetch fills the cache once with the pinned commit's sources and license, and later
/// lookups find the entry there, even offline.
#[test]
fn a_fetch_fills_the_cache_once() {
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let sources = cache_of(dir.path());
    let entry = sources.locate(&grammar).unwrap();
    assert!(entry.directory.starts_with(&sources.cache));

    // The entry holds `src/` and the license, and its files are exactly what a build watches.
    let parser = std::fs::read_to_string(entry.directory.join("src/parser.c")).unwrap();
    assert!(parser.contains("tree_sitter/parser.h"));
    assert_eq!(
        std::fs::read_to_string(entry.directory.join("LICENSE")).unwrap(),
        "MIT\n"
    );
    assert!(!entry.directory.join("README.md").exists());
    let mut files = entry.files.clone();
    files.sort();
    let expected: Vec<PathBuf> = [
        "LICENSE",
        "descriptor.toml",
        "src/parser.c",
        "src/tree_sitter/parser.h",
    ]
    .iter()
    .map(|file| entry.directory.join(file))
    .collect();
    assert_eq!(files, expected);

    // Found again offline, and nothing is left half-made.
    let offline = Sources {
        offline: true,
        ..sources.clone()
    };
    assert_eq!(offline.locate(&grammar).unwrap(), entry);
    assert_eq!(listing(&sources.cache.join("_tmp")), Vec::<PathBuf>::new());
}

/// A fetch of the queries takes the query files of the directories the manifest names and of
/// every directory their `; inherits:` lines name, and the license, and nothing else.
#[test]
fn a_fetch_of_the_queries_follows_their_inherits_lines() {
    let dir = tempfile::tempdir().unwrap();
    let queries = local_queries(dir.path());
    let sources = cache_of(dir.path());
    let entry = sources.locate(&queries).unwrap();
    assert!(entry.directory.starts_with(&sources.cache));
    assert!(
        entry
            .directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("helix-queries-")
    );
    let mut files = entry.files.clone();
    files.sort();
    let expected: Vec<PathBuf> = [
        "LICENSE",
        "descriptor.toml",
        "queries/a/highlights.scm",
        "queries/a/injections.scm",
        "queries/b/highlights.scm",
        "queries/b/locals.scm",
        "queries/c/locals.scm",
    ]
    .iter()
    .map(|file| entry.directory.join(file))
    .collect();
    assert_eq!(files, expected);
    // The files are Helix's, unmodified.
    assert_eq!(
        std::fs::read_to_string(entry.directory.join("queries/b/locals.scm")).unwrap(),
        B_LOCALS
    );

    // Found again offline, and as prepared sources once copied.
    let offline = Sources {
        offline: true,
        ..sources.clone()
    };
    assert_eq!(offline.locate(&queries).unwrap(), entry);
    let root = dir.path().join("prepared");
    entry.prepare(&root.join(Queries::NAME), &queries).unwrap();
    let prepared = Sources {
        prepared: Some(root.clone()),
        ..offline
    };
    let found = prepared.locate(&queries).unwrap();
    assert_eq!(found.directory, root.join(Queries::NAME));

    // Another set of directories is another entry, which the cached one does not stand for.
    let more = Queries {
        directories: vec!["a".to_owned(), "unused".to_owned()],
        ..queries.clone()
    };
    let err = prepared.locate(&more).unwrap_err();
    assert!(
        matches!(
            err,
            Error::InvalidPrepared {
                source: Invalid::Mismatch {
                    field: "directories",
                    ..
                },
                ..
            }
        ),
        "{err:?}"
    );
    let other = sources.locate(&more).unwrap();
    assert_ne!(other.directory, entry.directory);
    assert!(
        other
            .directory
            .join("queries/unused/highlights.scm")
            .is_file()
    );
}

/// A query directory Helix does not have fails the fetch, and nothing is published for it.
#[test]
fn a_query_directory_helix_lacks_fails_the_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let queries = Queries {
        directories: vec!["a".to_owned(), "nowhere".to_owned()],
        ..local_queries(dir.path())
    };
    let sources = cache_of(dir.path());
    let err = sources.locate(&queries).unwrap_err();
    let Error::Unavailable { source, .. } = &err else {
        panic!("{err:?}");
    };
    assert!(
        matches!(
            source.as_ref(),
            Error::InvalidFetch {
                source: Invalid::Missing { path },
                ..
            } if path == "runtime/queries/nowhere"
        ),
        "{err:?}"
    );
    assert!(fetch::find(&sources.cache, Pinned::Queries(&queries)).is_none());
}

/// Missing sources fail offline, a revision the repository lacks fails to fetch, and both
/// errors say what to do.
#[test]
fn missing_sources_fail_with_the_remedy() {
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let offline = Sources {
        offline: true,
        ..cache_of(dir.path())
    };
    let err = offline.locate(&grammar).unwrap_err();
    assert!(matches!(err, Error::Offline { .. }), "{err}");
    assert!(err.to_string().contains("task grammars:fetch"));
    assert!(!offline.cache.exists(), "an offline lookup writes nothing");

    // A pin the repository does not have fails, and nothing is published for it.
    let wrong = Grammar {
        rev: "0".repeat(40),
        ..grammar
    };
    let online = Sources {
        offline: false,
        ..offline
    };
    let err = online.locate(&wrong).unwrap_err();
    assert!(matches!(err, Error::Unavailable { .. }), "{err}");
    assert!(err.to_string().contains("CODE_REVIEW_GRAMMARS_DIR"));
    assert!(fetch::find(&online.cache, Pinned::Grammar(&wrong)).is_none());
}

/// A cached entry whose files no longer match its descriptor is left alone, and the grammar is
/// fetched again into a new entry.
#[test]
fn a_damaged_cache_entry_is_fetched_again_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let sources = cache_of(dir.path());
    let damaged = sources.locate(&grammar).unwrap();
    std::fs::write(damaged.directory.join("src/parser.c"), "").unwrap();

    let entry = sources.locate(&grammar).unwrap();
    assert_ne!(entry.directory, damaged.directory);
    assert!(entry.directory.to_string_lossy().ends_with(".1"));
    assert!(
        std::fs::metadata(entry.directory.join("src/parser.c"))
            .unwrap()
            .len()
            > 0
    );
    // The damaged entry is never changed, since a compiler may be reading it.
    assert_eq!(
        std::fs::metadata(damaged.directory.join("src/parser.c"))
            .unwrap()
            .len(),
        0
    );

    // An entry describing another revision is not this grammar's either.
    let descriptor = entry.directory.join("descriptor.toml");
    let text = std::fs::read_to_string(&descriptor).unwrap();
    std::fs::write(&descriptor, text.replace(&grammar.rev, &"1".repeat(40))).unwrap();
    let again = sources.locate(&grammar).unwrap();
    assert!(again.directory.to_string_lossy().ends_with(".2"));
}

/// A file changed in place to the same size, as a restored cache might hold it, is not the
/// pinned blob: a cached entry with it is fetched again beside it, and prepared sources with it
/// are refused.
#[test]
fn a_same_size_change_is_caught_by_its_blob_id() {
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let sources = cache_of(dir.path());
    let entry = sources.locate(&grammar).unwrap();
    let flip = |path: &Path| {
        let mut bytes = std::fs::read(path).unwrap();
        let size = bytes.len();
        if let Some(first) = bytes.first_mut() {
            *first ^= 0x20;
        }
        std::fs::write(path, &bytes).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            u64::try_from(size).unwrap()
        );
    };

    // A prepared copy with one byte flipped is refused for its content.
    let root = dir.path().join("prepared");
    entry.prepare(&root.join(&grammar.name), &grammar).unwrap();
    flip(&root.join(&grammar.name).join("src/parser.c"));
    let prepared = Sources {
        prepared: Some(root),
        ..cache_of(dir.path())
    };
    let err = prepared.locate(&grammar).unwrap_err();
    assert!(
        matches!(
            err,
            Error::InvalidPrepared {
                source: Invalid::Content { .. },
                ..
            }
        ),
        "{err:?}"
    );

    // The cached entry with one byte flipped is left alone, and the grammar fetched again.
    flip(&entry.directory.join("src/parser.c"));
    let again = sources.locate(&grammar).unwrap();
    assert_ne!(again.directory, entry.directory);
    assert_eq!(
        std::fs::read(again.directory.join("src/parser.c")).unwrap(),
        std::fs::read(dir.path().join("upstream/src/parser.c")).unwrap()
    );
}

/// Prepared sources win over the cache and are read without writes, and ones that do not match
/// the pin are refused.
#[test]
fn prepared_sources_are_checked_against_the_pin() {
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let entry = cache_of(dir.path()).locate(&grammar).unwrap();
    let root = dir.path().join("prepared");
    let prepared = root.join(&grammar.name);
    entry.prepare(&prepared, &grammar).unwrap();
    let sources = Sources {
        cache: dir.path().join("other-cache"),
        fallback: None,
        prepared: Some(root.clone()),
        offline: true,
    };
    let found = sources.locate(&grammar).unwrap();
    assert_eq!(found.directory, prepared);
    assert!(!sources.cache.exists());

    // A grammar the directory does not prepare comes from a cache.
    let other = Grammar {
        name: "other".to_owned(),
        ..grammar.clone()
    };
    let err = sources.locate(&other).unwrap_err();
    assert!(matches!(err, Error::Offline { .. }), "{err}");

    // Prepared sources of another revision, or without a descriptor, are an error.
    let descriptor = prepared.join("descriptor.toml");
    let text = std::fs::read_to_string(&descriptor).unwrap();
    std::fs::write(&descriptor, text.replace(&grammar.rev, &"1".repeat(40))).unwrap();
    let err = sources.locate(&grammar).unwrap_err();
    assert!(
        matches!(
            err,
            Error::InvalidPrepared {
                source: Invalid::Mismatch { field: "rev", .. },
                ..
            }
        ),
        "{err:?}"
    );
    std::fs::remove_file(&descriptor).unwrap();
    let err = sources.locate(&grammar).unwrap_err();
    assert!(
        matches!(
            err,
            Error::InvalidPrepared {
                source: Invalid::Unreadable(_),
                ..
            }
        ),
        "{err:?}"
    );

    // A prepared directory that does not exist is an error, not a silent fallback.
    let missing = Sources {
        prepared: Some(dir.path().join("nowhere")),
        ..sources
    };
    let err = missing.locate(&grammar).unwrap_err();
    assert!(matches!(err, Error::PreparedRoot { .. }), "{err}");
}

/// A read-only cache is read without writing to it, and a grammar it lacks is fetched into the
/// fallback cache instead.
#[cfg(unix)]
#[test]
fn a_read_only_cache_falls_back_for_fetches() {
    use std::os::unix::fs::PermissionsExt as _;

    let set_mode = |directory: &Path, mode: u32| {
        for path in listing(directory).into_iter().chain([directory.to_owned()]) {
            if path.is_dir() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let sources = Sources {
        fallback: Some(dir.path().join("out")),
        ..cache_of(dir.path())
    };
    let entry = sources.locate(&grammar).unwrap();
    set_mode(&sources.cache, 0o555);
    // Permissions do not bind a superuser, so the test has nothing to show then.
    if std::fs::write(sources.cache.join("probe"), "").is_ok() {
        set_mode(&sources.cache, 0o755);
        return;
    }
    let before = listing(&sources.cache);

    // The filled cache is read as it is.
    assert_eq!(sources.locate(&grammar).unwrap(), entry);
    let other = Grammar {
        name: "other".to_owned(),
        ..grammar.clone()
    };
    // The grammar it lacks lands in the fallback, as a build script's would under `OUT_DIR`.
    let fetched = sources.locate(&other).unwrap();
    set_mode(&sources.cache, 0o755);
    assert!(
        fetched.directory.starts_with(dir.path().join("out")),
        "{fetched:?}"
    );
    assert_eq!(listing(&sources.cache), before);
    // Found in the fallback from then on.
    assert_eq!(sources.locate(&other).unwrap(), fetched);
}

/// A build that finds the grammar's lock held waits for it, then reuses the entry the holder
/// published instead of fetching again.
///
/// The repository is away while the second build runs unblocked, so it can only finish by
/// waiting: a build that did not wait for the lock would fail to fetch, and finish early.
#[test]
fn a_second_producer_waits_and_reuses_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let upstream = dir.path().join("upstream");
    let away = dir.path().join("away");
    let sources = cache_of(dir.path());
    let lock = fetch::lock(&sources.cache, Pinned::Grammar(&grammar)).unwrap();
    std::fs::rename(&upstream, &away).unwrap();
    let (started, attempt) = std::sync::mpsc::channel();
    let second = std::thread::spawn({
        let sources = sources.clone();
        let grammar = grammar.clone();
        move || {
            started.send(()).unwrap();
            sources.locate(&grammar)
        }
    });
    attempt.recv().unwrap();
    // Time for an unblocked second build to fail its fetch; a waiting one is still waiting.
    std::thread::sleep(Duration::from_millis(300));
    assert!(!second.is_finished(), "the second build waits for the lock");

    // The holder fetches and publishes, and takes the repository away again before letting go.
    std::fs::rename(&away, &upstream).unwrap();
    let first = fetch::produce(&sources.cache, Pinned::Grammar(&grammar)).unwrap();
    std::fs::remove_dir_all(&upstream).unwrap();
    drop(lock);
    let second = second.join().unwrap().unwrap();
    assert_eq!(second, first);
}

/// Preparing sources publishes them whole into a directory that did not exist, and preparing
/// them again leaves the matching directory as it is, so a build reading it never sees a file
/// rewritten.
#[test]
fn prepared_sources_are_published_once() {
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let entry = cache_of(dir.path()).locate(&grammar).unwrap();
    let root = dir.path().join("prepared");
    let prepared = root.join(&grammar.name);
    entry.prepare(&prepared, &grammar).unwrap();
    // Nothing is left staged beside it.
    assert!(
        listing(&root)
            .iter()
            .all(|path| path.starts_with(&prepared)),
        "{:?}",
        listing(&root)
    );

    // Prepared again, the directory's files are not written: their time stays.
    let parser = prepared.join("src/parser.c");
    let past = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&parser)
        .unwrap()
        .set_modified(past)
        .unwrap();
    entry.prepare(&prepared, &grammar).unwrap();
    assert_eq!(
        std::fs::metadata(&parser).unwrap().modified().unwrap(),
        past
    );

    let sources = Sources {
        prepared: Some(root.clone()),
        offline: true,
        ..cache_of(dir.path())
    };
    assert_eq!(sources.locate(&grammar).unwrap().directory, prepared);
}

/// A directory that holds other sources, such as an older pin's, is refused rather than
/// replaced, since a build may be reading it, and it is left exactly as it was.
#[test]
fn a_prepared_directory_of_other_sources_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let entry = cache_of(dir.path()).locate(&grammar).unwrap();
    let root = dir.path().join("prepared");
    let prepared = root.join(&grammar.name);
    entry.prepare(&prepared, &grammar).unwrap();
    let descriptor = prepared.join("descriptor.toml");
    let text = std::fs::read_to_string(&descriptor).unwrap();
    let other = text.replace(&grammar.rev, &"1".repeat(40));
    std::fs::write(&descriptor, &other).unwrap();
    let before = listing(&root);

    let err = entry.prepare(&prepared, &grammar).unwrap_err();
    assert!(
        matches!(
            err,
            Error::PreparedMismatch {
                source: Invalid::Mismatch { field: "rev", .. },
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("fresh directory"), "{err}");
    assert_eq!(listing(&root), before);
    assert_eq!(std::fs::read_to_string(&descriptor).unwrap(), other);
}

/// A staging directory another user left behind, which this build cannot remove, makes the
/// cache unwritable for this build, so the grammar is fetched into the fallback cache.
#[cfg(unix)]
#[test]
fn a_stale_staging_directory_falls_back_for_fetches() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let sources = Sources {
        fallback: Some(dir.path().join("out")),
        ..cache_of(dir.path())
    };
    // The staging directory of the grammar's first entry, with a file in a directory this
    // build may not change.
    let name = descriptor::entry_name(Pinned::Grammar(&grammar));
    let locked = sources.cache.join("_tmp").join(name).join("repository.git");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Permissions do not bind a superuser, so the test has nothing to show then.
    if std::fs::write(locked.join("probe"), "").is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let fetched = sources.locate(&grammar);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fetched = fetched.unwrap();
    assert!(
        fetched.directory.starts_with(dir.path().join("out")),
        "{fetched:?}"
    );
}

/// git ignores the repository variables a git hook running cargo would pass on.
#[test]
fn git_ignores_the_callers_repository() {
    let dir = tempfile::tempdir().unwrap();
    let own = dir.path().join("own");
    let caller = dir.path().join("caller");
    for repository in [&own, &caller] {
        std::fs::create_dir_all(repository).unwrap();
        run_git(repository, &["init", "--quiet"]);
    }
    let git = Git::new(dir.path()).unwrap();
    let mut command = Command::new("git");
    command
        .env("GIT_DIR", caller.join(".git"))
        .env("GIT_WORK_TREE", &caller)
        .env("GIT_INDEX_FILE", caller.join(".git/index"));
    git.isolate(&mut command);
    let output = command
        .current_dir(&own)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let found = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(
        found.canonicalize().unwrap(),
        own.join(".git").canonicalize().unwrap()
    );

    // Everything git counts as repository-local is cleared.
    let listed = run_git(dir.path(), &["rev-parse", "--local-env-vars"]);
    let cleared: Vec<String> = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    for name in listed.lines() {
        assert!(
            cleared.iter().any(|cleared| cleared == name),
            "{name} is not cleared"
        );
    }
}

/// The manifest's rows read with their defaults, and rows the registry could not use are
/// refused, each with its reason.
#[test]
fn the_manifest_reads_and_refuses_unusable_rows() {
    let text = indoc! {r#"
        helix = "a714a818a3a4f6dd5aad3b003a78692f90a1703f"

        [[language]]
        name = "rust"
        tier = "tree-house"
        queries = "rust"
        proof = "rust"
        extensions = ["rs"]

        [[language]]
        name = "markdown.inline"
        tier = "tree-house"
        queries = "markdown.inline"
        injection-regex = "markdown\\.inline"
        locked = ["grammar"]

        [language.grammar]
        name = "markdown_inline"
        git = "https://github.com/tree-sitter-grammars/tree-sitter-markdown"
        rev = "f969cd3ae3f9fbd4e43205431d0ae286014c05b5"
        subpath = "tree-sitter-markdown-inline"
        sources = ["parser.c", "scanner.c"]
        license = "MIT"

        [[language]]
        name = "sql"
        tier = "bat"
        bat = "SQL"
        extensions = ["sql"]
    "#};
    let Ok(manifest) = Manifest::parse(text) else {
        panic!("the manifest parses");
    };
    let grammars: Vec<&Grammar> = manifest.grammars().collect();
    let [grammar] = grammars.as_slice() else {
        panic!("one grammar to compile: {manifest:?}");
    };
    assert_eq!(grammar.name, "markdown_inline");
    assert_eq!(grammar.license_file, "LICENSE");
    assert_eq!(
        grammar.subpath.as_deref(),
        Some("tree-sitter-markdown-inline")
    );

    // Each change makes one row unusable, and the error says why.
    let refused = [
        (
            "rev = \"f969cd3ae3f9fbd4e43205431d0ae286014c05b5\"",
            "rev = \"main\"",
        ),
        ("name = \"markdown_inline\"", "name = \"Markdown\""),
        ("name = \"markdown_inline\"", "name = \"2markdown\""),
        ("proof = \"rust\"", "proof = \"kotlin\""),
        ("queries = \"rust\"\n", ""),
        ("bat = \"SQL\"", ""),
        ("extensions = [\"sql\"]", "globs = [\"Makefile\"]"),
        ("extensions = [\"rs\"]", "extensions = [\".rs\"]"),
        ("markdown\\\\.inline", "markdown("),
        ("locked = [\"grammar\"]", "locked = [\"aliases\"]"),
        ("name = \"sql\"", "name = \"rust\""),
    ];
    for (from, to) in refused {
        assert!(text.contains(from), "{from}");
        let changed = text.replacen(from, to, 1);
        assert!(
            matches!(
                Manifest::parse(&changed),
                Err(ParseError::Invalid(Error::Language { .. }))
            ),
            "{from:?} as {to:?} is refused"
        );
    }
    // A tree-house row has one grammar: a proof language's, or one it fetches.
    let both = indoc! {r#"
        helix = "a714a818a3a4f6dd5aad3b003a78692f90a1703f"

        [[language]]
        name = "rust"
        tier = "tree-house"
        queries = "rust"
        proof = "rust"

        [language.grammar]
        git = "https://github.com/tree-sitter/tree-sitter-rust"
        rev = "77a3747266f4d621d0757825e6b11edcbf991ca5"
        sources = ["parser.c"]
        license = "MIT"
    "#};
    assert!(matches!(
        Manifest::parse(both),
        Err(ParseError::Invalid(Error::Language {
            problem: Problem::Grammar,
            ..
        }))
    ));
    // The queries are fetched at the `helix` commit, which must name one commit.
    let abbreviated = text.replacen("a714a818a3a4f6dd5aad3b003a78692f90a1703f", "a714a818", 1);
    assert!(matches!(
        Manifest::parse(&abbreviated),
        Err(ParseError::Invalid(Error::HelixRevision(_)))
    ));
}

/// Template rows read with their data nodes and local queries, rows may share a grammar they
/// pin alike, and a shared grammar is compiled once, while rows that pin it differently, data
/// without node kinds and local queries without a directory are refused.
#[test]
fn the_manifest_reads_templates_and_shared_grammars() {
    let text = indoc! {r#"
        helix = "a714a818a3a4f6dd5aad3b003a78692f90a1703f"

        [[language]]
        name = "erb"
        tier = "tree-house"
        queries = "erb"
        data = { nodes = ["content"] }

        [language.grammar]
        name = "embedded_template"
        git = "https://github.com/tree-sitter/tree-sitter-embedded-template"
        rev = "5bce9d580d3b78f48e13e331f69dfa0abd4ff68b"
        sources = ["parser.c"]
        license = "MIT"

        [[language]]
        name = "ejs"
        tier = "tree-house"
        queries = "ejs"
        local-queries = true
        data = { nodes = ["content", "raw_body"] }

        [language.grammar]
        name = "embedded_template"
        git = "https://github.com/tree-sitter/tree-sitter-embedded-template"
        rev = "5bce9d580d3b78f48e13e331f69dfa0abd4ff68b"
        sources = ["parser.c"]
        license = "MIT"
    "#};
    let Ok(manifest) = Manifest::parse(text) else {
        panic!("the manifest parses");
    };
    assert_eq!(manifest.grammars().count(), 1);
    let [erb, ejs] = manifest.languages.as_slice() else {
        panic!("two rows: {manifest:?}");
    };
    assert_eq!(
        erb.data,
        Some(TemplateData {
            nodes: vec!["content".to_owned()]
        })
    );
    assert!(!erb.local_queries && ejs.local_queries);
    // Local queries are not Helix's to fetch.
    assert_eq!(manifest.queries().directories, ["erb"]);
    assert_eq!(manifest.local_queries(), ["ejs"]);

    // Data without node kinds, or with a kind that is not one, and local queries without a
    // directory are refused.
    let refused = [
        ("nodes = [\"content\"]", "nodes = []"),
        ("nodes = [\"content\"]", "nodes = [\"(content) @x\"]"),
        ("queries = \"ejs\"\n", ""),
    ];
    // Rows that pin a shared grammar differently are refused.
    let shared = text.replacen(
        "5bce9d580d3b78f48e13e331f69dfa0abd4ff68b",
        &"1".repeat(40),
        1,
    );
    assert!(matches!(
        Manifest::parse(&shared),
        Err(ParseError::Invalid(Error::Language {
            problem: Problem::DuplicateGrammar(_),
            ..
        }))
    ));
    for (from, to) in refused {
        assert!(text.contains(from), "{from}");
        let changed = text.replacen(from, to, 1);
        assert!(
            matches!(
                Manifest::parse(&changed),
                Err(ParseError::Invalid(Error::Language { .. }))
            ),
            "{from:?} as {to:?} is refused"
        );
    }
}

/// A grammar may be pinned at an annotated tag, as Helix pins some, whose commit's sources are
/// fetched; the tag's own id is what the entry records.
#[test]
fn a_tag_pin_fetches_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    let grammar = local_grammar(dir.path());
    let repository = dir.path().join("upstream");
    run_git(&repository, &["tag", "--annotate", "v1", "--message", "v1"]);
    let tag = run_git(&repository, &["rev-parse", "v1"]);
    assert_ne!(tag, grammar.rev);
    let tagged = Grammar {
        rev: tag.clone(),
        ..grammar
    };
    let entry = cache_of(dir.path()).locate(&tagged).unwrap();
    let descriptor = std::fs::read_to_string(entry.directory.join("descriptor.toml")).unwrap();
    assert!(descriptor.contains(&tag), "{descriptor}");
    assert!(entry.directory.join("src/parser.c").is_file());
}

/// The queries to fetch are the directories the tree-house rows name, each once, at the
/// manifest's Helix commit; a bat row's directory is not fetched.
#[test]
fn the_manifest_names_the_queries_to_fetch() {
    let text = indoc! {r#"
        helix = "a714a818a3a4f6dd5aad3b003a78692f90a1703f"

        [[language]]
        name = "typescript"
        tier = "tree-house"
        queries = "typescript"
        proof = "typescript"

        [[language]]
        name = "javascript"
        tier = "tree-house"
        queries = "tsx"
        proof = "javascript"

        [[language]]
        name = "jsx"
        tier = "tree-house"
        queries = "tsx"
        proof = "tsx"

        [[language]]
        name = "go"
        tier = "bat"
        queries = "go"
        proof = "go"
        bat = "Go"
    "#};
    let Ok(manifest) = Manifest::parse(text) else {
        panic!("the manifest parses");
    };
    assert_eq!(
        manifest.queries(),
        Queries {
            git: HELIX_GIT.to_owned(),
            rev: "a714a818a3a4f6dd5aad3b003a78692f90a1703f".to_owned(),
            directories: vec!["tsx".to_owned(), "typescript".to_owned()],
        }
    );
}

/// `CARGO_NET_OFFLINE` reads as a boolean, so `false` allows fetching, and a relative
/// `CODE_REVIEW_GRAMMARS_DIR` names a directory under review-languages' own.
#[test]
fn the_environment_reads_as_documented() {
    assert!(offline(Some("true")));
    assert!(offline(Some("1")));
    assert!(!offline(Some("false")));
    assert!(!offline(None));
    // Absolute on every platform, drive included on Windows.
    let package = std::env::temp_dir().join("review-languages");
    let elsewhere = std::env::temp_dir().join("grammars");
    assert_eq!(
        prepared_dir(std::ffi::OsStr::new("vendor"), &package),
        package.join("vendor")
    );
    assert_eq!(prepared_dir(elsewhere.as_os_str(), &package), elsewhere);
}
