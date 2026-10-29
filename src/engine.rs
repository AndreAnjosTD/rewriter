//! Rewrite engine: turns recipe steps into ast-grep rules, applies them to
//! source text, and runs cargo-powered fixers.

use crate::recipes::{Mode, Resolved, StepKind};
use anyhow::{bail, Context, Result};
use ast_grep_config::{from_yaml_string, GlobalRules, RuleConfig};
use ast_grep_core::replacer::Replacer;
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::NodeMatch;
use ast_grep_language::{LanguageExt, SupportLang};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One concrete change, or one reported hazard for scan recipes.
#[derive(Debug, Clone)]
pub struct Finding {
    pub recipe: String,
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub before: String,
    /// `Some` for a rewrite (empty means the statement was deleted), `None` for
    /// a scan finding that reports without changing anything.
    pub after: Option<String>,
    pub message: String,
}

impl Finding {
    pub fn one_line(&self, root: &Path) -> String {
        let shown = self
            .path
            .strip_prefix(root)
            .unwrap_or(&self.path)
            .display()
            .to_string()
            .replace('\\', "/");
        let head = format!("{shown}:{}:{}  {}", self.line, self.column, self.recipe);
        match &self.after {
            Some(after) if after.is_empty() => {
                format!("{head}  {} -> (deleted)", normalize(&self.before))
            }
            Some(after) => format!(
                "{head}  {} -> {}",
                normalize(&self.before),
                normalize(after)
            ),
            None => format!("{head}  {}  ({})", normalize(&self.before), self.message),
        }
    }
}

fn normalize(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 90 {
        let short: String = flat.chars().take(87).collect();
        format!("{short}...")
    } else {
        flat
    }
}

struct Compiled {
    recipe: String,
    message: String,
    cfg: RuleConfig<SupportLang>,
    delete: bool,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl Compiled {
    fn allows(&self, relative: &str) -> bool {
        if let Some(exclude) = &self.exclude {
            if exclude.is_match(relative) {
                return false;
            }
        }
        match &self.include {
            Some(include) => include.is_match(relative),
            None => true,
        }
    }
}

pub struct Engine {
    compiled: Vec<Compiled>,
    cargo_steps: Vec<(String, Vec<String>)>,
}

impl Engine {
    pub fn compile(resolved: &[Resolved]) -> Result<Self> {
        let mut compiled = Vec::new();
        let mut cargo_steps = Vec::new();
        for r in resolved {
            for (index, resolved_step) in r.steps.iter().enumerate() {
                let step = &resolved_step.step;
                match step.kind {
                    StepKind::Structural => {
                        let rule = step.rule.as_ref().expect("validated at load time");
                        let fixing = resolved_step.mode == Mode::Fix;
                        // A scan recipe never carries a rewrite, even if a step
                        // defines one, so `scan` cannot silently write.
                        let fix = if fixing { step.fix.as_deref() } else { None };
                        let message = step
                            .message
                            .clone()
                            .unwrap_or_else(|| resolved_step.name.clone());
                        let cfg = build_rule(&resolved_step.recipe, &message, rule, fix)
                            .with_context(|| {
                                format!(
                                    "compiling step {} of `{}`",
                                    index + 1,
                                    resolved_step.recipe
                                )
                            })?;
                        compiled.push(Compiled {
                            recipe: resolved_step.recipe.clone(),
                            message,
                            cfg,
                            delete: step.delete && fixing,
                            include: build_globset(&step.include)?,
                            exclude: build_globset(&step.exclude)?,
                        });
                    }
                    StepKind::Cargo => {
                        let command = step.command.clone().expect("validated at load time");
                        cargo_steps.push((resolved_step.recipe.clone(), command));
                    }
                }
            }
        }
        Ok(Self {
            compiled,
            cargo_steps,
        })
    }

    /// Rewrite `text` to a fixpoint.
    ///
    /// Findings are collected from the original text so reported positions refer
    /// to the file the reader is looking at; matches created by a rewrite are
    /// still applied but not re-reported.
    pub fn rewrite(&self, text: &str, path: &Path, root: &Path) -> Result<(String, Vec<Finding>)> {
        let relative = relative_path(path, root);
        let mut findings = Vec::new();
        for compiled in self.compiled.iter().filter(|c| c.allows(&relative)) {
            let doc = SupportLang::Rust.ast_grep(text);
            let mut last_end = 0usize;
            for m in doc.root().find_all(&compiled.cfg.matcher) {
                let range = replace_range(compiled, &m, text);
                if range.start < last_end || range.end > text.len() {
                    continue;
                }
                last_end = range.end;
                findings.push(Finding {
                    recipe: compiled.recipe.clone(),
                    path: path.to_path_buf(),
                    line: m.start_pos().line() + 1,
                    column: m.start_pos().column(m.get_node()) + 1,
                    before: m.text().to_string(),
                    after: if compiled.delete {
                        Some(String::new())
                    } else if compiled.cfg.fixer.is_empty() {
                        None
                    } else {
                        Some(
                            String::from_utf8_lossy(
                                &compiled.cfg.fixer[0].generate_replacement(&m),
                            )
                            .into_owned(),
                        )
                    },
                    message: compiled.message.clone(),
                });
            }
        }

        let mut current = text.to_string();
        for _ in 0..8 {
            let doc = SupportLang::Rust.ast_grep(&current);
            let mut edits: Vec<(Range<usize>, String)> = Vec::new();
            let mut last_end = 0usize;
            for compiled in self.compiled.iter().filter(|c| c.allows(&relative)) {
                if compiled.cfg.fixer.is_empty() && !compiled.delete {
                    continue;
                }
                for m in doc.root().find_all(&compiled.cfg.matcher) {
                    let range = replace_range(compiled, &m, &current);
                    if range.start < last_end || range.end > current.len() {
                        continue;
                    }
                    last_end = range.end;
                    let replacement = if compiled.delete {
                        String::new()
                    } else {
                        String::from_utf8_lossy(&compiled.cfg.fixer[0].generate_replacement(&m))
                            .into_owned()
                    };
                    edits.push((range, replacement));
                }
            }
            if edits.is_empty() {
                break;
            }
            edits.sort_by_key(|(range, _)| range.start);
            let mut next = current.clone();
            for (range, replacement) in edits.iter().rev() {
                next.replace_range(range.clone(), replacement);
            }
            if next == current {
                break;
            }
            current = next;
        }

        Ok((current, findings))
    }

    /// Whether this engine carries cargo-backed steps.
    pub fn has_cargo_steps(&self) -> bool {
        !self.cargo_steps.is_empty()
    }

    /// Run cargo steps in order. They see the tree on disk, so they run only
    /// after structural rewrites have been written.
    pub fn run_cargo_steps(&self, root: &Path) -> Result<Vec<String>> {
        let mut log = Vec::new();
        for (recipe, command) in &self.cargo_steps {
            let (program, args) = command.split_first().context("empty cargo command")?;
            let output = Command::new(program)
                .args(args)
                .current_dir(root)
                .output()
                .with_context(|| format!("spawning `{program}` for `{recipe}`"))?;
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            log.push(format!("[{recipe}] {program} {}", args.join(" ")));
            if !output.status.success() {
                let lines: Vec<&str> = text.lines().collect();
                let start = lines.len().saturating_sub(40);
                bail!(
                    "cargo step `{recipe}` failed:\n{}",
                    lines[start..].join("\n")
                );
            }
        }
        Ok(log)
    }
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid glob `{pattern}`"))?);
    }
    Ok(Some(builder.build()?))
}

fn replace_range(
    compiled: &Compiled,
    m: &NodeMatch<'_, StrDoc<SupportLang>>,
    text: &str,
) -> Range<usize> {
    if compiled.delete {
        return whole_line_if_alone(text, m.range());
    }
    if compiled.cfg.fixer.is_empty() {
        return m.range();
    }
    compiled.cfg.fixer[0].get_replaced_range(m, &compiled.cfg.matcher)
}

/// Deleting a statement should not leave a blank line behind. When the match is
/// the only thing on its line, widen the range to the whole line; otherwise
/// delete just the match, so an inline trailing expression keeps its neighbours.
fn whole_line_if_alone(text: &str, range: Range<usize>) -> Range<usize> {
    let line_start = text[..range.start]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let line_end = text[range.end..]
        .find('\n')
        .map(|index| range.end + index + 1)
        .unwrap_or(text.len());
    let before = &text[line_start..range.start];
    let after = &text[range.end..line_end];
    if before.trim().is_empty() && after.trim().is_empty() {
        line_start..line_end
    } else {
        range
    }
}

fn build_rule(
    id: &str,
    name: &str,
    rule: &serde_yaml::Value,
    fix: Option<&str>,
) -> Result<RuleConfig<SupportLang>> {
    let mut yaml = String::new();
    yaml.push_str("id: ");
    yaml.push_str(&scalar(id));
    yaml.push_str("\nmessage: ");
    yaml.push_str(&scalar(name));
    yaml.push_str("\nseverity: info\nlanguage: rust\n");
    if let Some(fix) = fix {
        yaml.push_str("fix: ");
        yaml.push_str(&scalar(fix));
        yaml.push('\n');
    }
    yaml.push_str("rule:\n");
    yaml.push_str(&indent(&to_yaml(rule)?, 2));

    if std::env::var_os("RUSTREWRITE_DEBUG").is_some() {
        eprintln!("--- compiled rule yaml ---\n{yaml}\n---------------------------");
    }

    let configs = from_yaml_string::<SupportLang>(&yaml, &GlobalRules::default())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("rule yaml:\n{yaml}"))?;
    configs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("rule did not compile to a matcher"))
}

fn scalar(value: &str) -> String {
    serde_yaml::to_string(&serde_yaml::Value::String(value.to_string()))
        .map(|s| s.trim_end().to_string())
        .unwrap_or_else(|_| format!("{value:?}"))
}

fn to_yaml(value: &serde_yaml::Value) -> Result<String> {
    serde_yaml::to_string(value).context("re-serializing rule")
}

fn indent(text: &str, width: usize) -> String {
    let pad = " ".repeat(width);
    text.lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{pad}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Apply a recipe's structural steps to a snippet, for recipe self-tests and
/// `explain`. Cargo steps are ignored here by design.
pub fn apply_to_snippet(resolved: &Resolved, before: &str) -> Result<String> {
    let engine = Engine::compile(std::slice::from_ref(resolved))?;
    let (after, _) = engine.rewrite(before, Path::new("snippet.rs"), Path::new(""))?;
    Ok(after)
}
