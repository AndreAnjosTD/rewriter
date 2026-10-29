//! Phase 5: the warm server, and the promise not to start an analyzer at all
//! when nothing semantic is asked.
//!
//! These tests spawn a real background server, so they are serialised and each
//! one stops what it started.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn available() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

fn unique(suffix: &str) -> String {
    format!(
        "{}-{}-{}",
        suffix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    )
}

fn fixture(name: &str, suffix: &str) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    let destination = std::env::temp_dir().join(format!("rustrewrite-warm-{}", unique(suffix)));
    let _ = std::fs::remove_dir_all(&destination);
    copy_dir(&source, &destination);
    destination
}

fn state_dir(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rustrewrite-state-{}", unique(suffix)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create state dir");
    dir
}

fn run_with(args: &[&str], dir: &Path, state: &Path, analyzer: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rustrewrite"));
    command
        .args(args)
        .arg("--path")
        .arg(dir)
        .env("RUSTREWRITE_STATE_DIR", state);
    if let Some(analyzer) = analyzer {
        command.env("RUSTREWRITE_ANALYZER", analyzer);
    }
    command.output().expect("running rustrewrite")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read_main(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("src").join("main.rs")).expect("reading fixture")
}

/// `file:line:column` for the first occurrence of `needle` on the line holding
/// `marker`, 1-based.
fn locate(dir: &Path, text: &str, marker: &str, needle: &str) -> String {
    let line_index = text
        .lines()
        .position(|line| line.contains(marker))
        .unwrap_or_else(|| panic!("marker not found: {marker}"));
    let line = text.lines().nth(line_index).expect("line");
    let column_index = line.find(needle).expect("needle on line");
    let column = line[..column_index].chars().count() + 1;
    format!(
        "{}/src/main.rs:{}:{}",
        dir.display(),
        line_index + 1,
        column
    )
}

/// `(pid, requests)` from `serve --status`, or `None` when nothing runs.
fn server_status(dir: &Path, state: &Path) -> Option<(u32, u64)> {
    let output = run_with(&["serve", "--status"], dir, state, None);
    let text = stdout(&output);
    if text.contains("no warm server") {
        return None;
    }
    let pid = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("warm server pid "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|pid| pid.parse().ok())?;
    let requests = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("requests"))
        .and_then(|rest| rest.trim().parse().ok())?;
    Some((pid, requests))
}

#[test]
fn a_structural_run_never_starts_the_analyzer() {
    let _guard = guard();
    let dir = fixture("demo-crate", "structural");
    let state = state_dir("structural");
    // A path that cannot exist: if anything tried to launch the analyzer, this
    // run would fail instead of rewriting.
    let missing = std::env::temp_dir()
        .join("no-such-rust-analyzer")
        .display()
        .to_string();

    let output = run_with(
        &["run", "--recipe", "rust.idioms", "--apply"],
        &dir,
        &state,
        Some(&missing),
    );

    assert!(
        output.status.success(),
        "a structural recipe must not need the analyzer:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains(&missing),
        "the analyzer was touched:\n{}",
        stderr(&output)
    );
    assert!(
        server_status(&dir, &state).is_none(),
        "no warm server should have been started"
    );
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&state).ok();
}

#[test]
fn a_semantic_run_says_which_analyzer_is_missing() {
    let _guard = guard();
    let dir = fixture("semantic-crate", "missing");
    let state = state_dir("missing");
    let before = read_main(&dir);
    let at = locate(&dir, &before, "fn capacity", "capacity");
    let missing = std::env::temp_dir()
        .join("no-such-rust-analyzer")
        .display()
        .to_string();

    let output = run_with(
        &["rename", "--at", &at, "--to", "room", "--cold"],
        &dir,
        &state,
        Some(&missing),
    );

    assert!(
        !output.status.success(),
        "a rename cannot succeed without an analyzer"
    );
    let message = stderr(&output);
    assert!(
        message.contains("no-such-rust-analyzer"),
        "the error should name the binary it tried, not just the word rust-analyzer:\n{message}"
    );
    assert_eq!(
        read_main(&dir),
        before,
        "nothing may be written when the analyzer is missing"
    );
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&state).ok();
}

#[test]
fn a_second_run_is_answered_by_the_same_server() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("semantic-crate", "reuse");
    let state = state_dir("reuse");
    let text = read_main(&dir);
    let at = locate(&dir, &text, "fn capacity", "capacity");

    let first = run_with(&["rename", "--at", &at, "--to", "room"], &dir, &state, None);
    assert!(
        first.status.success(),
        "the first run should start a server and use it:\n{}\n{}",
        stdout(&first),
        stderr(&first)
    );
    let (pid, requests) = server_status(&dir, &state).expect("a server should be running");

    let second = run_with(&["rename", "--at", &at, "--to", "room"], &dir, &state, None);
    assert!(
        second.status.success(),
        "the second run should be answered by the warm server:\n{}\n{}",
        stdout(&second),
        stderr(&second)
    );
    let (same_pid, later_requests) =
        server_status(&dir, &state).expect("the server should still be running");
    assert_eq!(
        same_pid, pid,
        "the second run started a new server instead of reusing this one"
    );
    assert!(
        later_requests > requests,
        "the server did not serve the second run ({requests} then {later_requests})"
    );
    assert!(
        !stdout(&second).contains("warm server unavailable"),
        "the second run fell back to a one-shot analyzer:\n{}",
        stderr(&second)
    );

    let stopped = run_with(&["serve", "--stop"], &dir, &state, None);
    assert!(
        stopped.status.success(),
        "stopping the server failed:\n{}\n{}",
        stdout(&stopped),
        stderr(&stopped)
    );
    assert!(
        stdout(&stopped).contains("stopped the warm server"),
        "unexpected stop output:\n{}",
        stdout(&stopped)
    );
    assert!(
        server_status(&dir, &state).is_none(),
        "the server is still answering after being stopped"
    );

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&state).ok();
}

#[test]
fn cold_runs_leave_no_server_behind() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("semantic-crate", "cold");
    let state = state_dir("cold");
    let text = read_main(&dir);
    let at = locate(&dir, &text, "fn capacity", "capacity");

    let output = run_with(
        &["rename", "--at", &at, "--to", "room", "--cold"],
        &dir,
        &state,
        None,
    );
    assert!(
        output.status.success(),
        "a cold run should still work:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        server_status(&dir, &state).is_none(),
        "--cold must not start a warm server"
    );
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&state).ok();
}
