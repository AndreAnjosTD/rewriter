//! Rewrite engine: turns recipe steps into ast-grep rules, applies them to
//! source text, and runs cargo-powered fixers.

use crate::recipes::{InsertWhere, Mode, Resolved, SemanticAction, Step, StepKind};
use crate::util::line_column;
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

/// The `include`/`exclude` globs a step scopes itself with.
#[derive(Default)]
struct PathFilter {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl PathFilter {
    fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        Ok(Self {
            include: build_globset(include)?,
            exclude: build_globset(exclude)?,
        })
    }

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

struct Compiled {
    recipe: String,
    message: String,
    cfg: RuleConfig<SupportLang>,
    delete: bool,
    filter: PathFilter,
}

/// An `insert` step, compiled once and applied to every file in scope.
struct Insert {
    recipe: String,
    message: String,
    at: InsertWhere,
    /// The text to write, without its trailing newline.
    body: String,
    /// Presence of this marker means the work is already done.
    marker: String,
    filter: PathFilter,
}

pub struct Engine {
    compiled: Vec<Compiled>,
    insert_steps: Vec<Insert>,
    cargo_steps: Vec<(String, Vec<String>)>,
    semantic_steps: Vec<SemanticStep>,
}

/// A rename or argument rewrite that has to be resolved by the compiler.
#[derive(Debug, Clone)]
pub struct SemanticStep {
    pub recipe: String,
    pub action: SemanticAction,
    pub file: PathBuf,
    pub anchor: Option<String>,
    pub line: Option<u32>,
    pub from: String,
    pub to: String,
    pub index: Option<usize>,
    pub order: Option<Vec<usize>>,
    pub argument: Option<String>,
    pub parameter: Option<String>,
    pub title: Option<String>,
}

impl Engine {
    pub fn compile(resolved: &[Resolved]) -> Result<Self> {
        let mut compiled = Vec::new();
        let mut insert_steps = Vec::new();
        let mut cargo_steps = Vec::new();
        let mut semantic_steps = Vec::new();
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
                        let cfg = build_rule(&resolved_step.recipe, &message, rule, fix, step)
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
                            filter: PathFilter::new(&step.include, &step.exclude)?,
                        });
                    }
                    StepKind::Insert => {
                        // A scan recipe reports hazards; it never writes, so its
                        // insert steps are not compiled at all.
                        if resolved_step.mode != Mode::Fix {
                            continue;
                        }
                        let at = step.insert_where.expect("validated at load time");
                        let body = step
                            .text
                            .as_deref()
                            .expect("validated at load time")
                            .trim_end_matches(['\n', '\r'])
                            .to_string();
                        // `text` itself is the natural marker: once written, the
                        // file contains it, so a second run is a no-op.
                        let marker = step.unless.clone().unwrap_or_else(|| body.clone());
                        insert_steps.push(Insert {
                            recipe: resolved_step.recipe.clone(),
                            message: step
                                .message
                                .clone()
                                .unwrap_or_else(|| resolved_step.name.clone()),
                            at,
                            body,
                            marker,
                            filter: PathFilter::new(&step.include, &step.exclude)?,
                        });
                    }
                    StepKind::Cargo => {
                        let command = step.command.clone().expect("validated at load time");
                        cargo_steps.push((resolved_step.recipe.clone(), command));
                    }
                    StepKind::Semantic => {
                        let index = step
                            .index
                            .as_deref()
                            .map(|raw| {
                                raw.trim().parse::<usize>().with_context(|| {
                                    format!(
                                        "recipe `{}`: `index` must be a number, got `{raw}`",
                                        resolved_step.recipe
                                    )
                                })
                            })
                            .transpose()?;
                        let order = step
                            .order
                            .as_deref()
                            .map(|raw| -> Result<Vec<usize>> {
                                raw.split(',')
                                    .map(|part| {
                                        part.trim().parse::<usize>().with_context(|| {
                                            format!(
                                                "recipe `{}`: `order` must be a comma-separated \
                                                 list of numbers, got `{raw}`",
                                                resolved_step.recipe
                                            )
                                        })
                                    })
                                    .collect()
                            })
                            .transpose()?;
                        semantic_steps.push(SemanticStep {
                            recipe: resolved_step.recipe.clone(),
                            action: step.action.unwrap_or_default(),
                            file: PathBuf::from(step.file.clone().unwrap_or_default()),
                            anchor: step.anchor.clone(),
                            line: step.line,
                            from: step.from.clone().unwrap_or_default(),
                            to: step.to.clone().unwrap_or_default(),
                            index,
                            order,
                            argument: step.argument.clone(),
                            parameter: step.parameter.clone(),
                            title: step.title.clone(),
                        });
                    }
                }
            }
        }
        Ok(Self {
            compiled,
            insert_steps,
            cargo_steps,
            semantic_steps,
        })
    }

    /// Whether this engine carries steps that need a language server.
    pub fn has_semantic_steps(&self) -> bool {
        !self.semantic_steps.is_empty()
    }

    pub fn semantic_steps(&self) -> &[SemanticStep] {
        &self.semantic_steps
    }

    /// Rewrite `text` to a fixpoint.
    ///
    /// Findings are collected from the original text so reported positions refer
    /// to the file the reader is looking at; matches created by a rewrite are
    /// still applied but not re-reported.
    pub fn rewrite(&self, text: &str, path: &Path, root: &Path) -> Result<(String, Vec<Finding>)> {
        let relative = relative_path(path, root);
        let mut findings = Vec::new();
        for compiled in self.compiled.iter().filter(|c| c.filter.allows(&relative)) {
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
            for compiled in self.compiled.iter().filter(|c| c.filter.allows(&relative)) {
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

        // Inserts run after the structural pass: they append text rather than
        // replace a node, so there is nothing for the fixpoint to re-match.
        for insert in self
            .insert_steps
            .iter()
            .filter(|insert| insert.filter.allows(&relative))
        {
            let Some((next, offset)) = insert.apply(&current) else {
                continue;
            };
            let (line, column) = line_column(&current, offset);
            findings.push(Finding {
                recipe: insert.recipe.clone(),
                path: path.to_path_buf(),
                line,
                column,
                // Nothing was replaced: `after` carries the text that was added.
                before: String::new(),
                after: Some(insert.body.clone()),
                message: insert.message.clone(),
            });
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

impl Insert {
    /// Apply this insert to `text`, returning the new text plus the byte offset
    /// the text was written at. `None` means the file already has what this
    /// step would add.
    fn apply(&self, text: &str) -> Option<(String, usize)> {
        if !self.marker.is_empty() && text.contains(&self.marker) {
            return None;
        }
        // A `use` has a duplicate form the marker cannot see: a braced group
        // binds the same name without containing the inserted line.
        if let Some(name) = inserted_use_name(&self.body) {
            if binds_name(text, name) {
                return None;
            }
        }
        let (offset, block) = match self.at {
            InsertWhere::Top => (top_offset(text), format!("{}\n\n", self.body)),
            InsertWhere::AfterUses => match last_use_end(text) {
                Some(end) => {
                    let trailing = if end == text.len() { "\n" } else { "" };
                    (end, format!("\n{}{trailing}", self.body))
                }
                None => match first_item_start(text) {
                    Some(start) => (start, format!("{}\n\n", self.body)),
                    // No use and no item: an empty or comment-only file.
                    None if text.is_empty() => (0, format!("{}\n", self.body)),
                    None => (text.len(), format!("\n{}\n", self.body)),
                },
            },
        };
        let mut next = String::with_capacity(text.len() + block.len());
        next.push_str(&text[..offset]);
        next.push_str(&block);
        next.push_str(&text[offset..]);
        (next != text).then_some((next, offset))
    }
}

/// The first byte that is not a byte-order mark or shebang: a BOM only means
/// anything at the start of the file, and a shebang has to stay on line one.
fn top_offset(text: &str) -> usize {
    let mut offset = if text.starts_with('\u{feff}') { 3 } else { 0 };
    if text[offset..].starts_with("#!") && !text[offset..].starts_with("#![") {
        offset += text[offset..]
            .find('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }
    offset
}

/// End of the last top-level `use`, so the next import lands after it.
fn last_use_end(text: &str) -> Option<usize> {
    let doc = SupportLang::Rust.ast_grep(text);
    doc.root()
        .named_children()
        .filter(|node| node.kind() == "use_declaration")
        .map(|node| node.range().end)
        .last()
}

/// Start of the first item, including the comments and attributes that belong
/// to it, so an import does not split an item from its docs.
fn first_item_start(text: &str) -> Option<usize> {
    let doc = SupportLang::Rust.ast_grep(text);
    let children: Vec<_> = doc.root().named_children().collect();
    let mut first = children.iter().position(|node| !is_trivia(node))?;
    while first > 0 {
        let previous = &children[first - 1];
        // Inner docs and inner attributes describe the whole file and stay at
        // the top; everything after them belongs to the item below.
        if previous.kind() == "inner_attribute_item" || is_inner_doc(previous) {
            break;
        }
        if !is_trivia(previous) {
            break;
        }
        first -= 1;
    }
    Some(children[first].range().start)
}

fn is_trivia(node: &ast_grep_core::Node<'_, StrDoc<SupportLang>>) -> bool {
    matches!(
        node.kind().as_ref(),
        "line_comment" | "block_comment" | "attribute_item" | "inner_attribute_item"
    )
}

fn is_inner_doc(node: &ast_grep_core::Node<'_, StrDoc<SupportLang>>) -> bool {
    let text = node.text();
    text.starts_with("//!") || text.starts_with("/*!")
}

/// The name a plain `use path;` brings into scope, when the inserted text is
/// exactly that shape. A brace group, glob or alias has more than one answer and
/// is left to the marker check.
fn inserted_use_name(body: &str) -> Option<&str> {
    let rest = body.trim().strip_prefix("use ")?;
    let rest = rest.strip_suffix(';')?.trim();
    if rest.contains('{') || rest.contains('*') || rest.contains(" as ") {
        return None;
    }
    let name = rest.rsplit("::").next()?.trim();
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_alphabetic() || first == '_') || !chars.all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(name)
}

/// Whether an existing `use` declaration already brings `name` into scope.
///
/// Identifier tokens inside use declarations are matched rather than the whole
/// path, because that is what catches `use a::{BTreeMap, HashMap};` and several
/// declarations in one statement. An alias is the exception: `use a::Bar as Baz`
/// binds `Baz`, not `Bar`, so the alias is the only name that counts there.
fn binds_name(text: &str, name: &str) -> bool {
    let doc = SupportLang::Rust.ast_grep(text);
    let found = doc
        .root()
        .dfs()
        .any(|node| node.kind() == "use_declaration" && binds_inside(&node, name));
    found
}

fn binds_inside(node: &ast_grep_core::Node<'_, StrDoc<SupportLang>>, name: &str) -> bool {
    match node.kind().as_ref() {
        "use_as_clause" => node
            .field("alias")
            .map(|alias| alias.text() == name)
            .unwrap_or(false),
        "identifier" => node.text() == name,
        _ => node.children().any(|child| binds_inside(&child, name)),
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
    step: &Step,
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
    yaml.push('\n');
    for (key, value) in [("constraints", &step.constraints), ("utils", &step.utils)] {
        if let Some(value) = value {
            yaml.push_str(key);
            yaml.push_str(":\n");
            yaml.push_str(&indent(&to_yaml(value)?, 2));
            yaml.push('\n');
        }
    }

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
