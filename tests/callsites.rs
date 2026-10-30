//! Phase 3: rewriting an argument list means moving the declaration and every
//! resolved call site together, or the crate stops compiling.
//!
//! Each test runs the real binary against a throwaway copy of a fixture crate and
//! checks both the rewritten text and that the result still builds.

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
        .join("callsite-crate");
    let destination = std::env::temp_dir().join(format!(
        "rewriter-call-{}-{}-{}",
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_rewriter"));
    command.args(args).arg("--path").arg(dir);
    // Tests use a throwaway workspace per case, so a warm server would never be
    // reused and would outlive the test. The warm path has its own test file.
    if matches!(args.first().copied(), Some("run" | "check" | "rename")) {
        command.arg("--cold");
    }
    command.output().expect("running rewriter")
}

fn read(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("src").join("main.rs")).expect("reading fixture")
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

fn guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn delete_argument_moves_declaration_and_every_call_site() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("delete");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.delete-argument",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn record(scope: &str",
            "--set",
            "from=record",
            "--set",
            "index=0",
            "--apply",
        ],
        &dir,
    );

    assert!(
        output.status.success(),
        "run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("fn record(message: &str)"),
        "the parameter was not removed:\n{after}"
    );
    for message in ["mounted", "connected", "hot"] {
        assert!(
            after.contains(&format!("record(\"{message}\")")),
            "call site for {message} was not rewritten:\n{after}"
        );
    }
    assert!(
        !after.contains("record(\"disk\","),
        "a call site still passes the removed argument:\n{after}"
    );
    assert!(compiles(&dir));
}

#[test]
fn delete_argument_handles_a_self_receiver() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("receiver");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.delete-argument",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn emit(&self",
            "--set",
            "from=emit",
            "--set",
            "index=0",
            "--apply",
        ],
        &dir,
    );

    assert!(
        output.status.success(),
        "run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("fn emit(&self, message: &str)"),
        "the receiver must stay while the argument goes:\n{after}"
    );
    assert!(
        after.contains("report.emit(\"full\")"),
        "the call site kept the removed argument:\n{after}"
    );
    assert!(compiles(&dir));
}

#[test]
fn add_argument_writes_both_sides() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("add");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.add-argument",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn record(scope: &str",
            "--set",
            "from=record",
            "--set",
            "index=1",
            "--set",
            "argument=\"trace\"",
            "--set",
            "parameter=level: &str",
            "--apply",
        ],
        &dir,
    );

    assert!(
        output.status.success(),
        "run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("fn record(scope: &str, level: &str, message: &str)"),
        "the parameter was not added in the right position:\n{after}"
    );
    assert!(
        after.contains("record(\"disk\", \"trace\", \"mounted\")"),
        "the argument was not added in the right position:\n{after}"
    );
    assert!(compiles(&dir));
}

#[test]
fn reorder_arguments_permutes_both_sides() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("reorder");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.reorder-arguments",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn record(scope: &str",
            "--set",
            "from=record",
            "--set",
            "order=1,0",
            "--apply",
        ],
        &dir,
    );

    assert!(
        output.status.success(),
        "run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("fn record(message: &str, scope: &str)"),
        "the parameters were not swapped:\n{after}"
    );
    assert!(
        after.contains("record(\"mounted\", \"disk\")"),
        "the arguments were not swapped:\n{after}"
    );
    assert!(compiles(&dir));
}

#[test]
fn check_reports_argument_rewrites_without_writing() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("check");
    let before = read(&dir);

    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.delete-argument",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn record(scope: &str",
            "--set",
            "from=record",
            "--set",
            "index=0",
        ],
        &dir,
    );
    let report = stdout(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "check exits 1 when it finds something:\n{report}\n{}",
        stderr(&output)
    );
    assert!(
        report.contains("4 finding(s)"),
        "expected the declaration and three call sites:\n{report}"
    );
    assert_eq!(read(&dir), before, "check must not write");
}
