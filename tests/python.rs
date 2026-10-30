//! Python end-to-end tests: every case builds a throwaway project with a
//! `pyproject.toml` marker and drives the real binary against it.
//!
//! An apply runs the Python gate, which needs an interpreter. When none is
//! installed the apply tests skip with a loud message, as the analyzer tests do,
//! instead of failing for a reason that has nothing to do with the rewrite.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rustrewrite")
}

/// Mirrors the program order the Python gate tries.
fn python_available() -> bool {
    [&["py", "-3"][..], &["python3"][..], &["python"][..]]
        .iter()
        .any(|program| {
            Command::new(program[0])
                .args(&program[1..])
                .arg("--version")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        })
}

fn project(suffix: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rustrewrite-py-{}-{}-{}",
        suffix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create project");
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .expect("write project marker");
    for (name, text) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create source directory");
        }
        std::fs::write(path, text).expect("write source file");
    }
    dir
}

fn run(args: &[&str], dir: &Path) -> Output {
    let mut command = Command::new(bin());
    command.args(args).arg("--path").arg(dir);
    // Each case has its own throwaway project, so a warm analyzer would never
    // be reused and would outlive the test.
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

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).expect("reading source file")
}

#[test]
fn check_reports_a_real_occurrence_and_ignores_a_non_empty_comparison() {
    let dir = project(
        "check",
        &[(
            "app.py",
            "def report(items):\n    if len(items) > 0:\n        return items\n    if len(items) > 1:\n        return items[:1]\n    return None\n",
        )],
    );
    let before = read(&dir, "app.py");

    let output = run(&["check", "--recipe", "python.idiom.len-truthy"], &dir);
    let report = stdout(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "check exits 1 on findings:\n{report}\n{}",
        stderr(&output)
    );
    assert!(
        report.contains("python.idiom.len-truthy"),
        "expected the recipe in the report:\n{report}"
    );
    assert!(
        report.contains("len(items) > 0"),
        "expected the real occurrence in the report:\n{report}"
    );
    assert!(
        !report.contains("len(items) > 1"),
        "a comparison to one is not a length test:\n{report}"
    );
    assert_eq!(read(&dir, "app.py"), before, "check must not write");
}

#[test]
fn apply_rewrites_truthiness_and_passes_the_python_gate() {
    if !python_available() {
        eprintln!("SKIPPED: Python is not installed (py -3, python3 or python)");
        return;
    }
    let dir = project(
        "apply-len",
        &[(
            "app.py",
            "def describe(items):\n    if len(items) > 0:\n        return \"full\"\n    if len(items) == 0:\n        return \"empty\"\n    return None\n",
        )],
    );

    let output = run(
        &["run", "--recipe", "python.idiom.len-truthy", "--apply"],
        &dir,
    );
    assert!(
        output.status.success(),
        "apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("correctness gate passed"),
        "the syntax gate must run:\n{}",
        stdout(&output)
    );

    let after = read(&dir, "app.py");
    assert!(
        after.contains("if items:"),
        "truthiness not applied:\n{after}"
    );
    assert!(
        after.contains("if not items:"),
        "negation not applied:\n{after}"
    );
    assert!(
        !after.contains("len(items)"),
        "a length comparison survived:\n{after}"
    );
}

#[test]
fn dict_keys_is_rewritten_in_a_loop_and_left_as_a_value() {
    if !python_available() {
        eprintln!("SKIPPED: Python is not installed (py -3, python3 or python)");
        return;
    }
    let dir = project(
        "dict-keys",
        &[(
            "app.py",
            "def names(config):\n    for key in config.keys():\n        print(key)\n    return config.keys()\n",
        )],
    );

    let output = run(
        &["run", "--recipe", "python.idiom.dict-keys", "--apply"],
        &dir,
    );
    assert!(
        output.status.success(),
        "apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(stdout(&output).contains("correctness gate passed"));

    let after = read(&dir, "app.py");
    assert!(
        after.contains("for key in config:"),
        "the loop iterable was not rewritten:\n{after}"
    );
    assert!(
        after.contains("return config.keys()"),
        "a keys view used as a value must keep its type:\n{after}"
    );
}

#[test]
fn open_encoding_rewrites_only_the_single_positional_call() {
    if !python_available() {
        eprintln!("SKIPPED: Python is not installed (py -3, python3 or python)");
        return;
    }
    let dir = project(
        "open-encoding",
        &[(
            "app.py",
            "first = open(path)\nsecond = open(path, \"r\")\nthird = open(file=path)\nfourth = open(*args)\n",
        )],
    );

    let output = run(
        &["run", "--recipe", "python.migrate.open-encoding", "--apply"],
        &dir,
    );
    assert!(
        output.status.success(),
        "apply failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(stdout(&output).contains("correctness gate passed"));

    let after = read(&dir, "app.py");
    assert!(
        after.contains("first = open(path, encoding=\"utf-8\")"),
        "the single-argument call was not pinned:\n{after}"
    );
    assert!(
        after.contains("second = open(path, \"r\")"),
        "a call with a mode must keep its arguments:\n{after}"
    );
    assert!(
        after.contains("third = open(file=path)"),
        "a keyword argument is not a positional path:\n{after}"
    );
    assert!(
        after.contains("fourth = open(*args)"),
        "a splat already carries the argument list:\n{after}"
    );
}

#[test]
fn scan_reports_bare_except_without_writing() {
    let dir = project(
        "scan-bare-except",
        &[(
            "app.py",
            "def guarded():\n    try:\n        run()\n    except:\n        pass\n    try:\n        run()\n    except ValueError:\n        pass\n",
        )],
    );
    let before = read(&dir, "app.py");

    let output = run(&["check", "--recipe", "python.scan.bare-except"], &dir);
    let report = stdout(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "check exits 1 on findings:\n{report}\n{}",
        stderr(&output)
    );
    assert!(
        report.contains("python.scan.bare-except"),
        "expected the scan recipe in the report:\n{report}"
    );
    assert!(
        report.contains("bare except"),
        "expected the scan message:\n{report}"
    );
    assert!(
        report.contains("1 finding(s)"),
        "a typed except clause must not be reported:\n{report}"
    );
    assert_eq!(
        read(&dir, "app.py"),
        before,
        "a scan recipe must not modify the source"
    );
}

#[test]
fn scan_reports_shell_true_in_any_argument_position() {
    let dir = project(
        "scan-shell",
        &[(
            "app.py",
            "import subprocess\n\nsubprocess.run(command)\nsubprocess.run(command, shell=True)\nsubprocess.run(shell=True, args=command)\nsubprocess.run(command, shell=False)\n",
        )],
    );
    let before = read(&dir, "app.py");

    let output = run(&["check", "--recipe", "python.scan.shell-true"], &dir);
    let report = stdout(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "check exits 1 on findings:\n{report}\n{}",
        stderr(&output)
    );
    assert!(
        report.contains("2 finding(s)"),
        "shell=True must be found in either position, and shell=False ignored:\n{report}"
    );
    assert!(
        report.contains("shell=True passes the command to the system shell"),
        "expected the scan message:\n{report}"
    );
    assert_eq!(
        read(&dir, "app.py"),
        before,
        "a scan recipe must not modify the source"
    );
}
