//! The two remaining OpenRewrite primitives that map onto Rust: changing a
//! declared return type, and a recipe-level resolved rename.
//!
//! Each test runs the real binary against a throwaway copy of a fixture crate.
//! Return-type changes need no analyzer, but the rename recipes do, so those skip
//! themselves when `rust-analyzer` is not installed.

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
        .join("return-type-crate");
    let destination = std::env::temp_dir().join(format!(
        "rustrewrite-types-{}-{}-{}",
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_rustrewrite"));
    command.args(args).arg("--path").arg(dir);
    // Tests use a throwaway workspace per case, so a warm server would never be
    // reused and would outlive the test. The warm path has its own test file.
    if matches!(args.first().copied(), Some("run" | "check" | "rename")) {
        command.arg("--cold");
    }
    command.output().expect("running rustrewrite")
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

fn reported(output: &Output) -> String {
    format!("{}\n{}", stdout(output), stderr(output))
}

#[test]
fn change_return_type_replaces_an_existing_annotation() {
    let _guard = guard();
    let dir = fixture("replace");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.change-return-type",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn retries() -> usize",
            "--set",
            "from=retries",
            "--set",
            "to=u64",
            "--apply",
        ],
        &dir,
    );
    assert!(
        output.status.success(),
        "run failed:\n{}",
        reported(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("fn retries() -> u64"),
        "the return type was not replaced:\n{after}"
    );
    assert!(
        after.contains("let attempts = retries();"),
        "the untyped call site should be left alone:\n{after}"
    );
    assert!(
        stdout(&output).contains("correctness gate passed"),
        "the gate must run:\n{}",
        stdout(&output)
    );
    assert!(compiles(&dir), "the rewritten crate does not compile");
}

#[test]
fn change_return_type_inserts_an_arrow_where_there_is_none() {
    let _guard = guard();
    let dir = fixture("insert");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.change-return-type",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn forever()",
            "--set",
            "from=forever",
            "--set",
            "to=u64",
            "--apply",
        ],
        &dir,
    );
    assert!(
        output.status.success(),
        "run failed:\n{}",
        reported(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("fn forever() -> u64 {"),
        "an arrow was not inserted:\n{after}"
    );
    assert!(
        !after.contains("fn forever() -> u64 ->"),
        "the arrow was inserted twice:\n{after}"
    );
    assert!(compiles(&dir), "the rewritten crate does not compile");
}

#[test]
fn change_return_type_rolls_back_when_a_call_site_breaks() {
    let _guard = guard();
    let dir = fixture("rollback");
    let before = read(&dir);

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.change-return-type",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn limit() -> usize",
            "--set",
            "from=limit",
            "--set",
            "to=u64",
            "--apply",
        ],
        &dir,
    );
    let report = reported(&output);

    assert!(
        !output.status.success(),
        "a change that breaks a caller must fail the run:\n{report}"
    );
    assert!(
        report.contains("rolled back"),
        "the rollback must be reported:\n{report}"
    );
    assert_eq!(
        read(&dir),
        before,
        "a rejected change must leave nothing on disk"
    );
    assert!(compiles(&dir), "the rolled-back crate must still compile");
}

#[test]
fn check_reports_a_return_type_change_without_writing() {
    let _guard = guard();
    let dir = fixture("check-type");
    let before = read(&dir);

    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.change-return-type",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn retries() -> usize",
            "--set",
            "from=retries",
            "--set",
            "to=u64",
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
        report.contains("1 finding(s)"),
        "expected one declaration change:\n{report}"
    );
    assert!(
        report.contains("usize -> u64"),
        "expected the old and new types in the report:\n{report}"
    );
    assert_eq!(read(&dir), before, "check must not write");
}

#[test]
fn rename_symbol_leaves_the_same_name_on_another_type_alone() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("rename-const");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.rename-symbol",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=pub const MAX: u32 = 100",
            "--set",
            "from=MAX",
            "--set",
            "to=CEILING",
            "--apply",
        ],
        &dir,
    );
    assert!(
        output.status.success(),
        "run failed:\n{}",
        reported(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("pub const CEILING: u32 = 100;"),
        "the constant was not renamed:\n{after}"
    );
    assert!(
        after.contains("let configured = CEILING;"),
        "a reference to the constant was missed:\n{after}"
    );
    assert!(
        after.contains("pub const MAX: u32 = 10;"),
        "the associated constant on Config must be untouched:\n{after}"
    );
    assert!(
        after.contains("let nested = Config::MAX;"),
        "a use of the associated constant was renamed:\n{after}"
    );
    assert!(compiles(&dir), "the renamed crate does not compile");
}

#[test]
fn rename_symbol_renames_a_field_without_touching_another_type() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("rename-field");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.rename-symbol",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=pub value: u32",
            "--set",
            "from=value",
            "--set",
            "to=amount",
            "--apply",
        ],
        &dir,
    );
    assert!(
        output.status.success(),
        "run failed:\n{}",
        reported(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("pub amount: u32,"),
        "the field was not renamed:\n{after}"
    );
    assert!(
        after.contains("Reading { amount: 1 }"),
        "the struct literal was not updated:\n{after}"
    );
    assert!(
        after.contains("reading.amount"),
        "the field access was not updated:\n{after}"
    );
    assert!(
        after.contains("pub value: u64,") && after.contains("Sample { value: 2 }"),
        "the same-named field on Sample must be untouched:\n{after}"
    );
    assert!(compiles(&dir), "the renamed crate does not compile");
}

#[test]
fn a_recipe_composes_a_return_type_change_with_a_rename() {
    if !available() {
        eprintln!("SKIPPED: rust-analyzer is not installed");
        return;
    }
    let _guard = guard();
    let dir = fixture("compose");
    let before = read(&dir);

    let recipes = dir.join("recipes");
    std::fs::create_dir_all(&recipes).expect("create recipes dir");
    let recipe = recipes.join("compose.yml");
    std::fs::write(
        &recipe,
        r#"
id: test.return-type-then-rename
name: Change a return type, then rename the function
options:
  file:
    required: true
  anchor:
    required: true
  from:
    required: true
  to:
    required: true
  return_type:
    required: true
steps:
  - kind: semantic
    action: change-return-type
    file: ${file}
    anchor: ${anchor}
    from: ${from}
    to: ${return_type}
  - kind: semantic
    action: rename
    file: ${file}
    anchor: fn retries() -> u64
    from: ${from}
    to: ${to}
"#,
    )
    .expect("write recipe");

    let recipe_arg = recipe.display().to_string();
    let output = run(
        &[
            "check",
            "--recipe",
            "test.return-type-then-rename",
            "--set",
            "file=src/main.rs",
            "--set",
            "anchor=fn retries() -> usize",
            "--set",
            "from=retries",
            "--set",
            "to=retry_count",
            "--set",
            "return_type=u64",
            "--recipes",
            &recipe_arg,
            "--diff",
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
        report.contains("3 finding(s)"),
        "the return type and the rename's two edits should all report:\n{report}"
    );
    assert!(
        report.contains("fn retry_count() -> u64"),
        "the second step must resolve against the first step's pending text:\n{report}"
    );
    assert_eq!(read(&dir), before, "check must not write");
}
