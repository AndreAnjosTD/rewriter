//! rustrewrite - recipe-driven, verified refactoring for Rust.
//!
//! The shape follows OpenRewrite: recipes are declarative catalog entries, a
//! recipe can compose other recipes, `check` reports without writing, `run`
//! rewrites, and a failing compile rolls the whole run back.

mod engine;
mod recipes;
mod util;
mod verify;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use engine::{apply_to_snippet, Engine, Finding};
use recipes::{Catalog, Mode, Resolved};
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
    },
    /// Run the compile (and optionally test) gate alone.
    Verify {
        /// Also run the test suite.
        #[arg(long)]
        test: bool,
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
        Command::Check { recipe, diff, json } => check(&catalog, &root, &recipe, diff, json),
        Command::Run {
            recipe,
            apply,
            no_verify,
            test,
            allow_dirty,
            diff,
        } => run_recipes(
            &catalog,
            &root,
            &recipe,
            apply,
            !no_verify,
            test,
            allow_dirty,
            diff,
        ),
        Command::Verify { test } => verify_only(&root, test),
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
        let source = if sources {
            format!("  ({})", recipe.source)
        } else {
            String::new()
        };
        println!(
            "{:<40} {:<10} risk:{:<7}{}{}{}",
            recipe.id,
            kind,
            recipe.risk.to_string(),
            "",
            tags,
            source
        );
        if !recipe.name.is_empty() {
            println!("    {}", recipe.name);
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
    } else {
        println!();
        println!("(no examples declared, so this recipe is unproven)");
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
) -> Result<()> {
    let resolved = resolve_selector(catalog, selector)?;
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
            println!("run with --apply to write (rustrewrite run --recipe {selector} --apply)");
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

fn resolve_selector(catalog: &Catalog, selector: &str) -> Result<Vec<Resolved>> {
    let selected = catalog.select(selector)?;
    let mut resolved = Vec::new();
    for recipe in selected {
        resolved.push(catalog.resolve(recipe)?);
    }
    if resolved.is_empty() {
        bail!("`{selector}` matched no recipes");
    }
    Ok(resolved)
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
) -> Result<()> {
    let resolved = resolve_selector(catalog, selector)?;
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
    if !writes.is_empty() || engine.has_cargo_steps() {
        if apply {
            cargo_log = engine.run_cargo_steps(root).map_err(|err| {
                if let Some(snapshot) = &snapshot {
                    let _ = snapshot.restore();
                }
                err
            })?;
        }
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
    println!(
        "applied {} change(s) across {} file(s) for {selector}",
        findings.len(),
        writes.len()
    );
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
