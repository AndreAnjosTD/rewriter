//! The correctness gate: snapshot the tree, run the compiler and tests after a
//! rewrite, and roll everything back if any gate fails.
//!
//! This is what makes the tool safe to run unattended. A rewrite that does not
//! compile is not a rewrite, it is a broken build, and the tool refuses to leave
//! one behind.
//!
//! Which commands prove that is a property of the language, not of this file:
//! `Cargo.toml` means `cargo check`, `tsconfig.json` means `tsc --noEmit`, and
//! Python has no compiler at all so its gate parses every file instead. A
//! required command whose program is missing fails the gate rather than passing
//! it silently.

use crate::language::{CommandSpec, Language};
use crate::util;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Gate {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub struct Snapshot {
    root: PathBuf,
    language: &'static Language,
    files: Vec<(PathBuf, String)>,
}

impl Snapshot {
    pub fn take(root: &Path, language: &'static Language) -> Result<Self> {
        let mut files = Vec::new();
        for path in util::source_files(root, language)? {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("snapshotting {}", path.display()))?;
            files.push((path, text));
        }
        Ok(Self {
            root: root.to_path_buf(),
            language,
            files,
        })
    }

    /// Restore every snapshotted file and remove source files that did not exist
    /// before the run.
    pub fn restore(&self) -> Result<()> {
        for (path, text) in &self.files {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, text).with_context(|| format!("restoring {}", path.display()))?;
        }
        let known: BTreeSet<&PathBuf> = self.files.iter().map(|(p, _)| p).collect();
        for path in util::source_files(&self.root, self.language).unwrap_or_default() {
            if !known.contains(&path) {
                std::fs::remove_file(&path).ok();
            }
        }
        Ok(())
    }
}

pub fn run(root: &Path, language: &'static Language, with_tests: bool) -> Result<Vec<Gate>> {
    let mut gates = Vec::new();

    if let Some(format) = &language.gate.format {
        gates.push(optional(root, "format", format));
    }
    gates.push(required(root, "compile", &language.gate.check)?);
    if with_tests {
        if let Some(test) = &language.gate.test {
            gates.push(optional(root, "tests", test));
        }
    }

    Ok(gates)
}

/// A command whose absence is not a failure: a formatter or a test runner that
/// the machine does not have must not block a rewrite that otherwise compiles.
fn optional(root: &Path, name: &str, spec: &CommandSpec) -> Gate {
    match resolve(spec) {
        Some((program, prefix)) => run_spec(root, name, spec, &program, prefix),
        None => Gate {
            name: name.into(),
            ok: true,
            detail: format!(
                "skipped: none of {} is installed",
                spec.programs
                    .iter()
                    .map(|candidate| candidate.join(" "))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

/// A command the gate cannot do without.
fn required(root: &Path, name: &str, spec: &CommandSpec) -> Result<Gate> {
    let Some((program, prefix)) = resolve(spec) else {
        let tried = spec
            .programs
            .iter()
            .map(|candidate| candidate.join(" "))
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(Gate {
            name: name.into(),
            ok: false,
            detail: format!(
                "no {name} command found; tried {tried}. Install one, or the rewrite cannot be \
                 verified and nothing will be written."
            ),
        });
    };
    Ok(run_spec(root, name, spec, &program, prefix))
}

fn run_spec(
    root: &Path,
    name: &str,
    spec: &CommandSpec,
    program: &str,
    prefix: &'static [&'static str],
) -> Gate {
    // The prefix is the words before the program name, e.g. `py -3` is program
    // `py` with the prefix `-3`; the version probe ran with both.
    let mut command = Command::new(program);
    command.args(prefix).args(spec.args).current_dir(root);
    match command.output() {
        Ok(out) => Gate {
            name: name.into(),
            ok: out.status.success(),
            detail: tail(&combined(&out)),
        },
        Err(err) => Gate {
            name: name.into(),
            ok: false,
            detail: format!("could not run {program}: {err}"),
        },
    }
}

/// Find the first candidate program that is installed. Returns the program and
/// the words that must precede the arguments (for `py -3`, the `-3`).
fn resolve(spec: &CommandSpec) -> Option<(String, &'static [&'static str])> {
    for candidate in spec.programs {
        let (program, prefix) = candidate.split_first()?;
        let probe = Command::new(program).args(prefix).arg("--version").output();
        if probe.is_ok_and(|out| out.status.success()) {
            return Some(((*program).to_string(), prefix));
        }
    }
    None
}

fn combined(out: &std::process::Output) -> String {
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    text
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(40);
    lines[start..].join("\n")
}
