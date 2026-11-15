//! Which paths of the working tree git ignores, as a working-tree capture sees them.

use std::collections::HashSet;

use super::Repo;
use crate::error::Error;

impl Repo {
    /// Returns those of `paths` that git ignores and does not track, which a working-tree
    /// capture therefore leaves out.
    ///
    /// Paths are relative to the working tree, with `/` between components; a directory is
    /// asked about as `dir/`, so patterns that match only directories apply.
    /// The rules are git's own: `.gitignore` files, `info/exclude` and `core.excludesFile`.
    /// A tracked path is never ignored, since a capture records its changes whatever the
    /// rules say, and neither is a directory that holds a tracked file at any depth, so a
    /// walk that skips ignored directories still reaches every tracked file.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails, such as for a path inside a submodule.
    pub fn ignored<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashSet<String>, Error> {
        let mut paths = paths.into_iter().peekable();
        if paths.peek().is_none() {
            return Ok(HashSet::new());
        }
        // Exit status 1 means that none of the paths is ignored.
        let output = self.run_with_paths(&["check-ignore", "-z", "--stdin"], paths, &[0, 1])?;
        Ok(nul_separated(&output.stdout))
    }
}

/// The non-empty NUL-terminated paths of `stdout`.
fn nul_separated(stdout: &[u8]) -> HashSet<String> {
    stdout
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect()
}
