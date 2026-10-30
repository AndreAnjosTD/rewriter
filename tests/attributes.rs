//! The attribute structural recipes: OpenRewrite's `RemoveAnnotation` and
//! `ReplaceAnnotation`.
//!
//! Attributes are syntax, so these need no analyzer. Each test builds its own
//! throwaway crate and drives the real binary, and the removal and replacement
//! tests also let the correctness gate compile the result.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rewriter")
}

/// A minimal crate in a unique temp directory.
fn scratch(suffix: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rewriter-attributes-{}-{}-{}",
        suffix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("create src");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"attribute-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
         \n[lib]\npath = \"src/lib.rs\"\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("src").join("lib.rs"), source).expect("write source");
    dir
}

fn run(args: &[&str], dir: &Path) -> Output {
    let mut command = Command::new(bin());
    command.args(args).arg("--path").arg(dir);
    if matches!(args.first().copied(), Some("run" | "check")) {
        command.arg("--cold");
    }
    command.output().expect("running rewriter")
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

fn read(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("src").join("lib.rs")).expect("reading fixture")
}

fn compiles(dir: &Path) -> bool {
    Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(dir)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Parse every stdout line, failing on the first one that is not a JSON object.
fn lines(output: &Output) -> Vec<Value> {
    stdout(output)
        .lines()
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

#[test]
fn removes_the_named_attribute_from_every_item() {
    let dir = scratch(
        "remove",
        "//! Attribute fixture.\n\n\
         #[deprecated]\n\
         pub fn legacy() {}\n\n\
         #[deprecated(note = \"old\")]\n\
         pub struct Old;\n\n\
         #[allow(dead_code)]\n\
         pub fn allowed() {}\n",
    );
    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.remove-attribute",
            "--set",
            "attribute=deprecated",
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
        after.contains("pub fn legacy() {}"),
        "the bare attribute was not removed:\n{after}"
    );
    assert!(
        after.contains("pub struct Old;"),
        "the attribute with arguments was not removed:\n{after}"
    );
    assert!(
        !after
            .lines()
            .any(|line| line.trim_start().starts_with("#[deprecated")),
        "an attribute line survived:\n{after}"
    );
    assert!(
        after.contains("#[allow(dead_code)]"),
        "an unrelated attribute was removed:\n{after}"
    );
    assert!(compiles(&dir), "the rewritten crate does not compile");
}

#[test]
fn does_not_remove_an_attribute_whose_name_starts_with_the_same_text() {
    let dir = scratch(
        "prefix",
        "#[deprecated_since]\n\
         #[deprecated]\n\
         pub fn legacy() {}\n",
    );
    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.remove-attribute",
            "--set",
            "attribute=deprecated",
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
        "only the exact name may match:\n{report}"
    );
    assert!(
        !report.contains("deprecated_since"),
        "the name-prefixed sibling was reported:\n{report}"
    );
}

#[test]
fn a_comment_that_mentions_the_attribute_is_left_alone() {
    let dir = scratch(
        "comment",
        "// #[deprecated] is explained here.\n\
         #[deprecated]\n\
         pub fn legacy() {}\n",
    );
    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.remove-attribute",
            "--set",
            "attribute=deprecated",
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
        after.contains("// #[deprecated] is explained here."),
        "the comment was rewritten:\n{after}"
    );
    assert!(
        after.contains("pub fn legacy() {}"),
        "the attribute was not removed:\n{after}"
    );
}

#[test]
fn replaces_an_attribute_name_in_place() {
    let dir = scratch(
        "replace",
        "#[inline]\n\
         pub fn fast() -> u32 {\n    1\n}\n\n\
         // #[inline] is mentioned here but is not an attribute.\n\
         pub fn modern() {}\n",
    );
    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.replace-attribute",
            "--set",
            "from=inline",
            "--set",
            "to=cold",
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
        after.contains("#[cold]\npub fn fast()"),
        "the attribute was not renamed in place:\n{after}"
    );
    assert!(
        after.contains("// #[inline] is mentioned here"),
        "the comment was rewritten:\n{after}"
    );
    assert!(compiles(&dir), "the rewritten crate does not compile");
}

#[test]
fn replacement_keeps_the_attribute_arguments() {
    let dir = scratch(
        "arguments",
        "#[allow(unused_variables)]\n\
         pub fn noisy(value: u32) {}\n",
    );
    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.replace-attribute",
            "--set",
            "from=allow",
            "--set",
            "to=warn",
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
        after.contains("#[warn(unused_variables)]"),
        "the name changed but the arguments did not survive:\n{after}"
    );
    assert!(compiles(&dir), "the rewritten crate does not compile");
}

#[test]
fn check_json_reports_attribute_removals() {
    let dir = scratch(
        "json",
        "#[deprecated]\n\
         pub fn legacy() {}\n\n\
         #[deprecated(note = \"old\")]\n\
         pub struct Old;\n",
    );
    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.remove-attribute",
            "--set",
            "attribute=deprecated",
            "--json",
        ],
        &dir,
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "check exits 1 on findings:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let values = lines(&output);
    let findings: Vec<&Value> = values
        .iter()
        .filter(|value| value["type"] == "finding")
        .collect();
    assert_eq!(
        findings.len(),
        2,
        "expected one finding per attribute:\n{}",
        stdout(&output)
    );
    for finding in &findings {
        assert_eq!(finding["file"], "src/lib.rs");
        assert_eq!(finding["recipe"], "rust.migrate.remove-attribute");
        assert!(
            finding["before"]
                .as_str()
                .unwrap_or_default()
                .starts_with("#[deprecated"),
            "the finding must name the removed attribute: {finding}"
        );
        assert_eq!(finding["after"], "", "a removal reports an empty result");
    }

    let summary = values.last().expect("summary line");
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["command"], "check");
    assert_eq!(summary["recipe"], "rust.migrate.remove-attribute");
    assert_eq!(summary["findings"], 2);
    assert_eq!(summary["files"], 1);
    assert_eq!(summary["applied"], false);
}
