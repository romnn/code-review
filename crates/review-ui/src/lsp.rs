//! A minimal, synchronous language server client over stdio: `initialize`, `didOpen` and
//! `textDocument/definition`, which is all go-to-definition needs.
//!
//! It is written for this app rather than copied from Zed's `lsp` crate, which is asynchronous,
//! tied to Zed's executor and handles far more of the protocol than the slice uses.
//! Calls block, so front ends run them off the UI thread.

use std::collections::HashSet;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
    /// Server-specific initialization options.
    pub initialization_options: fn() -> Value,
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
            initialization_options: rust_analyzer_options,
        },
        "go" => ServerCommand {
            program: "gopls",
            args: &[],
            language_id: "go",
            initialization_options: no_options,
        },
        "py" => ServerCommand {
            program: "pyright-langserver",
            args: &["--stdio"],
            language_id: "python",
            initialization_options: no_options,
        },
        "ts" | "tsx" | "js" | "jsx" => ServerCommand {
            program: "typescript-language-server",
            args: &["--stdio"],
            language_id: if extension.starts_with('t') {
                "typescript"
            } else {
                "javascript"
            },
            initialization_options: no_options,
        },
        _ => return None,
    })
}

fn no_options() -> Value {
    Value::Null
}

/// Keeps rust-analyzer from building anything in the read-only worktree: no build scripts, proc
/// macros, check-on-save or cache priming.
/// Name resolution still works for definitions.
fn rust_analyzer_options() -> Value {
    json!({
        "cargo": { "buildScripts": { "enable": false } },
        "procMacro": { "enable": false },
        "checkOnSave": false,
        "cachePriming": { "enable": false },
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
    child: Child,
    /// Messages to the server, written on a thread of their own, so a server that stops
    /// reading never blocks a request past its deadline, nor dropping the server.
    outgoing: mpsc::Sender<Vec<u8>>,
    /// Messages from the server, read on a thread of their own so every wait is bounded.
    messages: mpsc::Receiver<Result<Value, LspError>>,
    next_id: i64,
    language_id: &'static str,
    opened: HashSet<PathBuf>,
}

impl LanguageServer {
    /// Starts `command` in `root` and completes the `initialize` handshake.
    ///
    /// # Errors
    ///
    /// Returns an error when the server cannot start or the handshake fails.
    pub fn start(command: &ServerCommand, root: &Path) -> Result<Self, LspError> {
        let mut server = Self::spawn(command.program, command.args, root, command.language_id)?;
        let root_uri = file_uri(root);
        server.request(
            Instant::now() + HANDSHAKE_DEADLINE,
            "initialize",
            &json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": "review" }],
                "initializationOptions": (command.initialization_options)(),
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
        server.notify("initialized", &json!({}))?;
        Ok(server)
    }

    /// Starts `program` in `root` with its reading and writing threads, before any handshake.
    fn spawn(
        program: &str,
        args: &[&str],
        root: &Path,
        language_id: &'static str,
    ) -> Result<Self, LspError> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| LspError::Spawn {
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
            child,
            outgoing,
            messages,
            next_id: 1,
            language_id,
            opened: HashSet::new(),
        })
    }

    /// Finds the definition of the symbol at `position` in `path`, whose content is `text`,
    /// retrying while the server is still indexing, until `deadline` passes.
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
        if self.opened.insert(path.to_path_buf()) {
            self.notify(
                "textDocument/didOpen",
                &json!({
                    "textDocument": { "uri": uri, "languageId": self.language_id, "version": 1, "text": text },
                }),
            )?;
        }
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
        let body = message.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        self.outgoing
            .send(frame.into_bytes())
            .map_err(|_| LspError::Protocol("server closed its input".to_owned()))
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
        let wait = deadline.saturating_duration_since(Instant::now());
        match self.messages.recv_timeout(wait) {
            Ok(message) => message,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(LspError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(LspError::Protocol("server closed its output".to_owned()))
            }
        }
    }
}

/// Writes queued messages to the server until the queue closes, when the server is dropped, or
/// a write fails, when the server is gone; killing the server ends a write it never reads.
fn write_messages(mut stdin: ChildStdin, to_write: &mpsc::Receiver<Vec<u8>>) {
    for bytes in to_write {
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
    /// `program` started in `root` as a server without a handshake, standing in for a real
    /// one where a test needs only a process that runs until the server is dropped.
    pub(crate) fn stand_in(program: &str, root: &Path) -> Result<Self, LspError> {
        Self::spawn(program, &[], root, "rust")
    }

    /// The process's identifier.
    pub(crate) fn process_id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for LanguageServer {
    fn drop(&mut self) {
        // Ask politely, then make sure the process is gone and reaped. Queuing the messages
        // never blocks, and killing the server ends any write it is not reading.
        let shutdown = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": "shutdown" });
        let exit = json!({ "jsonrpc": "2.0", "method": "exit" });
        if self
            .send(&shutdown)
            .and_then(|()| self.send(&exit))
            .is_err()
        {
            // The server is already gone; killing it below is enough.
        }
        if let Err(err) = self.child.kill().and_then(|()| self.child.wait().map(drop)) {
            drop(err);
        }
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
/// characters and `/`.
fn file_uri(path: &Path) -> String {
    use std::fmt::Write as _;

    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(byte));
        } else {
            // Writing to a `String` cannot fail.
            write!(uri, "%{byte:02X}").ok();
        }
    }
    uri
}

/// Decodes a `file://` URI into a path.
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
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
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

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    /// A server that never reads its input blocks neither a message nor dropping it.
    #[test]
    fn a_server_that_stops_reading_never_blocks() {
        let root = std::env::temp_dir();
        let started = Instant::now();
        let server = LanguageServer::spawn("sleep", &["30"], &root, "rust").unwrap();
        // Far more than a pipe holds: a blocking write would hang here.
        let text = "x".repeat(8 << 20);
        server
            .notify("textDocument/didOpen", &json!({ "text": text }))
            .unwrap();
        drop(server);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
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

    /// Runs rust-analyzer against a two-file crate; skipped where rust-analyzer is not installed.
    #[test]
    fn rust_analyzer_finds_a_definition_in_another_file() {
        if Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipped: rust-analyzer is not installed");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            indoc! {"
                [package]
                name = \"probe\"
                version = \"0.1.0\"
                edition = \"2024\"
            "},
        )
        .unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/util.rs"),
            indoc! {"
                pub fn helper() -> u32 {
                    7
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
        let main_path = root.path().join("src/main.rs");
        std::fs::write(&main_path, main).unwrap();

        let command = server_for(&main_path).unwrap();
        let mut server = LanguageServer::start(&command, root.path()).unwrap();
        let line = "    let _ = util::helper();";
        let column = utf16_column(line, line.find("helper").unwrap());
        let found = server
            .definition(
                &main_path,
                main,
                Position {
                    line: 3,
                    character: column,
                },
                Duration::from_secs(120),
            )
            .unwrap();
        assert_eq!(
            found.path,
            root.path().join("src/util.rs").canonicalize().unwrap()
        );
        assert_eq!(
            found.position,
            Position {
                line: 0,
                character: 7
            }
        );
    }
}
