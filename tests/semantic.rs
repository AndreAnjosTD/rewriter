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
        "rewriter-sem-{}-{}-{}",
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

#[test]
fn semantic_recipe_needs_its_options() {
    let dir = fixture("needs");
    let output = run(
        &["check", "--recipe", "rust.migrate.change-method-name"],
        &dir,
    );
    assert!(
        !output.status.success(),
        "a recipe with required options must refuse to run without them"
    );
    let text = stderr(&output);
    assert!(
        text.contains("--set file"),
        "the refusal must name the missing option:\n{text}"
    );
}

#[test]
fn change_method_name_recipe_reports_without_writing() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = fixture("recipe-check");
    let before = read(&dir);

    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.change-method-name",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn len(&self) -> usize",
            "--set",
            "from=len",
            "--set",
            "to=size",
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
        report.contains("rust.migrate.change-method-name"),
        "expected the recipe in the report:\n{report}"
    );
    assert!(
        report.contains("2 finding(s)"),
        "expected the declaration and its one call:\n{report}"
    );
    assert_eq!(read(&dir), before, "check must not write");
}

#[test]
fn change_method_name_recipe_applies_and_gates() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = fixture("recipe-apply");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.change-method-name",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn len(&self) -> usize",
            "--set",
            "from=len",
            "--set",
            "to=size",
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
    assert!(after.contains("fn size(&self) -> usize"), "{after}");
    assert!(after.contains("bag.size()"), "{after}");
    assert!(
        after.contains("numbers.len()"),
        "Vec::len must survive the recipe:\n{after}"
    );
    assert!(
        stdout(&output).contains("correctness gate passed"),
        "the gate must run:\n{}",
        stdout(&output)
    );
    assert!(compiles(&dir));
}

#[test]
fn change_type_recipe_renames_every_reference() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = fixture("recipe-type");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.change-type",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=struct Bag",
            "--set",
            "from=Bag",
            "--set",
            "to=Sack",
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
        after.contains("struct Sack;"),
        "declaration not renamed:\n{after}"
    );
    assert!(
        after.contains("impl Sack {"),
        "impl block not renamed:\n{after}"
    );
    assert!(
        after.contains("let bag = Sack;"),
        "use site not renamed:\n{after}"
    );
    assert!(!after.contains("Bag"), "a reference was missed:\n{after}");
    assert!(compiles(&dir));
}
