//! The visibility primitive: OpenRewrite's `ChangeMethodAccessLevel` expressed
//! as one declaration edit.
//!
//! Each test runs the real binary against a throwaway copy of a fixture crate,
//! then compiles the result. No analyzer is involved: the edit is textual and
//! the correctness gate is what decides whether the new visibility is valid.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        .join("visibility-crate");
    let destination = std::env::temp_dir().join(format!(
        "rustrewrite-visibility-{}-{}-{}",
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
    // reused and would outlive the test.
    if matches!(args.first().copied(), Some("run" | "check" | "rename")) {
        command.arg("--cold");
    }
    command.output().expect("running rustrewrite")
}

fn read(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("src").join("lib.rs")).expect("reading fixture")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn reported(output: &Output) -> String {
    format!("{}\n{}", stdout(output), stderr(output))
}

fn compiles(dir: &Path) -> bool {
    Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(dir)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn change(dir: &Path, anchor: &str, from: &str, to: &str) -> Output {
    run(
        &[
            "run",
            "--recipe",
            "rust.migrate.change-visibility",
            "--set",
            "file=src/lib.rs",
            "--set",
            &format!("anchor={anchor}"),
            "--set",
            &format!("from={from}"),
            "--set",
            &format!("to={to}"),
            "--apply",
        ],
        dir,
    )
}

#[test]
fn a_private_function_becomes_crate_visible() {
    let dir = fixture("private-to-crate");
    let output = change(&dir, "fn helper() -> u32", "helper", "pub(crate)");
    assert!(
        output.status.success(),
        "run failed:\n{}",
        reported(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("pub(crate) fn helper() -> u32"),
        "the visibility was not inserted:\n{after}"
    );
    assert!(
        stdout(&output).contains("correctness gate passed"),
        "the gate must run:\n{}",
        stdout(&output)
    );
    assert!(compiles(&dir), "the rewritten crate does not compile");
}

#[test]
fn a_public_function_becomes_private() {
    let dir = fixture("public-to-private");
    let output = change(&dir, "pub fn exported() -> u32", "exported", "");
    assert!(
        output.status.success(),
        "run failed:\n{}",
        reported(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("\nfn exported() -> u32"),
        "the visibility was not removed:\n{after}"
    );
    assert!(
        !after.contains("pub fn exported"),
        "the old visibility is still present:\n{after}"
    );
    assert!(compiles(&dir), "the rewritten crate does not compile");
}

#[test]
fn a_private_async_function_gets_visibility_before_async() {
    let dir = fixture("async");
    let output = change(&dir, "async fn work", "work", "pub(crate)");
    assert!(
        output.status.success(),
        "run failed:\n{}",
        reported(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("pub(crate) async fn work() {}"),
        "the modifier must land before `async`:\n{after}"
    );
    assert!(
        !after.contains("async pub(crate) fn work"),
        "the modifier landed in an order Rust rejects:\n{after}"
    );
    assert!(compiles(&dir), "the rewritten crate does not compile");
}

#[test]
fn an_empty_target_removes_an_existing_visibility() {
    let dir = fixture("remove");
    let output = change(&dir, "pub(crate) fn restricted() -> u32", "restricted", "");
    assert!(
        output.status.success(),
        "run failed:\n{}",
        reported(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("\nfn restricted() -> u32"),
        "the visibility was not removed:\n{after}"
    );
    assert!(
        !after.contains("pub(crate) fn restricted"),
        "the old visibility is still present:\n{after}"
    );
    assert!(compiles(&dir), "the rewritten crate does not compile");
}

#[test]
fn check_reports_a_visibility_change_without_writing() {
    let dir = fixture("check");
    let before = read(&dir);
    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.change-visibility",
            "--set",
            "file=src/lib.rs",
            "--set",
            "anchor=fn helper() -> u32",
            "--set",
            "from=helper",
            "--set",
            "to=pub(crate)",
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
        report.contains("-> pub(crate)"),
        "expected the new visibility in the report:\n{report}"
    );
    assert_eq!(read(&dir), before, "check must not write");
}

#[test]
fn a_missing_target_is_an_error() {
    let dir = fixture("missing-target");
    let recipe = dir.join("no-target.yml");
    std::fs::write(
        &recipe,
        r#"id: test.change-visibility-without-target
name: A visibility change without a target
steps:
  - kind: semantic
    action: change-visibility
    file: src/lib.rs
    anchor: fn helper() -> u32
    from: helper
"#,
    )
    .expect("write recipe");

    let recipe_arg = recipe.display().to_string();
    let output = run(
        &[
            "check",
            "--recipe",
            "test.change-visibility-without-target",
            "--recipes",
            &recipe_arg,
        ],
        &dir,
    );
    let report = reported(&output);

    assert!(
        !output.status.success(),
        "a step without `to` must be refused:\n{report}"
    );
    assert!(
        report.contains("without `to`"),
        "the error must name the missing target:\n{report}"
    );
}
