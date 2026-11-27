//! A minimal, synchronous language server client over stdio: `initialize`, `didOpen`,
//! `textDocument/definition` and `didClose`, which is all go-to-definition needs.
//!
//! It is written for this app rather than copied from Zed's `lsp` crate, which is asynchronous,
//! tied to Zed's executor and handles far more of the protocol than the slice uses.
//! Calls block, so front ends run them off the UI thread.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};

/// A language server failure.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// The server could not be started.
    #[error("failed to start language server {command}")]
    Spawn {
        /// The command that failed.
        command: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Reading from or writing to the server failed.
    #[error("language server i/o failed")]
    Io(#[from] std::io::Error),
    /// The server sent something that is not a JSON-RPC message.
    #[error("language server sent an invalid message: {0}")]
    Protocol(String),
    /// The server answered a request with an error.
    #[error("language server error {code}: {message}")]
    Response {
        /// The JSON-RPC error code.
        code: i64,
        /// The error message.
        message: String,
    },
    /// The server found no definition before the deadline, for example while still indexing.
    #[error("no definition found")]
    NotFound,
    /// The server did not answer in time.
    #[error("the language server did not answer in time")]
    Timeout,
    /// The server was stopped, as when trust was revoked, while it was waited on.
    #[error("the language server was stopped")]
    Stopped,
}

/// How often a wait for the server looks whether it was stopped meanwhile.
const STOP_POLL: Duration = Duration::from_millis(50);

/// How long a server asked to exit may take before it is killed.
const EXIT_DEADLINE: Duration = Duration::from_secs(2);

/// The id of the `shutdown` request that asks a server to exit, which no other request takes:
/// theirs start at 1.
const SHUTDOWN_ID: i64 = 0;

/// Stops a [`LanguageServer`] from another thread, without the lock of whoever uses it, as
/// [`end`] does, and fails a wait for it, such as the `initialize` handshake, at once.
#[derive(Clone)]
pub struct Stopper {
    child: Arc<Mutex<Option<Child>>>,
    stopped: Arc<AtomicBool>,
    outgoing: mpsc::Sender<Outgoing>,
    exit_deadline: Duration,
}

impl Stopper {
    /// Stops the server and fails any wait for it, so once this returns the server is gone,
    /// whoever still holds it; see [`end`].
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        end(&mut self.child.lock(), &self.outgoing, self.exit_deadline);
    }
}

/// What the writing thread sends the server.
enum Outgoing {
    /// A framed message.
    Message(Vec<u8>),
    /// The end of the server's input, which tells it to exit.
    Close,
}

/// Stops the server and what it started, and reaps the server; the system reaps the others,
/// which are not this process's children.
///
/// It first asks the server to exit, with a `shutdown` request, an `exit` notification and the
/// end of its input, and waits up to `exit_deadline` for it to exit, so it stops the
/// processes it started outside its process group itself, as rust-analyzer stops the commands
/// it runs in process groups of their own.
/// Then it kills the server's process group, whatever is left of it, such as Cargo, build
/// scripts and proc macros a server that ignored the request left running, or that it started
/// and did not stop.
/// A process the server started outside its group and left running is out of reach.
///
/// The server leaves `child` only as it is reaped, after its group is killed, so the group's id
/// stays the server's until then and no other group is ever signalled.
fn end(child: &mut Option<Child>, outgoing: &mpsc::Sender<Outgoing>, exit_deadline: Duration) {
    let Some(mut child) = child.take() else {
        return;
    };
    let asked = [
        json!({ "jsonrpc": "2.0", "id": SHUTDOWN_ID, "method": "shutdown" }),
        json!({ "jsonrpc": "2.0", "method": "exit" }),
    ]
    .iter()
    .map(|message| Outgoing::Message(frame(message)))
    .chain([Outgoing::Close])
    .all(|message| outgoing.send(message).is_ok());
    if asked {
        // A deadline further off than an `Instant` reaches, as a test sets, waits for the exit.
        let deadline = Instant::now().checked_add(exit_deadline);
        while !exited(&mut child) && deadline.is_none_or(|deadline| Instant::now() < deadline) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(unix)]
    {
        // The server leads a process group of its own, whose id is the server's.
        let group = rustix::process::Pid::from_child(&child);
        if let Err(err) = rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
        {
            tracing::debug!(%err, "the language server's process group is gone already");
        }
    }
    #[cfg(windows)]
    {
        let killed = Command::new("taskkill")
            .args(["/F", "/T", "/PID"])
            .arg(child.id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Err(err) = killed {
            // Killing the server below still stops it.
            drop(err);
        }
    }
    if let Err(err) = child.kill() {
        // The process is gone already.
        drop(err);
    }
    if let Err(err) = child.wait() {
        // It was reaped already.
        drop(err);
    }
}

/// Whether the server's process exited, without reaping it, so its process group's id stays
/// its own.
#[cfg(unix)]
fn exited(child: &mut Child) -> bool {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};

    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    // An error says there is no such child to wait for, which has exited too.
    !matches!(
        waitid(WaitId::Pid(Pid::from_child(child)), options),
        Ok(None)
    )
}

/// Whether the server's process exited, reaping it if it did; nothing on Windows signals a
/// process tree by an id that could be handed out again.
#[cfg(not(unix))]
fn exited(child: &mut Child) -> bool {
    !matches!(child.try_wait(), Ok(None))
}

/// How long a server may take to answer the `initialize` handshake.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(60);

/// The JSON-RPC error code a server uses when a result went stale while it was computed.
const CONTENT_MODIFIED: i64 = -32801;

/// How to start the server for one language.
#[derive(Debug, Clone)]
pub struct ServerCommand {
    /// The program.
    pub program: &'static str,
    /// Its arguments.
    pub args: &'static [&'static str],
    /// The LSP language identifier for `didOpen`.
    pub language_id: &'static str,
}

/// Returns the server for a file, by its extension.
#[must_use]
pub fn server_for(path: &Path) -> Option<ServerCommand> {
    let extension = path.extension()?.to_str()?;
    Some(match extension {
        "rs" => ServerCommand {
            program: "rust-analyzer",
            args: &[],
            language_id: "rust",
        },
        "go" => ServerCommand {
            program: "gopls",
            args: &[],
            language_id: "go",
        },
        "py" => ServerCommand {
            program: "pyright-langserver",
            args: &["--stdio"],
            language_id: "python",
        },
        "ts" | "tsx" | "js" | "jsx" => ServerCommand {
            program: "typescript-language-server",
            args: &["--stdio"],
            language_id: if extension.starts_with('t') {
                "typescript"
            } else {
                "javascript"
            },
        },
        _ => return None,
    })
}

/// A position in a file: a zero-based line and a zero-based column in UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// The line.
    pub line: usize,
    /// The column, in UTF-16 code units as LSP requires.
    pub character: usize,
}

/// A definition's location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// The file.
    pub path: PathBuf,
    /// Where the definition starts.
    pub position: Position,
}

/// A running language server for one workspace root.
pub struct LanguageServer {
    /// The server's process until it is reaped, which leads a process group of its own on Unix.
    child: Arc<Mutex<Option<Child>>>,
    /// Set once the server was stopped through a [`Stopper`].
    stopped: Arc<AtomicBool>,
    /// Whether the `initialize` handshake is done.
    initialized: bool,
    /// Messages to the server, written on a thread of their own, so a server that stops
    /// reading never blocks a request past its deadline, nor dropping the server.
    outgoing: mpsc::Sender<Outgoing>,
    /// Messages from the server, read on a thread of their own so every wait is bounded.
    messages: mpsc::Receiver<Result<Value, LspError>>,
    next_id: i64,
    language_id: &'static str,
    /// How long the server may take to exit once asked before it is killed:
    /// [`EXIT_DEADLINE`], unless a test waits for the exit with [`Duration::MAX`].
    exit_deadline: Duration,
}

impl LanguageServer {
    /// Starts `command` in `root`, without the `initialize` handshake, which
    /// [`Self::initialize`] completes; starting takes only as long as spawning the process.
    ///
    /// # Errors
    ///
    /// Returns an error when the server cannot start.
    pub fn launch(command: &ServerCommand, root: &Path) -> Result<Self, LspError> {
        Self::spawn(command.program, command.args, root, command.language_id)
    }

    /// Returns what stops this server from another thread.
    #[must_use]
    pub fn stopper(&self) -> Stopper {
        Stopper {
            child: Arc::clone(&self.child),
            stopped: Arc::clone(&self.stopped),
            outgoing: self.outgoing.clone(),
            exit_deadline: self.exit_deadline,
        }
    }

    /// Completes the `initialize` handshake in `root`, unless it is done.
    ///
    /// # Errors
    ///
    /// Returns [`LspError::Stopped`] when the server is stopped meanwhile, and an error when
    /// the handshake fails.
    pub fn initialize(&mut self, root: &Path) -> Result<(), LspError> {
        if self.initialized {
            return Ok(());
        }
        let root_uri = file_uri(root);
        self.request(
            Instant::now() + HANDSHAKE_DEADLINE,
            "initialize",
            &json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": "review" }],
                "capabilities": {
                    "textDocument": {
                        "definition": { "linkSupport": true },
                        "synchronization": { "didSave": false },
                    },
                    "workspace": { "workspaceFolders": true, "configuration": true },
                    "window": { "workDoneProgress": true },
                },
            }),
        )?;
        self.notify("initialized", &json!({}))?;
        self.initialized = true;
        Ok(())
    }

    /// Starts `program` in `root` with its reading and writing threads, before any handshake.
    fn spawn(
        program: &str,
        args: &[&str],
        root: &Path,
        language_id: &'static str,
    ) -> Result<Self, LspError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // The server leads a process group of its own, so stopping it stops what it started.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().map_err(|source| LspError::Spawn {
            command: program.to_owned(),
            source,
        })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(LspError::Protocol("server has no stdio pipes".to_owned()));
        };
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || read_messages(BufReader::new(stdout), &sender));
        let (outgoing, to_write) = mpsc::channel();
        std::thread::spawn(move || write_messages(stdin, &to_write));
        Ok(Self {
            child: Arc::new(Mutex::new(Some(child))),
            stopped: Arc::new(AtomicBool::new(false)),
            initialized: false,
            outgoing,
            messages,
            next_id: 1,
            language_id,
            exit_deadline: EXIT_DEADLINE,
        })
    }

    /// Finds the definition of the symbol at `position` in `path`, whose content is `text`,
    /// retrying while the server is still indexing, until `deadline` passes.
    ///
    /// The server is told the document holds `text` for the request only, and closed after,
    /// so it never holds text of the client's that the file may no longer have: other requests
    /// find every file as it is on disk.
    ///
    /// # Errors
    ///
    /// Returns [`LspError::NotFound`] when the server finds nothing in time, and an error when
    /// the server fails.
    pub fn definition(
        &mut self,
        path: &Path,
        text: &str,
        position: Position,
        deadline: Duration,
    ) -> Result<Definition, LspError> {
        let uri = file_uri(path);
        self.notify(
            "textDocument/didOpen",
            &json!({
                "textDocument": { "uri": uri, "languageId": self.language_id, "version": 1, "text": text },
            }),
        )?;
        let found = self.ask_definition(&uri, position, deadline);
        let closed = self.notify(
            "textDocument/didClose",
            &json!({ "textDocument": { "uri": uri } }),
        );
        let found = found?;
        closed?;
        Ok(found)
    }

    /// Asks for the definition of the symbol at `position` in the open document `uri`,
    /// retrying while the server is still indexing, until `deadline` passes.
    fn ask_definition(
        &mut self,
        uri: &str,
        position: Position,
        deadline: Duration,
    ) -> Result<Definition, LspError> {
        let started = Instant::now();
        let until = started + deadline;
        loop {
            let response = self.request(
                until,
                "textDocument/definition",
                &json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.line, "character": position.character },
                }),
            );
            match response {
                Ok(result) => {
                    if let Some(found) = parse_definition(&result) {
                        return Ok(found);
                    }
                }
                Err(LspError::Response { code, .. }) if code == CONTENT_MODIFIED => {}
                Err(LspError::Timeout) => return Err(LspError::NotFound),
                Err(err) => return Err(err),
            }
            if started.elapsed() >= deadline {
                return Err(LspError::NotFound);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Queues a message for the writing thread; it never waits for the server.
    fn send(&self, message: &Value) -> Result<(), LspError> {
        self.outgoing
            .send(Outgoing::Message(frame(message)))
            .map_err(|_| {
                // Stopping closes the server's input after it marks the server stopped.
                if self.stopped.load(Ordering::SeqCst) {
                    LspError::Stopped
                } else {
                    LspError::Protocol("server closed its input".to_owned())
                }
            })
    }

    fn notify(&self, method: &str, params: &Value) -> Result<(), LspError> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Sends a request and reads messages until its response arrives, answering the server's own
    /// requests with an empty result and ignoring notifications.
    fn request(
        &mut self,
        deadline: Instant,
        method: &str,
        params: &Value,
    ) -> Result<Value, LspError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let message = self.read_message(deadline)?;
            let is_request = message.get("method").is_some();
            match (message.get("id"), is_request) {
                (Some(request_id), true) => {
                    // The server asks something (configuration, progress tokens): answer with
                    // nothing, which every server treats as "use defaults".
                    let reply =
                        json!({ "jsonrpc": "2.0", "id": request_id, "result": Value::Null });
                    self.send(&reply)?;
                }
                (Some(response_id), false) if response_id.as_i64() == Some(id) => {
                    if let Some(error) = message.get("error") {
                        return Err(LspError::Response {
                            code: error
                                .get("code")
                                .and_then(Value::as_i64)
                                .unwrap_or_default(),
                            message: error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        });
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                _ => {}
            }
        }
    }

    /// The next message from the server, waiting at most until `deadline`.
    fn read_message(&mut self, deadline: Instant) -> Result<Value, LspError> {
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return Err(LspError::Stopped);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(LspError::Timeout);
            }
            let received = self.messages.recv_timeout(left.min(STOP_POLL));
            // A stopped server's output ends however it ends, which says nothing more.
            if self.stopped.load(Ordering::SeqCst) {
                return Err(LspError::Stopped);
            }
            match received {
                Ok(message) => return message,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(LspError::Protocol("server closed its output".to_owned()));
                }
            }
        }
    }
}

/// `message` framed with its `Content-Length` header.
fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

/// Writes queued messages to the server until its input is closed, as stopping it does, the
/// queue closes, when the server is dropped, or a write fails, when the server is gone; killing
/// the server ends a write it never reads.
fn write_messages(mut stdin: ChildStdin, to_write: &mpsc::Receiver<Outgoing>) {
    for outgoing in to_write {
        let Outgoing::Message(bytes) = outgoing else {
            break;
        };
        if stdin
            .write_all(&bytes)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            break;
        }
    }
}

/// Reads messages from the server's output until it closes, which it does when the server
/// exits, so the reading thread always ends.
fn read_messages(
    mut stdout: BufReader<ChildStdout>,
    messages: &mpsc::Sender<Result<Value, LspError>>,
) {
    loop {
        let message = read_one(&mut stdout);
        let closed = message.is_err();
        if messages.send(message).is_err() || closed {
            return;
        }
    }
}

/// Reads one `Content-Length`-framed JSON-RPC message.
fn read_one(stdout: &mut BufReader<ChildStdout>) -> Result<Value, LspError> {
    let mut length: Option<usize> = None;
    loop {
        let mut header = String::new();
        if stdout.read_line(&mut header)? == 0 {
            return Err(LspError::Protocol("server closed its output".to_owned()));
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok();
        }
    }
    let length = length.ok_or_else(|| LspError::Protocol("missing Content-Length".to_owned()))?;
    let mut body = vec![0; length];
    stdout.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|err| LspError::Protocol(err.to_string()))
}

#[cfg(all(test, target_os = "linux"))]
impl LanguageServer {
    /// `program` started with `args` in `root` as a server without a handshake, standing in for a real
    /// one where a test needs only a process that runs until the server is dropped.
    pub(crate) fn stand_in(program: &str, args: &[&str], root: &Path) -> Result<Self, LspError> {
        Self::spawn(program, args, root, "rust")
    }

    /// The process's identifier, or 0, which no process has, once it is reaped.
    pub(crate) fn process_id(&self) -> u32 {
        self.child.lock().as_ref().map_or(0, Child::id)
    }
}

impl Drop for LanguageServer {
    fn drop(&mut self) {
        // A server a `Stopper` stopped is gone already.
        end(&mut self.child.lock(), &self.outgoing, self.exit_deadline);
    }
}

/// Parses a definition result: a `Location`, a list of them, or a list of `LocationLink`s.
fn parse_definition(result: &Value) -> Option<Definition> {
    let first = match result {
        Value::Array(items) => items.first()?,
        Value::Object(_) => result,
        _ => return None,
    };
    let uri = first
        .get("targetUri")
        .or_else(|| first.get("uri"))?
        .as_str()?;
    let range = first
        .get("targetSelectionRange")
        .or_else(|| first.get("range"))?;
    let start = range.get("start")?;
    Some(Definition {
        path: path_from_uri(uri)?,
        position: Position {
            line: usize::try_from(start.get("line")?.as_u64()?).ok()?,
            character: usize::try_from(start.get("character")?.as_u64()?).ok()?,
        },
    })
}

/// Encodes an absolute path as a `file://` URI, percent-encoding everything but unreserved
/// characters, `/` and `:`. A Windows path such as `C:\x` becomes `file:///C:/x`.
fn file_uri(path: &Path) -> String {
    use std::fmt::Write as _;

    let mut uri = String::from("file://");
    for byte in uri_path(path).bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(byte));
        } else {
            // Writing to a `String` cannot fail.
            write!(uri, "%{byte:02X}").ok();
        }
    }
    uri
}

/// The path part of a file URI: the path itself on Unix; on Windows without the `\\?\` prefix
/// that canonical paths carry, with `/` separators and a leading `/` before the drive.
fn uri_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        let text = text
            .strip_prefix(r"\\?\")
            .unwrap_or(&text)
            .replace('\\', "/");
        if text.starts_with('/') {
            text
        } else {
            format!("/{text}")
        }
    } else {
        text.into_owned()
    }
}

/// Decodes a `file://` URI into a path; on Windows, `/C:/x` becomes `C:/x`.
fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = tail.get(2..)?;
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    let path = String::from_utf8(bytes).ok()?;
    if cfg!(windows)
        && let Some(drive) = path.strip_prefix('/')
        && drive.as_bytes().get(1) == Some(&b':')
    {
        return Some(PathBuf::from(drive));
    }
    Some(PathBuf::from(path))
}

/// Converts a character index within `line` into a UTF-16 column.
#[must_use]
pub fn utf16_column(line: &str, byte_offset: usize) -> usize {
    line.get(..byte_offset)
        .map_or(0, |prefix| prefix.encode_utf16().count())
}

/// Converts a UTF-16 column within `line` into a byte offset, clamped to the line.
#[must_use]
pub fn byte_column(line: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (offset, character) in line.char_indices() {
        if units >= utf16 {
            return offset;
        }
        units += character.len_utf16();
    }
    line.len()
}

/// Whether a working rust-analyzer is on the path. rustup installs a `rust-analyzer` proxy even
/// without the component, which runs and fails, so only a successful `--version` counts.
#[cfg(test)]
pub(crate) fn rust_analyzer_installed() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A fresh directory under the project's own `target/tmp`, for a test crate that rust-analyzer
/// builds in: its build output must never land in the system's temporary directory, which may
/// be a small RAM disk.
#[cfg(test)]
pub(crate) fn scratch_dir() -> tempfile::TempDir {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
    std::fs::create_dir_all(&parent).expect("the project's target directory is writable");
    tempfile::tempdir_in(parent).expect("the project's target directory is writable")
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    /// A server that never reads its input blocks neither a message nor dropping it.
    ///
    /// The server never reads, and ends only after ten minutes, so a message or a drop that
    /// waited for it would not return before the test profile's bound in
    /// `.config/nextest.toml` fails the test.
    #[test]
    fn a_server_that_stops_reading_never_blocks() {
        let root = std::env::temp_dir();
        let server = LanguageServer::spawn("sleep", &["600"], &root, "rust").unwrap();
        // Far more than a pipe holds: a blocking write would hang here.
        let text = "x".repeat(8 << 20);
        server
            .notify("textDocument/didOpen", &json!({ "text": text }))
            .unwrap();
        drop(server);
    }

    /// A canonical Windows path loses its `\\?\` prefix and gets `/` separators and a slash
    /// before the drive; decoding gives back the drive path.
    #[cfg(windows)]
    #[test]
    fn windows_paths_become_file_uris() {
        let uri = file_uri(Path::new(r"\\?\C:\a b\x.rs"));
        assert_eq!(uri, "file:///C:/a%20b/x.rs");
        assert_eq!(path_from_uri(&uri), Some(PathBuf::from("C:/a b/x.rs")));
    }

    #[test]
    fn uris_round_trip_and_definitions_parse() {
        let path = Path::new("/tmp/a b/ü.rs");
        let uri = file_uri(path);
        assert_eq!(uri, "file:///tmp/a%20b/%C3%BC.rs");
        assert_eq!(path_from_uri(&uri).as_deref(), Some(path));

        let location = json!({ "uri": uri, "range": { "start": { "line": 3, "character": 7 }, "end": { "line": 3, "character": 9 } } });
        let link = json!([{ "targetUri": "file:///x.rs", "targetRange": {}, "targetSelectionRange": { "start": { "line": 1, "character": 2 } } }]);
        assert_eq!(
            parse_definition(&location).map(|found| found.position),
            Some(Position {
                line: 3,
                character: 7
            })
        );
        assert_eq!(
            parse_definition(&link).map(|found| found.path),
            Some(PathBuf::from("/x.rs"))
        );
        assert_eq!(parse_definition(&json!([])), None);
        assert_eq!(utf16_column("aü𝄞b", "aü𝄞".len()), 4);
    }

    /// A named pipe `name` in `dir`.
    #[cfg(target_os = "linux")]
    fn fifo(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let made = Command::new("mkfifo").arg(&path).status().unwrap();
        assert!(made.success());
        path
    }

    /// The named pipe `fifo` opened for writing when `write`, else for reading, once a process
    /// opens its other end, or `None` when the server, process `server`, ends first, so a
    /// script that fails before it opens the pipe fails the test instead of leaving it waiting.
    #[cfg(target_os = "linux")]
    fn open_unless_ended(fifo: &Path, write: bool, server: u32) -> Option<std::fs::File> {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};

        let (event, events) = mpsc::channel();
        std::thread::spawn({
            let event = event.clone();
            let fifo = fifo.to_owned();
            move || {
                let opened = std::fs::OpenOptions::new()
                    .read(!write)
                    .write(write)
                    .open(fifo);
                event.send(Some(opened.unwrap())).ok();
            }
        });
        let server = Pid::from_raw(server.cast_signed()).unwrap();
        std::thread::spawn(move || {
            // An error says the server was reaped, which it was only once it ended.
            let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
            waitid(WaitId::Pid(server), options).ok();
            event.send(None).ok();
        });
        events.recv().unwrap()
    }

    /// A server asked to exit stops what it started outside its process group itself, as
    /// rust-analyzer stops the commands it runs in groups of their own, and stopping it waits
    /// for it to exit, and no longer.
    ///
    /// The server decides when it exits, once the test lets it: its exit deadline is none.
    /// A stop that never returns fails at the test profile's bound in `.config/nextest.toml`.
    /// The server, which leads a process group of its own, and its child, in a session of its
    /// own, are out of reach of the profile's kill of the test's group, so they end on their
    /// own within ten minutes.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_server_that_exits_on_request_stops_what_it_started_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let (held, go) = (fifo(dir.path(), "held"), fifo(dir.path(), "go"));
        // A child in a session of its own holds `held`; once the server's input ends, the
        // server waits for a line on `go`, then kills the child and exits.
        let script = r#"setsid sleep 600 3>"$1" & child=$!
            cat >/dev/null
            read -r _ <"$2"
            kill "$child""#;
        let paths = [held.to_str().unwrap(), go.to_str().unwrap()];
        let mut server = LanguageServer::stand_in(
            "timeout",
            &["600", "sh", "-c", script, "sh", paths[0], paths[1]],
            dir.path(),
        )
        .unwrap();
        server.exit_deadline = Duration::MAX;
        // Taken before the stop, which holds the server until it is gone.
        let process = server.process_id();
        let mut left = open_unless_ended(&held, false, process).expect("the server ended");

        let stopper = server.stopper();
        let stopping = std::thread::spawn(move || stopper.stop());
        // `go` opens once the stop ended the server's input, and the server waits on it.
        let mut go = open_unless_ended(&go, true, process).expect("the stop killed the server");
        assert!(
            !stopping.is_finished(),
            "the stop did not wait for the exit"
        );
        go.write_all(b"\n").unwrap();
        drop(go);
        stopping.join().unwrap();
        // The pipe ends once the child is gone.
        let mut written = String::new();
        left.read_to_string(&mut written).unwrap();
        assert_eq!(written, "");
    }

    /// A server that ignores the request to exit is killed with its process group once the
    /// deadline passes, the children it started there, as Cargo and the build scripts a
    /// language server runs, included.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_server_that_ignores_the_request_to_exit_is_killed_with_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let held = fifo(dir.path(), "held");
        // A child in the server's group holds `held`, and writes to it only if it outlives the
        // `sleep`; the server waits for it.
        let script = r#"{ sleep 30; echo survived >&3; } 3>"$1" & wait"#;
        let path = held.to_str().unwrap();
        let server =
            LanguageServer::stand_in("sh", &["-c", script, "sh", path], dir.path()).unwrap();
        let process = server.process_id();
        let mut left = open_unless_ended(&held, false, process).expect("the server ended");

        server.stopper().stop();
        // The pipe ends once the child is gone, without its having written.
        let mut written = String::new();
        left.read_to_string(&mut written).unwrap();
        assert_eq!(written, "", "the server's child outlived the stop");
        drop(server);
    }

    /// Writes a crate to `root` whose `src/main.rs`, which it returns, calls `helper` of
    /// `src/util.rs`, defined on its first line, beside `other` on its fifth; a workspace of its
    /// own, which the project's workspace around `target/` does not claim.
    fn probe_crate(root: &Path) -> &'static str {
        std::fs::write(
            root.join("Cargo.toml"),
            indoc! {"
                [package]
                name = \"probe\"
                version = \"0.1.0\"
                edition = \"2024\"

                [workspace]
            "},
        )
        .unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/util.rs"),
            indoc! {"
                pub fn helper() -> u32 {
                    7
                }

                pub fn other() -> u32 {
                    8
                }
            "},
        )
        .unwrap();
        let main = indoc! {"
            mod util;

            fn main() {
                let _ = util::helper();
            }
        "};
        std::fs::write(root.join("src/main.rs"), main).unwrap();
        main
    }

    /// Runs rust-analyzer against a two-file crate, and again once the file asked about changed,
    /// and once a file asked about before changed, which the server must answer about as it is
    /// on disk rather than as it was first told; skipped where rust-analyzer is not installed.
    #[test]
    fn rust_analyzer_answers_about_the_files_as_edited() {
        if !rust_analyzer_installed() {
            eprintln!("skipped: rust-analyzer is not installed");
            return;
        }
        let dir = scratch_dir();
        let root = dir.path();
        let main = probe_crate(root);
        let main_path = root.join("src/main.rs");

        let command = server_for(&main_path).unwrap();
        let mut server = LanguageServer::launch(&command, root).unwrap();
        server.initialize(root).unwrap();
        let line = "    let _ = util::helper();";
        let position = Position {
            line: 3,
            character: utf16_column(line, line.find("helper").unwrap()),
        };
        let found = server
            .definition(&main_path, main, position, Duration::from_secs(120))
            .unwrap();
        // Canonical on both sides, as the server answers with canonical paths, which on
        // Windows carry a `\\?\` prefix.
        assert_eq!(
            found.path.canonicalize().unwrap(),
            root.join("src/util.rs").canonicalize().unwrap()
        );
        assert_eq!(
            found.position,
            Position {
                line: 0,
                character: 7
            }
        );

        // The call now names `other`, which starts where `helper` did.
        let edited = main.replace("helper", "other");
        std::fs::write(&main_path, &edited).unwrap();
        let found = server
            .definition(&main_path, &edited, position, Duration::from_secs(120))
            .unwrap();
        assert_eq!(
            found.position,
            Position {
                line: 4,
                character: 7
            },
            "the answer is about the text before the edit"
        );

        // A question about `util.rs` itself, which the server then reads from disk again, so
        // two lines added above `other` there move the answer about its caller to them.
        let util_path = root.join("src/util.rs");
        let util = std::fs::read_to_string(&util_path).unwrap();
        let other = Position {
            line: 4,
            character: 7,
        };
        server
            .definition(&util_path, &util, other, Duration::from_secs(120))
            .unwrap();
        let above = indoc! {"
            // one
            // two
            pub fn other"};
        std::fs::write(&util_path, util.replace("pub fn other", above)).unwrap();
        // The server reads the edit through its own watch on the files, so it may answer about
        // the old text a moment longer; a file it still held open would keep it for good.
        let asked = Instant::now();
        let moved = Position {
            line: 6,
            character: 7,
        };
        loop {
            let found = server
                .definition(&main_path, &edited, position, Duration::from_secs(120))
                .unwrap();
            if found.position == moved {
                break;
            }
            assert!(
                asked.elapsed() < Duration::from_secs(60),
                "the answer stays at {:?}, before the edit of util.rs",
                found.position
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
