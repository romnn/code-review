//! The git layer: every repository access goes through the `git` CLI.
//!
//! It produces verified, immutable inputs (object IDs and blob contents); everything after it is
//! pure analysis over object IDs.

mod blobs;
mod diff_tree;
mod snapshot;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use blobs::{Blob, BlobReader, MAX_TEXT_BLOB_BYTES};
pub use diff_tree::{ChangeStatus, EntryType, FileMode, RawChange, RawSide};

use crate::error::Error;
use crate::ids::ObjectId;

/// A git repository, addressed by its working tree or git directory.
#[derive(Debug, Clone)]
pub struct Repo {
    work_tree: Option<PathBuf>,
    git_dir: PathBuf,
    /// The directory holding objects and refs, shared by all worktrees of one clone.
    common_dir: PathBuf,
}

/// A ref and the object it points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// The full ref name.
    pub name: String,
    /// The object the ref points to.
    pub target: ObjectId,
}

/// A configured remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The remote's name, such as `origin`.
    pub name: String,
    /// Its fetch URL.
    pub url: String,
}

/// Raw output of a finished git command.
struct Output {
    stdout: Vec<u8>,
}

impl Repo {
    /// Opens the repository containing `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` is not inside a git repository or `git` cannot run.
    pub fn discover(path: &Path) -> Result<Self, Error> {
        let probe = Self {
            work_tree: Some(path.to_path_buf()),
            git_dir: PathBuf::new(),
            common_dir: PathBuf::new(),
        };
        let git_dir = probe.run_text(["rev-parse", "--absolute-git-dir"])?;
        let common_dir =
            probe.run_text(["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let bare = probe.run_text(["rev-parse", "--is-bare-repository"])? == "true";
        let work_tree = if bare {
            None
        } else {
            Some(PathBuf::from(
                probe.run_text(["rev-parse", "--show-toplevel"])?,
            ))
        };
        Ok(Self {
            work_tree,
            git_dir: PathBuf::from(git_dir),
            common_dir: PathBuf::from(common_dir),
        })
    }

    /// Returns the absolute common git directory, which holds the objects and refs that every
    /// worktree of this clone shares.
    #[must_use]
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// Returns the top-level working tree directory, if the repository has one.
    #[must_use]
    pub fn work_tree(&self) -> Option<&Path> {
        self.work_tree.as_deref()
    }

    /// Returns the absolute git directory.
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Returns a `git` command bound to this repository.
    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new("git");
        match &self.work_tree {
            Some(work_tree) => command.arg("-C").arg(work_tree),
            None => command.arg("--git-dir").arg(&self.git_dir),
        };
        // Never prompt for credentials and keep messages parseable.
        command.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
        command
    }

    /// Runs a git command and returns its standard output.
    fn run<I, S>(&self, args: I) -> Result<Output, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::run_command(self.command(), args)
    }

    fn run_command<I, S>(mut command: Command, args: I) -> Result<Output, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let shown = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let output = command
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .map_err(|source| Error::Spawn {
                program: "git",
                args: shown.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(Error::CommandFailed {
                program: "git",
                args: shown,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(Output {
            stdout: output.stdout,
        })
    }

    /// Runs a git command and returns its trimmed standard output as text.
    fn run_text<I, S>(&self, args: I) -> Result<String, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run(args)?;
        String::from_utf8(output.stdout)
            .map(|text| text.trim_end().to_owned())
            .map_err(|err| Error::UnexpectedOutput {
                program: "git",
                command: "rev-parse".to_owned(),
                detail: err.to_string(),
            })
    }

    /// Resolves `rev` to the commit it names.
    ///
    /// # Errors
    ///
    /// Returns an error when `rev` does not name a commit.
    pub fn resolve_commit(&self, rev: &str) -> Result<ObjectId, Error> {
        let spec = format!("{rev}^{{commit}}");
        let text = self
            .run_text([
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ])
            .map_err(|_| Error::InvalidTarget(rev.to_owned()))?;
        Ok(ObjectId::parse(&text)?)
    }

    /// Returns every merge base of `target` and `head`, best first, deepening a shallow clone from
    /// `deepen_from` when needed.
    ///
    /// Deepening never asks the remote for the head: a working-tree snapshot or an unpublished
    /// local commit exists only here.
    /// It deepens the clone's shallow boundary instead, whose commits all came from the remote,
    /// plus the target, with no ref updates and no `FETCH_HEAD`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoMergeBase`] when the histories are unrelated or a shallow clone cannot be
    /// deepened far enough.
    pub fn merge_bases(
        &self,
        target: &ObjectId,
        head: &ObjectId,
        deepen_from: Option<&str>,
    ) -> Result<Vec<ObjectId>, Error> {
        for _ in 0..8 {
            let output = self.run(["merge-base", "--all", target.as_str(), head.as_str()]);
            if let Ok(output) = output {
                let bases = parse_oid_lines(&output.stdout)?;
                if !bases.is_empty() {
                    return Ok(bases);
                }
            }
            let shallow = self.run_text(["rev-parse", "--is-shallow-repository"])? == "true";
            let Some(remote) = deepen_from.filter(|_| shallow) else {
                break;
            };
            if !self.deepen(remote, target)? {
                break;
            }
        }
        Err(Error::NoMergeBase {
            target: target.clone(),
            head: head.clone(),
        })
    }

    /// Deepens the shallow history by 200 commits from the shallow boundary and from `target`.
    ///
    /// When the remote does not know one of the wanted commits (an unpublished target, or a
    /// boundary commit force-pushed away), the commits are fetched one by one and the unknown ones
    /// are dropped.
    /// Returns whether any fetch succeeded.
    fn deepen(&self, remote: &str, target: &ObjectId) -> Result<bool, Error> {
        let shallow_file = self.run_text([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "shallow",
        ])?;
        let boundary = std::fs::read_to_string(&shallow_file).unwrap_or_default();
        let mut wants: Vec<String> = vec![target.as_str().to_owned()];
        wants.extend(
            boundary
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned),
        );
        if self.fetch_with(remote, &wants, &["--deepen=200"]).is_ok() {
            return Ok(true);
        }
        let mut deepened = false;
        for want in &wants {
            deepened |= self
                .fetch_with(remote, std::slice::from_ref(want), &["--deepen=200"])
                .is_ok();
        }
        Ok(deepened)
    }

    /// Lists the changed entries between two commits, with rename detection.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails or prints output the engine cannot parse.
    pub fn diff_tree(&self, base: &ObjectId, head: &ObjectId) -> Result<Vec<RawChange>, Error> {
        let output = self.run([
            "diff-tree",
            "-r",
            "-z",
            "--raw",
            "-M",
            "--no-abbrev",
            "--no-ext-diff",
            base.as_str(),
            head.as_str(),
        ])?;
        diff_tree::parse(&output.stdout)
    }

    /// Points `name` at `oid`, creating or overwriting the ref.
    ///
    /// # Errors
    ///
    /// Returns an error when git refuses the update.
    pub fn update_ref(&self, name: &str, oid: &ObjectId) -> Result<(), Error> {
        self.run(["update-ref", "--no-deref", name, oid.as_str()])?;
        Ok(())
    }

    /// Deletes the ref `name` if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails to delete an existing ref.
    pub fn delete_ref(&self, name: &str) -> Result<(), Error> {
        if self.ref_target(name)?.is_some() {
            self.run(["update-ref", "-d", name])?;
        }
        Ok(())
    }

    /// Lists the commits in `base..head`, newest first, at most 500.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails.
    pub fn commits(&self, base: &str, head: &str) -> Result<Vec<crate::review::Commit>, Error> {
        let range = format!("{base}..{head}");
        let output = self.run([
            "log",
            "--max-count=500",
            crate::review::LOG_FORMAT,
            &range,
            "--",
        ])?;
        Ok(crate::review::parse_log(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    /// Returns the object `name` points to, or `None` when the ref does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when git prints output the engine cannot parse.
    pub fn ref_target(&self, name: &str) -> Result<Option<ObjectId>, Error> {
        match self.run(["rev-parse", "--verify", "--quiet", name]) {
            Ok(output) => Ok(parse_oid_lines(&output.stdout)?.into_iter().next()),
            Err(Error::CommandFailed { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Lists every ref under `prefix` with its target.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails or prints output the engine cannot parse.
    pub fn refs_under(&self, prefix: &str) -> Result<Vec<RefEntry>, Error> {
        let output = self.run(["for-each-ref", "--format=%(refname) %(objectname)", prefix])?;
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .map(|line| {
                let (name, oid) = line
                    .rsplit_once(' ')
                    .ok_or_else(|| Error::UnexpectedOutput {
                        program: "git",
                        command: "for-each-ref".to_owned(),
                        detail: line.to_owned(),
                    })?;
                Ok(RefEntry {
                    name: name.to_owned(),
                    target: ObjectId::parse(oid)?,
                })
            })
            .collect()
    }

    /// Starts a long-lived `git cat-file --batch` reader.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be started.
    pub fn blob_reader(&self) -> Result<BlobReader, Error> {
        BlobReader::spawn(self)
    }

    /// Reads `linguist-generated` and `diff` attributes of `paths` from the `.gitattributes`
    /// files at commit `rev`.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails or prints output the engine cannot parse.
    pub fn generated_attributes(
        &self,
        rev: &ObjectId,
        paths: &[&str],
    ) -> Result<HashMap<String, bool>, Error> {
        use std::io::Write as _;

        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let mut child = self
            .command()
            .args(["check-attr", "-z", "--stdin", "--source", rev.as_str()])
            .args(["linguist-generated", "diff"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn {
                program: "git",
                args: "check-attr".to_owned(),
                source,
            })?;
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_bytes());
            input.push(0);
        }
        // Feed stdin from a thread so a large path list cannot deadlock against stdout.
        let stdin = child.stdin.take();
        let writer = std::thread::spawn(move || -> std::io::Result<()> {
            if let Some(mut stdin) = stdin {
                stdin.write_all(&input)?;
            }
            Ok(())
        });
        let output = child.wait_with_output().map_err(|source| Error::Spawn {
            program: "git",
            args: "check-attr".to_owned(),
            source,
        })?;
        let written = writer.join().map_err(|_| Error::UnexpectedOutput {
            program: "git",
            command: "check-attr".to_owned(),
            detail: "stdin writer panicked".to_owned(),
        })?;
        if let Err(source) = written {
            return Err(Error::Spawn {
                program: "git",
                args: "check-attr".to_owned(),
                source,
            });
        }
        if !output.status.success() {
            return Err(Error::CommandFailed {
                program: "git",
                args: "check-attr".to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(parse_generated_attributes(&output.stdout))
    }

    /// Fetches `refspecs` from `remote`, updating no ref outside the refspecs' destinations and
    /// not touching `FETCH_HEAD`.
    ///
    /// # Errors
    ///
    /// Returns an error when the fetch fails.
    pub fn fetch(&self, remote: &str, refspecs: &[String]) -> Result<(), Error> {
        self.fetch_with(remote, refspecs, &[])
    }

    /// Fetches exactly `refspecs`: no tags, no `FETCH_HEAD`, and an empty `--refmap` so the
    /// remote's configured fetch refspecs never update remote-tracking refs.
    fn fetch_with(&self, remote: &str, refspecs: &[String], options: &[&str]) -> Result<(), Error> {
        let mut args: Vec<String> = [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
        ]
        .into_iter()
        .chain(options.iter().copied())
        .map(str::to_owned)
        .collect();
        args.push(remote.to_owned());
        args.extend(refspecs.iter().cloned());
        self.run(args)?;
        Ok(())
    }

    /// Returns the remote to deepen a shallow clone from: `origin`, or else the first remote.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails.
    pub fn default_remote(&self) -> Result<Option<String>, Error> {
        let remotes = self.remotes()?;
        let origin = remotes.iter().find(|remote| remote.name == "origin");
        Ok(origin.or(remotes.first()).map(|remote| remote.name.clone()))
    }

    /// Lists the configured remotes with their fetch URLs.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails.
    pub fn remotes(&self) -> Result<Vec<Remote>, Error> {
        let output = self.run(["remote", "-v"])?;
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(text
            .lines()
            .filter(|line| line.ends_with("(fetch)"))
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                Some(Remote {
                    name: parts.next()?.to_owned(),
                    url: parts.next()?.to_owned(),
                })
            })
            .collect())
    }

    /// Returns the current branch name, or `None` on a detached HEAD.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails.
    pub fn current_branch(&self) -> Result<Option<String>, Error> {
        let name = self.run_text(["branch", "--show-current"])?;
        Ok((!name.is_empty()).then_some(name))
    }

    /// Creates a detached worktree at `path` checked out at `commit`.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot create the worktree.
    pub fn add_worktree(&self, path: &Path, commit: &ObjectId) -> Result<(), Error> {
        let mut args: Vec<&OsStr> = vec![OsStr::new("worktree"), OsStr::new("add")];
        args.extend([OsStr::new("--detach"), OsStr::new("--quiet")]);
        args.extend([path.as_os_str(), OsStr::new(commit.as_str())]);
        self.run(args)?;
        Ok(())
    }

    /// Removes the worktree at `path`, discarding it even if files were changed there.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot remove the worktree.
    pub fn remove_worktree(&self, path: &Path) -> Result<(), Error> {
        let args = [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            path.as_os_str(),
        ];
        self.run(args)?;
        Ok(())
    }
}

fn parse_oid_lines(stdout: &[u8]) -> Result<Vec<ObjectId>, Error> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| ObjectId::parse(line.trim()).map_err(Error::from))
        .collect()
}

/// Parses `check-attr -z` output (`path NUL attribute NUL value NUL`) into whether each path is
/// generated: `linguist-generated` set or true, or `diff` unset.
fn parse_generated_attributes(stdout: &[u8]) -> HashMap<String, bool> {
    let mut generated: HashMap<String, bool> = HashMap::new();
    let fields: Vec<&[u8]> = stdout.split(|&byte| byte == 0).collect();
    let (records, _) = fields.as_chunks::<3>();
    for [path, attribute, value] in records {
        let path = String::from_utf8_lossy(path).into_owned();
        let is_generated = matches!(
            (*attribute, *value),
            (b"linguist-generated", b"set" | b"true") | (b"diff", b"unset")
        );
        *generated.entry(path).or_default() |= is_generated;
    }
    generated
}
