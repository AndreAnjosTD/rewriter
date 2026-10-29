//! Recipe schema, catalog loading, and composite resolution.
//!
//! A recipe is the unit of change, mirroring OpenRewrite: it has an id, a
//! human name, a risk class, a mode (`fix` writes, `scan` only reports), and an
//! ordered list of steps. A recipe may instead be a composite that names other
//! recipes; composites flatten to their steps in declaration order.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/builtin_recipes.rs"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Rewrites source.
    #[default]
    Fix,
    /// Reports findings only; never writes.
    Scan,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    #[default]
    Low,
    Medium,
    High,
}

impl std::fmt::Display for Risk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Risk::Low => write!(f, "low"),
            Risk::Medium => write!(f, "medium"),
            Risk::High => write!(f, "high"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StepKind {
    /// An ast-grep structural rule, optionally with a rewrite template.
    #[default]
    Structural,
    /// A cargo-powered fixer (compiler or clippy machine-applicable fixes).
    Cargo,
    /// A rename resolved by rust-analyzer, so two same-named methods on
    /// different types are told apart by the compiler rather than by text.
    Semantic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SemanticAction {
    #[default]
    Rename,
    DeleteArgument,
    AddArgument,
    ReorderArguments,
    /// Run one of rust-analyzer's built-in assists, chosen by title.
    Assist,
    /// Replace the declared return type of a resolved function.
    ChangeReturnType,
}

/// A recipe parameter, the equivalent of OpenRewrite's `@Option`.
#[derive(Debug, Clone, Deserialize)]
pub struct OptionSpec {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    /// Whether the value is substituted as literal text into an ast-grep
    /// pattern or template. A value ast-grep would read as a metavariable is
    /// refused instead of being substituted.
    #[serde(default)]
    pub literal: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Step {
    #[serde(default)]
    pub kind: StepKind,
    /// ast-grep rule object (pattern, kind, all/any/not/inside, constraints, utils...).
    pub rule: Option<serde_yaml::Value>,
    /// ast-grep `constraints`, applied to metavariables bound by `rule`.
    pub constraints: Option<serde_yaml::Value>,
    /// ast-grep `utils`, reusable named sub-rules referenced with `matches:`.
    pub utils: Option<serde_yaml::Value>,
    /// Rewrite template. Omit for a scan-only step. `$VAR`/`$$$` metavariables
    /// bound by `rule` are substituted.
    pub fix: Option<String>,
    /// Finding text for scan steps, defaulting to the recipe name.
    pub message: Option<String>,
    /// Remove the whole line when the match is alone on it (used for dead statements).
    #[serde(default)]
    pub delete: bool,
    /// Only apply to paths matching one of these globs (relative to the run root).
    #[serde(default)]
    pub include: Vec<String>,
    /// Skip paths matching one of these globs.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Command for a cargo step, e.g. ["cargo", "fix", "--all-targets"].
    pub command: Option<Vec<String>>,
    /// Semantic action; defaults to `rename`.
    pub action: Option<SemanticAction>,
    /// File the symbol lives in, relative to the run root.
    pub file: Option<String>,
    /// Text on the declaration line, used to locate it without pinning a line
    /// number that any edit above would invalidate.
    pub anchor: Option<String>,
    /// 1-based line number, as an alternative to `anchor`.
    pub line: Option<u32>,
    /// The identifier to rename on that line.
    pub from: Option<String>,
    /// The new name.
    pub to: Option<String>,
    /// Argument position for the argument operations, as text so that an option
    /// can supply it. Parsed once the options are substituted.
    pub index: Option<String>,
    /// Comma-separated permutation, e.g. `1,0,2`.
    pub order: Option<String>,
    /// The text written at every call site by `add-argument`.
    pub argument: Option<String>,
    /// The parameter written into the declaration by `add-argument`, e.g. `retries: u32`.
    pub parameter: Option<String>,
    /// The assist title to run, e.g. `Inline variable`.
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Example {
    pub before: String,
    pub after: String,
    #[serde(default)]
    pub why: Option<String>,
    /// Option values for this example, the equivalent of `--set` on a run.
    /// A recipe that declares required options cannot be proven without them.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Recipe {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub risk: Risk,
    #[serde(default)]
    pub mode: Mode,
    /// Composite: ids of recipes to run, in order.
    #[serde(default)]
    pub recipes: Vec<String>,
    /// Parameters, the equivalent of OpenRewrite's `@Option` fields.
    #[serde(default)]
    pub options: BTreeMap<String, OptionSpec>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub examples: Vec<Example>,
    /// Where this recipe was loaded from, for `list --sources` style reporting.
    #[serde(skip)]
    pub source: String,
}

/// A step paired with the recipe that actually declares it, so findings are
/// attributed to the leaf recipe even when it ran through a composite.
#[derive(Debug, Clone)]
pub struct ResolvedStep {
    pub recipe: String,
    pub name: String,
    pub mode: Mode,
    pub step: Step,
}

/// A recipe after composites have been flattened into concrete steps.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub recipe: Recipe,
    pub steps: Vec<ResolvedStep>,
    pub lineage: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Catalog {
    recipes: BTreeMap<String, Recipe>,
}

impl Catalog {
    pub fn load(extra: &[PathBuf], root: &Path) -> Result<Self> {
        let mut catalog = Catalog::default();

        for (origin, text) in BUILTIN_RECIPES {
            let recipe = parse_recipe(text, &format!("builtin:{origin}"))
                .with_context(|| format!("built-in recipe {origin}"))?;
            catalog.insert(recipe)?;
        }

        // Project-local recipes take precedence over built-ins: this is how a
        // team pins its own migration catalog next to the code it migrates.
        for dir in [root.join(".rustrewrite").join("recipes")] {
            if dir.is_dir() {
                catalog.load_dir(&dir)?;
            }
        }

        for path in extra {
            if path.is_dir() {
                catalog.load_dir(path)?;
            } else {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading recipe file {}", path.display()))?;
                let recipe = parse_recipe(&text, &path.display().to_string())?;
                catalog.insert(recipe)?;
            }
        }

        Ok(catalog)
    }

    fn load_dir(&mut self, dir: &Path) -> Result<()> {
        let mut entries: Vec<PathBuf> = walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yml") | Some("yaml")
                )
            })
            .collect();
        entries.sort();
        for path in entries {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading recipe file {}", path.display()))?;
            let recipe = parse_recipe(&text, &path.display().to_string())
                .with_context(|| format!("recipe file {}", path.display()))?;
            self.insert(recipe)?;
        }
        Ok(())
    }

    fn insert(&mut self, recipe: Recipe) -> Result<()> {
        if self.recipes.contains_key(&recipe.id) {
            bail!(
                "duplicate recipe id `{}` (again from {})",
                recipe.id,
                recipe.source
            );
        }
        if recipe.steps.is_empty() && recipe.recipes.is_empty() {
            bail!("recipe `{}` has no steps and no sub-recipes", recipe.id);
        }
        for step in &recipe.steps {
            match step.kind {
                StepKind::Structural if step.rule.is_none() => {
                    bail!(
                        "recipe `{}` has a structural step with no `rule`",
                        recipe.id
                    )
                }
                StepKind::Cargo if step.command.is_none() => {
                    bail!("recipe `{}` has a cargo step with no `command`", recipe.id)
                }
                StepKind::Semantic => {
                    if step.file.is_none() || step.from.is_none() {
                        bail!(
                            "recipe `{}` has a semantic step without `file` and `from`",
                            recipe.id
                        );
                    }
                    if step.anchor.is_none() && step.line.is_none() {
                        bail!(
                            "recipe `{}` has a semantic step with neither `anchor` nor `line`",
                            recipe.id
                        );
                    }
                    let action = step.action.unwrap_or_default();
                    match action {
                        SemanticAction::Rename if step.to.is_none() => {
                            bail!("recipe `{}` has a rename step without `to`", recipe.id)
                        }
                        SemanticAction::DeleteArgument if step.index.is_none() => bail!(
                            "recipe `{}` has a delete-argument step without `index`",
                            recipe.id
                        ),
                        SemanticAction::AddArgument
                            if step.index.is_none()
                                || step.argument.is_none()
                                || step.parameter.is_none() =>
                        {
                            bail!(
                                "recipe `{}` has an add-argument step without `index`, \
                                 `argument` and `parameter`",
                                recipe.id
                            )
                        }
                        SemanticAction::ReorderArguments if step.order.is_none() => bail!(
                            "recipe `{}` has a reorder-arguments step without `order`",
                            recipe.id
                        ),
                        SemanticAction::Assist if step.title.is_none() => {
                            bail!("recipe `{}` has an assist step without `title`", recipe.id)
                        }
                        SemanticAction::ChangeReturnType
                            if step.to.as_deref().is_none_or(|to| to.trim().is_empty()) =>
                        {
                            bail!(
                                "recipe `{}` has a change-return-type step without a non-empty \
                                 `to`",
                                recipe.id
                            )
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        // A recipe that both edits text and renames a symbol would ask the
        // server to resolve positions against a file the structural steps have
        // already moved. Keep the two apart instead of guessing.
        let kinds: Vec<StepKind> = recipe.steps.iter().map(|step| step.kind).collect();
        if kinds.contains(&StepKind::Structural) && kinds.contains(&StepKind::Semantic) {
            bail!(
                "recipe `{}` mixes structural and semantic steps; split it into two recipes",
                recipe.id
            );
        }
        self.recipes.insert(recipe.id.clone(), recipe);
        Ok(())
    }

    pub fn all(&self) -> impl Iterator<Item = &Recipe> {
        self.recipes.values()
    }

    pub fn get(&self, id: &str) -> Option<&Recipe> {
        self.recipes.get(id)
    }

    /// Resolve a selector: a recipe id, or `all`.
    pub fn select(&self, selector: &str) -> Result<Vec<&Recipe>> {
        if selector == "all" {
            return Ok(self
                .recipes
                .values()
                .filter(|r| r.mode == Mode::Fix && r.recipes.is_empty())
                // A recipe that needs parameters cannot run from a bare `all`;
                // it has to be named, with its options supplied.
                .filter(|r| !r.options.values().any(|option| option.required))
                .collect());
        }
        // Prefix support: `rust.idiom` selects every recipe under that namespace.
        if !self.recipes.contains_key(selector) {
            let matches: Vec<&Recipe> = self
                .recipes
                .values()
                .filter(|r| r.id.starts_with(&format!("{selector}.")))
                .collect();
            if matches.is_empty() {
                let known: Vec<&String> = self.recipes.keys().collect();
                bail!(
                    "unknown recipe `{}`\n\navailable:\n  {}",
                    selector,
                    known
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("\n  ")
                );
            }
            return Ok(matches);
        }
        Ok(vec![self.recipes.get(selector).expect("checked")])
    }

    /// Flatten composites into concrete steps, preserving order and detecting
    /// cycles so a bad catalog fails loudly instead of hanging.
    pub fn resolve(&self, recipe: &Recipe) -> Result<Resolved> {
        let mut steps = Vec::new();
        let mut lineage = vec![recipe.id.clone()];
        self.flatten(
            recipe,
            &mut steps,
            &mut lineage,
            &mut vec![recipe.id.clone()],
        )?;
        // Sub-recipes run first, then this recipe's own steps. A leaf recipe has
        // no sub-recipes, so this is where the common case lands.
        for step in &recipe.steps {
            steps.push(ResolvedStep {
                recipe: recipe.id.clone(),
                name: recipe.name.clone(),
                mode: recipe.mode,
                step: step.clone(),
            });
        }
        Ok(Resolved {
            recipe: recipe.clone(),
            steps,
            lineage,
        })
    }

    fn flatten(
        &self,
        recipe: &Recipe,
        steps: &mut Vec<ResolvedStep>,
        lineage: &mut Vec<String>,
        stack: &mut Vec<String>,
    ) -> Result<()> {
        for child_id in &recipe.recipes {
            let child = self.get(child_id).with_context(|| {
                format!("recipe `{}` references unknown `{child_id}`", recipe.id)
            })?;
            if stack.contains(child_id) {
                bail!("recipe cycle: {} -> {child_id}", stack.join(" -> "));
            }
            if !lineage.contains(child_id) {
                lineage.push(child_id.clone());
            }
            stack.push(child_id.clone());
            self.flatten(child, steps, lineage, stack)?;
            stack.pop();
            for step in &child.steps {
                steps.push(ResolvedStep {
                    recipe: child.id.clone(),
                    name: child.name.clone(),
                    mode: child.mode,
                    step: step.clone(),
                });
            }
        }
        Ok(())
    }
}

fn parse_recipe(text: &str, source: &str) -> Result<Recipe> {
    let mut recipe: Recipe =
        serde_yaml::from_str(text).with_context(|| format!("parsing YAML from {source}"))?;
    recipe.source = source.to_string();
    Ok(recipe)
}

/// Fill a recipe's parameters, substituting `${name}` in every string it holds.
///
/// This is the Rust equivalent of OpenRewrite injecting `@Option` fields, and it
/// is what turns `ChangeType` from a hard-coded migration into a reusable
/// primitive.
pub fn apply_options(resolved: &mut Resolved, provided: &BTreeMap<String, String>) -> Result<()> {
    let declared = &resolved.recipe.options;
    for key in provided.keys() {
        if !declared.contains_key(key) {
            let known = if declared.is_empty() {
                "  (none)".to_string()
            } else {
                declared
                    .keys()
                    .map(|name| format!("  {name}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            bail!(
                "recipe `{}` takes no option `{key}`\n\noptions:\n{known}",
                resolved.recipe.id
            );
        }
    }
    for (name, spec) in declared {
        if !spec.literal {
            continue;
        }
        if let Some(value) = provided.get(name) {
            if has_metavariable(value) {
                bail!(
                    "recipe `{}` option `{name}` is substituted as literal text, so it \
                     cannot contain an ast-grep metavariable such as `$NAME`, `$5` or `$$$`.\n\
                     Nothing was rewritten.",
                    resolved.recipe.id
                );
            }
            if value.contains("${") {
                bail!(
                    "recipe `{}` option `{name}` is substituted as literal text, so it \
                     cannot contain `${{...}}`, which the recipe engine reads as an option \
                     placeholder.\nNothing was rewritten.",
                    resolved.recipe.id
                );
            }
        }
    }
    let missing: Vec<(&String, &OptionSpec)> = declared
        .iter()
        .filter(|(name, spec)| spec.required && !provided.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        let lines = missing
            .iter()
            .map(|(name, spec)| {
                let detail = if spec.description.is_empty() {
                    String::new()
                } else {
                    format!("   {}", spec.description)
                };
                format!("  --set {name}=...{detail}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let noun = if missing.len() == 1 {
            "an option"
        } else {
            "options"
        };
        bail!("recipe `{}` needs {noun}:\n{lines}", resolved.recipe.id);
    }
    if provided.is_empty() {
        return Ok(());
    }

    for step in &mut resolved.steps {
        let raw = &mut step.step;
        for field in [
            &mut raw.fix,
            &mut raw.message,
            &mut raw.file,
            &mut raw.anchor,
            &mut raw.from,
            &mut raw.to,
            &mut raw.index,
            &mut raw.order,
            &mut raw.argument,
            &mut raw.parameter,
            &mut raw.title,
        ]
        .into_iter()
        .flatten()
        {
            *field = substitute(field, provided);
        }
        for value in [&mut raw.rule, &mut raw.constraints, &mut raw.utils]
            .into_iter()
            .flatten()
        {
            substitute_value(value, provided);
        }
        if let Some(command) = &mut raw.command {
            for argument in command.iter_mut() {
                *argument = substitute(argument, provided);
            }
        }
        let rendered = serde_yaml::to_string(&step.step).unwrap_or_default();
        if let Some(missing) = first_placeholder(&rendered) {
            bail!(
                "recipe `{}` uses `${{{missing}}}` but no value was provided for it",
                resolved.recipe.id
            );
        }
    }
    Ok(())
}

fn substitute(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = text.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("${{{key}}}"), value);
    }
    out
}

/// Whether ast-grep would read part of `value` as a metavariable.
///
/// This mirrors ast-grep's own rule so a literal option can still contain a
/// dollar sign: `$NAME`, `$_` and `$5` are captures (the fix template parser
/// accepts a digit as a name), and three or more dollars start a multi
/// metavariable, while `$name` stays literal text.
fn has_metavariable(value: &str) -> bool {
    let mut dollars = 0usize;
    for c in value.chars() {
        if c == '$' {
            dollars += 1;
            continue;
        }
        let capture = dollars > 0 && (c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if dollars >= 3 || capture {
            return true;
        }
        dollars = 0;
    }
    dollars >= 3
}

fn substitute_value(value: &mut serde_yaml::Value, vars: &BTreeMap<String, String>) {
    match value {
        serde_yaml::Value::String(text) => *text = substitute(text, vars),
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                substitute_value(item, vars);
            }
        }
        serde_yaml::Value::Mapping(entries) => {
            for (_, item) in entries.iter_mut() {
                substitute_value(item, vars);
            }
        }
        _ => {}
    }
}

fn first_placeholder(text: &str) -> Option<&str> {
    let start = text.find("${")? + 2;
    let end = text[start..].find('}')? + start;
    Some(&text[start..end])
}
