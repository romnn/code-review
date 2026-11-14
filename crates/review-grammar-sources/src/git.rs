//! git, run so that nothing of the caller's repository reaches it.
//!
//! A git hook may run cargo with `GIT_DIR`, `GIT_INDEX_FILE` and the like set, and a build
//! script inherits them, so every command here clears them, names its repository explicitly,
//! and runs no hooks.
//! Every command is bounded by [`TIMEOUT`].

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::Error;

/// How long one `git` command may run before it and its helpers are killed.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(600);

/// The variables that point git at a repository, which are cleared even when git cannot list
/// its own with `git rev-parse --local-env-vars`.
const LOCAL: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

/// git with the caller's repository variables cleared.
#[derive(Debug, Clone)]
pub(crate) struct Git {
    clear: Vec<OsString>,
}

impl Git {
    /// git, clearing [`LOCAL`] and whatever else `git rev-parse --local-env-vars`, run in
    /// `directory`, lists.
    pub(crate) fn new(directory: &Path) -> Result<Self, Error> {
        let mut git = Self {
            clear: LOCAL.iter().map(OsString::from).collect(),
        };
        let listed = git.run(
            directory,
            &[OsStr::new("rev-parse"), OsStr::new("--local-env-vars")],
            None,
        )?;
        for name in String::from_utf8_lossy(&listed).lines() {
            let name = OsString::from(name.trim());
            if !name.is_empty() && !git.clear.contains(&name) {
                git.clear.push(name);
            }
        }
        Ok(git)
    }

    /// Clears the caller's repository variables from `command`, and keeps git from asking for
    /// credentials, so a private or missing repository fails instead of waiting for an answer.
    pub(crate) fn isolate(&self, command: &mut Command) {
        for name in &self.clear {
            command.env_remove(name);
        }
        command.env("GIT_TERMINAL_PROMPT", "0");
    }

    /// Runs git with `args` in `directory` within [`TIMEOUT`], writing its standard output to
    /// `stdout` when given and returning it otherwise.
    fn run(
        &self,
        directory: &Path,
        args: &[&OsStr],
        stdout: Option<std::fs::File>,
    ) -> Result<Vec<u8>, Error> {
        let text = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let io = |action| {
            let path = directory.to_owned();
            move |source| Error::Io {
                action,
                path,
                source,
            }
        };
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(directory)
            .stdin(Stdio::null())
            .stdout(stdout.map_or_else(Stdio::piped, Stdio::from))
            .stderr(Stdio::piped());
        self.isolate(&mut command);
        // git leads a process group of its own, so a timeout kills its helpers with it.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().map_err(io("run git in"))?;
        // The pipes are drained while git runs, so a large output cannot stall it.
        let output = child.stdout.take().map(drain);
        let errors = child.stderr.take().map(drain);
        let started = Instant::now();
        let mut pause = Duration::from_millis(1);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < TIMEOUT => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(50));
                }
                Ok(None) => {
                    kill(&mut child);
                    return Err(Error::Timeout { command: text });
                }
                Err(source) => {
                    kill(&mut child);
                    return Err(io("wait for git in")(source));
                }
            }
        };
        let output = collect(output).map_err(io("read git's output in"))?;
        let errors = collect(errors).map_err(io("read git's errors in"))?;
        if !status.success() {
            return Err(Error::Git {
                command: text,
                stderr: String::from_utf8_lossy(&errors).trim().to_owned(),
            });
        }
        Ok(output)
    }
}

/// A bare repository of its own, which a fetch fills and files are read from.
#[derive(Debug)]
pub(crate) struct Repository {
    git: Git,
    directory: PathBuf,
}

impl Repository {
    /// Creates an empty bare repository at `directory`, without the hooks of any template, with
    /// SHA-1 object ids whatever the user's default, since descriptors record SHA-1 blob ids.
    pub(crate) fn init(directory: &Path) -> Result<Self, Error> {
        let parent = directory.parent().unwrap_or(directory);
        let git = Git::new(parent)?;
        git.run(
            parent,
            &[
                OsStr::new("init"),
                OsStr::new("--bare"),
                OsStr::new("--quiet"),
                OsStr::new("--template="),
                OsStr::new("--object-format=sha1"),
                directory.as_os_str(),
            ],
            None,
        )?;
        Ok(Self {
            git,
            directory: directory.to_owned(),
        })
    }

    /// Runs git with `args` in the repository and returns its standard output.
    pub(crate) fn output(&self, args: &[&str]) -> Result<String, Error> {
        let output = self.git.run(&self.directory, &self.args(args), None)?;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }

    /// Runs git with `args` in the repository, writing its standard output to `file`.
    pub(crate) fn output_to(&self, args: &[&str], file: std::fs::File) -> Result<(), Error> {
        self.git
            .run(&self.directory, &self.args(args), Some(file))
            .map(|_| ())
    }

    /// `args` after the options every command takes: the repository, no hooks, pathspecs
    /// taken literally, and an abort when a transfer stalls, which git's helpers honor.
    fn args<'a>(&'a self, args: &'a [&'a str]) -> Vec<&'a OsStr> {
        let mut all = vec![
            OsStr::new("-c"),
            OsStr::new("core.hooksPath=/nonexistent/code-review-hooks"),
            OsStr::new("-c"),
            OsStr::new("http.lowSpeedLimit=1000"),
            OsStr::new("-c"),
            OsStr::new("http.lowSpeedTime=60"),
            OsStr::new("--git-dir"),
            self.directory.as_os_str(),
            OsStr::new("--literal-pathspecs"),
        ];
        all.extend(args.iter().map(OsStr::new));
        all
    }
}

/// Reads all of `pipe` on a thread of its own.
fn drain(mut pipe: impl Read + Send + 'static) -> JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).map(|_| bytes)
    })
}

/// What [`drain`] read, or nothing for a pipe that was not taken.
fn collect(reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>) -> std::io::Result<Vec<u8>> {
    match reader.map(JoinHandle::join) {
        None => Ok(Vec::new()),
        Some(Ok(read)) => read,
        Some(Err(_)) => Err(std::io::Error::other(
            "the thread reading git's output panicked",
        )),
    }
}

/// Kills git and the helpers it started, such as `git-remote-https`, and reaps git; the system
/// reaps the helpers, which are not this process's children.
///
/// Killing is best effort: a process that already exited cannot be killed, and the caller
/// reports the timeout either way.
fn kill(child: &mut Child) {
    #[cfg(unix)]
    {
        // git leads its process group, so the group's id is git's.
        let group = rustix::process::Pid::from_child(child);
        rustix::process::kill_process_group(group, rustix::process::Signal::KILL).ok();
    }
    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/F", "/T", "/PID"])
            .arg(child.id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
    }
    child.kill().ok();
    child.wait().ok();
}
