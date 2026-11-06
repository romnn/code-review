//! The git layer: every repository access goes through the `git` CLI.
//!
//! It produces verified, immutable inputs (object IDs and blob contents); everything after it is
//! pure analysis over object IDs.

mod blobs;
mod diff_tree;
mod snapshot;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use blobs::{Blob, BlobReader, MAX_TEXT_BLOB_BYTES};
pub use diff_tree::{ChangeStatus, EntryType, FileMode, RawChange, RawSide};

use crate::error::Error;
use crate::ids::ObjectId;

/// A file of a commit's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFile {
    /// Its path from the root of the tree.
    pub path: String,
    /// Its blob.
    pub oid: ObjectId,
}

/// A line that `git grep` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepLine {
    /// The file's path from the root of the tree.
    pub path: String,
    /// The zero-based line number.
    pub line: usize,
    /// The line's text, without its terminator.
    pub text: String,
}

/// A git repository, addressed by its working tree or git directory.
#[derive(Debug, Clone)]
pub struct Repo {
    work_tree: Option<PathBuf>,
    git_dir: PathBuf,
    /// The directory holding objects and refs, shared by all worktrees of one clone.
    common_dir: PathBuf,
    /// Variables added to the environment of every command run for the repository; see
    /// [`Self::set_command_env`].
    env: Vec<(OsString, OsString)>,
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
            env: Vec::new(),
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
            env: Vec::new(),
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

    /// Sets `vars` in the environment of every `git` and `gh` command run for the repository
    /// from now on, replacing the variables set before, on top of the process's own
    /// environment.
    ///
    /// A front end that draws on the terminal sets the variables that keep `ssh` and credential
    /// helpers from prompting on it, once the review is open.
    pub fn set_command_env<K, V>(&mut self, vars: impl IntoIterator<Item = (K, V)>)
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env = vars
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
    }

    /// Adds the variables of [`Self::set_command_env`] to `command`'s environment.
    pub(crate) fn apply_env(&self, command: &mut Command) {
        command.envs(self.env.iter().map(|(key, value)| (key, value)));
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
        self.apply_env(&mut command);
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

    /// Lists every blob of the tree of `commit` (regular files and symlinks) with its object.
    /// Paths that are not valid UTF-8 are left out, since nothing could name them.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails or prints output the engine cannot parse.
    pub fn tree_files(&self, commit: &ObjectId) -> Result<Vec<TreeFile>, Error> {
        let output = self.run(["ls-tree", "-r", "-z", "--full-tree", commit.as_str()])?;
        let unexpected = |detail: &str| Error::UnexpectedOutput {
            program: "git",
            command: "ls-tree".to_owned(),
            detail: detail.to_owned(),
        };
        let mut files = Vec::new();
        for entry in output
            .stdout
            .split(|&byte| byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            // `<mode> <type> <object>\t<path>`; only the path may be other than ASCII.
            let tab = entry
                .iter()
                .position(|&byte| byte == b'\t')
                .ok_or_else(|| unexpected(&String::from_utf8_lossy(entry)))?;
            let (meta, path) = entry.split_at(tab);
            let meta = std::str::from_utf8(meta).map_err(|err| unexpected(&err.to_string()))?;
            let Ok(path) = std::str::from_utf8(path.get(1..).unwrap_or_default()) else {
                continue;
            };
            let mut fields = meta.split(' ');
            let (Some(_mode), Some(kind), Some(oid)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(unexpected(meta));
            };
            if kind != "blob" {
                continue;
            }
            files.push(TreeFile {
                path: path.to_owned(),
                oid: ObjectId::parse(oid).map_err(|err| unexpected(&err.to_string()))?,
            });
        }
        Ok(files)
    }

    /// Finds the lines of the tree of `commit` that contain `needle` as a fixed string, in path
    /// and line order, ignoring case when `ignore_case` is set. Binary files and paths that are
    /// not valid UTF-8 are skipped. Stops after `limit` lines, and once `cancel` is set, which
    /// kills git within a few milliseconds even while it prints nothing; a stopped search
    /// returns what it found so far.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run, or fails other than by finding nothing.
    pub fn grep(
        &self,
        commit: &ObjectId,
        needle: &str,
        ignore_case: bool,
        limit: usize,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<GrepLine>, Error> {
        use std::io::{BufRead as _, Read as _};
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut command = self.command();
        command.args(["grep", "-n", "-z", "-I", "--no-color", "--fixed-strings"]);
        if ignore_case {
            command.arg("--ignore-case");
        }
        command
            .arg("-e")
            .arg(needle)
            .arg(commit.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let spawn_error = |source| Error::Spawn {
            program: "git",
            args: "grep".to_owned(),
            source,
        };
        let mut child = command.spawn().map_err(spawn_error)?;
        let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
            child.kill().ok();
            child.wait().ok();
            return Err(spawn_error(std::io::Error::other("no pipes to git grep")));
        };
        let child = Mutex::new(child);
        let stop = |child: &Mutex<std::process::Child>| {
            if let Ok(mut child) = child.lock() {
                child.kill().ok();
            }
        };
        let finished = AtomicBool::new(false);
        let stopped = AtomicBool::new(false);
        let prefix = format!("{commit}:");
        let mut found = Vec::new();
        let errors = std::thread::scope(|scope| {
            let errors = scope.spawn(move || {
                let mut text = String::new();
                stderr.read_to_string(&mut text).ok();
                text
            });
            // Cancelling kills git even while it is silent, between lines.
            scope.spawn(|| {
                while !finished.load(Ordering::Relaxed) {
                    if cancel.load(Ordering::Relaxed) {
                        stopped.store(true, Ordering::Relaxed);
                        stop(&child);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            });
            for line in std::io::BufReader::new(stdout).split(b'\n') {
                if cancel.load(Ordering::Relaxed) || found.len() >= limit {
                    stopped.store(true, Ordering::Relaxed);
                    stop(&child);
                    break;
                }
                let Ok(line) = line else {
                    break;
                };
                if let Some(hit) = grep_line(&line, &prefix) {
                    found.push(hit);
                }
            }
            finished.store(true, Ordering::Relaxed);
            errors.join().unwrap_or_default()
        });
        let status = child
            .into_inner()
            .map_err(|_| spawn_error(std::io::Error::other("git grep's handle was poisoned")))?
            .wait()
            .map_err(spawn_error)?;
        // Exit status 1 means nothing was found; a search stopped on purpose is not a failure.
        if stopped.load(Ordering::Relaxed) || status.success() || status.code() == Some(1) {
            return Ok(found);
        }
        Err(Error::CommandFailed {
            program: "git",
            args: format!("grep {needle}"),
            stderr: errors.trim().to_owned(),
        })
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

    /// Returns the value of the git configuration `key` for the repository, such as
    /// `core.sshCommand`, or `None` when it is not set.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run.
    pub fn config_value(&self, key: &str) -> Result<Option<String>, Error> {
        match self.run_text(["config", "--get", key]) {
            Ok(value) => Ok(Some(value)),
            // `git config --get` fails for a key that is not set.
            Err(Error::CommandFailed { .. }) => Ok(None),
            Err(err) => Err(err),
        }
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

    /// Returns the short name of the repository's default branch, such as `origin/main`, or
    /// `None` when there is none to find.
    ///
    /// It is the branch `refs/remotes/origin/HEAD` points to, or else the first of `origin/main`,
    /// `origin/master`, `main` and `master` that exists.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run or prints output the engine cannot parse.
    pub fn default_branch(&self) -> Result<Option<String>, Error> {
        let origin_head =
            match self.run_text(["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"]) {
                Ok(target) => Some(target),
                // Not set, or not a symbolic ref.
                Err(Error::CommandFailed { .. }) => None,
                Err(err) => return Err(err),
            };
        let candidates = origin_head.into_iter().chain(
            [
                "refs/remotes/origin/main",
                "refs/remotes/origin/master",
                "refs/heads/main",
                "refs/heads/master",
            ]
            .map(str::to_owned),
        );
        for name in candidates {
            // A dangling `origin/HEAD` falls through to the fixed names.
            if self.ref_target(&format!("{name}^{{commit}}"))?.is_some() {
                let short = name
                    .strip_prefix("refs/remotes/")
                    .or_else(|| name.strip_prefix("refs/heads/"))
                    .unwrap_or(&name);
                return Ok(Some(short.to_owned()));
            }
        }
        Ok(None)
    }

    /// Counts the commits reachable from `head` but not from `base`.
    ///
    /// # Errors
    ///
    /// Returns an error when either does not name a commit or git fails.
    pub fn count_commits(&self, base: &str, head: &str) -> Result<usize, Error> {
        let count = self.run_text(["rev-list", "--count", &format!("{base}..{head}"), "--"])?;
        count.parse().map_err(|_| Error::UnexpectedOutput {
            program: "git",
            command: "rev-list --count".to_owned(),
            detail: count,
        })
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

/// One line of `git grep -n -z` output, `<commit>:<path>\0<line>\0<text>`, as a hit; `None`
/// for a line that is not one, or whose path is not valid UTF-8.
fn grep_line(line: &[u8], prefix: &str) -> Option<GrepLine> {
    let mut parts = line.splitn(3, |&byte| byte == 0);
    let (path, number, text) = (parts.next()?, parts.next()?, parts.next()?);
    let path = std::str::from_utf8(path).ok()?.strip_prefix(prefix)?;
    let number: usize = std::str::from_utf8(number).ok()?.parse().ok()?;
    Some(GrepLine {
        path: path.to_owned(),
        line: number.saturating_sub(1),
        text: String::from_utf8_lossy(text)
            .trim_end_matches('\r')
            .to_owned(),
    })
}
