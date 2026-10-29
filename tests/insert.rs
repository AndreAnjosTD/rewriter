//! `insert` steps: text written at a file position for changes that have no
//! node to replace, such as a license header or an import.
//!
//! Every test builds its own throwaway crate under the temp directory, because
//! `--apply` runs the real format and compile gates.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rustrewrite")
}

/// A minimal crate in a unique temp directory.
fn scratch(suffix: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rustrewrite-insert-{}-{}-{}",
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
        "[package]\nname = \"insert-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
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
    command.output().expect("running rustrewrite")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("src").join("lib.rs")).expect("reading fixture")
}

#[test]
fn license_header_is_written_once() {
    let dir = scratch(
        "header",
        "//! Tiny crate.\n\npub fn one() -> u8 {\n    1\n}\n",
    );
    // A vendored file must not be stamped: the recipe excludes it.
    std::fs::create_dir_all(dir.join("src").join("vendor")).expect("create vendor");
    std::fs::write(
        dir.join("src").join("vendor").join("dep.rs"),
        "pub fn dep() {}\n",
    )
    .expect("write vendored file");

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.add-license-header",
            "--apply",
        ],
        &dir,
    );
    assert!(
        output.status.success(),
        "apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        after.starts_with("// SPDX-License-Identifier: MIT\n\n//! Tiny crate."),
        "header must sit above the module docs:\n{after}"
    );
    assert_eq!(
        after.matches("SPDX-License-Identifier").count(),
        1,
        "one header only:\n{after}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("src").join("vendor").join("dep.rs")).expect("vendored"),
        "pub fn dep() {}\n",
        "the excluded directory must be untouched"
    );

    // Running the same recipe again finds nothing and writes nothing.
    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.add-license-header",
            "--apply",
        ],
        &dir,
    );
    assert!(
        output.status.success(),
        "second apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("no changes to write"),
        "expected a second run to be a no-op:\n{}",
        stdout(&output)
    );
    assert_eq!(
        read(&dir),
        after,
        "the second run must not rewrite the file"
    );
}

#[test]
fn import_is_added_after_the_last_use() {
    let dir = scratch(
        "import-after",
        "use std::collections::HashMap;\n\npub fn make() -> HashMap<&'static str, u8> {\n    \
         HashMap::new()\n}\n",
    );

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.add-import",
            "--set",
            "path=std::collections::BTreeMap",
            "--apply",
            // Verified separately: the gate's formatter reorders imports, so a
            // gated run would not prove where the step put this one.
            "--no-verify",
        ],
        &dir,
    );
    assert!(
        output.status.success(),
        "apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        after.contains("use std::collections::HashMap;\nuse std::collections::BTreeMap;"),
        "the new import must follow the existing one:\n{after}"
    );
}

#[test]
fn import_already_in_a_braced_group_is_left_alone() {
    let dir = scratch(
        "import-braced",
        "use std::collections::{BTreeMap, HashMap};\n\npub fn make() -> BTreeMap<u8, u8> {\n    \
         BTreeMap::new()\n}\n",
    );
    let before = read(&dir);

    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.add-import",
            "--set",
            "path=std::collections::BTreeMap",
        ],
        &dir,
    );
    assert!(
        output.status.success(),
        "check must report no findings:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("no findings"),
        "expected no findings:\n{}",
        stdout(&output)
    );
    assert_eq!(read(&dir), before, "check must not write");
}

#[test]
fn import_lands_before_the_first_item_when_there_is_no_use() {
    let dir = scratch(
        "import-no-use",
        "//! Module docs come first.\n\npub fn one() -> u8 {\n    1\n}\n",
    );

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.add-import",
            "--set",
            "path=std::collections::BTreeMap",
            "--apply",
        ],
        &dir,
    );
    assert!(
        output.status.success(),
        "apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );

    let after = read(&dir);
    assert!(
        after.starts_with(
            "//! Module docs come first.\n\nuse std::collections::BTreeMap;\n\npub fn one()"
        ),
        "the import must go above the first item, below the docs:\n{after}"
    );
}

#[test]
fn check_reports_a_header_without_writing() {
    let dir = scratch("check-header", "pub fn one() -> u8 {\n    1\n}\n");
    let before = read(&dir);

    let output = run(
        &["check", "--recipe", "rust.migrate.add-license-header"],
        &dir,
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "check exits 1 when it reports findings"
    );
    let report = stdout(&output);
    assert!(
        report.contains("rust.migrate.add-license-header"),
        "expected the recipe in:\n{report}"
    );
    assert!(
        report.contains("SPDX-License-Identifier"),
        "expected the inserted text in:\n{report}"
    );
    assert_eq!(read(&dir), before, "check must not modify the source");
}

#[test]
fn json_output_stays_ndjson() {
    let dir = scratch("json", "pub fn one() -> u8 {\n    1\n}\n");
    let before = read(&dir);

    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.add-license-header",
            "--json",
        ],
        &dir,
    );
    assert_eq!(output.status.code(), Some(1), "check exits 1 on findings");

    let values: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap_or_else(|err| {
                panic!(
                    "stdout line is not JSON: {line:?} ({err})\nstderr:\n{}",
                    stderr(&output)
                )
            })
        })
        .collect();

    let findings: Vec<&Value> = values
        .iter()
        .filter(|value| value["type"] == "finding")
        .collect();
    assert_eq!(
        findings.len(),
        1,
        "one insertion is one finding: {values:?}"
    );
    assert_eq!(findings[0]["file"], "src/lib.rs");
    assert_eq!(findings[0]["recipe"], "rust.migrate.add-license-header");
    assert_eq!(
        findings[0]["before"], "",
        "an insertion replaces nothing, so `before` is empty"
    );
    assert!(
        findings[0]["after"]
            .as_str()
            .unwrap_or_default()
            .contains("SPDX-License-Identifier"),
        "the inserted text belongs in `after`: {findings:?}"
    );

    let summary = values.last().expect("a summary line");
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["command"], "check");
    assert_eq!(summary["findings"], 1);
    assert_eq!(summary["changed"], true);

    assert_eq!(read(&dir), before, "check must not write");
}

/// Each broken recipe gets its own crate: the catalog is loaded whole, so an
/// invalid neighbour would fail the run before the recipe under test is selected.
fn broken_recipe(suffix: &str, yaml: &str) -> String {
    let dir = scratch(suffix, "pub fn one() -> u8 {\n    1\n}\n");
    let recipes = dir.join(".rustrewrite").join("recipes");
    std::fs::create_dir_all(&recipes).expect("create recipe dir");
    std::fs::write(recipes.join("broken.yml"), yaml).expect("write recipe");
    let output = run(&["check", "--recipe", "test.broken"], &dir);
    assert!(!output.status.success(), "a broken step must fail to load");
    stderr(&output)
}

#[test]
fn insert_step_names_the_missing_field() {
    let text = broken_recipe(
        "validation-text",
        "id: test.broken\nname: Insert nothing\nsteps:\n  - kind: insert\n    where: top\n",
    );
    assert!(
        text.contains("insert step with no `text`"),
        "expected the missing `text` to be named:\n{text}"
    );

    let text = broken_recipe(
        "validation-where",
        "id: test.broken\nname: Insert nowhere\nsteps:\n  - kind: insert\n    text: \"// hi\"\n",
    );
    assert!(
        text.contains("insert step with no `where`"),
        "expected the missing `where` to be named:\n{text}"
    );
}
