//! Which paths of the working tree git ignores, as a working-tree capture sees them.

use std::collections::HashSet;

use super::scratch::Scratch;
use super::{PrivateRepo, Repo};
use crate::error::Error;

/// What git ignores in a reviewed checkout, judged as a working-tree capture judges it, on a
/// copy of its index in the review's archive, so asking writes nothing to the checkout.
#[derive(Debug, Clone)]
pub struct IgnoreRules {
    source: Repo,
    archive: PrivateRepo,
}

/// What git ignores in a reviewed checkout as its index and rules were when this was read; see
/// [`IgnoreRules::read`].
#[derive(Debug)]
pub struct Ignores {
    scratch: Scratch,
}

impl IgnoreRules {
    /// The rules of `source`'s checkout, read with copies of its index in `archive`.
    pub(crate) fn new(source: Repo, archive: PrivateRepo) -> Self {
        Self { source, archive }
    }

    /// Returns the reviewed repository.
    #[must_use]
    pub fn repo(&self) -> &Repo {
        &self.source
    }

    /// Reads the checkout's index and rules as they are now, for the questions of one moment,
    /// such as every level of one walk of the working tree.
    ///
    /// # Errors
    ///
    /// Returns an error for a repository without a working tree, and when git fails.
    pub fn read(&self) -> Result<Ignores, Error> {
        Ok(Ignores {
            scratch: Scratch::new(&self.archive, &self.source)?,
        })
    }
}

impl Ignores {
    /// Returns those of `paths` that git ignores and the index does not track, which a
    /// working-tree capture therefore leaves out.
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
        let output = Repo::run_command_with_paths(
            self.scratch.command(),
            &["check-ignore", "-z", "--stdin"],
            paths.map(str::as_bytes),
            &[0, 1],
        )?;
        Ok(output
            .stdout
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect())
    }
}
