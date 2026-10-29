//! Newline-delimited JSON for the commands that report findings.
//!
//! The contract is one object per line on stdout and nothing else, so the
//! stream pipes straight into `jq`. `type` is the discriminator: readers can
//! skip lines they do not understand, and adding a type later stays backwards
//! compatible.
//!
//! The schema is documented in the README, which is the only place that
//! promises field names to users; keep the two in step.

use crate::engine::Finding;
use crate::verify::Gate;
use serde_json::json;
use std::path::Path;

/// A path relative to the run root, with forward slashes, so the output has
/// the same shape on every platform.
pub fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// One change or hazard. `recipe` is `None` for `rename`, which is positional
/// rather than driven by a recipe.
pub struct FindingLine<'a> {
    pub root: &'a Path,
    pub finding: &'a Finding,
    pub recipe: Option<&'a str>,
}

impl FindingLine<'_> {
    pub fn line(&self) -> String {
        json!({
            "type": "finding",
            "file": relative(&self.finding.path, self.root),
            "line": self.finding.line,
            "column": self.finding.column,
            "recipe": self.recipe,
            "before": self.finding.before,
            "after": self.finding.after,
        })
        .to_string()
    }
}

/// The terminal line of a successful command. `gates` stays out of the object
/// entirely when no gate ran, so consumers can tell "not run" from "passed".
pub struct Summary<'a> {
    pub command: &'a str,
    pub recipe: Option<&'a str>,
    pub findings: usize,
    pub files: usize,
    pub changed: bool,
    pub applied: bool,
    pub gates: Option<&'a [Gate]>,
}

impl Summary<'_> {
    pub fn line(&self) -> String {
        let mut value = json!({
            "type": "summary",
            "command": self.command,
            "recipe": self.recipe,
            "findings": self.findings,
            "files": self.files,
            "changed": self.changed,
            "applied": self.applied,
        });
        if let Some(gates) = self.gates {
            let gates: Vec<_> = gates
                .iter()
                .map(|gate| json!({ "name": gate.name, "ok": gate.ok }))
                .collect();
            value["gates"] = json!(gates);
        }
        value.to_string()
    }
}

/// The last stdout line of a failed command; the exit status is non-zero too.
pub fn error(message: &str) -> String {
    json!({ "type": "error", "message": message }).to_string()
}
