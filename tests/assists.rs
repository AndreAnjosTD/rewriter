//! Phase 4: rust-analyzer's built-in assists, driven through the recipe model.
//!
//! An assist is resolved by title at a position, converted into concrete edits,
//! applied, and then compiled like any other change.

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
        .join("assist-crate");
    let destination = std::env::temp_dir().join(format!(
        "rewriter-assist-{}-{}-{}",
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

const INLINE: [&str; 12] = [
    "run",
    "--recipe",
    "rust.migrate.assist",
    "--set",
    "file=src/main.rs",
    "--set",
    "anchor=let sum",
    "--set",
    "from=sum",
    "--set",
    "title=Inline variable",
    "--apply",
];

#[test]
fn assist_inlines_a_variable_and_gates_the_result() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("inline");

    let output = run(&INLINE, &dir);
    assert!(
        output.status.success(),
        "run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        !after.contains("let sum"),
        "the binding should be gone:\n{after}"
    );
    assert!(
        after.contains("items.iter().sum::<u32>() * 2"),
        "the expression was not inlined:\n{after}"
    );
    assert!(
        stdout(&output).contains("correctness gate passed"),
        "the gate must run:\n{}",
        stdout(&output)
    );
    assert!(compiles(&dir));
}

#[test]
fn check_reports_an_assist_without_writing() {
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
            "rust.migrate.assist",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=let sum",
            "--set",
            "from=sum",
            "--set",
            "title=Inline variable",
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
        report.contains("rust.migrate.assist"),
        "expected the assist in the report:\n{report}"
    );
    assert_eq!(read(&dir), before, "check must not write");
}

#[test]
fn an_unknown_assist_lists_what_is_available() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("unknown");

    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.assist",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=let sum",
            "--set",
            "from=sum",
            "--set",
            "title=Make me a sandwich",
        ],
        &dir,
    );

    assert!(
        !output.status.success(),
        "an assist that does not exist must fail"
    );
    let text = stderr(&output);
    assert!(
        text.contains("Inline variable"),
        "the error should name what is available:\n{text}"
    );
}
