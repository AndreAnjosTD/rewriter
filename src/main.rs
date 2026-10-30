//! rustrewrite - recipe-driven, verified refactoring for Rust.
//!
//! The shape follows OpenRewrite: recipes are declarative catalog entries, a
//! recipe can compose other recipes, `check` reports without writing, `run`
//! rewrites, and a failing compile rolls the whole run back.

mod callsites;
mod daemon;
mod engine;
mod recipes;
mod report;
mod semantic;
mod signature;
mod util;
mod verify;

#[cfg(test)]
mod idempotency;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use engine::{apply_to_snippet, Engine, Finding, SemanticStep};
use recipes::{Catalog, Mode, Resolved, SemanticAction};
use semantic::{Analyzer, Server};
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
        /// Start a fresh analyzer instead of reusing the warm server.
        #[arg(long)]
        cold: bool,
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
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// Recipe option, e.g. --set file=src/main.rs. Repeatable.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        options: Vec<String>,
        /// Seconds to wait for the analyzer to index, for semantic recipes.
        #[arg(long, default_value_t = 180)]
        wait: u64,
        /// Start a fresh analyzer instead of reusing the warm server.
        #[arg(long)]
        cold: bool,
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
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// Seconds to wait for the analyzer to load and index the workspace.
        #[arg(long, default_value_t = 180)]
        wait: u64,
        /// Start a fresh analyzer instead of reusing the warm server.
        #[arg(long)]
        cold: bool,
    },
    /// Keep a warm analyzer alive for this workspace, so semantic runs skip the
    /// load.
    ///
    /// The server is shared by every `rustrewrite` command on this workspace and
    /// exits after `--idle` seconds without a request. Semantic commands start
    /// one automatically; this command exists to start it ahead of time, and to
    /// inspect or stop it.
    Serve {
        /// Stop the running server instead of starting one.
        #[arg(long)]
        stop: bool,
        /// Report on the running server instead of starting one.
        #[arg(long)]
        status: bool,
        /// Seconds of inactivity before the server exits. 0 keeps it alive.
        #[arg(long, default_value_t = 900)]
        idle: u64,
    },
}

fn main() {
    let cli = Cli::parse();
    // A failure must still land as JSON when the caller asked for JSON, or a
    // pipeline would silently lose the reason.
    let json = match &cli.command {
        Command::Check { json, .. } | Command::Run { json, .. } | Command::Rename { json, .. } => {
            *json
        }
        _ => false,
    };
    if let Err(err) = run(cli) {
        let message = format!("{err:#}");
        if json {
            println!("{}", report::error(&message));
        }
        eprintln!("rustrewrite: {message}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let root = absolute(&cli.path)?;
    // Loaded per command: `serve`, `verify` and `rename` do not read recipes, and
    // a warm server should not wait on a recipe scan it never uses.
    let catalog = || Catalog::load(&cli.recipes, &root);

    match cli.command {
        Command::List { tag, sources } => list(&catalog()?, tag.as_deref(), sources),
        Command::Explain { recipe } => explain(&catalog()?, &recipe),
        Command::Test { recipe } => test_examples(&catalog()?, recipe.as_deref()),
        Command::Check {
            recipe,
            diff,
            json,
            options,
            wait,
            cold,
        } => check(
            &catalog()?,
            &root,
            &recipe,
            diff,
            json,
            &options,
            AnalyzerOpts { wait, warm: !cold },
        ),
        Command::Run {
            recipe,
            apply,
            no_verify,
            test,
            allow_dirty,
            diff,
            json,
            options,
            wait,
            cold,
        } => run_recipes(
            &catalog()?,
            &root,
            &recipe,
            apply,
            !no_verify,
            test,
            allow_dirty,
            diff,
            json,
            &options,
            AnalyzerOpts { wait, warm: !cold },
        ),
        Command::Verify { test } => verify_only(&root, test),
        Command::Rename {
            at,
            to,
            apply,
            no_verify,
            allow_dirty,
            json,
            wait,
            cold,
        } => rename_symbol(
            &root,
            &at,
            &to,
            apply,
            !no_verify,
            allow_dirty,
            json,
            AnalyzerOpts { wait, warm: !cold },
        ),
        Command::Serve { stop, status, idle } => serve_command(&root, stop, status, idle),
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

/// One example's view of a recipe: the resolved steps with that example's
/// `options` applied, so a parameterized recipe is proven with real values
/// rather than with `${name}` placeholders.
fn resolve_example(resolved: &Resolved, example: &recipes::Example) -> Result<Resolved> {
    let mut one = resolved.clone();
    recipes::apply_options(&mut one, &example.options)?;
    Ok(one)
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
            let example_resolved = resolve_example(&resolved, example)
                .with_context(|| format!("example {}", index + 1))?;
            let got = apply_to_snippet(&example_resolved, &example.before)?;
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
        // cargo recipes have no snippet to rewrite. An insert step can be
        // proven on a snippet, but some insert recipes scope themselves with
        // globs, so only the ones carrying examples are exercised here.
        let structural = recipe
            .steps
            .iter()
            .any(|step| step.kind == recipes::StepKind::Structural);
        let testable_insert = !recipe.examples.is_empty()
            && recipe
                .steps
                .iter()
                .any(|step| step.kind == recipes::StepKind::Insert);
        if !structural && !testable_insert {
            continue;
        }
        let resolved = catalog.resolve(recipe)?;
        if resolved.recipe.examples.is_empty() {
            println!("{:40}  NO EXAMPLES", resolved.recipe.id);
            failed += 1;
            continue;
        }
        for (index, example) in resolved.recipe.examples.iter().enumerate() {
            let example_resolved = resolve_example(&resolved, example)
                .with_context(|| format!("example {}", index + 1))?;
            let got = apply_to_snippet(&example_resolved, &example.before)?;
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
    analyzer_opts: AnalyzerOpts,
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
        let outcome = run_semantic(root, engine.semantic_steps(), false, analyzer_opts)?;
        findings.extend(outcome.findings);
        changed_files += outcome.writes.len();
        pending.extend(outcome.writes);
    }

    if json {
        for finding in &findings {
            println!(
                "{}",
                report::FindingLine {
                    root,
                    finding,
                    recipe: Some(&finding.recipe),
                }
                .line()
            );
        }
    } else {
        for finding in &findings {
            println!("{}", finding.one_line(root));
        }
    }

    if show_diff {
        for (path, old, new) in &pending {
            human_raw(
                json,
                &util::unified_diff(old, new, &path.display().to_string()),
            );
        }
    }

    if json {
        println!(
            "{}",
            report::Summary {
                command: "check",
                recipe: Some(selector),
                findings: findings.len(),
                files: distinct_files(&findings),
                changed: changed_files > 0,
                applied: false,
                gates: None,
            }
            .line()
        );
    } else {
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

/// Human-facing text: stdout normally, stderr when stdout is reserved for JSON.
fn human(json: bool, text: impl std::fmt::Display) {
    if json {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
}

/// The same, for text that already carries its own line endings (diffs).
fn human_raw(json: bool, text: &str) {
    if json {
        eprint!("{text}");
    } else {
        print!("{text}");
    }
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

impl SemanticOutcome {
    /// Keep one pending write per file. Two steps over the same file should show
    /// one diff from the original text, not two overlapping ones.
    fn record(&mut self, path: PathBuf, before: String, after: String) {
        match self.writes.iter_mut().find(|(known, _, _)| *known == path) {
            Some(entry) => entry.2 = after,
            None if before != after => self.writes.push((path, before, after)),
            None => {}
        }
    }
}

/// How a command should talk to the analyzer.
#[derive(Debug, Clone, Copy)]
struct AnalyzerOpts {
    /// Seconds to wait for the analyzer to load and index the workspace.
    wait: u64,
    /// Reuse the warm server for this workspace instead of paying the load
    /// again.
    warm: bool,
}

/// Pick where semantic answers come from: a warm server when one is available,
/// otherwise a server that lives for this command only.
fn analyzer(root: &Path, opts: AnalyzerOpts) -> Result<Box<dyn Analyzer>> {
    if opts.warm {
        match daemon::ensure(root) {
            Ok(remote) => return Ok(Box::new(remote)),
            Err(err) => eprintln!(
                "note: warm server unavailable ({err:#}); starting a one-shot analyzer instead"
            ),
        }
    }
    Ok(Box::new(Server::start(root)?))
}

/// Run semantic steps against one analyzer, applying edits only when asked.
/// Edits are always reported, so `check` can show what would change.
fn run_semantic(
    root: &Path,
    steps: &[SemanticStep],
    write: bool,
    opts: AnalyzerOpts,
) -> Result<SemanticOutcome> {
    let mut outcome = SemanticOutcome {
        findings: Vec::new(),
        writes: Vec::new(),
    };
    if steps.is_empty() {
        // Nothing semantic to ask: do not start an analyzer at all. This is what
        // keeps a structural run at structural speed.
        return Ok(outcome);
    }

    // A change-return-type step edits the declaration without asking the server
    // anything, so the server starts only when a step actually needs it. Pending
    // text lives here until it is written, so a dry run composes the same way an
    // applied one does.
    let mut session: Option<Box<dyn Analyzer>> = None;
    let mut overlay: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut opened: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut ready = false;
    let budget = std::time::Duration::from_secs(opts.wait);

    for step in steps {
        let path = util::normalize_path(&root.join(&step.file))?;
        let text = read_source(&overlay, &path)?;
        let (offset, line, character) = locate_token(&text, step, &path)?;

        // Declaration edits need no server: they rewrite one token tree and
        // leave every caller to the compile gate. The server starts lazily only
        // when a later step actually needs it.
        if matches!(
            step.action,
            SemanticAction::ChangeReturnType | SemanticAction::ChangeVisibility
        ) {
            let (range, previous, replacement) = match step.action {
                SemanticAction::ChangeReturnType => {
                    let edit = signature::set_return_type(&text, offset, &step.from, &step.to)
                        .with_context(|| {
                            format!(
                                "changing the return type of `{}` in {}",
                                step.from,
                                step.file.display()
                            )
                        })?;
                    (edit.range, edit.previous, edit.replacement)
                }
                SemanticAction::ChangeVisibility => {
                    let edit = signature::set_visibility(&text, offset, &step.from, &step.to)
                        .with_context(|| {
                            format!(
                                "changing the visibility of `{}` in {}",
                                step.from,
                                step.file.display()
                            )
                        })?;
                    (edit.range, edit.previous, edit.replacement)
                }
                _ => unreachable!("the enclosing match limits the actions"),
            };
            let mut updated = text.clone();
            updated.replace_range(range.clone(), &replacement);
            if updated == text {
                continue;
            }
            let (found_line, found_column) = util::line_column(&text, range.start);
            outcome.findings.push(Finding {
                recipe: step.recipe.clone(),
                path: path.clone(),
                line: found_line,
                column: found_column,
                before: previous,
                after: Some(step.to.trim().to_string()),
                message: String::new(),
            });
            if write {
                std::fs::write(&path, &updated)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            if let Some(server) = session.as_mut() {
                open_or_refresh(server.as_mut(), &mut opened, &path, &updated)?;
            }
            overlay.insert(path.clone(), updated.clone());
            outcome.record(path, text, updated);
            continue;
        }

        if session.is_none() {
            session = Some(analyzer(root, opts)?);
        }
        let server = session.as_mut().expect("started above");
        let encoding = server.encoding();
        let position = semantic::char_position(&text, line, character, encoding)
            .with_context(|| format!("locating `{}` in {}", step.from, path.display()))?;

        open_or_refresh(server.as_mut(), &mut opened, &path, &text)?;
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
            SemanticAction::ChangeReturnType => unreachable!("handled above"),
            SemanticAction::ChangeVisibility => unreachable!("handled above"),
            _ => {
                let op = argument_op(step)?;
                let rewrites = plan_argument_rewrites(
                    server.as_mut(),
                    step,
                    &op,
                    &path,
                    &text,
                    position,
                    encoding,
                    budget,
                    &overlay,
                )?;
                apply_rewrites(
                    server.as_mut(),
                    &mut opened,
                    &step.recipe,
                    rewrites,
                    write,
                    &mut overlay,
                    &mut outcome,
                )?;
                continue;
            }
        };

        for file_edits in &edits {
            let current = read_source(&overlay, &file_edits.path)?;
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
            }
            open_or_refresh(server.as_mut(), &mut opened, &file_edits.path, &updated)?;
            overlay.insert(file_edits.path.clone(), updated.clone());
            outcome.record(file_edits.path.clone(), current, updated);
        }
    }

    if let Some(mut server) = session {
        server.shutdown();
    }
    Ok(outcome)
}

/// The file as this run currently sees it: pending text wins over disk so that
/// one semantic step resolves against what the previous one did.
fn read_source(overlay: &BTreeMap<PathBuf, String>, path: &Path) -> Result<String> {
    match overlay.get(path) {
        Some(text) => Ok(text.clone()),
        None => {
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
        }
    }
}

/// Keep the server's copy of an open document in step with the text this run is
/// working on, whether or not that text has reached disk yet.
fn open_or_refresh(
    server: &mut dyn Analyzer,
    opened: &mut std::collections::HashSet<PathBuf>,
    path: &Path,
    text: &str,
) -> Result<()> {
    if opened.contains(path) {
        server.refresh(path, text)
    } else {
        server.did_open(path, text)?;
        opened.insert(path.to_path_buf());
        Ok(())
    }
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
        SemanticAction::ChangeReturnType => {
            unreachable!("change-return-type is handled separately")
        }
        SemanticAction::ChangeVisibility => {
            unreachable!("change-visibility is handled separately")
        }
    }
}

/// Work out every list that has to change: the declaration's parameters and the
/// arguments of every resolved call site.
#[allow(clippy::too_many_arguments)]
fn plan_argument_rewrites(
    server: &mut dyn Analyzer,
    step: &SemanticStep,
    op: &callsites::Op,
    decl_path: &Path,
    decl_text: &str,
    position: semantic::Position,
    encoding: semantic::PositionEncoding,
    budget: std::time::Duration,
    overlay: &BTreeMap<PathBuf, String>,
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

    let mut texts: BTreeMap<PathBuf, String> = overlay.clone();
    texts.insert(decl_path.to_path_buf(), decl_text.to_string());
    let mut skipped = 0usize;

    for reference in references {
        let text = match texts.get(&reference.path) {
            Some(text) => text.clone(),
            None => {
                let text = read_source(overlay, &reference.path)?;
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
    server: &mut dyn Analyzer,
    opened: &mut std::collections::HashSet<PathBuf>,
    recipe: &str,
    rewrites: Vec<(PathBuf, std::ops::Range<usize>, String)>,
    write: bool,
    overlay: &mut BTreeMap<PathBuf, String>,
    outcome: &mut SemanticOutcome,
) -> Result<()> {
    let mut by_file: BTreeMap<PathBuf, Vec<(std::ops::Range<usize>, String)>> = BTreeMap::new();
    for (path, range, replacement) in rewrites {
        by_file.entry(path).or_default().push((range, replacement));
    }

    for (path, mut edits) in by_file {
        let original = read_source(overlay, &path)?;
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
            let (line, column) = util::line_column(&original, range.start);
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
            }
            open_or_refresh(server, opened, &path, &updated)?;
            overlay.insert(path.clone(), updated.clone());
            outcome.record(path, original, updated);
        }
    }
    Ok(())
}

/// Find the token to act on, by anchor text or by line number. Returns its byte
/// offset plus its 0-based line and character column: the server wants the
/// position, and the return-type edit wants the offset.
fn locate_token(text: &str, step: &SemanticStep, path: &Path) -> Result<(usize, u32, u32)> {
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
    let start = line_start(text, line_index).with_context(|| {
        format!(
            "line {} is past the end of {}",
            line_index + 1,
            path.display()
        )
    })?;
    Ok((
        start + byte,
        line_index as u32,
        line[..byte].chars().count() as u32,
    ))
}

fn line_start(text: &str, index: usize) -> Option<usize> {
    let mut start = 0usize;
    for _ in 0..index {
        start += text[start..].find('\n')? + 1;
    }
    Some(start)
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
    json: bool,
    options: &[String],
    analyzer_opts: AnalyzerOpts,
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

    // A dry run must leave the tree alone, so only an apply reaches disk. The
    // semantic steps below then resolve against the rewritten files, as they
    // must: the analyzer reads files, not our buffers.
    if apply {
        for (path, _, new) in &writes {
            std::fs::write(path, new).with_context(|| format!("writing {}", path.display()))?;
        }
    }

    // Semantic steps run after structural edits are on disk, because the
    // language server resolves positions against files, not against our buffers.
    if engine.has_semantic_steps() {
        let outcome = run_semantic(root, engine.semantic_steps(), apply, analyzer_opts)?;
        findings.extend(outcome.findings);
        writes.extend(outcome.writes);
    }

    if show_diff || !apply {
        for (path, old, new) in &writes {
            human_raw(
                json,
                &util::unified_diff(old, new, &path.display().to_string()),
            );
        }
    }

    if json {
        for finding in &findings {
            println!(
                "{}",
                report::FindingLine {
                    root,
                    finding,
                    recipe: Some(&finding.recipe),
                }
                .line()
            );
        }
    } else {
        // Kept on stderr in the human path too: the diff is the report, and the
        // one-line findings are the index to it.
        for finding in &findings {
            eprintln!("{}", finding.one_line(root));
        }
    }

    let mut cargo_log = Vec::new();
    if (!writes.is_empty() || engine.has_cargo_steps()) && apply {
        cargo_log = engine.run_cargo_steps(root).inspect_err(|_err| {
            if let Some(snapshot) = &snapshot {
                let _ = snapshot.restore();
            }
        })?;
    }

    let files = distinct_files(&findings);
    let changed = !writes.is_empty();

    if !apply {
        human(json, "");
        human(
            json,
            format!(
                "dry run: {} change(s) across {} file(s) for {selector}",
                findings.len(),
                writes.len()
            ),
        );
        human(
            json,
            "re-run with --apply to write, then the compile gate decides.",
        );
        if json {
            println!(
                "{}",
                report::Summary {
                    command: "run",
                    recipe: Some(selector),
                    findings: findings.len(),
                    files,
                    changed,
                    applied: false,
                    gates: None,
                }
                .line()
            );
        }
        return Ok(());
    }
    human(json, "");
    if writes.is_empty() {
        human(json, format!("no changes to write for {selector}"));
    } else {
        human(
            json,
            format!(
                "applied {} change(s) across {} file(s) for {selector}",
                findings.len(),
                writes.len()
            ),
        );
    }
    for line in cargo_log {
        human(json, format!("  {line}"));
    }

    let mut gates = None;
    if verify_after {
        human(json, "running the correctness gate...");
        let results = verify::run(root, with_tests)?;
        let mut failed = Vec::new();
        for gate in &results {
            human(
                json,
                format!(
                    "  {:<8} {}",
                    gate.name,
                    if gate.ok { "ok" } else { "FAILED" }
                ),
            );
            if !gate.ok {
                failed.push(gate);
            }
        }
        if !failed.is_empty() {
            if let Some(snapshot) = &snapshot {
                snapshot.restore()?;
                human(json, "rolled back every change from this run.");
            }
            for gate in failed {
                eprintln!();
                eprintln!("--- {} output ---", gate.name);
                eprintln!("{}", gate.detail);
            }
            bail!(
                "the correctness gate rejected this rewrite, so nothing was kept.\n\
                 Either a rule is too broad, or the change needs edits this recipe does not \
                 make: a caller, an implementation, or an import. A recipe that edits one \
                 declaration, such as change-return-type or change-type, fails here when its \
                 callers still expect the old shape."
            );
        }
        human(json, "correctness gate passed.");
        gates = Some(results);
    } else {
        human(
            json,
            "compile gate skipped (--no-verify). Review the diff before committing.",
        );
    }

    // A summary is the last line on success; on failure the error line main
    // writes is, so a consumer always terminates on a typed object.
    if json {
        println!(
            "{}",
            report::Summary {
                command: "run",
                recipe: Some(selector),
                findings: findings.len(),
                files,
                changed,
                applied: true,
                gates: gates.as_deref(),
            }
            .line()
        );
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

#[allow(clippy::too_many_arguments)]
fn rename_symbol(
    root: &Path,
    at: &str,
    new_name: &str,
    apply: bool,
    verify_after: bool,
    allow_dirty: bool,
    json: bool,
    opts: AnalyzerOpts,
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

    let mut server = analyzer(&root, opts)?;
    let encoding = server.encoding();
    let position = semantic::char_position(&original, line - 1, column - 1, encoding)
        .with_context(|| format!("line {line} is past the end of {}", path.display()))?;

    server.did_open(&path, &original)?;
    let ready = server.wait_ready(std::time::Duration::from_secs(opts.wait));
    if !ready {
        eprintln!("note: no indexing progress reported; asking the analyzer directly");
    }

    let edits = server.rename(
        &path,
        position,
        new_name,
        std::time::Duration::from_secs(opts.wait),
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

    let mut findings: Vec<Finding> = Vec::new();
    let mut writes: Vec<(PathBuf, String, String)> = Vec::new();
    for file_edits in &edits {
        let text = std::fs::read_to_string(&file_edits.path)
            .with_context(|| format!("reading {}", file_edits.path.display()))?;
        let updated = semantic::apply_edits(&text, &file_edits.edits, encoding)?;
        for edit in &file_edits.edits {
            // Report the text actually replaced, not the token that anchored
            // the rename; one rename can touch several unrelated spans.
            let replaced = semantic::position_to_offset(&text, edit.range.start, encoding)
                .zip(semantic::position_to_offset(
                    &text,
                    edit.range.end,
                    encoding,
                ))
                .map(|(start, end)| text[start..end].to_string())
                .unwrap_or_default();
            findings.push(Finding {
                recipe: String::new(),
                path: file_edits.path.clone(),
                line: edit.range.start.line as usize + 1,
                column: edit.range.start.character as usize + 1,
                before: replaced,
                after: Some(edit.new_text.clone()),
                message: String::new(),
            });
        }
        if updated != text {
            writes.push((file_edits.path.clone(), text, updated));
        }
    }

    if json {
        // Rename is positional, so its findings carry no recipe.
        for finding in &findings {
            println!(
                "{}",
                report::FindingLine {
                    root: &root,
                    finding,
                    recipe: None,
                }
                .line()
            );
        }
    }

    for (path, old, new) in &writes {
        human_raw(
            json,
            &util::unified_diff(old, new, &path.display().to_string()),
        );
    }

    let files = distinct_files(&findings);
    let changed = !writes.is_empty();

    if !apply {
        human(json, "");
        human(
            json,
            format!(
                "dry run: {} edit(s) across {} file(s); re-run with --apply to write",
                findings.len(),
                writes.len()
            ),
        );
        if json {
            println!(
                "{}",
                report::Summary {
                    command: "rename",
                    recipe: None,
                    findings: findings.len(),
                    files,
                    changed,
                    applied: false,
                    gates: None,
                }
                .line()
            );
        }
        return Ok(());
    }

    for (path, _, new) in &writes {
        std::fs::write(path, new).with_context(|| format!("writing {}", path.display()))?;
    }
    human(json, "");
    human(
        json,
        format!(
            "renamed to `{new_name}`: {} edit(s) across {} file(s)",
            findings.len(),
            writes.len()
        ),
    );

    let mut gates = None;
    if verify_after {
        human(json, "running the correctness gate...");
        let results = verify::run(&root, false)?;
        let mut failed = Vec::new();
        for gate in &results {
            human(
                json,
                format!(
                    "  {:<8} {}",
                    gate.name,
                    if gate.ok { "ok" } else { "FAILED" }
                ),
            );
            if !gate.ok {
                failed.push(gate);
            }
        }
        if !failed.is_empty() {
            if let Some(snapshot) = &snapshot {
                snapshot.restore()?;
                human(json, "rolled back the rename.");
            }
            for gate in failed {
                eprintln!();
                eprintln!("--- {} output ---", gate.name);
                eprintln!("{}", gate.detail);
            }
            bail!("the rename did not compile, so it was rolled back.");
        }
        human(json, "correctness gate passed.");
        gates = Some(results);
    }

    if json {
        println!(
            "{}",
            report::Summary {
                command: "rename",
                recipe: None,
                findings: findings.len(),
                files,
                changed,
                applied: true,
                gates: gates.as_deref(),
            }
            .line()
        );
    }

    Ok(())
}

fn serve_command(root: &Path, stop: bool, status: bool, idle_secs: u64) -> Result<()> {
    if stop {
        if daemon::stop(root)? {
            println!("stopped the warm server for {}", root.display());
        } else {
            println!("no warm server is running for {}", root.display());
        }
        return Ok(());
    }
    if status {
        match daemon::status(root)? {
            Some(status) => {
                println!("warm server pid {} for {}", status.pid, root.display());
                println!(
                    "  indexed    {}",
                    if status.ready { "yes" } else { "not yet" }
                );
                println!("  requests   {}", status.queries);
                println!("  idle for   {}s", status.idle_secs);
            }
            None => println!("no warm server is running for {}", root.display()),
        }
        return Ok(());
    }
    daemon::serve(root, std::time::Duration::from_secs(idle_secs))
}
