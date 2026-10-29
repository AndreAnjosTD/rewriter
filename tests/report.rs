//! The `--json` contract: stdout is newline-delimited JSON and nothing else.
//!
//! Every test parses each stdout line with `serde_json`, so a stray progress
//! line, blank line or human summary fails here rather than in a pipeline.

use serde_json::Value;
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
fn fixture(name: &str, suffix: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    let dst = std::env::temp_dir().join(format!(
        "rustrewrite-json-{}-{}-{}",
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
    // reused and would outlive the test.
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

/// Parse every stdout line, failing on the first one that is not a JSON object.
fn lines(output: &Output) -> Vec<Value> {
    let text = stdout(output);
    text.lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap_or_else(|err| {
                panic!(
                    "stdout line is not JSON: {line:?} ({err})\nstderr:\n{}",
                    stderr(output)
                )
            })
        })
        .collect()
}

fn findings(values: &[Value]) -> Vec<&Value> {
    values
        .iter()
        .filter(|value| value["type"] == "finding")
        .collect()
}

fn summary(values: &[Value]) -> &Value {
    let last = values.last().expect("at least a summary line");
    assert_eq!(last["type"], "summary", "last line: {last}");
    last
}

fn read(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("src").join("main.rs")).expect("reading fixture")
}

fn locate(dir: &Path, text: &str, marker: &str, needle: &str) -> String {
    let line_index = text
        .lines()
        .position(|line| line.contains(marker))
        .unwrap_or_else(|| panic!("marker not found: {marker}"));
    let line = text.lines().nth(line_index).expect("line");
    let column = line.find(needle).expect("needle on line");
    format!(
        "{}/src/main.rs:{}:{}",
        dir.display(),
        line_index + 1,
        line[..column].chars().count() + 1
    )
}

fn rust_analyzer_available() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[test]
fn check_json_ends_with_a_summary_and_exits_one() {
    let dir = fixture("demo-crate", "check");
    let before = read(&dir);

    let output = run(&["check", "--recipe", "rust.idioms", "--json"], &dir);
    assert_eq!(
        output.status.code(),
        Some(1),
        "check exits 1 on findings:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let values = lines(&output);
    let found = findings(&values);
    assert!(!found.is_empty(), "expected findings:\n{}", stdout(&output));
    for finding in &found {
        assert_eq!(finding["file"], "src/main.rs");
        assert!(finding["line"].is_number());
        assert!(finding["column"].is_number());
        assert!(finding["recipe"].is_string());
        assert!(finding["before"].is_string());
    }

    let summary = summary(&values);
    assert_eq!(summary["command"], "check");
    assert_eq!(summary["recipe"], "rust.idioms");
    assert_eq!(summary["findings"], found.len());
    assert_eq!(summary["files"], 1);
    assert_eq!(summary["changed"], true);
    assert_eq!(summary["applied"], false);
    assert!(
        summary.get("gates").is_none(),
        "check runs no gate, so `gates` must be absent: {summary}"
    );

    assert_eq!(read(&dir), before, "check must not write");
}

#[test]
fn run_json_dry_run_reports_without_gates_or_writes() {
    let dir = fixture("demo-crate", "run-dry");
    let before = read(&dir);

    let output = run(&["run", "--recipe", "rust.idioms", "--json"], &dir);
    assert!(
        output.status.success(),
        "dry run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let values = lines(&output);
    let found = findings(&values);
    assert!(!found.is_empty(), "expected findings:\n{}", stdout(&output));

    let summary = summary(&values);
    assert_eq!(summary["command"], "run");
    assert_eq!(summary["recipe"], "rust.idioms");
    assert_eq!(summary["findings"], found.len());
    assert_eq!(summary["changed"], true);
    assert_eq!(summary["applied"], false);
    assert!(
        summary.get("gates").is_none(),
        "a dry run never gates: {summary}"
    );

    assert_eq!(read(&dir), before, "a dry run must not write");
}

#[test]
fn run_json_apply_summarizes_the_gates() {
    let dir = fixture("demo-crate", "run-apply");

    let output = run(
        &["run", "--recipe", "rust.idioms", "--apply", "--json"],
        &dir,
    );
    assert!(
        output.status.success(),
        "apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let values = lines(&output);
    let found = findings(&values);
    assert!(!found.is_empty(), "expected findings:\n{}", stdout(&output));

    let summary = summary(&values);
    assert_eq!(summary["command"], "run");
    assert_eq!(summary["findings"], found.len());
    assert_eq!(summary["applied"], true);
    assert_eq!(summary["changed"], true);

    let gates = summary["gates"]
        .as_array()
        .unwrap_or_else(|| panic!("expected the gate results in {summary}"));
    let names: Vec<&str> = gates
        .iter()
        .map(|gate| gate["name"].as_str().expect("gate name"))
        .collect();
    assert_eq!(names, ["format", "compile"]);
    assert!(
        gates.iter().all(|gate| gate["ok"] == true),
        "the gate must pass: {summary}"
    );

    let after = read(&dir);
    assert!(
        after.contains("items.is_empty()"),
        "the rewrite was not written:\n{after}"
    );
}

#[test]
fn rename_json_reports_null_recipe_and_gates() {
    if !rust_analyzer_available() {
        eprintln!("SKIPPED: rust-analyzer is not installed (rustup component add rust-analyzer)");
        return;
    }
    let dir = fixture("semantic-crate", "rename");
    let before = read(&dir);
    let at = locate(&dir, &before, "fn len(&self) -> usize", "len");

    let output = run(
        &["rename", "--at", &at, "--to", "size", "--apply", "--json"],
        &dir,
    );
    assert!(
        output.status.success(),
        "rename failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let values = lines(&output);
    let found = findings(&values);
    assert!(!found.is_empty(), "expected edits:\n{}", stdout(&output));
    for finding in &found {
        assert!(
            finding["recipe"].is_null(),
            "rename has no recipe: {finding}"
        );
        assert_eq!(finding["file"], "src/main.rs");
    }

    let summary = summary(&values);
    assert_eq!(summary["command"], "rename");
    assert!(summary["recipe"].is_null());
    assert_eq!(summary["findings"], found.len());
    assert_eq!(summary["files"], 1);
    assert_eq!(summary["applied"], true);
    assert_eq!(summary["changed"], true);
    let gates = summary["gates"]
        .as_array()
        .unwrap_or_else(|| panic!("expected the gate results in {summary}"));
    assert!(
        gates.iter().all(|gate| gate["ok"] == true),
        "the gate must pass: {summary}"
    );

    let after = read(&dir);
    assert!(after.contains("fn size(&self) -> usize"), "{after}");
    assert!(after.contains("bag.size()"), "{after}");
    assert!(
        after.contains("numbers.len()"),
        "Vec::len must survive the rename:\n{after}"
    );
}

#[test]
fn json_failure_ends_with_an_error_object() {
    let dir = fixture("demo-crate", "error");

    let output = run(&["check", "--recipe", "rust.nope", "--json"], &dir);
    assert!(!output.status.success(), "an unknown recipe must fail");

    let values = lines(&output);
    let last = values.last().expect("at least the error line");
    assert_eq!(last["type"], "error");
    assert!(
        last["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown recipe"),
        "the error must carry the reason: {last}"
    );
}
