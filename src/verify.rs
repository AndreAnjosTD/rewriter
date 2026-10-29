//! The correctness gate: snapshot the tree, run the compiler and tests after a
//! rewrite, and roll everything back if any gate fails.
//!
//! This is what makes the tool safe to run unattended. A rewrite that does not
//! compile is not a rewrite, it is a broken build, and the tool refuses to leave
//! one behind.

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
    files: Vec<(PathBuf, String)>,
}

impl Snapshot {
    pub fn take(root: &Path) -> Result<Self> {
        let mut files = Vec::new();
        for path in util::rust_files(root)? {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("snapshotting {}", path.display()))?;
            files.push((path, text));
        }
        Ok(Self {
            root: root.to_path_buf(),
            files,
        })
    }

    /// Restore every snapshotted file and remove `.rs` files that did not exist
    /// before the run.
    pub fn restore(&self) -> Result<()> {
        for (path, text) in &self.files {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, text).with_context(|| format!("restoring {}", path.display()))?;
        }
        let known: BTreeSet<&PathBuf> = self.files.iter().map(|(p, _)| p).collect();
        for path in util::rust_files(&self.root).unwrap_or_default() {
            if !known.contains(&path) {
                std::fs::remove_file(&path).ok();
            }
        }
        Ok(())
    }
}

pub fn run(root: &Path, with_tests: bool) -> Result<Vec<Gate>> {
    let mut gates = Vec::new();

    // Formatting is applied rather than asserted: ast-grep replacements are
    // syntactically correct but not always layout-correct, and a formatter pass
    // is the cheapest way to leave the tree tidy without changing meaning.
    let fmt = Command::new("cargo")
        .args(["fmt", "--all"])
        .current_dir(root)
        .output();
    match fmt {
        Ok(out) => gates.push(Gate {
            name: "format".into(),
            ok: out.status.success(),
            detail: tail(&combined(&out)),
        }),
        Err(err) => gates.push(Gate {
            name: "format".into(),
            ok: false,
            detail: format!("could not run cargo fmt: {err}"),
        }),
    }

    let mut check = Command::new("cargo");
    check
        .args(["check", "--all-targets", "--quiet"])
        .current_dir(root);
    let out = check.output().context("running cargo check")?;
    gates.push(Gate {
        name: "compile".into(),
        ok: out.status.success(),
        detail: tail(&combined(&out)),
    });

    if with_tests {
        let out = Command::new("cargo")
            .args(["test", "--all-targets", "--quiet"])
            .current_dir(root)
            .output()
            .context("running cargo test")?;
        gates.push(Gate {
            name: "tests".into(),
            ok: out.status.success(),
            detail: tail(&combined(&out)),
        });
    }

    Ok(gates)
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
