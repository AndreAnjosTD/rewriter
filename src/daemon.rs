//! A warm rust-analyzer server, shared between command invocations.
//!
//! Loading a large workspace costs tens of seconds: on a 38k-line crate the
//! analyzer spent about 78 s and 1.9 GB before it would answer a single rename.
//! Paying that on every `rustrewrite run` made semantic recipes unusable in a
//! loop, so this module keeps one analyzer alive per workspace and lets later
//! commands ask the same questions of the same process.
//!
//! The shape is deliberately small: the server lives on a loopback port, writes
//! a state file naming that port and a token into the user's cache directory,
//! and speaks one JSON object per line. Everything the analyzer returns is
//! data, so the client still owns diffing, writing, gating and rollback.
//!
//! Two consequences worth knowing:
//!
//! - The server is keyed by workspace root, so two workspaces get two servers.
//! - It is not a daemon in the "always running" sense. It exits after
//!   `--idle` seconds without a request, and `rustrewrite serve --stop` ends it
//!   immediately.

use crate::semantic::{
    Analyzer, CodeAction, FileEdits, Location, Position, PositionEncoding, Range, Server,
};
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a warm server survives without a request.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(15 * 60);

/// A client connection to a warm server.
pub struct Remote {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    encoding: PositionEncoding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerState {
    pid: u32,
    port: u16,
    token: String,
    root: String,
    version: String,
}

/// What `rustrewrite serve --status` reports.
pub struct Status {
    pub pid: u32,
    pub ready: bool,
    pub queries: u64,
    pub idle_secs: u64,
}

/// Optional request trace, enabled with `RUSTREWRITE_TRACE=1`.
///
/// The server usually runs with its stderr in the void, so this is the only way
/// to see what it was asked and why it closed a connection.
fn trace(message: &str) {
    if std::env::var_os("RUSTREWRITE_TRACE").is_some() {
        eprintln!("[warm] {message}");
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Attach to a warm server for `root` if one is listening.
pub fn attach(root: &Path) -> Result<Option<Remote>> {
    let root = util::normalize_path(root)?;
    let Some(state) = read_state(&root) else {
        return Ok(None);
    };
    let address = format!("127.0.0.1:{}", state.port);
    let Ok(socket) = TcpStream::connect_timeout(
        &address.parse().expect("loopback address"),
        Duration::from_secs(3),
    ) else {
        // The port is closed, so the recorded process is gone. Its state file is
        // stale and would make every later run wait for a server that will
        // never answer.
        let _ = std::fs::remove_file(state_path(&root)?);
        return Ok(None);
    };
    match Remote::handshake(socket, &root, &state) {
        Ok(remote) => Ok(Some(remote)),
        // A live process that will not complete the handshake is left alone: it
        // may be mid-startup. Spawning a second server overwrites the state file,
        // and the first one exits on its own idle timer.
        Err(_) => Ok(None),
    }
}

/// Return a warm server for `root`, starting one if necessary.
pub fn ensure(root: &Path) -> Result<Remote> {
    let root = util::normalize_path(root)?;
    if let Some(remote) = attach(&root)? {
        return Ok(remote);
    }
    spawn_server(&root)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(remote) = attach(&root)? {
            return Ok(remote);
        }
        if Instant::now() >= deadline {
            bail!(
                "the warm server for {} did not come up within 30s",
                root.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Remote {
    fn handshake(socket: TcpStream, root: &Path, state: &ServerState) -> Result<Self> {
        socket.set_nodelay(true).ok();
        let mut remote = Remote {
            reader: BufReader::new(socket.try_clone().context("cloning the server socket")?),
            writer: socket,
            encoding: PositionEncoding::Utf16,
        };
        let hello = remote.call(
            json!({"op": "hello", "token": state.token, "root": root.display().to_string()}),
            Duration::from_secs(10),
        )?;
        remote.encoding = match hello.get("encoding").and_then(Value::as_str) {
            Some("utf-8") => PositionEncoding::Utf8,
            _ => PositionEncoding::Utf16,
        };
        Ok(remote)
    }

    fn call(&mut self, payload: Value, timeout: Duration) -> Result<Value> {
        self.writer.set_read_timeout(Some(timeout))?;
        serde_json::to_writer(&mut self.writer, &payload)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .with_context(|| format!("waiting for the warm server ({}s)", timeout.as_secs()))?;
        if read == 0 {
            bail!("the warm server closed the connection");
        }
        let reply: Value = serde_json::from_str(line.trim())
            .with_context(|| format!("the warm server sent unreadable JSON: {line:?}"))?;
        if reply.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(reply.get("result").cloned().unwrap_or(Value::Null))
        } else {
            bail!(
                "{}",
                reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the warm server failed without a message")
            )
        }
    }

    fn shutdown_server(&mut self) -> Result<()> {
        self.call(json!({"op": "shutdown"}), Duration::from_secs(15))
            .map(|_| ())
    }
}

impl Analyzer for Remote {
    fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    fn did_open(&mut self, path: &Path, text: &str) -> Result<()> {
        self.call(
            json!({"op": "did_open", "path": path.display().to_string(), "text": text}),
            Duration::from_secs(120),
        )
        .map(|_| ())
    }

    fn refresh(&mut self, path: &Path, text: &str) -> Result<()> {
        self.call(
            json!({"op": "refresh", "path": path.display().to_string(), "text": text}),
            Duration::from_secs(120),
        )
        .map(|_| ())
    }

    fn wait_ready(&mut self, budget: Duration) -> bool {
        let timeout = budget + Duration::from_secs(30);
        match self.call(
            json!({"op": "wait_ready", "budget": budget.as_secs()}),
            timeout,
        ) {
            Ok(value) => value.get("ready").and_then(Value::as_bool).unwrap_or(false),
            Err(err) => {
                eprintln!("note: {err:#}");
                false
            }
        }
    }

    fn rename(
        &mut self,
        path: &Path,
        position: Position,
        new_name: &str,
        budget: Duration,
    ) -> Result<Vec<FileEdits>> {
        let result = self.call(
            json!({
                "op": "rename",
                "path": path.display().to_string(),
                "line": position.line,
                "character": position.character,
                "newName": new_name,
                "budget": budget.as_secs()
            }),
            budget + Duration::from_secs(30),
        )?;
        Ok(serde_json::from_value(result)?)
    }

    fn references(
        &mut self,
        path: &Path,
        position: Position,
        budget: Duration,
    ) -> Result<Vec<Location>> {
        let result = self.call(
            json!({
                "op": "references",
                "path": path.display().to_string(),
                "line": position.line,
                "character": position.character,
                "budget": budget.as_secs()
            }),
            budget + Duration::from_secs(30),
        )?;
        Ok(serde_json::from_value(result)?)
    }

    fn code_actions(
        &mut self,
        path: &Path,
        range: Range,
        budget: Duration,
    ) -> Result<Vec<CodeAction>> {
        let result = self.call(
            json!({
                "op": "code_actions",
                "path": path.display().to_string(),
                "start": {"line": range.start.line, "character": range.start.character},
                "end": {"line": range.end.line, "character": range.end.character},
                "budget": budget.as_secs()
            }),
            budget + Duration::from_secs(30),
        )?;
        Ok(serde_json::from_value(result)?)
    }

    fn resolve_action(&mut self, action: &CodeAction, budget: Duration) -> Result<Vec<FileEdits>> {
        let result = self.call(
            json!({"op": "resolve_action", "action": action, "budget": budget.as_secs()}),
            budget + Duration::from_secs(30),
        )?;
        Ok(serde_json::from_value(result)?)
    }

    fn shutdown(&mut self) {
        let _ = self.call(json!({"op": "bye"}), Duration::from_secs(5));
    }
}

// ---------------------------------------------------------------------------
// Lifecycle helpers
// ---------------------------------------------------------------------------

/// Report on the warm server for `root`, if one is listening.
pub fn status(root: &Path) -> Result<Option<Status>> {
    let root = util::normalize_path(root)?;
    let Some(mut remote) = attach(&root)? else {
        return Ok(None);
    };
    let value = remote.call(json!({"op": "status"}), Duration::from_secs(10))?;
    Ok(Some(Status {
        pid: value.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
        ready: value.get("ready").and_then(Value::as_bool).unwrap_or(false),
        queries: value.get("queries").and_then(Value::as_u64).unwrap_or(0),
        idle_secs: value.get("idleSecs").and_then(Value::as_u64).unwrap_or(0),
    }))
}

/// Stop the warm server for `root`. Returns false when none was running.
pub fn stop(root: &Path) -> Result<bool> {
    let root = util::normalize_path(root)?;
    let Some(mut remote) = attach(&root)? else {
        return Ok(false);
    };
    remote.shutdown_server()?;
    // Give the process a moment to exit and to remove its own state file.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if attach(&root)?.is_none() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("the warm server for {} did not exit", root.display())
}

fn spawn_server(root: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("locating the rustrewrite binary")?;
    let args = vec![
        "--path".to_string(),
        root.display().to_string(),
        "serve".to_string(),
        "--idle".to_string(),
        DEFAULT_IDLE.as_secs().to_string(),
    ];
    spawn_detached(&exe, &args)
}

/// Start the server with no inherited handles and no console.
///
/// The obvious implementation — `Command::spawn` with null stdio — leaks the
/// caller's stdout pipe into the server, because a Windows child inherits every
/// inheritable handle of its parent. The pipe then stays open for as long as the
/// server lives, so `rustrewrite run ... | tee log` never sees it close and
/// appears to hang. `bInheritHandles = FALSE` is the only way to say "nothing of
/// mine", and std exposes it only on nightly.
#[cfg(windows)]
fn spawn_detached(exe: &Path, args: &[String]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };

    let mut command_line: Vec<u16> = Vec::new();
    let mut parts: Vec<String> = Vec::with_capacity(args.len() + 1);
    parts.push(exe.display().to_string());
    parts.extend(args.iter().cloned());
    let joined = parts
        .iter()
        .map(|part| quote_argument(part))
        .collect::<Vec<_>>()
        .join(" ");
    command_line.extend(joined.encode_utf16());
    command_line.push(0);

    let exe_wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let created = unsafe {
        CreateProcessW(
            exe_wide.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            FALSE,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error())
            .context("starting a warm server as a detached process");
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

/// Windows argument quoting, as the CRT parses it back.
#[cfg(windows)]
fn quote_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_string();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for ch in argument.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(other);
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(not(windows))]
fn spawn_detached(exe: &Path, args: &[String]) -> Result<()> {
    Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting a warm server")?;
    Ok(())
}

fn state_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("RUSTREWRITE_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("TEMP").map(PathBuf::from))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    };
    Ok(base
        .context("no cache directory; set RUSTREWRITE_STATE_DIR")?
        .join("rustrewrite")
        .join("servers"))
}

fn state_id(root: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    // Windows paths are case-insensitive, so one workspace should be one server
    // however the path was spelled.
    root.display().to_string().to_lowercase().hash(&mut hasher);
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn state_path(root: &Path) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!("{}.json", state_id(root))))
}

fn read_state(root: &Path) -> Option<ServerState> {
    let path = state_path(root).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let state: ServerState = serde_json::from_str(&text).ok()?;
    let expected = root.display().to_string();
    if state.version != env!("CARGO_PKG_VERSION") || !state.root.eq_ignore_ascii_case(&expected) {
        return None;
    }
    Some(state)
}

fn write_state(root: &Path, state: &ServerState) -> Result<()> {
    let path = state_path(root)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// Defence in depth, not authentication: the server answers on loopback only,
/// and everything it returns is data the local user can already read. The token
/// stops an unrelated local process from borrowing the connection by accident.
fn new_token(root: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    root.hash(&mut hasher);
    let first = hasher.finish();
    format!("{first:016x}{:016x}", first.rotate_left(17))
}

/// Removes this server's state file when the process ends, whatever ended it.
struct StateGuard {
    root: PathBuf,
    pid: u32,
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        if let Some(state) = read_state(&self.root) {
            if state.pid == self.pid {
                if let Ok(path) = state_path(&self.root) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Run a warm server in the foreground until it is stopped or goes idle.
pub fn serve(root: &Path, idle: Duration) -> Result<()> {
    let root = util::normalize_path(root)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("binding a loopback port for the warm server")?;
    let port = listener
        .local_addr()
        .context("reading the warm server's port")?
        .port();
    let state = ServerState {
        pid: std::process::id(),
        port,
        token: new_token(&root),
        root: root.display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    write_state(&root, &state)?;
    let guard = StateGuard {
        root: root.clone(),
        pid: state.pid,
    };

    let mut session = Session::start(&root)?;
    listener.set_nonblocking(true)?;
    eprintln!(
        "rustrewrite: warm server for {} on 127.0.0.1:{port}",
        root.display()
    );

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                trace("accepted a connection");
                session.last = Instant::now();
                if session.handle_connection(stream, &state)? {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if !idle.is_zero() && session.last.elapsed() >= idle {
                    eprintln!(
                        "rustrewrite: warm server idle for {}s, exiting",
                        idle.as_secs()
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(err) => return Err(err).context("accepting a warm-server connection"),
        }
    }

    trace("leaving the accept loop");
    session.server.shutdown();
    drop(guard);
    Ok(())
}

struct Session {
    server: Server,
    root: PathBuf,
    opened: HashSet<PathBuf>,
    ready: bool,
    queries: u64,
    last: Instant,
}

impl Session {
    fn start(root: &Path) -> Result<Self> {
        Ok(Session {
            server: Server::start(root)?,
            root: root.to_path_buf(),
            opened: HashSet::new(),
            ready: false,
            queries: 0,
            last: Instant::now(),
        })
    }

    /// Serve one client until it hangs up. Returns true when the client asked
    /// the server itself to stop.
    fn handle_connection(&mut self, stream: TcpStream, state: &ServerState) -> Result<bool> {
        // The listener is non-blocking so the accept loop can check its idle
        // timer. On Windows an accepted socket inherits that flag, which would
        // make every read after the first request fail with `WouldBlock` and
        // close the connection under the client's feet.
        stream
            .set_nonblocking(false)
            .context("making the connection blocking")?;
        stream
            .set_read_timeout(Some(Duration::from_secs(300)))
            .context("setting the connection timeout")?;
        stream.set_nodelay(true).ok();
        let mut reader = BufReader::new(stream.try_clone().context("cloning the connection")?);
        let mut writer = stream;
        let mut authorised = false;

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    trace("client closed the connection");
                    return Ok(false);
                }
                Ok(_) => {}
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    trace("connection timed out");
                    return Ok(false);
                }
                Err(err) => return Err(err).context("reading a warm-server request"),
            }
            trace(&format!("request {}", line.trim()));
            let request: Value = match serde_json::from_str(line.trim()) {
                Ok(value) => value,
                Err(err) => {
                    reply(&mut writer, Err(anyhow!("malformed request: {err}")))?;
                    continue;
                }
            };
            let op = request.get("op").and_then(Value::as_str).unwrap_or("");

            if !authorised {
                if op != "hello" {
                    reply(
                        &mut writer,
                        Err(anyhow!("the first request must be `hello`")),
                    )?;
                    return Ok(false);
                }
                if request.get("token").and_then(Value::as_str) != Some(state.token.as_str()) {
                    reply(&mut writer, Err(anyhow!("bad token")))?;
                    return Ok(false);
                }
                authorised = true;
                reply(
                    &mut writer,
                    Ok(json!({
                        "encoding": match self.server.encoding() {
                            PositionEncoding::Utf8 => "utf-8",
                            PositionEncoding::Utf16 => "utf-16",
                        },
                        "root": state.root,
                        "pid": state.pid
                    })),
                )?;
                continue;
            }

            match op {
                "bye" => {
                    reply(&mut writer, Ok(Value::Null))?;
                    trace("client said goodbye");
                    return Ok(false);
                }
                "shutdown" => {
                    reply(&mut writer, Ok(Value::Null))?;
                    trace("client asked the server to stop");
                    return Ok(true);
                }
                _ => {}
            }

            self.queries += 1;
            let outcome = self.handle(&request);
            trace(&format!(
                "{op} -> {}",
                if outcome.is_ok() { "ok" } else { "failed" }
            ));
            reply(&mut writer, outcome)?;
        }
    }

    fn handle(&mut self, request: &Value) -> Result<Value> {
        let op = request
            .get("op")
            .and_then(Value::as_str)
            .context("request has no `op`")?;
        let budget =
            Duration::from_secs(request.get("budget").and_then(Value::as_u64).unwrap_or(180));
        match op {
            "ping" => Ok(json!({"pid": std::process::id(), "ready": self.ready})),
            "status" => Ok(json!({
                "pid": std::process::id(),
                "ready": self.ready,
                "queries": self.queries,
                "root": self.root.display().to_string(),
                "idleSecs": self.last.elapsed().as_secs()
            })),
            "did_open" | "refresh" => {
                let path = self.file(request)?;
                let text = request
                    .get("text")
                    .and_then(Value::as_str)
                    .context("request has no `text`")?;
                if self.opened.contains(&path) {
                    self.server.refresh(&path, text)?;
                } else {
                    self.server.did_open(&path, text)?;
                    self.opened.insert(path);
                }
                Ok(Value::Null)
            }
            "wait_ready" => {
                let ready = if self.ready {
                    true
                } else {
                    let ready = self.server.wait_ready(budget);
                    if ready {
                        // Priming the disk cache is what makes the *next* cold
                        // start cheaper; it costs nothing to ask for here.
                        self.server.prefill_caches();
                    }
                    ready
                };
                self.ready = ready;
                Ok(json!({"ready": ready}))
            }
            "rename" => {
                let path = self.file(request)?;
                let position = position(request)?;
                let new_name = request
                    .get("newName")
                    .and_then(Value::as_str)
                    .context("request has no `newName`")?;
                let edits = self.server.rename(&path, position, new_name, budget)?;
                Ok(serde_json::to_value(edits)?)
            }
            "references" => {
                let path = self.file(request)?;
                let position = position(request)?;
                let locations = self.server.references(&path, position, budget)?;
                Ok(serde_json::to_value(locations)?)
            }
            "code_actions" => {
                let path = self.file(request)?;
                let range = Range {
                    start: point(request.get("start"), "start")?,
                    end: point(request.get("end"), "end")?,
                };
                let actions = self.server.code_actions(&path, range, budget)?;
                Ok(serde_json::to_value(actions)?)
            }
            "resolve_action" => {
                let action: CodeAction = serde_json::from_value(
                    request
                        .get("action")
                        .cloned()
                        .context("request has no `action`")?,
                )
                .context("request carries an unreadable code action")?;
                let edits = self.server.resolve_action(&action, budget)?;
                Ok(serde_json::to_value(edits)?)
            }
            other => bail!("unknown request `{other}`"),
        }
    }

    fn file(&self, request: &Value) -> Result<PathBuf> {
        let raw = request
            .get("path")
            .and_then(Value::as_str)
            .context("request has no `path`")?;
        let path = util::normalize_path(Path::new(raw))?;
        if !path.starts_with(&self.root) {
            bail!(
                "{} is outside the workspace {}",
                path.display(),
                self.root.display()
            );
        }
        Ok(path)
    }
}

fn position(request: &Value) -> Result<Position> {
    Ok(Position {
        line: request
            .get("line")
            .and_then(Value::as_u64)
            .context("request has no `line`")? as u32,
        character: request
            .get("character")
            .and_then(Value::as_u64)
            .context("request has no `character`")? as u32,
    })
}

fn point(value: Option<&Value>, field: &str) -> Result<Position> {
    let value = value.with_context(|| format!("request has no `{field}`"))?;
    Ok(Position {
        line: value
            .get("line")
            .and_then(Value::as_u64)
            .with_context(|| format!("`{field}` has no line"))? as u32,
        character: value
            .get("character")
            .and_then(Value::as_u64)
            .with_context(|| format!("`{field}` has no character"))? as u32,
    })
}

fn reply(writer: &mut TcpStream, outcome: Result<Value>) -> Result<()> {
    let message = match outcome {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(err) => json!({"ok": false, "error": format!("{err:#}")}),
    };
    serde_json::to_writer(&mut *writer, &message)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_state_file_per_workspace_and_none_when_absent() {
        let first = state_id(Path::new("C:\\work\\alpha"));
        let again = state_id(Path::new("C:\\work\\alpha"));
        let other = state_id(Path::new("C:\\work\\beta"));
        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(first, state_id(Path::new("c:\\WORK\\Alpha")));
    }

    #[test]
    fn a_stale_state_file_is_not_mistaken_for_an_absent_one() {
        let root = std::env::temp_dir().join(format!("rustrewrite-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create");
        let state = ServerState {
            pid: 1,
            port: 1,
            token: "t".into(),
            root: root.display().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        write_state(&root, &state).expect("write");
        let read = read_state(&root).expect("read back");
        assert_eq!(read.port, 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
