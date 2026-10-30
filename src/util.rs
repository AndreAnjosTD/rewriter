//! Source discovery, diff rendering, and the working-tree safety check.

use crate::language::{self, Language};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Every source file of `language` under `root`, skipping dependency and build
/// directories.
pub fn source_files(root: &Path, language: &Language) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            if language::is_skipped(entry) {
                return false;
            }
            // Hidden directories are caches and tool state, never project source.
            let name = entry.file_name().to_string_lossy();
            !(entry.file_type().is_dir() && name.starts_with('.') && name != ".")
        })
    {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let extension = entry.path().extension().and_then(|value| value.to_str());
        if extension.is_some_and(|extension| language.extensions.contains(&extension)) {
            files.push(entry.into_path());
        }
    }
    files.sort();
    if files.is_empty() {
        let extensions = language
            .extensions
            .iter()
            .map(|extension| format!(".{extension}"))
            .collect::<Vec<_>>()
            .join(" or ");
        anyhow::bail!(
            "no {extensions} files under {} (run from the project root, or pass --path)",
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
