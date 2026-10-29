//! Phase 1 of the type-attribution layer: the LSP client.
//!
//! Each test drives the real binary against a throwaway copy of a fixture crate
//! and asserts on what the *resolved* rename touched. These tests need
//! `rust-analyzer` on PATH; they skip themselves with a loud message when it is
//! not installed rather than failing for the wrong reason.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

/// rust-analyzer is memory hungry; running these in parallel is a good way to
/// make a laptop swap instead of running a test.
static SERIAL: Mutex<()> = Mutex::new(());

fn available() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
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

fn fixture(suffix: &str) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("semantic-crate");
    let destination = std::env::temp_dir().join(format!(
        "rustrewrite-sem-{}-{}-{}",
        suffix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&destination);
    copy_dir(&source, &destination);
    destination
}

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rustrewrite"))
        .args(args)
        .arg("--path")
        .arg(dir)
        .output()
        .expect("running rustrewrite")
}

fn read(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("src").join("main.rs")).expect("reading fixture")
}

/// Build a `file:line:column` location for `needle` on the line holding `marker`.
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn compiles(dir: &Path) -> bool {
    Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(dir)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[test]
fn rename_distinguishes_methods_with_the_same_name() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed (rustup component add rust-analyzer)");
        return;
    }
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = fixture("len");
    let before = read(&dir);
    let at = locate(&dir, &before, "fn len(&self) -> usize", "len");

    let output = run(&["rename", "--at", &at, "--to", "size", "--apply"], &dir);
    assert!(
        output.status.success(),
        "rename failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("fn size(&self) -> usize"),
        "the declaration was not renamed:\n{after}"
    );
    assert!(
        after.contains("bag.size()"),
        "the call on Bag was not renamed:\n{after}"
    );
    assert!(
        after.contains("numbers.len()"),
        "Vec::len must be left alone:\n{after}"
    );
    assert!(
        !after.contains("numbers.size()"),
        "Vec::len was renamed, which is the bug this layer exists to prevent:\n{after}"
    );
    assert!(
        stdout(&output).contains("2 edit(s)"),
        "expected exactly two edits:\n{}",
        stdout(&output)
    );
    assert!(compiles(&dir), "the renamed crate does not compile");
}

#[test]
fn rename_covers_every_call_site() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = fixture("sites");
    let before = read(&dir);
    let at = locate(&dir, &before, "fn start(&self)", "start");

    let output = run(&["rename", "--at", &at, "--to", "spawn", "--apply"], &dir);
    assert!(
        output.status.success(),
        "rename failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert_eq!(
        after.matches("server.spawn()").count(),
        3,
        "all three call sites should be renamed:\n{after}"
    );
    assert!(
        !after.contains("server.start()"),
        "a call site was missed:\n{after}"
    );
    assert!(
        after.contains("server.stop()"),
        "an unrelated method was renamed:\n{after}"
    );
    assert!(compiles(&dir));
}

#[test]
fn rename_of_a_trait_implementation_is_not_left_behind() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = fixture("trait");
    let before = read(&dir);
    let at = locate(&dir, &before, "fn clone(&self) -> Self", "clone");

    let output = run(
        &["rename", "--at", &at, "--to", "duplicate", "--apply"],
        &dir,
    );
    let report = format!("{}\n{}", stdout(&output), stderr(&output));

    // Renaming a trait implementation member is not a legal refactor. The server
    // may decline it outright, or it may apply a rename that no longer satisfies
    // the trait; either way the outcome must be a refusal and an untouched file.
    assert!(
        !output.status.success(),
        "renaming a trait implementation member must not report success:\n{report}"
    );
    assert!(
        report.contains("declined") || report.contains("rolled back"),
        "the refusal must be reported in plain terms:\n{report}"
    );
    assert_eq!(
        read(&dir),
        before,
        "a refused rename must leave the file untouched"
    );
}

#[test]
fn dry_run_reports_without_writing() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = fixture("dry");
    let before = read(&dir);
    let at = locate(&dir, &before, "fn len(&self) -> usize", "len");

    let output = run(&["rename", "--at", &at, "--to", "size"], &dir);
    assert!(
        output.status.success(),
        "dry run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("dry run"),
        "expected a dry-run summary:\n{}",
        stdout(&output)
    );
    assert_eq!(read(&dir), before, "a dry run must not modify the source");
}
