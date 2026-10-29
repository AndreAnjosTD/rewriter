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
use serde::Deserialize;
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

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone)]
pub struct FileEdits {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
}

pub struct Server {
    child: Child,
    writer: Arc<Mutex<ChildStdin>>,
    rx: Receiver<Value>,
    next_id: i64,
    encoding: PositionEncoding,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

impl Server {
    /// Spawn `rust-analyzer` and complete the initialize handshake.
    pub fn start(root: &Path) -> Result<Self> {
        let mut child = Command::new("rust-analyzer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(root)
            .spawn()
            .with_context(|| {
                "starting rust-analyzer\n\n\
                 install it with `rustup component add rust-analyzer`, or point PATH at a \
                 rust-analyzer binary"
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
                        "textDocument": { "rename": { "prepareSupport": true } }
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
        let mut active: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut saw_progress = false;
        let mut saw_begin = false;
        loop {
            if saw_begin && active.is_empty() {
                return true;
            }
            if !saw_progress && started.elapsed() > silent_grace {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
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
                Ok(value) => return parse_workspace_edit(value),
                Err(err) => last_error = Some(err),
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

    pub fn shutdown(&mut self) {
        let _ = self.request("shutdown", Value::Null, Duration::from_secs(5));
        self.notify("exit", Value::Null);
        thread::sleep(Duration::from_millis(100));
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

fn position_to_offset(text: &str, position: Position, encoding: PositionEncoding) -> Option<usize> {
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
