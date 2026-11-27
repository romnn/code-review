//! Shared setup of the unit tests.

/// Isolates the tests' git commands from the user's and the system's configuration, with the
/// workspace's `tests/gitconfig` in place of the user's, and pins the commit identity.
pub(crate) const GIT_ENV: [(&str, &str); 6] = [
    (
        "GIT_CONFIG_GLOBAL",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/gitconfig"),
    ),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
];
