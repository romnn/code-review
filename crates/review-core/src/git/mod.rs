//! The git layer: every repository access goes through the `git` CLI.
//!
//! It produces verified, immutable inputs (object IDs and blob contents); everything after it is
//! pure analysis over object IDs.

mod blobs;
mod diff_tree;
mod filters;
mod ignore;
mod refs;
mod snapshot;
mod state;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use blobs::{Blob, BlobReader, MAX_TEXT_BLOB_BYTES};
pub use diff_tree::{ChangeStatus, EntryType, FileMode, RawChange, RawSide};
pub use snapshot::WorktreeCapture;
pub(crate) use state::IndexStamp;
pub use state::{BranchIdentity, Busy, Head};

use crate::error::Error;
use crate::ids::ObjectId;

/// Configuration every git command the engine runs gets, as `-c` options.
///
/// Hooks and a filesystem monitor are programs the repository's configuration names, and the
/// engine never runs them: it only reads the repository and writes its own objects and refs.
pub(crate) const COMMAND_CONFIG: [&str; 4] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
];

/// Environment every git command the engine runs gets.
///
/// Replace refs would make an object ID stand for other content than its own, so the engine
/// reads objects as they are.
/// Git tries to fetch none of the objects a partial clone lacks, so reading one fails at once.
/// Git never asks for credentials on the terminal itself, and messages stay parseable.
pub(crate) const COMMAND_ENV: [(&str, &str); 4] = [
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("LC_ALL", "C"),
];

/// Environment of every command that reaches a remote for a repository whose commands may not
/// prompt: an empty `GIT_ASKPASS`, which git asks for credentials before `core.askPass` and
/// `SSH_ASKPASS` and which stands in for both, and no askpass helper for ssh, which asks one
/// when it has no terminal.
const NO_PROMPT_ENV: [(&str, &str); 2] = [("GIT_ASKPASS", ""), ("SSH_ASKPASS_REQUIRE", "never")];

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
    /// Whether those commands may not prompt; see [`Self::forbid_prompts`].
    no_prompts: bool,
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

/// Runs `command` in a session of its own, which has no controlling terminal: opening
/// `/dev/tty` fails in it and in everything it runs.
#[cfg(unix)]
fn without_terminal(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: between fork and exec, the child only calls `setsid`, which is async-signal-safe,
    // and builds its error without allocating.
    #[expect(
        unsafe_code,
        reason = "std has no stable way to start a process in a session of its own"
    )]
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()
                .map(drop)
                .map_err(std::io::Error::from)
        });
    }
}

/// Starts `command` without a console, so that neither it nor what it runs can prompt on the
/// app's.
///
/// `DETACHED_PROCESS` rather than `CREATE_NO_WINDOW`, which gives the child a console of its
/// own, only hidden, that ssh would prompt on and wait for unseen.
/// Git for Windows starts its own children, ssh among them, detached in turn when it has no
/// console (`compat/mingw.c`), so none opens a console window either.
#[cfg(windows)]
fn without_terminal(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    /// The process creation flag that starts a console program without its parent's console.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(DETACHED_PROCESS);
}

impl Repo {
    /// Opens the repository containing `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` is not inside a git repository or `git` cannot run.
    #[tracing::instrument(skip_all)]
    pub fn discover(path: &Path) -> Result<Self, Error> {
        let probe = Self {
            work_tree: Some(path.to_path_buf()),
            git_dir: PathBuf::new(),
            common_dir: PathBuf::new(),
            env: Vec::new(),
            no_prompts: false,
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
            no_prompts: false,
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

    /// Runs every command for the repository that reaches a remote from now on, its fetches and
    /// `gh`, so that nothing it runs can prompt.
    ///
    /// On Unix each runs in a session of its own, without the controlling terminal that git, ssh
    /// and credential helpers prompt on, whatever command the user set to run ssh.
    /// None asks an askpass helper either, which would prompt in a window.
    /// A command that would have to ask, such as a fetch that needs a passphrase, fails instead.
    /// No other command can prompt, since none reaches a remote (see [`Self::fetch`]).
    pub fn forbid_prompts(&mut self) {
        self.no_prompts = true;
    }

    /// Prepares `command`, which reaches a remote, as [`Self::forbid_prompts`] has it.
    pub(crate) fn prepare_remote(&self, command: &mut Command) {
        if self.no_prompts {
            command.envs(NO_PROMPT_ENV);
            without_terminal(command);
        }
    }

    /// Returns a `git` command bound to this repository, with [`COMMAND_CONFIG`] and
    /// [`COMMAND_ENV`] applied, that may use no transport.
    ///
    /// It reaches no remote whatever git's version and configuration, not even for an object a
    /// partial clone lacks, which some versions and paths fetch despite `GIT_NO_LAZY_FETCH`.
    pub(crate) fn command(&self) -> Command {
        let mut command = self.git();
        command.env("GIT_ALLOW_PROTOCOL", "");
        command
    }

    /// Returns a `git` command bound to this repository, with [`COMMAND_CONFIG`] and
    /// [`COMMAND_ENV`] applied, that may use the transports the user allows.
    fn git(&self) -> Command {
        let mut command = Command::new("git");
        match &self.work_tree {
            Some(work_tree) => command.arg("-C").arg(work_tree),
            None => command.arg("--git-dir").arg(&self.git_dir),
        };
        command.args(COMMAND_CONFIG).envs(COMMAND_ENV);
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
            let Some(remote) = deepen_from else {
                break;
            };
            if !self.is_shallow()? {
                break;
            }
            if !self.deepen(remote, target)? {
                break;
            }
        }
        Err(Error::NoMergeBase {
            target: target.clone(),
            head: head.clone(),
        })
    }

    /// Returns whether the clone is shallow: its history stops at commits whose parents it does
    /// not have.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run.
    pub fn is_shallow(&self) -> Result<bool, Error> {
        Ok(self.run_text(["rev-parse", "--is-shallow-repository"])? == "true")
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

    /// Returns whether `ancestor` is `descendant` or one of its ancestors; an object that does
    /// not resolve counts as not.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot be run.
    pub fn is_ancestor(&self, ancestor: &ObjectId, descendant: &str) -> Result<bool, Error> {
        match self.run(["merge-base", "--is-ancestor", ancestor.as_str(), descendant]) {
            Ok(_) => Ok(true),
            Err(Error::CommandFailed { .. }) => Ok(false),
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

    /// Returns the size in bytes of the object `oid`, without reading its content, which a
    /// [`BlobReader`] streams through its pipe even for a blob it does not keep.
    ///
    /// # Errors
    ///
    /// Returns an error when the object does not exist or git prints no size.
    pub fn blob_size(&self, oid: &ObjectId) -> Result<u64, Error> {
        let text = self.run_text(["cat-file", "-s", oid.as_str()])?;
        text.parse().map_err(|_| Error::UnexpectedOutput {
            program: "git",
            command: "cat-file -s".to_owned(),
            detail: text,
        })
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
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let output = self.run_with_paths(
            &[
                "check-attr",
                "-z",
                "--stdin",
                "--source",
                rev.as_str(),
                "linguist-generated",
                "diff",
            ],
            paths.iter().copied(),
            &[0],
        )?;
        Ok(parse_generated_attributes(&output.stdout))
    }

    /// Runs a git command that reads NUL-terminated `paths` on standard input, and returns its
    /// standard output when it exits with one of the `success` codes.
    fn run_with_paths<'a>(
        &self,
        args: &[&str],
        paths: impl IntoIterator<Item = &'a str>,
        success: &[i32],
    ) -> Result<Output, Error> {
        Self::run_command_with_paths(
            self.command(),
            args,
            paths.into_iter().map(str::as_bytes),
            success,
        )
    }

    /// Runs `command` with `args` like [`Self::run_with_paths`], with paths given as bytes.
    fn run_command_with_paths<'a>(
        command: Command,
        args: &[&str],
        paths: impl IntoIterator<Item = &'a [u8]>,
        success: &[i32],
    ) -> Result<Output, Error> {
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path);
            input.push(0);
        }
        Self::run_command_with_input(command, args, input, success)
    }

    /// Runs `command` with `args`, writing `input` to its standard input, and returns its
    /// standard output when it exits with one of the `success` codes.
    fn run_command_with_input(
        mut command: Command,
        args: &[&str],
        input: Vec<u8>,
        success: &[i32],
    ) -> Result<Output, Error> {
        use std::io::Write as _;

        let shown = args.join(" ");
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn {
                program: "git",
                args: shown.clone(),
                source,
            })?;
        // Feed stdin from a thread so a large input cannot deadlock against stdout.
        let stdin = child.stdin.take();
        let writer = std::thread::spawn(move || -> std::io::Result<()> {
            if let Some(mut stdin) = stdin {
                stdin.write_all(&input)?;
            }
            Ok(())
        });
        let output = child.wait_with_output().map_err(|source| Error::Spawn {
            program: "git",
            args: shown.clone(),
            source,
        })?;
        let written = writer.join().map_err(|_| Error::UnexpectedOutput {
            program: "git",
            command: shown.clone(),
            detail: "stdin writer panicked".to_owned(),
        })?;
        if let Err(source) = written {
            return Err(Error::Spawn {
                program: "git",
                args: shown,
                source,
            });
        }
        if !output
            .status
            .code()
            .is_some_and(|code| success.contains(&code))
        {
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

    /// Fetches `refspecs` from `remote`, updating no ref outside the refspecs' destinations and
    /// not touching `FETCH_HEAD`.
    ///
    /// Fetches are the only git commands that reach a remote, over the transports the user
    /// allows, and run as [`Self::forbid_prompts`] has it.
    ///
    /// # Errors
    ///
    /// Returns an error when the fetch fails.
    pub fn fetch(&self, remote: &str, refspecs: &[String]) -> Result<(), Error> {
        self.fetch_with(remote, refspecs, &[])
    }

    /// Fetches exactly `refspecs`: no tags, no `FETCH_HEAD`, no submodules, and an empty
    /// `--refmap` so the remote's configured fetch refspecs never update remote-tracking refs.
    fn fetch_with(&self, remote: &str, refspecs: &[String], options: &[&str]) -> Result<(), Error> {
        let mut args: Vec<String> = [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--refmap=",
        ]
        .into_iter()
        .chain(options.iter().copied())
        .map(str::to_owned)
        .collect();
        args.push(remote.to_owned());
        args.extend(refspecs.iter().cloned());
        let mut command = self.git();
        self.prepare_remote(&mut command);
        Self::run_command(command, args)?;
        Ok(())
    }

    /// Returns the repository's default remote, which a shallow clone deepens from and whose
    /// default branch is the repository's: `origin`, or else the first remote.
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

    /// Lists the configured remotes with their URLs as configured, before any `insteadOf`
    /// rewrite, which [`Self::remotes`] applies.
    ///
    /// # Errors
    ///
    /// Returns an error when git fails.
    pub(crate) fn configured_remote_urls(&self) -> Result<Vec<Remote>, Error> {
        // `git config --get-regexp` exits with 1 when nothing matches.
        let output = Self::run_command_with_paths(
            self.command(),
            &["config", "-z", "--get-regexp", r"^remote\..*\.url$"],
            [],
            &[0, 1],
        )?;
        // Records of `key NL value NUL`, with the remote's name, which may hold dots, between
        // `remote.` and `.url`.
        Ok(output
            .stdout
            .split(|&byte| byte == 0)
            .filter_map(|record| {
                let text = std::str::from_utf8(record).ok()?;
                let (key, url) = text.split_once('\n')?;
                let name = key.strip_prefix("remote.")?.strip_suffix(".url")?;
                Some(Remote {
                    name: name.to_owned(),
                    url: url.to_owned(),
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
    /// `None` when there is none to find; see [`Self::default_branch_ref`].
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run or prints output the engine cannot parse.
    pub fn default_branch(&self) -> Result<Option<String>, Error> {
        Ok(self.default_branch_ref()?.map(|name| {
            name.strip_prefix("refs/remotes/")
                .or_else(|| name.strip_prefix("refs/heads/"))
                .unwrap_or(&name)
                .to_owned()
        }))
    }

    /// Returns the full ref of the repository's default branch, such as
    /// `refs/remotes/origin/main`, or `None` when there is none to find.
    ///
    /// It is the branch `<remote>/HEAD` points to, or else the first of `<remote>/main`,
    /// `<remote>/master`, `main` and `master` that exists, where `<remote>` is the
    /// [default remote](Self::default_remote), or `origin` without one.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run or prints output the engine cannot parse.
    pub fn default_branch_ref(&self) -> Result<Option<String>, Error> {
        // Remote-tracking refs left without their remote are `origin`'s, if anyone's.
        let remote = self
            .default_remote()?
            .unwrap_or_else(|| "origin".to_owned());
        let remote_head = match self.run_text([
            "symbolic-ref",
            "--quiet",
            &format!("refs/remotes/{remote}/HEAD"),
        ]) {
            Ok(target) => Some(target),
            // Not set, or not a symbolic ref.
            Err(Error::CommandFailed { .. }) => None,
            Err(err) => return Err(err),
        };
        let candidates = remote_head.into_iter().chain(
            [
                format!("refs/remotes/{remote}/main"),
                format!("refs/remotes/{remote}/master"),
            ]
            .into_iter()
            .chain(["refs/heads/main", "refs/heads/master"].map(str::to_owned)),
        );
        for name in candidates {
            // A dangling `<remote>/HEAD` falls through to the fixed names.
            if self.ref_target(&format!("{name}^{{commit}}"))?.is_some() {
                return Ok(Some(name));
            }
        }
        Ok(None)
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

#[cfg(all(test, unix))]
mod tests {
    use color_eyre::eyre;

    use super::*;
    use crate::testing;

    /// A command that reaches a remote, for a repository whose commands may not prompt, has git
    /// ask no askpass helper for credentials, not even one `core.askPass` names, which git asks
    /// with its own prompt off, and fail instead.
    #[test]
    fn forbidden_prompts_ask_no_helper() -> eyre::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let repo = testing::committed(&[("a.rs", "fn a() {}\n")])?;
        let asked = repo.path().join("asked");
        let helper = repo.path().join("askpass");
        let script = indoc::formatdoc! {"
            #!/bin/sh
            touch '{asked}'
            echo secret
        ", asked = asked.display()};
        std::fs::write(&helper, script)?;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))?;
        let helper = helper.display().to_string();
        testing::git(repo.path(), &["config", "core.askPass", &helper])?;
        let mut git = Repo::discover(repo.path())?;
        git.set_command_env([
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ]);
        git.forbid_prompts();

        let mut fill = git.command();
        git.prepare_remote(&mut fill);
        let mut fill = fill
            .args(["credential", "fill"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        fill.stdin
            .take()
            .ok_or_else(|| eyre::eyre!("no stdin"))?
            .write_all(b"protocol=https\nhost=example.invalid\n\n")?;
        let output = fill.wait_with_output()?;
        assert!(!output.status.success(), "{output:?}");
        assert!(!asked.exists(), "the helper was asked");
        Ok(())
    }

    /// A blob's size comes without its content, and an object that does not exist has none.
    #[test]
    fn a_blob_size_needs_no_content() -> eyre::Result<()> {
        let repo = testing::committed(&[("a.rs", "fn a() {}\n")])?;
        let git = Repo::discover(repo.path())?;
        let blob = ObjectId::parse(&testing::git(repo.path(), &["rev-parse", "HEAD:a.rs"])?)?;
        assert_eq!(git.blob_size(&blob)?, 10);
        let missing = ObjectId::parse(&"1".repeat(40))?;
        assert!(git.blob_size(&missing).is_err());
        Ok(())
    }
}
