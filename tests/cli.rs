//! End-to-end tests: every test runs the real binary against a throwaway copy of
//! a fixture crate, so nothing shares state and nothing touches the repo.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rustrewrite")
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

/// A fresh copy of a fixture in its own temp directory.
fn fixture(suffix: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("demo-crate");
    let dst = std::env::temp_dir().join(format!(
        "rustrewrite-it-{}-{}-{}",
        suffix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dst);
    copy_dir(&src, &dst);
    dst
}

fn run(args: &[&str], dir: &Path) -> Output {
    let mut command = Command::new(bin());
    command.args(args).arg("--path").arg(dir);
    // Tests use a throwaway workspace per case, so a warm server would never be
    // reused and would outlive the test. The warm path has its own test file.
    if matches!(args.first().copied(), Some("run" | "check" | "rename")) {
        command.arg("--cold");
    }
    command.output().expect("running rustrewrite")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("src").join("main.rs")).expect("reading fixture")
}

#[test]
fn recipe_catalog_examples_all_pass() {
    let output = Command::new(bin())
        .arg("test")
        .output()
        .expect("running rustrewrite test");
    assert!(
        output.status.success(),
        "recipe examples failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(stdout(&output).contains("0 failed"));
}

#[test]
fn check_reports_findings_and_never_writes() {
    let dir = fixture("check");
    let before = read(&dir);

    let output = run(&["check", "--recipe", "rust.idioms"], &dir);
    let report = stdout(&output);

    assert_eq!(output.status.code(), Some(1), "check exits 1 on findings");
    assert!(
        report.contains("rust.idiom.string-len-zero"),
        "expected a len-zero finding in:\n{report}"
    );
    assert!(
        report.contains("rust.idiom.bool-compare"),
        "expected a bool-compare finding in:\n{report}"
    );
    assert_eq!(read(&dir), before, "check must not modify the source");
}

#[test]
fn apply_rewrites_and_compiles() {
    let dir = fixture("apply");

    let output = run(&["run", "--recipe", "rust.idioms", "--apply"], &dir);
    assert!(
        output.status.success(),
        "apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("items.is_empty()"),
        "len-zero not applied:\n{after}"
    );
    assert!(
        after.contains("if ready {"),
        "bool-compare not applied:\n{after}"
    );
    assert!(
        after.contains("if !ready {"),
        "negated bool-compare not applied:\n{after}"
    );
    assert!(
        after.contains("String::new()"),
        "string-from-empty not applied:\n{after}"
    );
    assert!(
        after.contains("words.next()"),
        "nth-zero not applied:\n{after}"
    );

    // And the result is still a working crate.
    let check = Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(&dir)
        .output()
        .expect("cargo check");
    assert!(
        check.status.success(),
        "rewritten crate does not compile:\n{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(stdout(&output).contains("correctness gate passed"));
}

#[test]
fn apply_rolls_back_when_the_rewrite_breaks_the_build() {
    let dir = fixture("rollback");
    let before = read(&dir);

    // A project-local recipe that is deliberately wrong: it renames the length
    // check to a method that does not exist.
    let recipes = dir.join(".rustrewrite").join("recipes");
    std::fs::create_dir_all(&recipes).expect("create recipe dir");
    std::fs::write(
        recipes.join("broken.yml"),
        r#"
id: test.broken
name: Break the build on purpose
steps:
  - rule:
      pattern: $X.len() == 0
    fix: $X.definitely_not_a_method()
"#,
    )
    .expect("write broken recipe");

    let output = run(&["run", "--recipe", "test.broken", "--apply"], &dir);
    assert!(
        !output.status.success(),
        "a rewrite that breaks the build must fail:\n{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("rolled back"),
        "expected a visible rollback:\n{}",
        stdout(&output)
    );
    assert_eq!(
        read(&dir),
        before,
        "the failing rewrite must be rolled back"
    );
}

#[test]
fn scan_recipe_reports_without_writing() {
    let dir = fixture("scan");
    let before = read(&dir);

    let output = run(&["check", "--recipe", "rust.scan.unwrap"], &dir);
    let report = stdout(&output);

    assert!(
        report.contains("rust.scan.unwrap"),
        "expected an unwrap finding:\n{report}"
    );
    assert!(
        report.contains("panics on None/Err"),
        "expected the scan message:\n{report}"
    );
    assert_eq!(read(&dir), before, "scan must not modify the source");
}

#[test]
fn unknown_recipe_fails_with_a_useful_message() {
    let dir = fixture("unknown");
    let output = run(&["check", "--recipe", "rust.nope"], &dir);
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(
        text.contains("unknown recipe") && text.contains("rust.idiom.string-len-zero"),
        "expected the catalog listing in:\n{text}"
    );
}
