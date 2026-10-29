//! Source discovery, diff rendering, and the working-tree safety check.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn rust_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        !(e.file_type().is_dir()
            && (name == "target"
                || name == "node_modules"
                || name == ".git"
                || (name.starts_with('.') && name != ".")))
    }) {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|e| e.to_str()) == Some("rs")
        {
            files.push(entry.into_path());
        }
    }
    files.sort();
    if files.is_empty() {
        anyhow::bail!(
            "no .rs files under {} (run from a crate root, or pass --path)",
            root.display()
        );
    }
    Ok(files)
}

/// `Some(true)` clean, `Some(false)` dirty, `None` not a git repo or no git.
pub fn git_clean(root: &Path) -> Option<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

pub fn unified_diff(old: &str, new: &str, name: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.unified_diff().header(name, name).to_string()
}

/// 1-based line and character column of a byte offset.
pub fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map(|tail| tail.chars().count())
        .unwrap_or(0)
        + 1;
    (line, column)
}

/// Canonicalize a path and drop the Windows verbatim prefix, so that a
/// canonicalized file path and a user-supplied root compare equal and print
/// readably. On other platforms this is just `canonicalize`.
pub fn normalize_path(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    #[cfg(windows)]
    {
        let text = canonical.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(canonical)
}
