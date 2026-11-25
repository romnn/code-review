//! Running git with the repository's filter drivers turned off.
//!
//! A filter driver (`filter.<name>.clean`, `smudge` or `process`) is a program the configuration
//! names and the reviewed tree's `.gitattributes` selects, so a capture of reviewed content would
//! run it.
//! Commands that read the working tree turn every configured driver off instead.

use std::collections::BTreeSet;
use std::process::Command;

use super::Repo;
use crate::error::Error;

/// The variables the `--config-env` overrides read their values from.
///
/// `--config-env` rather than `-c` takes the key up to the last `=`, so a driver whose name
/// contains `=` is turned off too.
const OVERRIDE_ENV: [(&str, &str); 2] = [
    ("CODE_REVIEW_FILTER_OFF", ""),
    ("CODE_REVIEW_FILTER_NOT_REQUIRED", "false"),
];

/// Options that turn off every filter driver configured for a repository.
#[derive(Debug, Clone)]
pub(crate) struct FiltersOff {
    args: Vec<String>,
}

impl FiltersOff {
    /// Returns a git command for `repo`, as [`Repo::command`] builds it, with every configured
    /// filter driver turned off.
    pub(crate) fn command(&self, repo: &Repo) -> Command {
        let mut command = repo.command();
        command.args(&self.args).envs(OVERRIDE_ENV);
        command
    }
}

impl Repo {
    /// Lists the configured filter drivers and returns the options that turn them all off.
    ///
    /// An empty command is no filter, and `required` set to false lets git store and check out
    /// the bytes as they are instead of failing for want of the filter.
    pub(crate) fn filters_off(&self) -> Result<FiltersOff, Error> {
        // `git config --get-regexp` exits with 1 when nothing matches.
        let output = Self::run_command_with_paths(
            self.command(),
            &["config", "-z", "--get-regexp", r"^filter\."],
            [],
            &[0, 1],
        )?;
        let names = driver_names(&output.stdout)?;
        let args = names
            .iter()
            .flat_map(|name| {
                [
                    format!("--config-env=filter.{name}.clean=CODE_REVIEW_FILTER_OFF"),
                    format!("--config-env=filter.{name}.smudge=CODE_REVIEW_FILTER_OFF"),
                    format!("--config-env=filter.{name}.process=CODE_REVIEW_FILTER_OFF"),
                    format!("--config-env=filter.{name}.required=CODE_REVIEW_FILTER_NOT_REQUIRED"),
                ]
            })
            .collect();
        Ok(FiltersOff { args })
    }
}

/// Parses `git config -z --get-regexp` output, records of `key NL value NUL` (or `key NUL` for
/// a key without a value), into the names of the filter drivers its keys configure.
///
/// A name that is not UTF-8 is an error rather than skipped, since skipping it would leave that
/// driver on.
fn driver_names(stdout: &[u8]) -> Result<BTreeSet<String>, Error> {
    let mut names = BTreeSet::new();
    for record in stdout
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
    {
        let key = record
            .split(|&byte| byte == b'\n')
            .next()
            .unwrap_or_default();
        let key = std::str::from_utf8(key).map_err(|err| Error::UnexpectedOutput {
            program: "git",
            command: "config --get-regexp".to_owned(),
            detail: format!("a filter driver's name is not UTF-8: {err}"),
        })?;
        let Some((name, _)) = key
            .strip_prefix("filter.")
            .and_then(|rest| rest.rsplit_once('.'))
        else {
            continue;
        };
        names.insert(name.to_owned());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Driver names keep their case and any dots or `=` in them, and keys without a value count.
    #[test]
    fn driver_names_come_from_every_key() -> Result<(), Error> {
        let records: [&[u8]; 3] = [
            b"filter.lfs.clean\ngit-lfs clean -- %f\0",
            b"filter.lfs.required\0",
            b"filter.My.Driver=x.smudge\ncat\0",
        ];
        let names: Vec<String> = driver_names(&records.concat())?.into_iter().collect();
        assert_eq!(names, ["My.Driver=x", "lfs"]);
        Ok(())
    }
}
