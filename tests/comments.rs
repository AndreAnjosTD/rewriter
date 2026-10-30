//! Comment recipes and the node-relative `insert` placements behind them.
//!
//! Each test runs the real binary against its own throwaway crate. The
//! placement tests pass `--no-verify` because the compile gate is not what is
//! under test here, and the formatter must not be able to move a comment before
//! the assertion reads it.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rustrewrite")
}

/// A minimal crate in a unique temp directory.
fn scratch(suffix: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rustrewrite-comments-{}-{}-{}",
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
        "[package]\nname = \"comments-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
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

fn apply(dir: &Path, extra: &[&str]) {
    let mut args = vec![
        "run",
        "--recipe",
        "rust.migrate.add-comment-to-method",
        "--set",
        "pattern=fn $NAME(&self)",
    ];
    args.extend_from_slice(extra);
    args.push("--apply");
    args.push("--no-verify");
    let output = run(&args, dir);
    assert!(
        output.status.success(),
        "apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
}

fn method_fixture() -> &'static str {
    "struct Job;\n\
     impl Job {\n\
     \x20   fn start(&self) {\n\
     \x20       run();\n\
     \x20   }\n\
     \n\
     \x20   fn stop(&self) {\n\
     \x20       run();\n\
     \x20   }\n\
     }\n"
}

#[test]
fn comment_lands_above_a_method_with_the_impl_indentation() {
    let dir = scratch("indent", method_fixture());
    apply(&dir, &["--set", "comment=// TODO: document"]);

    let after = read(&dir);
    assert!(
        after.contains("impl Job {\n    // TODO: document\n    fn start(&self) {"),
        "the comment must keep the block's four-space indent:\n{after}"
    );
}

#[test]
fn a_multi_line_comment_is_indented_on_every_line() {
    let dir = scratch("multiline", method_fixture());
    apply(&dir, &["--set", "comment=// First line\n// Second line"]);

    let after = read(&dir);
    assert!(
        after.contains("impl Job {\n    // First line\n    // Second line\n    fn start(&self) {"),
        "every line of the comment needs the node's indent:\n{after}"
    );
}

#[test]
fn every_matching_method_gets_one_comment() {
    let dir = scratch("two-methods", method_fixture());
    apply(&dir, &["--set", "comment=// TODO: document"]);

    let after = read(&dir);
    assert_eq!(
        after.matches("// TODO: document").count(),
        2,
        "both methods match the pattern:\n{after}"
    );
    assert!(
        after.contains("    // TODO: document\n    fn start(&self) {"),
        "the first method gets a comment:\n{after}"
    );
    assert!(
        after.contains("    // TODO: document\n    fn stop(&self) {"),
        "the second method gets a comment:\n{after}"
    );
}

#[test]
fn a_second_run_is_a_no_op() {
    let dir = scratch("second-run", method_fixture());
    apply(&dir, &["--set", "comment=// TODO: document"]);
    let first = read(&dir);

    apply(&dir, &["--set", "comment=// TODO: document"]);
    assert_eq!(read(&dir), first, "a second run must not add a comment");
    assert_eq!(
        first.matches("// TODO: document").count(),
        2,
        "one comment per method, not two:\n{first}"
    );
}

#[test]
fn a_method_that_already_has_the_comment_is_skipped() {
    let source = "struct Job;\n\
                  impl Job {\n\
                  \x20   // TODO: document\n\
                  \x20   fn start(&self) {\n\
                  \x20       run();\n\
                  \x20   }\n\
                  \n\
                  \x20   fn stop(&self) {\n\
                  \x20       run();\n\
                  \x20   }\n\
                  }\n";
    let dir = scratch("partial", source);

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.add-comment-to-method",
            "--set",
            "pattern=fn $NAME(&self)",
            "--set",
            "comment=// TODO: document",
            "--apply",
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
    assert!(
        stdout(&output).contains("applied 1 change(s)"),
        "only the uncommented method is a change:\n{}",
        stdout(&output)
    );

    let after = read(&dir);
    assert_eq!(
        after.matches("// TODO: document").count(),
        2,
        "the existing comment stays and the other method gets one:\n{after}"
    );
    assert!(
        after.contains("    // TODO: document\n    fn stop(&self) {"),
        "the second method is the one that was skipped before:\n{after}"
    );
}

#[test]
fn json_emits_one_finding_per_insertion() {
    let dir = scratch("json", method_fixture());
    let before = read(&dir);

    let output = run(
        &[
            "check",
            "--recipe",
            "rust.migrate.add-comment-to-method",
            "--set",
            "pattern=fn $NAME(&self)",
            "--set",
            "comment=// TODO: document",
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
        2,
        "one insertion is one finding: {values:?}"
    );
    assert_eq!(findings[0]["file"], "src/lib.rs");
    assert_eq!(findings[0]["line"], 3);
    assert_eq!(findings[1]["line"], 7);
    for finding in &findings {
        assert_eq!(
            finding["before"], "",
            "an insertion replaces nothing, so `before` is empty"
        );
        assert_eq!(
            finding["after"].as_str().unwrap_or_default().trim_start(),
            "// TODO: document",
            "`after` is the text as written, indentation included"
        );
    }

    let summary = values.last().expect("a summary line");
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["findings"], 2);
    assert_eq!(read(&dir), before, "check must not write");
}

#[test]
fn a_group_of_imports_is_annotated_in_one_run() {
    let source = "use std::io;\nuse std::fs;\nuse anyhow::Result;\n\nfn main() {}\n";
    let dir = scratch("imports", source);

    let output = run(
        &[
            "run",
            "--recipe",
            "rust.migrate.add-comment-to-import",
            "--set",
            "pattern=use std::$NAME;",
            "--set",
            "comment=// Legacy std imports.",
            "--apply",
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
        after.starts_with(
            "// Legacy std imports.\nuse std::io;\n// Legacy std imports.\nuse std::fs;\nuse anyhow::Result;"
        ),
        "both std imports are annotated and the third import is untouched:\n{after}"
    );
}

#[test]
fn a_match_placement_without_a_rule_is_refused() {
    let dir = scratch("no-rule", method_fixture());
    let recipes = dir.join(".rustrewrite").join("recipes");
    std::fs::create_dir_all(&recipes).expect("create recipe dir");
    std::fs::write(
        recipes.join("broken.yml"),
        "id: test.broken\n\
         name: Insert without a rule\n\
         steps:\n\
         \x20 - kind: insert\n\
         \x20   where: before-match\n\
         \x20   text: \"// TODO\"\n",
    )
    .expect("write recipe");

    let output = run(&["check", "--recipe", "test.broken"], &dir);
    assert!(
        !output.status.success(),
        "a rule is required to select a node"
    );
    assert!(
        stderr(&output).contains("no `rule` to select a node"),
        "the missing `rule` must be named:\n{}",
        stderr(&output)
    );
}

#[test]
fn after_match_writes_on_the_line_below_the_node() {
    let dir = scratch("after-match", method_fixture());
    let recipes = dir.join(".rustrewrite").join("recipes");
    std::fs::create_dir_all(&recipes).expect("create recipe dir");
    std::fs::write(
        recipes.join("after.yml"),
        "id: test.after\n\
         name: Comment after a node\n\
         steps:\n\
         \x20 - kind: insert\n\
         \x20   where: after-match\n\
         \x20   rule:\n\
         \x20     pattern: \"fn stop(&self) { $$$BODY }\"\n\
         \x20   text: \"// Called after stop.\"\n",
    )
    .expect("write recipe");

    let output = run(
        &["run", "--recipe", "test.after", "--apply", "--no-verify"],
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
        after.contains("    fn stop(&self) {\n        run();\n    }\n    // Called after stop.\n"),
        "the comment belongs on the line below the method's closing brace:\n{after}"
    );

    // The same placement is idempotent too.
    let output = run(
        &["run", "--recipe", "test.after", "--apply", "--no-verify"],
        &dir,
    );
    assert!(
        stdout(&output).contains("no changes to write"),
        "a second run must find nothing:\n{}",
        stdout(&output)
    );
    assert_eq!(
        read(&dir),
        after,
        "the second run must not add a second comment"
    );
}
