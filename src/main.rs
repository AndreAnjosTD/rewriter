//! rustrewrite - recipe-driven, verified refactoring for Rust.
//!
//! The shape follows OpenRewrite: recipes are declarative catalog entries, a
//! recipe can compose other recipes, `check` reports without writing, `run`
//! rewrites, and a failing compile rolls the whole run back.

mod callsites;
mod engine;
mod recipes;
mod semantic;
mod util;
mod verify;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use engine::{apply_to_snippet, Engine, Finding, SemanticStep};
use recipes::{Catalog, Mode, Resolved, SemanticAction};
use semantic::Server;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "rustrewrite",
    version,
    about = "Recipe-driven, verified refactoring for Rust",
    long_about = "OpenRewrite-style recipes for Rust: declarative catalog entries, composable \
                  recipes, dry-run reporting, and an automatic compile/test gate that rolls a \
                  failed rewrite back."
)]
struct Cli {
    /// Root of the crate or workspace to work on.
    #[arg(short, long, global = true, default_value = ".")]
    path: PathBuf,

    /// Extra recipe directory or file (repeatable). Project-local
    /// `.rustrewrite/recipes` is loaded automatically.
    #[arg(long, global = true)]
    recipes: Vec<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List available recipes.
    List {
        /// Only recipes carrying this tag.
        #[arg(short, long)]
        tag: Option<String>,
        /// Show where each recipe was loaded from.
        #[arg(long)]
        sources: bool,
    },
    /// Explain one recipe, and run its examples as a proof.
    Explain { recipe: String },
    /// Verify every recipe example. Does not write.
    Test {
        /// Limit to one recipe or namespace.
        #[arg(short, long)]
        recipe: Option<String>,
    },
    /// Report what a recipe would change. Never writes.
    Check {
        /// Recipe id, namespace prefix, or `all`.
        #[arg(short, long, default_value = "all")]
        recipe: String,
        /// Print a unified diff of the pending changes.
        #[arg(long)]
        diff: bool,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// Recipe option, e.g. --set file=src/main.rs. Repeatable.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        options: Vec<String>,
        /// Seconds to wait for the analyzer to index, for semantic recipes.
        #[arg(long, default_value_t = 180)]
        wait: u64,
    },
    /// Apply a recipe.
    Run {
        /// Recipe id, namespace prefix, or `all`.
        #[arg(short, long, default_value = "all")]
        recipe: String,
        /// Write changes. Without this, run is a dry run.
        #[arg(long)]
        apply: bool,
        /// Skip the compile gate. Dangerous: prefers speed over correctness.
        #[arg(long)]
        no_verify: bool,
        /// Also run the test suite after rewriting.
        #[arg(long)]
        test: bool,
        /// Allow running with uncommitted changes.
        #[arg(long)]
        allow_dirty: bool,
        /// Print a unified diff.
        #[arg(long)]
        diff: bool,
        /// Recipe option, e.g. --set file=src/main.rs. Repeatable.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        options: Vec<String>,
        /// Seconds to wait for the analyzer to index, for semantic recipes.
        #[arg(long, default_value_t = 180)]
        wait: u64,
    },
    /// Run the compile (and optionally test) gate alone.
    Verify {
        /// Also run the test suite.
        #[arg(long)]
        test: bool,
    },
    /// Rename one resolved symbol using rust-analyzer's type information.
    ///
    /// Unlike a recipe this is positional: it renames exactly the symbol at the
    /// given location, so two methods with the same name on different types are
    /// told apart by the compiler rather than by text.
    Rename {
        /// file:line:column, 1-based, column counted in characters.
        #[arg(long)]
        at: String,
        /// The new name.
        #[arg(long)]
        to: String,
        /// Write the rename. Without this, only report it.
        #[arg(long)]
        apply: bool,
        /// Skip the compile gate after writing.
        #[arg(long)]
        no_verify: bool,
        /// Allow running with uncommitted changes.
        #[arg(long)]
        allow_dirty: bool,
        /// Seconds to wait for the analyzer to load and index the workspace.
        #[arg(long, default_value_t = 180)]
        wait: u64,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("rustrewrite: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = absolute(&cli.path)?;
    let catalog = Catalog::load(&cli.recipes, &root)?;

    match cli.command {
        Command::List { tag, sources } => list(&catalog, tag.as_deref(), sources),
        Command::Explain { recipe } => explain(&catalog, &recipe),
        Command::Test { recipe } => test_examples(&catalog, recipe.as_deref()),
        Command::Check {
            recipe,
            diff,
            json,
            options,
            wait,
        } => check(&catalog, &root, &recipe, diff, json, &options, wait),
        Command::Run {
            recipe,
            apply,
            no_verify,
            test,
            allow_dirty,
            diff,
            options,
            wait,
        } => run_recipes(
            &catalog,
            &root,
            &recipe,
            apply,
            !no_verify,
            test,
            allow_dirty,
            diff,
            &options,
            wait,
        ),
        Command::Verify { test } => verify_only(&root, test),
        Command::Rename {
            at,
            to,
            apply,
            no_verify,
            allow_dirty,
            wait,
        } => rename_symbol(&root, &at, &to, apply, !no_verify, allow_dirty, wait),
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(path)
}

fn list(catalog: &Catalog, tag: Option<&str>, sources: bool) -> Result<()> {
    let mut any = false;
    for recipe in catalog.all() {
        if let Some(tag) = tag {
            if !recipe.tags.iter().any(|t| t == tag) {
                continue;
            }
        }
        any = true;
        let kind = if !recipe.recipes.is_empty() {
            "composite"
        } else {
            match recipe.mode {
                Mode::Fix => "fix",
                Mode::Scan => "scan",
            }
        };
        let tags = if recipe.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", recipe.tags.join(", "))
        };
        let needs: Vec<&String> = recipe
            .options
            .iter()
            .filter(|(_, spec)| spec.required)
            .map(|(name, _)| name)
            .collect();
        let needs = if needs.is_empty() {
            String::new()
        } else {
            format!(
                "  needs --set {}",
                needs
                    .iter()
                    .map(|name| format!("{name}=.."))
                    .collect::<Vec<_>>()
                    .join(" --set ")
            )
        };
        let source = if sources {
            format!("  ({})", recipe.source)
        } else {
            String::new()
        };
        println!(
            "{:<40} {:<10} risk:{:<7}{}{}",
            recipe.id,
            kind,
            recipe.risk.to_string(),
            tags,
            source
        );
        if !recipe.name.is_empty() {
            println!("    {}{}", recipe.name, needs);
        }
    }
    if !any {
        bail!("no recipes matched");
    }
    Ok(())
}

fn explain(catalog: &Catalog, selector: &str) -> Result<()> {
    let selected = catalog.select(selector)?;
    if selected.len() != 1 {
        bail!(
            "`{selector}` matches {} recipes; explain takes one id",
            selected.len()
        );
    }
    let recipe = selected[0];
    let resolved = catalog.resolve(recipe)?;
    println!("{}  ({})", recipe.id, recipe.name);
    println!("risk: {}", recipe.risk);
    if !recipe.tags.is_empty() {
        println!("tags: {}", recipe.tags.join(", "));
    }
    if !recipe.description.is_empty() {
        println!();
        println!("{}", recipe.description);
    }
    if !recipe.options.is_empty() {
        println!();
        println!("options:");
        for (name, spec) in &recipe.options {
            let required = if spec.required {
                "required"
            } else {
                "optional"
            };
            println!("  --set {name}=..   ({required}) {}", spec.description);
        }
    }
    if !recipe.recipes.is_empty() {
        println!();
        println!("composed of:");
        for id in &recipe.recipes {
            println!("  - {id}");
        }
        println!("resolves to {} step(s) from:", resolved.steps.len());
        for id in resolved.lineage.iter().skip(1) {
            println!("  - {id}");
        }
    }
    if !recipe.examples.is_empty() {
        println!();
        println!("examples:");
        for (index, example) in recipe.examples.iter().enumerate() {
            let got = apply_to_snippet(&resolved, &example.before)?;
            let ok = got == example.after;
            let status = if ok { "PASS" } else { "FAIL" };
            println!("  [{status}] example {}", index + 1);
            if let Some(why) = &example.why {
                println!("    {why}");
            }
            println!(
                "    before: {}",
                example.before.replace('\n', "\n            ")
            );
            println!(
                "    after:  {}",
                example.after.replace('\n', "\n            ")
            );
            if !ok {
                println!("    got:    {}", got.replace('\n', "\n            "));
            }
        }
    } else if recipe
        .steps
        .iter()
        .any(|step| step.kind == recipes::StepKind::Structural)
    {
        println!();
        println!("(no examples declared, so this recipe is unproven)");
    } else if recipe
        .steps
        .iter()
        .any(|step| step.kind == recipes::StepKind::Semantic)
    {
        println!();
        println!("(semantic recipe: proven by integration tests, not by snippet examples)");
    }
    Ok(())
}

fn test_examples(catalog: &Catalog, selector: Option<&str>) -> Result<()> {
    let recipes: Vec<_> = match selector {
        Some(selector) => catalog.select(selector)?,
        None => catalog.all().collect(),
    };
    let mut passed = 0usize;
    let mut failed = 0usize;
    for recipe in recipes {
        // Recipes with no structural steps of their own are either cargo-only
        // or pure composites. Composites are proven through their children, and
        // cargo recipes have no snippet to rewrite.
        if !recipe
            .steps
            .iter()
            .any(|step| step.kind == recipes::StepKind::Structural)
        {
            continue;
        }
        let resolved = catalog.resolve(recipe)?;
        if resolved.recipe.examples.is_empty() {
            println!("{:40}  NO EXAMPLES", resolved.recipe.id);
            failed += 1;
            continue;
        }
        for (index, example) in resolved.recipe.examples.iter().enumerate() {
            let got = apply_to_snippet(&resolved, &example.before)?;
            if got == example.after {
                passed += 1;
            } else {
                failed += 1;
                println!("{:40}  FAIL example {}", recipe.id, index + 1);
                println!("  before: {}", example.before);
                println!("  want:   {}", example.after);
                println!("  got:    {}", got);
            }
        }
    }
    println!();
    println!("recipe examples: {passed} passed, {failed} failed");
    if failed > 0 {
        bail!("{failed} recipe example(s) failed");
    }
    Ok(())
}

fn check(
    catalog: &Catalog,
    root: &Path,
    selector: &str,
    show_diff: bool,
    json: bool,
    options: &[String],
    wait_secs: u64,
) -> Result<()> {
    let resolved = resolve_selector(catalog, selector, options)?;
    let engine = Engine::compile(&resolved)?;
    let files = util::rust_files(root)?;

    let mut findings: Vec<Finding> = Vec::new();
    let mut pending: Vec<(PathBuf, String, String)> = Vec::new();
    let mut changed_files = 0usize;

    for path in &files {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let (next, mut found) = engine.rewrite(&text, path, root)?;
        findings.append(&mut found);
        if next != text {
            changed_files += 1;
            pending.push((path.clone(), text, next));
        }
    }

    if engine.has_semantic_steps() {
        let outcome = run_semantic(root, engine.semantic_steps(), false, wait_secs)?;
        findings.extend(outcome.findings);
        changed_files += outcome.writes.len();
        pending.extend(outcome.writes);
    }

    if json {
        for finding in &findings {
            println!("{}", serde_json_line(finding, root));
        }
    } else {
        for finding in &findings {
            println!("{}", finding.one_line(root));
        }
    }

    if show_diff {
        for (path, old, new) in &pending {
            print!(
                "{}",
                util::unified_diff(old, new, &path.display().to_string())
            );
        }
    }

    if !json {
        println!();
        if findings.is_empty() {
            println!("no findings for {selector} in {}", root.display());
        } else {
            println!(
                "{} finding(s) in {} file(s); {} file(s) would change",
                findings.len(),
                distinct_files(&findings),
                changed_files
            );
            let supplied = options
                .iter()
                .map(|option| {
                    if option.contains(' ') {
                        format!(" --set \"{option}\"")
                    } else {
                        format!(" --set {option}")
                    }
                })
                .collect::<String>();
            println!("write them with: rustrewrite run --recipe {selector} --apply{supplied}");
        }
    }

    if findings.is_empty() {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

fn serde_json_line(finding: &Finding, root: &Path) -> String {
    let path = finding
        .path
        .strip_prefix(root)
        .unwrap_or(&finding.path)
        .display()
        .to_string()
        .replace('\\', "/");
    format!(
        "{{\"file\":{},\"line\":{},\"column\":{},\"recipe\":{},\"before\":{},\"after\":{}}}",
        json_string(&path),
        finding.line,
        finding.column,
        json_string(&finding.recipe),
        json_string(&finding.before),
        finding
            .after
            .as_ref()
            .map(|a| json_string(a))
            .unwrap_or_else(|| "null".to_string())
    )
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn distinct_files(findings: &[Finding]) -> usize {
    let mut set = std::collections::BTreeSet::new();
    for finding in findings {
        set.insert(&finding.path);
    }
    set.len()
}

fn resolve_selector(
    catalog: &Catalog,
    selector: &str,
    options: &[String],
) -> Result<Vec<Resolved>> {
    let provided = parse_options(options)?;
    let selected = catalog.select(selector)?;
    let mut resolved = Vec::new();
    for recipe in selected {
        let mut one = catalog.resolve(recipe)?;
        recipes::apply_options(&mut one, &provided)?;
        resolved.push(one);
    }
    if resolved.is_empty() {
        bail!("`{selector}` matched no recipes");
    }
    Ok(resolved)
}

fn parse_options(raw: &[String]) -> Result<BTreeMap<String, String>> {
    let mut options = BTreeMap::new();
    for item in raw {
        let (key, value) = item
            .split_once('=')
            .with_context(|| format!("expected `--set key=value`, got `{item}`"))?;
        options.insert(key.trim().to_string(), value.to_string());
    }
    Ok(options)
}

struct SemanticOutcome {
    findings: Vec<Finding>,
    writes: Vec<(PathBuf, String, String)>,
}

/// Run semantic steps against one language server, applying edits only when
/// asked. Edits are always reported, so `check` can show what would change.
fn run_semantic(
    root: &Path,
    steps: &[SemanticStep],
    write: bool,
    wait_secs: u64,
) -> Result<SemanticOutcome> {
    let mut outcome = SemanticOutcome {
        findings: Vec::new(),
        writes: Vec::new(),
    };
    if steps.is_empty() {
        return Ok(outcome);
    }

    let mut server = Server::start(root)?;
    let encoding = server.encoding();
    let budget = std::time::Duration::from_secs(wait_secs);
    let mut opened: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut ready = false;

    for step in steps {
        let path = util::normalize_path(&root.join(&step.file))?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let (line, character) = locate_symbol(&text, step, &path)?;
        let position = semantic::char_position(&text, line, character, encoding)
            .with_context(|| format!("locating `{}` in {}", step.from, path.display()))?;

        if opened.contains(&path) {
            server.refresh(&path, &text)?;
        } else {
            server.did_open(&path, &text)?;
            opened.insert(path.clone());
        }
        if !ready {
            ready = server.wait_ready(budget);
        }

        let edits = match step.action {
            SemanticAction::Rename => {
                let edits = server.rename(&path, position, &step.to, budget)?;
                if edits.is_empty() {
                    bail!(
                        "`{}` declined to rename `{}` in {}.\n\
                         A trait implementation member cannot be renamed. Nothing was written.",
                        step.recipe,
                        step.from,
                        step.file.display()
                    );
                }
                edits
            }
            SemanticAction::Assist => {
                let title = step
                    .title
                    .clone()
                    .with_context(|| format!("`{}` needs an assist title", step.recipe))?;
                let end = semantic::char_position(
                    &text,
                    line,
                    character + step.from.chars().count() as u32,
                    encoding,
                )
                .with_context(|| format!("locating the end of `{}`", step.from))?;
                let actions = server.code_actions(
                    &path,
                    semantic::Range {
                        start: position,
                        end,
                    },
                    budget,
                )?;
                if actions.is_empty() {
                    bail!(
                        "rust-analyzer offers no assists at `{}` in {}",
                        step.from,
                        step.file.display()
                    );
                }
                let wanted = title.to_lowercase();
                let chosen = actions
                    .iter()
                    .find(|action| action.title.eq_ignore_ascii_case(&title))
                    .or_else(|| {
                        actions
                            .iter()
                            .find(|action| action.title.to_lowercase().contains(&wanted))
                    })
                    .with_context(|| {
                        format!(
                            "`{}` is not offered here. Available: {}",
                            title,
                            actions
                                .iter()
                                .map(|action| match &action.kind {
                                    Some(kind) => format!("`{}` ({kind})", action.title),
                                    None => format!("`{}`", action.title),
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;
                let edits = server.resolve_action(chosen, budget)?;
                if edits.is_empty() {
                    bail!(
                        "the `{}` assist produced no edits; nothing was written",
                        chosen.title
                    );
                }
                edits
            }
            _ => {
                let op = argument_op(step)?;
                let rewrites = plan_argument_rewrites(
                    &mut server,
                    step,
                    &op,
                    &path,
                    &text,
                    position,
                    encoding,
                    budget,
                )?;
                apply_rewrites(
                    &mut server,
                    &mut opened,
                    &step.recipe,
                    rewrites,
                    write,
                    &mut outcome,
                )?;
                continue;
            }
        };

        for file_edits in &edits {
            let current = std::fs::read_to_string(&file_edits.path)
                .with_context(|| format!("reading {}", file_edits.path.display()))?;
            let updated = semantic::apply_edits(&current, &file_edits.edits, encoding)?;
            for edit in &file_edits.edits {
                // Report the text actually replaced, not the token we anchored
                // on: an assist can rewrite several unrelated spans.
                let replaced = semantic::position_to_offset(&current, edit.range.start, encoding)
                    .zip(semantic::position_to_offset(
                        &current,
                        edit.range.end,
                        encoding,
                    ))
                    .map(|(start, end)| current[start..end].to_string())
                    .unwrap_or_else(|| step.from.clone());
                outcome.findings.push(Finding {
                    recipe: step.recipe.clone(),
                    path: file_edits.path.clone(),
                    line: edit.range.start.line as usize + 1,
                    column: edit.range.start.character as usize + 1,
                    before: replaced,
                    after: Some(edit.new_text.clone()),
                    message: String::new(),
                });
            }
            if write {
                std::fs::write(&file_edits.path, &updated)
                    .with_context(|| format!("writing {}", file_edits.path.display()))?;
                server.refresh(&file_edits.path, &updated)?;
                opened.insert(file_edits.path.clone());
            }
            outcome
                .writes
                .push((file_edits.path.clone(), current, updated));
        }
    }

    server.shutdown();
    Ok(outcome)
}

fn argument_op(step: &SemanticStep) -> Result<callsites::Op> {
    match step.action {
        SemanticAction::DeleteArgument => Ok(callsites::Op::Delete {
            index: step
                .index
                .with_context(|| format!("`{}` needs an index", step.recipe))?,
        }),
        SemanticAction::AddArgument => Ok(callsites::Op::Add {
            index: step
                .index
                .with_context(|| format!("`{}` needs an index", step.recipe))?,
            text: step
                .argument
                .clone()
                .with_context(|| format!("`{}` needs an argument", step.recipe))?,
        }),
        SemanticAction::ReorderArguments => Ok(callsites::Op::Reorder {
            order: step
                .order
                .clone()
                .with_context(|| format!("`{}` needs an order", step.recipe))?,
        }),
        SemanticAction::Rename => unreachable!("rename is handled separately"),
        SemanticAction::Assist => unreachable!("assist is handled separately"),
    }
}

/// Work out every list that has to change: the declaration's parameters and the
/// arguments of every resolved call site.
#[allow(clippy::too_many_arguments)]
fn plan_argument_rewrites(
    server: &mut Server,
    step: &SemanticStep,
    op: &callsites::Op,
    decl_path: &Path,
    decl_text: &str,
    position: semantic::Position,
    encoding: semantic::PositionEncoding,
    budget: std::time::Duration,
) -> Result<Vec<(PathBuf, std::ops::Range<usize>, String)>> {
    let offset = semantic::position_to_offset(decl_text, position, encoding)
        .with_context(|| format!("locating `{}` in {}", step.from, decl_path.display()))?;
    let (site, has_self) = callsites::declaration(decl_text, offset).with_context(|| {
        format!(
            "`{}` in {} is not inside a function signature",
            step.from,
            decl_path.display()
        )
    })?;

    // A receiver occupies parameter slot zero but no argument slot, so every
    // argument index shifts by one when applied to the declaration.
    let shift = usize::from(has_self);
    let (decl_op, decl_index) = match op {
        callsites::Op::Delete { index } => (
            callsites::Op::Delete {
                index: index + shift,
            },
            index + shift,
        ),
        callsites::Op::Add { index, .. } => (
            callsites::Op::Add {
                index: index + shift,
                text: step
                    .parameter
                    .clone()
                    .with_context(|| format!("`{}` needs a parameter", step.recipe))?,
            },
            index + shift,
        ),
        callsites::Op::Reorder { order } => {
            let shifted: Vec<usize> = if has_self {
                std::iter::once(0)
                    .chain(order.iter().map(|position| position + shift))
                    .collect()
            } else {
                order.clone()
            };
            (callsites::Op::Reorder { order: shifted }, 0)
        }
    };
    let items = callsites::rewrite(&site.items, &decl_op, decl_index)?;
    let mut rewrites = vec![(
        decl_path.to_path_buf(),
        site.range.clone(),
        callsites::render(&items),
    )];

    let references = server.references(decl_path, position, budget)?;
    if references.is_empty() {
        bail!(
            "`{}` found no references to `{}`, so there is nothing to rewrite",
            step.recipe,
            step.from
        );
    }

    let mut texts: BTreeMap<PathBuf, String> = BTreeMap::new();
    texts.insert(decl_path.to_path_buf(), decl_text.to_string());
    let mut skipped = 0usize;

    for reference in references {
        let text = match texts.get(&reference.path) {
            Some(text) => text.clone(),
            None => {
                let text = std::fs::read_to_string(&reference.path)
                    .with_context(|| format!("reading {}", reference.path.display()))?;
                texts.insert(reference.path.clone(), text.clone());
                text
            }
        };
        let Some(reference_offset) =
            semantic::position_to_offset(&text, reference.range.start, encoding)
        else {
            skipped += 1;
            continue;
        };
        match callsites::call_site(&text, reference_offset) {
            Some(site) => {
                let index = match op {
                    callsites::Op::Delete { index } | callsites::Op::Add { index, .. } => *index,
                    callsites::Op::Reorder { .. } => 0,
                };
                let items = callsites::rewrite(&site.items, op, index)?;
                rewrites.push((
                    reference.path.clone(),
                    site.range.clone(),
                    callsites::render(&items),
                ));
            }
            None => skipped += 1,
        }
    }

    if skipped > 0 {
        eprintln!(
            "note: {skipped} reference(s) to `{}` were not call sites and were left alone",
            step.from
        );
    }
    Ok(rewrites)
}

/// Apply the planned list rewrites, one file at a time, rejecting overlaps
/// instead of corrupting a file when two rewrites disagree.
fn apply_rewrites(
    server: &mut Server,
    opened: &mut std::collections::HashSet<PathBuf>,
    recipe: &str,
    rewrites: Vec<(PathBuf, std::ops::Range<usize>, String)>,
    write: bool,
    outcome: &mut SemanticOutcome,
) -> Result<()> {
    let mut by_file: BTreeMap<PathBuf, Vec<(std::ops::Range<usize>, String)>> = BTreeMap::new();
    for (path, range, replacement) in rewrites {
        by_file.entry(path).or_default().push((range, replacement));
    }

    for (path, mut edits) in by_file {
        let original = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        edits.sort_by_key(|(range, _)| range.start);
        for pair in edits.windows(2) {
            if pair[0].0.end > pair[1].0.start {
                bail!(
                    "two rewrites overlap in {}; refusing to guess",
                    path.display()
                );
            }
        }

        let mut updated = original.clone();
        for (range, replacement) in edits.iter().rev() {
            updated.replace_range(range.clone(), replacement);
        }
        for (range, replacement) in &edits {
            let (line, column) = line_column(&original, range.start);
            outcome.findings.push(Finding {
                recipe: recipe.to_string(),
                path: path.clone(),
                line,
                column,
                before: original[range.clone()].to_string(),
                after: Some(replacement.clone()),
                message: String::new(),
            });
        }
        if updated != original {
            if write {
                std::fs::write(&path, &updated)
                    .with_context(|| format!("writing {}", path.display()))?;
                server.refresh(&path, &updated)?;
                opened.insert(path.clone());
            }
            outcome.writes.push((path, original, updated));
        }
    }
    Ok(())
}

fn line_column(text: &str, offset: usize) -> (usize, usize) {
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

/// Find the identifier to rename, by anchor text or by line number.
fn locate_symbol(text: &str, step: &SemanticStep, path: &Path) -> Result<(u32, u32)> {
    let (line_index, line) = if let Some(anchor) = &step.anchor {
        let index = text
            .lines()
            .position(|candidate| candidate.contains(anchor))
            .with_context(|| {
                format!(
                    "no line in {} contains the anchor `{anchor}`",
                    path.display()
                )
            })?;
        (index, text.lines().nth(index).unwrap_or_default())
    } else if let Some(number) = step.line {
        let index = number.saturating_sub(1) as usize;
        let line = text
            .lines()
            .nth(index)
            .with_context(|| format!("line {number} is past the end of {}", path.display()))?;
        (index, line)
    } else {
        bail!(
            "semantic step for `{}` has neither `anchor` nor `line`",
            path.display()
        );
    };

    let byte = line.find(&step.from).with_context(|| {
        format!(
            "`{}` is not on the located line of {}",
            step.from,
            path.display()
        )
    })?;
    Ok((line_index as u32, line[..byte].chars().count() as u32))
}

#[allow(clippy::too_many_arguments)]
fn run_recipes(
    catalog: &Catalog,
    root: &Path,
    selector: &str,
    apply: bool,
    verify_after: bool,
    with_tests: bool,
    allow_dirty: bool,
    show_diff: bool,
    options: &[String],
    wait_secs: u64,
) -> Result<()> {
    let resolved = resolve_selector(catalog, selector, options)?;
    let engine = Engine::compile(&resolved)?;
    let files = util::rust_files(root)?;

    if apply {
        if let Some(false) = util::git_clean(root) {
            if !allow_dirty {
                bail!(
                    "{}\n\nhas uncommitted changes. Commit or stash first, or pass --allow-dirty.\n\
                     A clean tree is the difference between a rollback and a lost afternoon.",
                    root.display()
                );
            }
        }
    }

    let snapshot = if apply {
        Some(verify::Snapshot::take(root)?)
    } else {
        None
    };

    let mut findings: Vec<Finding> = Vec::new();
    let mut writes: Vec<(PathBuf, String, String)> = Vec::new();
    for path in &files {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let (next, mut found) = engine.rewrite(&text, path, root)?;
        findings.append(&mut found);
        if next != text {
            writes.push((path.clone(), text, next));
        }
    }

    for (path, _, new) in &writes {
        std::fs::write(path, new).with_context(|| format!("writing {}", path.display()))?;
    }

    // Semantic steps run after structural edits are on disk, because the
    // language server resolves positions against files, not against our buffers.
    if engine.has_semantic_steps() {
        let outcome = run_semantic(root, engine.semantic_steps(), apply, wait_secs)?;
        findings.extend(outcome.findings);
        writes.extend(outcome.writes);
    }

    if show_diff || !apply {
        for (path, old, new) in &writes {
            print!(
                "{}",
                util::unified_diff(old, new, &path.display().to_string())
            );
        }
    }

    for finding in &findings {
        eprintln!("{}", finding.one_line(root));
    }

    let mut cargo_log = Vec::new();
    if (!writes.is_empty() || engine.has_cargo_steps()) && apply {
        cargo_log = engine.run_cargo_steps(root).inspect_err(|_err| {
            if let Some(snapshot) = &snapshot {
                let _ = snapshot.restore();
            }
        })?;
    }

    if !apply {
        println!();
        println!(
            "dry run: {} change(s) across {} file(s) for {selector}",
            findings.len(),
            writes.len()
        );
        println!("re-run with --apply to write, then the compile gate decides.");
        return Ok(());
    }
    println!();
    if writes.is_empty() {
        println!("no changes to write for {selector}");
    } else {
        println!(
            "applied {} change(s) across {} file(s) for {selector}",
            findings.len(),
            writes.len()
        );
    }
    for line in cargo_log {
        println!("  {line}");
    }

    if verify_after {
        println!("running the correctness gate...");
        let gates = verify::run(root, with_tests)?;
        let mut failed = Vec::new();
        for gate in &gates {
            println!(
                "  {:<8} {}",
                gate.name,
                if gate.ok { "ok" } else { "FAILED" }
            );
            if !gate.ok {
                failed.push(gate);
            }
        }
        if !failed.is_empty() {
            if let Some(snapshot) = &snapshot {
                snapshot.restore()?;
                println!("rolled back every change from this run.");
            }
            for gate in failed {
                eprintln!();
                eprintln!("--- {} output ---", gate.name);
                eprintln!("{}", gate.detail);
            }
            bail!(
                "the correctness gate rejected this rewrite, so nothing was kept. \
                 This is a recipe bug: report it with the file and the pattern that matched."
            );
        }
        println!("correctness gate passed.");
    } else {
        println!("compile gate skipped (--no-verify). Review the diff before committing.");
    }

    Ok(())
}

fn verify_only(root: &Path, with_tests: bool) -> Result<()> {
    let gates = verify::run(root, with_tests)?;
    let mut ok = true;
    for gate in &gates {
        println!("{:<8} {}", gate.name, if gate.ok { "ok" } else { "FAILED" });
        if !gate.ok {
            ok = false;
            eprintln!("{}", gate.detail);
        }
    }
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// `file:line:column`, 1-based. Split from the right so a Windows drive letter
/// in the path is not mistaken for a separator.
fn parse_location(at: &str) -> Result<(PathBuf, u32, u32)> {
    let mut parts = at.rsplitn(3, ':');
    let column = parts.next().and_then(|value| value.parse::<u32>().ok());
    let line = parts.next().and_then(|value| value.parse::<u32>().ok());
    let file = parts.next();
    match (file, line, column) {
        (Some(file), Some(line), Some(column)) if line >= 1 && column >= 1 && !file.is_empty() => {
            Ok((PathBuf::from(file), line, column))
        }
        _ => bail!("expected --at file:line:column with 1-based numbers, got `{at}`"),
    }
}

fn rename_symbol(
    root: &Path,
    at: &str,
    new_name: &str,
    apply: bool,
    verify_after: bool,
    allow_dirty: bool,
    wait_secs: u64,
) -> Result<()> {
    let (file, line, column) = parse_location(at)?;
    let root = util::normalize_path(root)?;
    let file = if file.is_absolute() {
        file
    } else {
        root.join(file)
    };
    let path = util::normalize_path(&file)?;
    if !path.starts_with(&root) {
        bail!(
            "{} is outside the workspace {}",
            path.display(),
            root.display()
        );
    }
    let original =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;

    if apply {
        if let Some(false) = util::git_clean(&root) {
            if !allow_dirty {
                bail!(
                    "{}\n\nhas uncommitted changes. Commit or stash first, or pass --allow-dirty.",
                    root.display()
                );
            }
        }
    }
    let snapshot = if apply {
        Some(verify::Snapshot::take(&root)?)
    } else {
        None
    };

    let mut server = Server::start(&root)?;
    let encoding = server.encoding();
    let position = semantic::char_position(&original, line - 1, column - 1, encoding)
        .with_context(|| format!("line {line} is past the end of {}", path.display()))?;

    server.did_open(&path, &original)?;
    let ready = server.wait_ready(std::time::Duration::from_secs(wait_secs));
    if !ready {
        eprintln!("note: no indexing progress reported; asking the analyzer directly");
    }

    let edits = server.rename(
        &path,
        position,
        new_name,
        std::time::Duration::from_secs(wait_secs),
    )?;
    server.shutdown();
    drop(server);

    if edits.is_empty() {
        bail!(
            "rust-analyzer declined to rename the symbol at {at}.\n\
             A trait implementation member cannot be renamed, and generated code is \
             often off limits too. Nothing was written."
        );
    }

    let mut total = 0usize;
    let mut writes: Vec<(PathBuf, String, String)> = Vec::new();
    for file_edits in &edits {
        let text = std::fs::read_to_string(&file_edits.path)
            .with_context(|| format!("reading {}", file_edits.path.display()))?;
        let updated = semantic::apply_edits(&text, &file_edits.edits, encoding)?;
        total += file_edits.edits.len();
        if updated != text {
            writes.push((file_edits.path.clone(), text, updated));
        }
    }

    for (path, old, new) in &writes {
        print!(
            "{}",
            util::unified_diff(old, new, &path.display().to_string())
        );
    }

    if !apply {
        println!();
        println!(
            "dry run: {total} edit(s) across {} file(s); re-run with --apply to write",
            writes.len()
        );
        return Ok(());
    }

    for (path, _, new) in &writes {
        std::fs::write(path, new).with_context(|| format!("writing {}", path.display()))?;
    }
    println!();
    println!(
        "renamed to `{new_name}`: {total} edit(s) across {} file(s)",
        writes.len()
    );

    if verify_after {
        println!("running the correctness gate...");
        let gates = verify::run(&root, false)?;
        let mut failed = Vec::new();
        for gate in &gates {
            println!(
                "  {:<8} {}",
                gate.name,
                if gate.ok { "ok" } else { "FAILED" }
            );
            if !gate.ok {
                failed.push(gate);
            }
        }
        if !failed.is_empty() {
            if let Some(snapshot) = &snapshot {
                snapshot.restore()?;
                println!("rolled back the rename.");
            }
            for gate in failed {
                eprintln!();
                eprintln!("--- {} output ---", gate.name);
                eprintln!("{}", gate.detail);
            }
            bail!("the rename did not compile, so it was rolled back.");
        }
        println!("correctness gate passed.");
    }

    Ok(())
}
