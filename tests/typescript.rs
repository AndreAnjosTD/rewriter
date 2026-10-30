//! End-to-end tests for the TypeScript pack: a throwaway `tsconfig.json`
//! project per case, driven through the real binary.
//!
//! The TypeScript gate needs `tsc`, which is not installed on every machine that
//! builds this tool. `check` never runs the gate, so every recipe is proven
//! through `check` and `check --diff`; the one test that applies a rewrite pins
//! the honest failure when no compiler is present and asserts the passing gate
//! when one is, so the suite is not tied to this machine.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rewriter")
}

/// A fresh TypeScript project in its own temp directory.
fn project(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rewriter-ts-{}-{}-{}",
        suffix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("create project");
    std::fs::write(
        dir.join("tsconfig.json"),
        r#"{
  "compilerOptions": {
    "target": "ES2020",
    "module": "ESNext",
    "strict": true
  }
}
"#,
    )
    .expect("write tsconfig");
    dir
}

fn source(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join("src").join(name);
    std::fs::write(&path, text).expect("write source");
    path
}

fn run(args: &[&str], dir: &Path) -> Output {
    let mut command = Command::new(bin());
    command.args(args).arg("--path").arg(dir);
    // Each case owns a throwaway workspace, so a warm analyzer would never be
    // reused and would outlive the test. Nothing in this pack is semantic.
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

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("reading the source")
}

#[test]
fn check_reports_a_fix_and_never_writes() {
    let dir = project("check");
    let file = source(
        &dir,
        "values.ts",
        "const items = [1, 2, 3];\n\nif (items.indexOf(2) >= 0) {\n  items.push(4);\n}\n",
    );
    let before = read(&file);

    let output = run(
        &[
            "check",
            "--recipe",
            "typescript.idiom.array-includes",
            "--diff",
        ],
        &dir,
    );
    let report = stdout(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "check exits 1 on findings:\n{report}\n{}",
        stderr(&output)
    );
    assert!(
        report.contains("typescript.idiom.array-includes"),
        "expected the recipe id in:\n{report}"
    );
    assert!(
        report.contains("items.indexOf(2) >= 0 -> items.includes(2)"),
        "expected the exact replacement in:\n{report}"
    );
    assert!(
        report.contains("-if (items.indexOf(2) >= 0) {")
            && report.contains("+if (items.includes(2)) {"),
        "expected the diff to carry the replacement:\n{report}"
    );
    assert_eq!(read(&file), before, "check must not modify the source");
}

#[test]
fn check_exits_zero_when_nothing_matches() {
    let dir = project("clean");
    source(
        &dir,
        "values.ts",
        "const items = [1, 2, 3];\n\nif (items.includes(2)) {\n  items.push(4);\n}\n",
    );

    let output = run(
        &["check", "--recipe", "typescript.idiom.array-includes"],
        &dir,
    );
    let report = stdout(&output);

    assert!(
        output.status.success(),
        "a tree with no match must pass:\n{report}\n{}",
        stderr(&output)
    );
    assert!(
        report.contains("no findings"),
        "expected the quiet result in:\n{report}"
    );
}

#[test]
fn check_diff_shows_the_exact_rewrite_and_its_exclusions() {
    let dir = project("diff");
    let file = source(
        &dir,
        "declarations.ts",
        "var count = 1;\n\
         var label: string = \"x\";\n\
         var total = 0, extra = 1;\n\
         for (var i = 0; i < 3; i++) {\n\
         \tlog(i);\n\
         }\n",
    );
    let before = read(&file);

    let output = run(
        &[
            "check",
            "--recipe",
            "typescript.migrate.var-to-let",
            "--diff",
        ],
        &dir,
    );
    let report = stdout(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected findings in:\n{report}"
    );
    assert!(
        report.contains("var count = 1; -> let count = 1;"),
        "expected the finding for the plain declaration:\n{report}"
    );
    assert!(
        report.contains("var label: string = \"x\"; -> let label: string = \"x\";"),
        "expected the finding for the annotated declaration:\n{report}"
    );
    assert!(
        report.contains("-var count = 1;") && report.contains("+let count = 1;"),
        "expected the diff to carry the replacement:\n{report}"
    );
    assert!(
        !report.contains("-var total"),
        "a multi-declarator declaration must be left alone:\n{report}"
    );
    assert!(
        !report.contains("let i = 0"),
        "a loop's var must be left alone:\n{report}"
    );
    assert_eq!(
        read(&file),
        before,
        "check --diff must not modify the source"
    );
}

#[test]
fn scan_recipes_report_without_writing() {
    let dir = project("scan");
    let file = source(
        &dir,
        "hazards.ts",
        "const el = document.body as any;\n\
         \n\
         function first(values: number[]): number {\n\
         \tconsole.log(\"picking\", values);\n\
         \treturn values[0]!;\n\
         }\n\
         \n\
         // @ts-ignore\n\
         const missing: number = el.dataset.count;\n\
         // @ts-expect-error\n\
         const other: string = el.dataset.count;\n\
         \n\
         const name = user && user.name;\n",
    );
    let before = read(&file);

    let output = run(&["check", "--recipe", "typescript.scan"], &dir);
    let report = stdout(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected findings in:\n{report}"
    );
    for id in [
        "typescript.scan.console-log",
        "typescript.scan.any",
        "typescript.scan.ts-ignore",
        "typescript.scan.non-null",
        "typescript.scan.and-chain",
    ] {
        assert!(report.contains(id), "expected {id} in:\n{report}");
    }
    assert!(
        report.matches("typescript.scan.ts-ignore").count() == 1,
        "only the @ts-ignore comment is reported, not @ts-expect-error:\n{report}"
    );
    assert_eq!(read(&file), before, "a scan must not modify the source");
}

#[test]
fn apply_refuses_to_claim_a_rewrite_the_compiler_cannot_check() {
    let dir = project("apply");
    let file = source(
        &dir,
        "values.ts",
        "const items = [1, 2, 3];\n\nif (items.indexOf(2) >= 0) {\n  items.push(4);\n}\n",
    );
    let before = read(&file);

    let output = run(
        &[
            "run",
            "--recipe",
            "typescript.idiom.array-includes",
            "--apply",
        ],
        &dir,
    );

    if tsc_is_installed() {
        assert!(
            output.status.success(),
            "with tsc present the gate must pass:\n{}\n{}",
            stdout(&output),
            stderr(&output)
        );
        assert!(
            stdout(&output).contains("correctness gate passed"),
            "expected the gate result in:\n{}",
            stdout(&output)
        );
        let after = std::fs::read_to_string(&file).expect("reading the source");
        assert!(after.contains("items.includes(2)"), "not applied:\n{after}");
    } else {
        assert!(
            !output.status.success(),
            "without tsc an apply must fail rather than keep an unverified rewrite:\n{}",
            stdout(&output)
        );
        let err = stderr(&output);
        assert!(
            err.contains("tsc"),
            "the failure must name the missing compiler:\n{err}"
        );
        assert!(
            err.contains("no compile command found"),
            "the failure must say the gate found no command:\n{err}"
        );
        assert!(
            stdout(&output).contains("rolled back"),
            "the gate must restore the tree:\n{}",
            stdout(&output)
        );
        assert_eq!(
            read(&file),
            before,
            "a failed gate must leave the tree unchanged"
        );
    }
}

/// Whether the gate's first candidate compiler answers, mirroring
/// `verify::resolve` so the branch above tracks the same install.
fn tsc_is_installed() -> bool {
    Command::new("tsc")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}
