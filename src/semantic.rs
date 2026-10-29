//! A minimal LSP client for rust-analyzer.
//!
//! The point of this module is type attribution: a syntax engine cannot tell a
//! `Vec::len` from a `MyBag::len`, so a rename has to ask the compiler-backed
//! server which references are real. Everything here is deliberately narrow —
//! it speaks just enough of the protocol to open a workspace, ask a semantic
//! question, and shut down cleanly.
//!
//! Edits are returned to the caller rather than written, so the caller keeps
//! control of diffing, gating and rollback.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextDocumentIdentifierOuter {
    uri: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextDocumentEdit {
    text_document: TextDocumentIdentifierOuter,
    edits: Vec<TextEdit>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceEdit {
    changes: Option<BTreeMap<String, Vec<TextEdit>>>,
    document_changes: Option<Vec<Value>>,
}

/// Edits for one file, already mapped back to a real path.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileEdits {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
}

/// A reference to a resolved symbol: a call, an import, or a use as a value.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

/// One assist offered by the server. The raw JSON is kept because resolving an
/// action means sending it back verbatim.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodeAction {
    pub title: String,
    pub kind: Option<String>,
    raw: Value,
}

/// The semantic questions this tool asks, independent of where the answer comes
/// from.
///
/// Two implementations exist: [`Server`], which spawns rust-analyzer for the
/// duration of one command, and [`crate::daemon::Remote`], which asks a warm
/// server that is already loaded. Callers take `&mut dyn Analyzer`, so the
/// recipe engine never needs to know which one it has.
pub trait Analyzer {
    fn encoding(&self) -> PositionEncoding;
    fn did_open(&mut self, path: &Path, text: &str) -> Result<()>;
    fn refresh(&mut self, path: &Path, text: &str) -> Result<()>;
    fn wait_ready(&mut self, budget: Duration) -> bool;
    fn rename(
        &mut self,
        path: &Path,
        position: Position,
        new_name: &str,
        budget: Duration,
    ) -> Result<Vec<FileEdits>>;
    fn references(
        &mut self,
        path: &Path,
        position: Position,
        budget: Duration,
    ) -> Result<Vec<Location>>;
    fn code_actions(
        &mut self,
        path: &Path,
        range: Range,
        budget: Duration,
    ) -> Result<Vec<CodeAction>>;
    fn resolve_action(&mut self, action: &CodeAction, budget: Duration) -> Result<Vec<FileEdits>>;
    /// End this session. For a warm server that means hanging up, not stopping
    /// the server.
    fn shutdown(&mut self);
}

pub struct Server {
    child: Child,
    writer: Arc<Mutex<ChildStdin>>,
    rx: Receiver<Value>,
    next_id: i64,
    encoding: PositionEncoding,
    stderr: Arc<Mutex<VecDeque<String>>>,
    /// Whether this server has ever resolved something. A cold analyzer can
    /// reject a valid position while its crate graph is still being built, so
    /// until it answers once, a rejection is worth retrying.
    answered: bool,
}

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a "no such symbol" rejection is retried on a cold server.
const COLD_REJECTION_GRACE: Duration = Duration::from_secs(20);

/// The analyzer binary. `RUSTREWRITE_ANALYZER` exists so a builder-pinned or
/// side-by-side rust-analyzer can be used, and so tests can prove that a
/// structural-only run never consults it.
pub fn analyzer_binary() -> String {
    std::env::var("RUSTREWRITE_ANALYZER").unwrap_or_else(|_| "rust-analyzer".to_string())
}

/// Whether a JSON-RPC error is a rejection rather than a not-analysed-yet signal.
///
/// A `null` result means "not analysed yet", so it is retried. An error like
/// `No references found at position` means the line and column name nothing the
/// server can resolve — but only once the workspace is loaded, which is why the
/// caller decides whether to believe it yet.
fn is_definitive(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("-32602") || text.contains("No references found at position")
}

impl Server {
    /// Spawn `rust-analyzer` and complete the initialize handshake.
    pub fn start(root: &Path) -> Result<Self> {
        let binary = analyzer_binary();
        let mut child = Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(root)
            .spawn()
            .with_context(|| {
                format!(
                    "starting `{binary}`\n\n\
                     install it with `rustup component add rust-analyzer`, or point PATH at a \
                     rust-analyzer binary"
                )
            })?;

        let stdin = child.stdin.take().context("rust-analyzer stdin")?;
        let stdout = child.stdout.take().context("rust-analyzer stdout")?;
        let stderr = child.stderr.take().context("rust-analyzer stderr")?;

        let writer = Arc::new(Mutex::new(stdin));
        let (tx, rx) = channel();

        // The reader answers server-to-client requests itself, because the
        // server blocks on those and the main thread may be waiting on a
        // response of its own.
        let reader_writer = Arc::clone(&writer);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(message)) = read_message(&mut reader) {
                if message.get("method").is_some() && message.get("id").is_some() {
                    let reply = reply_for(&message);
                    if let Ok(mut stdin) = reader_writer.lock() {
                        let _ = write_message(&mut *stdin, &reply);
                    }
                    continue;
                }
                if tx.send(message).is_err() {
                    break;
                }
            }
        });

        let stderr_log = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_sink = Arc::clone(&stderr_log);
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if let Ok(mut log) = stderr_sink.lock() {
                    log.push_back(line);
                    if log.len() > 200 {
                        log.pop_front();
                    }
                }
            }
        });

        let mut server = Server {
            child,
            writer,
            rx,
            next_id: 1,
            encoding: PositionEncoding::Utf16,
            stderr: stderr_log,
            answered: false,
        };

        let response = server
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": path_to_uri(root),
                    "capabilities": {
                        "general": { "positionEncodings": ["utf-8"] },
                        "window": { "workDoneProgress": true },
                        "textDocument": {
                            "rename": { "prepareSupport": true },
                            // Without literal support the server returns no
                            // assists at all, and without resolveSupport it
                            // returns them without the edit that makes them
                            // useful.
                            "codeAction": {
                                "codeActionLiteralSupport": {
                                    "codeActionKind": {
                                        "valueSet": [
                                            "", "quickfix", "refactor",
                                            "refactor.extract", "refactor.inline",
                                            "refactor.rewrite", "source",
                                            "source.fixAll"
                                        ]
                                    }
                                },
                                "resolveSupport": { "properties": ["edit"] }
                            }
                        }
                    },
                    "workspaceFolders": null
                }),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .map_err(|err| {
                let log = server.stderr_tail();
                if log.contains("Unknown binary 'rust-analyzer.exe'") {
                    // rustup proxies rust-analyzer and honours a rust-toolchain pin,
                    // so a project can pin a toolchain that has no analyzer.
                    anyhow::anyhow!(
                        "{err}\n\nThis project pins a Rust toolchain that has no rust-analyzer \
                     component installed. Add it with:\n  \
                     rustup component add rust-analyzer --toolchain <the pinned toolchain>"
                    )
                } else {
                    err
                }
            })?;

        if let Some(encoding) = response
            .get("capabilities")
            .and_then(|c| c.get("positionEncoding"))
            .and_then(|e| e.as_str())
        {
            server.encoding = if encoding.eq_ignore_ascii_case("utf-8") {
                PositionEncoding::Utf8
            } else {
                PositionEncoding::Utf16
            };
        }

        server.notify("initialized", json!({}));
        Ok(server)
    }

    pub fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    /// Snapshot of rust-analyzer's own stderr, for diagnosing a failed run.
    pub fn stderr_tail(&self) -> String {
        match self.stderr.lock() {
            Ok(log) => log.iter().cloned().collect::<Vec<_>>().join("\n"),
            Err(_) => String::new(),
        }
    }

    fn write(&self, message: &Value) -> Result<()> {
        let mut stdin = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("rust-analyzer stdin lock poisoned"))?;
        write_message(&mut *stdin, message)
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "rust-analyzer did not answer `{method}` within {}s\n{}",
                    timeout.as_secs(),
                    self.stderr_tail()
                );
            }
            match self.rx.recv_timeout(remaining) {
                Ok(message) => {
                    if message.get("method").is_some() {
                        continue; // notification
                    }
                    if message.get("id").and_then(Value::as_i64) != Some(id) {
                        continue; // a response to something else
                    }
                    if let Some(error) = message.get("error") {
                        bail!("rust-analyzer rejected `{method}`: {error}");
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    bail!(
                        "rust-analyzer exited while handling `{method}`\n{}",
                        self.stderr_tail()
                    )
                }
            }
        }
    }

    /// Block until the server has finished loading and indexing this workspace.
    ///
    /// rust-analyzer reports work through `$/progress` begin/end pairs and,
    /// sometimes, `experimental/serverStatus`. Waiting for those is what stops a
    /// large workspace from being mistaken for a symbol the server cannot rename.
    pub fn wait_ready(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        let started = Instant::now();
        // If the server reports no progress at all, stop waiting and let the
        // caller proceed: some builds simply do not emit it, and sitting on the
        // full budget would turn every run into a coffee break.
        let silent_grace = Duration::from_secs(10);
        let trace = std::env::var_os("RUSTREWRITE_TRACE").is_some();
        let mut active: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut saw_progress = false;
        let mut saw_begin = false;
        loop {
            if saw_begin && active.is_empty() {
                if trace {
                    eprintln!("[warm] ready after {:?}", started.elapsed());
                }
                return true;
            }
            if !saw_progress && started.elapsed() > silent_grace {
                if trace {
                    eprintln!(
                        "[warm] no progress after {:?}, not waiting",
                        started.elapsed()
                    );
                }
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if trace {
                    eprintln!("[warm] budget spent with {} active token(s)", active.len());
                }
                return saw_progress && active.is_empty();
            }
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(message) => match message.get("method").and_then(Value::as_str) {
                    Some("$/progress") => {
                        let params = message.get("params");
                        let token = params
                            .and_then(|p| p.get("token"))
                            .map(|t| t.to_string())
                            .unwrap_or_default();
                        let kind = params
                            .and_then(|p| p.get("value"))
                            .and_then(|v| v.get("kind"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let title = params
                            .and_then(|p| p.get("value"))
                            .and_then(|v| v.get("title"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if trace {
                            eprintln!("[warm] progress {kind} {token} {title}");
                        }
                        saw_progress = true;
                        match kind {
                            "begin" => {
                                saw_begin = true;
                                active.insert(token);
                            }
                            "end" => {
                                active.remove(&token);
                            }
                            _ => {}
                        }
                    }
                    Some("experimental/serverStatus") => {
                        if message
                            .get("params")
                            .and_then(|p| p.get("quiescent"))
                            .and_then(Value::as_bool)
                            == Some(true)
                        {
                            return true;
                        }
                    }
                    _ => {}
                },
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    pub fn did_open(&mut self, path: &Path, text: &str) -> Result<()> {
        let uri = path_to_uri(path);
        let language_id = "rust";
        self.write(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }
        }))
    }

    /// Replace the server's copy of an open document after this tool wrote new
    /// text to disk, so the next semantic step resolves against reality.
    pub fn refresh(&mut self, path: &Path, text: &str) -> Result<()> {
        let uri = path_to_uri(path);
        self.write(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": { "textDocument": { "uri": uri } }
        }))?;
        self.did_open(path, text)
    }

    /// Rename the symbol at `position`, returning the edits the server wants.
    ///
    /// A `null` response means "not answerable yet", so it is retried until the
    /// budget runs out; that is how a cold workspace behaves for the first
    /// minute. A non-null response with no edits is a genuine refusal — a trait
    /// implementation member, for example — and is returned as an empty list so
    /// the caller can report it instead of treating it as success.
    pub fn rename(
        &mut self,
        path: &Path,
        position: Position,
        new_name: &str,
        budget: Duration,
    ) -> Result<Vec<FileEdits>> {
        let deadline = Instant::now() + budget;
        // A rejection is only final once this server has answered something:
        // a cold analyzer rejects even a correct position while it is still
        // building its crate graph.
        let rejection_deadline = if self.answered {
            Instant::now()
        } else {
            Instant::now() + COLD_REJECTION_GRACE
        };
        let mut delay = Duration::from_millis(250);
        let mut last_error: Option<anyhow::Error> = None;
        loop {
            match self.request(
                "textDocument/rename",
                json!({
                    "textDocument": { "uri": path_to_uri(path) },
                    "position": { "line": position.line, "character": position.character },
                    "newName": new_name
                }),
                DEFAULT_REQUEST_TIMEOUT,
            ) {
                Ok(Value::Null) => {}
                Ok(value) => {
                    let edits = parse_workspace_edit(value)?;
                    self.answered = true;
                    return Ok(edits);
                }
                Err(err) => {
                    if is_definitive(&err) && Instant::now() >= rejection_deadline {
                        bail!(
                            "rust-analyzer cannot rename the symbol at {}:{}:{}: {err}\n\
                             Check that the line and column point at a name, not at whitespace.",
                            path.display(),
                            position.line + 1,
                            position.character + 1
                        );
                    }
                    last_error = Some(err);
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(2));
        }
        match last_error {
            Some(err) => Err(err.context("rust-analyzer produced no rename before the deadline")),
            None => bail!(
                "rust-analyzer produced no rename within {}s.\n\
                 On a large workspace the first request pays for the whole load; raise --wait.",
                budget.as_secs()
            ),
        }
    }

    /// Every reference to the symbol at `position`. `includeDeclaration` is false
    /// so the result is uses only, which is what a call-site rewrite needs.
    pub fn references(
        &mut self,
        path: &Path,
        position: Position,
        budget: Duration,
    ) -> Result<Vec<Location>> {
        let deadline = Instant::now() + budget;
        // An empty list is ambiguous: no references, or not analysed yet. Give it
        // a short grace to tell the two apart, then trust the answer.
        let grace = Instant::now() + budget.min(Duration::from_secs(8));
        let rejection_deadline = if self.answered {
            Instant::now()
        } else {
            Instant::now() + COLD_REJECTION_GRACE
        };
        let mut delay = Duration::from_millis(250);
        let mut last_error: Option<anyhow::Error> = None;
        let mut saw_empty = false;
        loop {
            match self.request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": path_to_uri(path) },
                    "position": { "line": position.line, "character": position.character },
                    "context": { "includeDeclaration": false }
                }),
                DEFAULT_REQUEST_TIMEOUT,
            ) {
                Ok(Value::Null) => {}
                Ok(value) => {
                    let locations = parse_locations(value)?;
                    self.answered = true;
                    if locations.is_empty() {
                        saw_empty = true;
                    } else {
                        return Ok(locations);
                    }
                }
                Err(err) => {
                    if is_definitive(&err) && Instant::now() >= rejection_deadline {
                        bail!(
                            "rust-analyzer cannot list references at {}:{}:{}: {err}\n\
                             Check that the line and column point at a name, not at whitespace.",
                            path.display(),
                            position.line + 1,
                            position.character + 1
                        );
                    }
                    last_error = Some(err);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if saw_empty && now >= grace {
                return Ok(Vec::new());
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(2));
        }
        if saw_empty {
            return Ok(Vec::new());
        }
        match last_error {
            Some(err) => {
                Err(err.context("rust-analyzer produced no references before the deadline"))
            }
            None => bail!(
                "rust-analyzer produced no references within {}s.\n\
                 On a large workspace the first request pays for the whole load; raise --wait.",
                budget.as_secs()
            ),
        }
    }

    /// Assists the server offers at `range`, in the order it returns them.
    ///
    /// rust-analyzer answers with `data` and no edit; the edit arrives from
    /// `codeAction/resolve`, so the raw action is kept for that round trip.
    pub fn code_actions(
        &mut self,
        path: &Path,
        range: Range,
        budget: Duration,
    ) -> Result<Vec<CodeAction>> {
        let deadline = Instant::now() + budget;
        let grace = Instant::now() + budget.min(Duration::from_secs(8));
        let mut delay = Duration::from_millis(250);
        loop {
            let value = self.request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": path_to_uri(path) },
                    "range": {
                        "start": { "line": range.start.line, "character": range.start.character },
                        "end": { "line": range.end.line, "character": range.end.character }
                    },
                    "context": { "diagnostics": [] }
                }),
                DEFAULT_REQUEST_TIMEOUT,
            )?;
            let actions = parse_code_actions(value);
            self.answered = true;
            if !actions.is_empty() {
                return Ok(actions);
            }
            let now = Instant::now();
            if now >= deadline || now >= grace {
                return Ok(Vec::new());
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    /// Turn an assist into concrete edits.
    pub fn resolve_action(
        &mut self,
        action: &CodeAction,
        budget: Duration,
    ) -> Result<Vec<FileEdits>> {
        let value = self.request("codeAction/resolve", action.raw.clone(), budget)?;
        match value.get("edit") {
            Some(edit) if !edit.is_null() => parse_workspace_edit(edit.clone()),
            _ => Ok(Vec::new()),
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.request("shutdown", Value::Null, Duration::from_secs(5));
        self.notify("exit", Value::Null);
        thread::sleep(Duration::from_millis(100));
    }

    /// Ask the server to warm its own caches for this workspace.
    ///
    /// rust-analyzer builds a disk cache for dependency sources and proc-macro
    /// expansions; priming it costs the same work once, in the background, and
    /// saves it on the next load. The command is a hint: a build that does not
    /// know it answers with an error, which is ignored on purpose.
    pub fn prefill_caches(&mut self) {
        let _ = self.request(
            "workspace/executeCommand",
            json!({"command": "rust-analyzer.prefillCaches", "arguments": []}),
            Duration::from_secs(10),
        );
    }
}

impl Analyzer for Server {
    fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    fn did_open(&mut self, path: &Path, text: &str) -> Result<()> {
        Server::did_open(self, path, text)
    }

    fn refresh(&mut self, path: &Path, text: &str) -> Result<()> {
        Server::refresh(self, path, text)
    }

    fn wait_ready(&mut self, budget: Duration) -> bool {
        Server::wait_ready(self, budget)
    }

    fn rename(
        &mut self,
        path: &Path,
        position: Position,
        new_name: &str,
        budget: Duration,
    ) -> Result<Vec<FileEdits>> {
        Server::rename(self, path, position, new_name, budget)
    }

    fn references(
        &mut self,
        path: &Path,
        position: Position,
        budget: Duration,
    ) -> Result<Vec<Location>> {
        Server::references(self, path, position, budget)
    }

    fn code_actions(
        &mut self,
        path: &Path,
        range: Range,
        budget: Duration,
    ) -> Result<Vec<CodeAction>> {
        Server::code_actions(self, path, range, budget)
    }

    fn resolve_action(&mut self, action: &CodeAction, budget: Duration) -> Result<Vec<FileEdits>> {
        Server::resolve_action(self, action, budget)
    }

    fn shutdown(&mut self) {
        Server::shutdown(self)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn reply_for(message: &Value) -> Value {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        // The server asks for permission to register dynamic capabilities and
        // to create progress tokens; both are granted by returning null.
        "client/registerCapability" | "window/workDoneProgress/create" => Value::Null,
        "workspace/configuration" => {
            let count = message
                .get("params")
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            json!(vec![Value::Null; count])
        }
        _ => Value::Null,
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            length = rest.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = length else {
        return Ok(None);
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body).ok())
}

fn write_message<W: Write>(writer: &mut W, message: &Value) -> Result<()> {
    let body = serde_json::to_string(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()?;
    Ok(())
}

fn parse_workspace_edit(value: Value) -> Result<Vec<FileEdits>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let edit: WorkspaceEdit = serde_json::from_value(value).context("parsing WorkspaceEdit")?;

    let mut by_path: BTreeMap<PathBuf, Vec<TextEdit>> = BTreeMap::new();
    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            if let Some(path) = uri_to_path(&uri) {
                by_path.entry(path).or_default().extend(edits);
            }
        }
    }
    for change in edit.document_changes.unwrap_or_default() {
        // Resource operations (create/rename/delete file) carry no textDocument
        // and are not something a rename should produce; ignore them.
        if change.get("textDocument").is_none() {
            continue;
        }
        match serde_json::from_value::<TextDocumentEdit>(change) {
            Ok(document_edit) => {
                if let Some(path) = uri_to_path(&document_edit.text_document.uri) {
                    by_path.entry(path).or_default().extend(document_edit.edits);
                }
            }
            Err(err) => bail!("parsing textDocument edit: {err}"),
        }
    }

    Ok(by_path
        .into_iter()
        .map(|(path, edits)| FileEdits { path, edits })
        .collect())
}

fn parse_locations(value: Value) -> Result<Vec<Location>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawLocation {
        uri: String,
        range: Range,
    }
    let raw: Vec<RawLocation> =
        serde_json::from_value(value).context("parsing textDocument/references")?;
    Ok(raw
        .into_iter()
        .filter_map(|location| {
            uri_to_path(&location.uri).map(|path| Location {
                path,
                range: location.range,
            })
        })
        .collect())
}

fn parse_code_actions(value: Value) -> Vec<CodeAction> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let title = item.get("title")?.as_str()?.to_string();
            // A `Command` has a title but no data, kind or edit; only real
            // CodeAction literals can be resolved into an edit.
            if item.get("data").is_none()
                && item.get("edit").is_none()
                && item.get("kind").is_none()
            {
                return None;
            }
            Some(CodeAction {
                title,
                kind: item.get("kind").and_then(Value::as_str).map(str::to_string),
                raw: item.clone(),
            })
        })
        .collect()
}

/// Apply edits to in-memory text, returning the new text.
pub fn apply_edits(text: &str, edits: &[TextEdit], encoding: PositionEncoding) -> Result<String> {
    let mut offsets = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = position_to_offset(text, edit.range.start, encoding)
            .with_context(|| format!("edit start out of range: {:?}", edit.range.start))?;
        let end = position_to_offset(text, edit.range.end, encoding)
            .with_context(|| format!("edit end out of range: {:?}", edit.range.end))?;
        if start > end {
            bail!("edit range is inverted: {start}..{end}");
        }
        offsets.push((start, end, edit.new_text.clone()));
    }
    offsets.sort_by_key(|(start, _, _)| *start);
    for pair in offsets.windows(2) {
        if pair[0].1 > pair[1].0 {
            bail!("rust-analyzer returned overlapping edits; refusing to apply them");
        }
    }
    let mut result = text.to_string();
    for (start, end, replacement) in offsets.iter().rev() {
        result.replace_range(*start..*end, replacement);
    }
    Ok(result)
}

pub fn position_to_offset(
    text: &str,
    position: Position,
    encoding: PositionEncoding,
) -> Option<usize> {
    let mut line_start = 0usize;
    for _ in 0..position.line {
        line_start += text[line_start..].find('\n')? + 1;
    }
    let line_end = text[line_start..]
        .find('\n')
        .map(|index| line_start + index)
        .unwrap_or(text.len());
    let line = &text[line_start..line_end];

    match encoding {
        PositionEncoding::Utf8 => {
            let offset = line_start + position.character as usize;
            (offset <= line_end).then_some(offset)
        }
        PositionEncoding::Utf16 => {
            let mut units = 0u32;
            for (byte, ch) in line.char_indices() {
                if units == position.character {
                    return Some(line_start + byte);
                }
                units += ch.len_utf16() as u32;
                if units > position.character {
                    return None; // position splits a surrogate pair
                }
            }
            (units == position.character).then_some(line_end)
        }
    }
}

pub fn path_to_uri(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut text = absolute.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        text.insert(0, '/');
    }
    format!("file://{}", percent_encode(&text))
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b' ' | b'#' | b'?' | b'%' | b'[' | b']' => out.push_str(&format!("%{byte:02X}")),
            _ => out.push(byte as char),
        }
    }
    out
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    let trimmed = if cfg!(windows) && decoded.len() > 2 && decoded.as_bytes()[0] == b'/' {
        // `file:///C:/x` decodes to `/C:/x`; the drive letter needs no slash.
        decoded[1..].to_string()
    } else {
        decoded
    };
    Some(PathBuf::from(
        trimmed.replace('/', std::path::MAIN_SEPARATOR_STR),
    ))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Convert a human position (0-based line, 0-based character index) into the
/// wire position for the negotiated encoding. A person counting columns is
/// counting characters, which is not what LSP counts by default.
pub fn char_position(
    text: &str,
    line0: u32,
    char0: u32,
    encoding: PositionEncoding,
) -> Option<Position> {
    let mut line_start = 0usize;
    for _ in 0..line0 {
        line_start += text[line_start..].find('\n')? + 1;
    }
    let line_end = text[line_start..]
        .find('\n')
        .map(|index| line_start + index)
        .unwrap_or(text.len());
    let line = &text[line_start..line_end];

    let mut byte = line.len();
    for (count, (index, _)) in line.char_indices().enumerate() {
        if count as u32 == char0 {
            byte = index;
            break;
        }
    }
    let prefix = &line[..byte];
    let character = match encoding {
        PositionEncoding::Utf8 => prefix.len() as u32,
        PositionEncoding::Utf16 => prefix.chars().map(|c| c.len_utf16() as u32).sum(),
    };
    Some(Position {
        line: line0,
        character,
    })
}
