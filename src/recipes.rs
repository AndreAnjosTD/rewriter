//! Recipe schema, catalog loading, and composite resolution.
//!
//! A recipe is the unit of change, mirroring OpenRewrite: it has an id, a
//! human name, a risk class, a mode (`fix` writes, `scan` only reports), and an
//! ordered list of steps. A recipe may instead be a composite that names other
//! recipes; composites flatten to their steps in declaration order.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StepKind {
    /// An ast-grep structural rule, optionally with a rewrite template.
    #[default]
    Structural,
    /// A cargo-powered fixer (compiler or clippy machine-applicable fixes).
    Cargo,
}

#[derive(Debug, Clone, Deserialize, Default)]
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct Example {
    pub before: String,
    pub after: String,
    #[serde(default)]
    pub why: Option<String>,
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
                _ => {}
            }
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
