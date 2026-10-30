//! Application state and layout.
//!
//! The UI never touches the filesystem or a child process itself: a job runs
//! the CLI on background threads, and this module only turns the resulting
//! records into state that the next frame draws.

use crate::cli::{self, Action, Event, Job, JobSpec};
use crate::model::{Diff, Finding, Recipe, Record, Summary};
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_LOG_LINES: usize = 4000;

const ERROR_BACKGROUND: Color32 = Color32::from_rgb(96, 28, 28);
const ERROR_TEXT: Color32 = Color32::from_rgb(255, 214, 214);
const ADDED: Color32 = Color32::from_rgb(124, 214, 142);
const REMOVED: Color32 = Color32::from_rgb(244, 138, 138);
const HUNK: Color32 = Color32::from_rgb(122, 176, 244);

#[derive(Debug, Default)]
struct Flags {
    test: bool,
    no_verify: bool,
    allow_dirty: bool,
    /// On by default: reusing the warm analyzer is the fast path. Turning it
    /// off passes `--cold`.
    warm: bool,
}

/// The selectors and option values one action hands to the CLI.
#[derive(Debug, Default, PartialEq, Eq)]
struct JobPlan {
    recipes: Vec<String>,
    sets: Vec<(String, String)>,
}

pub struct RustrewriteApp {
    ctx: egui::Context,
    binary: String,
    root: String,
    search: String,
    recipes: Vec<Recipe>,
    /// The inspected recipe: whose detail and options the window shows. It is
    /// not the run target while anything is checked.
    selected: Option<usize>,
    /// Ids of the recipes an action runs. Ids, not indices, so a catalog reload
    /// cannot point the set at the wrong recipe. Catalog order is argv order.
    checked: BTreeSet<String>,
    /// Typed option values per recipe id, so inspecting or checking another
    /// recipe does not throw away what was typed.
    option_values: BTreeMap<String, BTreeMap<String, String>>,
    flags: Flags,
    settings_open: bool,

    job: Option<Job>,
    cancelled: bool,
    findings: Vec<Finding>,
    diffs: Vec<Diff>,
    selected_diff: Option<String>,
    log: VecDeque<String>,
    summary: Option<Summary>,
    error: Option<String>,
    status: String,
}

impl RustrewriteApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            ctx: cc.egui_ctx.clone(),
            binary: cli::DEFAULT_BINARY.to_string(),
            root: std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            search: String::new(),
            recipes: Vec::new(),
            selected: None,
            checked: BTreeSet::new(),
            option_values: BTreeMap::new(),
            flags: Flags {
                warm: true,
                ..Flags::default()
            },
            settings_open: false,
            job: None,
            cancelled: false,
            findings: Vec::new(),
            diffs: Vec::new(),
            selected_diff: None,
            log: VecDeque::new(),
            summary: None,
            error: None,
            status: String::new(),
        }
    }

    fn selected_recipe(&self) -> Option<&Recipe> {
        self.selected.and_then(|index| self.recipes.get(index))
    }

    /// The checked recipes in catalog order, which is also argv order.
    fn checked_recipes(&self) -> Vec<&Recipe> {
        self.recipes
            .iter()
            .filter(|recipe| self.checked.contains(&recipe.id))
            .collect()
    }

    fn is_batch(&self) -> bool {
        self.checked.len() > 1
    }

    /// The recipe a single-recipe action runs: the checked one when exactly one
    /// is checked, otherwise the inspected one.
    fn single_target(&self) -> Option<&Recipe> {
        match self.checked.len() {
            0 => self.selected_recipe(),
            1 => self.checked_recipes().first().copied(),
            _ => None,
        }
    }

    fn select_recipe(&mut self, index: usize) {
        self.selected = Some(index);
        if let Some(recipe) = self.recipes.get(index).cloned() {
            self.option_values_for(&recipe);
        }
        self.reset_results();
        self.status.clear();
    }

    /// Check or uncheck one recipe. Only the checked set changes: inspection
    /// stays where the user left it.
    fn set_checked(&mut self, id: &str, checked: bool) {
        if checked {
            self.checked.insert(id.to_string());
        } else {
            self.checked.remove(id);
        }
    }

    /// Check every eligible recipe the search currently shows. Recipes that
    /// need a required option are left out, visibly, because one batch cannot
    /// carry their `--set` values.
    fn select_all_shown(&mut self) {
        let query = self.search.clone();
        let ids: Vec<String> = self
            .recipes
            .iter()
            .filter(|recipe| recipe.matches(&query) && !recipe.needs_sets())
            .map(|recipe| recipe.id.clone())
            .collect();
        self.checked.extend(ids);
    }

    /// The stored option values for one recipe, seeded from its declared
    /// defaults the first time it is opened.
    fn option_values_for(&mut self, recipe: &Recipe) -> &mut BTreeMap<String, String> {
        let defaults = recipe.initial_values();
        self.option_values
            .entry(recipe.id.clone())
            .or_insert(defaults)
    }

    fn reset_results(&mut self) {
        self.findings.clear();
        self.diffs.clear();
        self.selected_diff = None;
        self.log.clear();
        self.summary = None;
        self.error = None;
    }

    /// Option values the CLI should receive as `--set key=value`. A required
    /// option left blank stops the run here, where the message can name it.
    fn collect_sets(&self, recipe: &Recipe) -> Result<Vec<(String, String)>, String> {
        let values = self.option_values.get(&recipe.id);
        let mut sets = Vec::new();
        for (name, option) in &recipe.options {
            let value = values
                .and_then(|values| values.get(name))
                .cloned()
                .unwrap_or_else(|| option.default_text());
            if option.required && value.trim().is_empty() {
                return Err(format!("Option `{name}` is required."));
            }
            if !value.is_empty() {
                sets.push((name.clone(), value));
            }
        }
        Ok(sets)
    }

    /// What one action sends: the selectors, one per recipe, and the option
    /// values of a single-recipe run. Nothing checked means the inspected
    /// recipe; one checked means that recipe; several is a batch, which carries
    /// selectors only because one `--set` map cannot belong to many recipes.
    fn plan_job(&self, action: Action) -> Result<JobPlan, String> {
        if action == Action::List {
            return Ok(JobPlan::default());
        }
        let checked = self.checked_recipes();
        match checked.len() {
            0 => {
                let Some(recipe) = self.selected_recipe() else {
                    return Err("Select a recipe first.".to_string());
                };
                Ok(JobPlan {
                    recipes: vec![recipe.id.clone()],
                    sets: self.collect_sets(recipe)?,
                })
            }
            1 => {
                let recipe = checked[0];
                Ok(JobPlan {
                    recipes: vec![recipe.id.clone()],
                    sets: self.collect_sets(recipe)?,
                })
            }
            _ => {
                if let Some(blocker) = cli::batch_blocker(&checked) {
                    return Err(blocker.message());
                }
                Ok(JobPlan {
                    recipes: checked.iter().map(|recipe| recipe.id.clone()).collect(),
                    sets: Vec::new(),
                })
            }
        }
    }

    fn start_job(&mut self, action: Action) {
        if self.job.is_some() {
            return;
        }
        let root = self.root.trim().to_string();
        if root.is_empty() {
            self.error = Some("Choose a workspace root first.".to_string());
            return;
        }
        let plan = match self.plan_job(action) {
            Ok(plan) => plan,
            Err(message) => {
                self.error = Some(message);
                return;
            }
        };
        let count = plan.recipes.len();

        let spec = JobSpec {
            binary: self.binary.clone(),
            root,
            action,
            recipes: plan.recipes,
            sets: plan.sets,
            test: self.flags.test,
            no_verify: self.flags.no_verify,
            allow_dirty: self.flags.allow_dirty,
            warm: self.flags.warm,
        };
        let ctx = self.ctx.clone();
        match cli::spawn(&spec, move || ctx.request_repaint()) {
            Ok(job) => {
                self.job = Some(job);
                self.cancelled = false;
                self.reset_results();
                self.status = match action {
                    Action::List => "Loading catalog…".to_string(),
                    Action::Check if count > 1 => format!("Checking {count} recipes…"),
                    Action::Apply if count > 1 => {
                        format!("Applying {count} recipes and verifying…")
                    }
                    Action::Check => "Checking…".to_string(),
                    Action::Apply => "Applying and verifying…".to_string(),
                };
            }
            Err(message) => self.error = Some(message),
        }
    }

    fn cancel_job(&mut self) {
        if let Some(job) = &self.job {
            job.cancel();
            self.cancelled = true;
            self.status = "Cancelling…".to_string();
        }
    }

    fn drain_events(&mut self) {
        let mut events = Vec::new();
        if let Some(job) = &self.job {
            while let Ok(event) = job.receiver.try_recv() {
                events.push(event);
            }
        }

        let mut finished = None;
        for event in events {
            match event {
                Event::Record(record) => self.apply_record(record),
                Event::Log(line) => self.push_log(line),
                Event::Finished { success, code } => finished = Some((success, code)),
            }
        }

        let Some((success, code)) = finished else {
            return;
        };
        let kind = self.job.as_ref().map(|job| job.kind);
        self.job = None;

        if self.cancelled {
            self.status = "Cancelled.".to_string();
            return;
        }
        // `check` exits 1 when it has findings. That is a completed check with
        // a summary, not a failure.
        if self.error.is_some() {
            self.status = "Failed.".to_string();
        } else if success || self.summary.is_some() {
            self.status = match kind {
                Some(Action::List) => format!("Loaded {} recipes.", self.recipes.len()),
                Some(Action::Check) => "Check complete.".to_string(),
                Some(Action::Apply) => "Apply complete.".to_string(),
                None => "Complete.".to_string(),
            };
        } else {
            self.error = Some(self.exit_message(code));
            self.status = "Failed.".to_string();
        }
    }

    /// A non-zero exit with neither an `error` record nor a summary. The CLI
    /// already put the reason on stderr, so prefer its last line to a bare
    /// status code.
    fn exit_message(&self, code: Option<i32>) -> String {
        let detail = self
            .log
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| !line.is_empty());
        match (code, detail) {
            (Some(code), Some(detail)) => {
                format!("rewriter exited with status {code}: {detail}")
            }
            (Some(code), None) => format!("rewriter exited with status {code}."),
            (None, Some(detail)) => format!("rewriter stopped: {detail}"),
            (None, None) => "rewriter stopped without an exit status.".to_string(),
        }
    }

    fn apply_record(&mut self, record: Record) {
        match record {
            Record::Catalog(recipes) => {
                let previous = self.selected_recipe().map(|recipe| recipe.id.clone());
                self.recipes = recipes;
                self.selected =
                    previous.and_then(|id| self.recipes.iter().position(|recipe| recipe.id == id));
                // Ids that the new catalog no longer lists cannot be checked or
                // carry options any more.
                self.checked
                    .retain(|id| self.recipes.iter().any(|recipe| &recipe.id == id));
                self.option_values
                    .retain(|id, _| self.recipes.iter().any(|recipe| &recipe.id == id));
                if let Some(recipe) = self.selected_recipe().cloned() {
                    self.option_values_for(&recipe);
                }
            }
            Record::Finding(finding) => self.findings.push(finding),
            Record::Diff(diff) => {
                if self.selected_diff.is_none() {
                    self.selected_diff = Some(diff.file.clone());
                }
                self.diffs.push(diff);
            }
            Record::Summary(summary) => self.summary = Some(summary),
            Record::Error(message) => {
                self.error = Some(message);
                self.status = "Failed.".to_string();
            }
        }
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() >= MAX_LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Workspace");
            ui.add(
                egui::TextEdit::singleline(&mut self.root)
                    .desired_width(320.0)
                    .hint_text("path to a Rust crate or workspace"),
            );
            if ui.button("Browse…").clicked() {
                let start = self.root.trim().to_string();
                let mut dialog = rfd::FileDialog::new();
                if !start.is_empty() {
                    dialog = dialog.set_directory(start);
                }
                if let Some(folder) = dialog.pick_folder() {
                    self.root = folder.display().to_string();
                }
            }
            if ui
                .add_enabled(self.job.is_none(), egui::Button::new("Load catalog"))
                .clicked()
            {
                self.start_job(Action::List);
            }
            ui.separator();
            ui.label("Search");
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .desired_width(200.0)
                    .hint_text("id, name, tag"),
            );
            if !self.search.is_empty()
                && ui
                    .small_button("✕")
                    .on_hover_text("Clear the search")
                    .clicked()
            {
                self.search.clear();
            }
            ui.separator();
            let idle = self.job.is_none();
            let query = self.search.clone();
            let selectable = self.recipes.iter().any(|recipe| {
                recipe.matches(&query) && !recipe.needs_sets() && !self.checked.contains(&recipe.id)
            });
            if ui
                .add_enabled(selectable && idle, egui::Button::new("Select all shown"))
                .on_hover_text(
                    "Check every recipe the search shows. Recipes that need --set are skipped, \
                     because a batch cannot carry their options.",
                )
                .clicked()
            {
                self.select_all_shown();
            }
            if ui
                .add_enabled(!self.checked.is_empty() && idle, egui::Button::new("Clear"))
                .on_hover_text("Uncheck every recipe.")
                .clicked()
            {
                self.checked.clear();
            }
            if !self.checked.is_empty() {
                ui.label(
                    RichText::new(format!("{} checked", self.checked.len()))
                        .small()
                        .weak(),
                );
            }
            ui.separator();
            if ui.button("Settings").clicked() {
                self.settings_open = !self.settings_open;
            }
        });
    }

    fn recipe_list(&mut self, ui: &mut egui::Ui) {
        let query = self.search.clone();
        let selected = self.selected;
        let busy = self.job.is_some();
        let mut pick = None;
        let mut toggle = None;

        ui.horizontal(|ui| {
            ui.heading("Recipes");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // The catalog is filtered by the project's language, so a single
                // language here names the workspace; a mixed one stays quiet.
                let languages: BTreeSet<&str> = self
                    .recipes
                    .iter()
                    .filter_map(|recipe| recipe.language.as_deref())
                    .collect();
                let label = if languages.len() == 1 {
                    format!(
                        "{} loaded · {}",
                        self.recipes.len(),
                        languages.iter().next().copied().unwrap_or_default()
                    )
                } else {
                    format!("{} loaded", self.recipes.len())
                };
                ui.label(RichText::new(label).small().weak());
            });
        });
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Switching recipes mid-run would let the old job's results
                // land under the new selection.
                ui.add_enabled_ui(!busy, |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    let mut shown = 0usize;
                    for (index, recipe) in self.recipes.iter().enumerate() {
                        if !recipe.matches(&query) {
                            continue;
                        }
                        shown += 1;
                        let risk = if recipe.risk.is_empty() {
                            "unknown"
                        } else {
                            &recipe.risk
                        };
                        let kind = if recipe.kind.is_empty() {
                            "?"
                        } else {
                            &recipe.kind
                        };
                        let text = format!(
                            "{}\n{}  ·  {}  ·  risk {}",
                            recipe.name, recipe.id, kind, risk
                        );
                        let required = recipe.required_options();
                        let mut hover = recipe.description.clone();
                        if !required.is_empty() {
                            if !hover.is_empty() {
                                hover.push('\n');
                            }
                            hover.push_str(&format!(
                                "Needs --set {}: a batch shares one option map, so this recipe can only run on its own.",
                                required.join(", ")
                            ));
                        }
                        ui.horizontal(|ui| {
                            // The checkbox picks what the actions run; the label
                            // picks what the detail pane shows. Neither one moves
                            // the other.
                            let mut checked = self.checked.contains(&recipe.id);
                            let box_response = ui.add_enabled(
                                required.is_empty(),
                                egui::Checkbox::without_text(&mut checked),
                            );
                            let box_response = if required.is_empty() {
                                box_response.on_hover_text("Include this recipe in the batch")
                            } else {
                                box_response.on_disabled_hover_text(format!(
                                    "This recipe needs --set {}; it can only run on its own.",
                                    required.join(", ")
                                ))
                            };
                            if box_response.changed() {
                                toggle = Some((recipe.id.clone(), checked));
                            }

                            let mut response =
                                ui.selectable_label(selected == Some(index), text);
                            if !hover.is_empty() {
                                response = response.on_hover_text(hover);
                            }
                            if response.clicked() {
                                pick = Some(index);
                            }
                            if !required.is_empty() {
                                ui.label(RichText::new("needs --set").small().weak())
                                    .on_hover_text(format!(
                                        "Options are single-recipe only: fill {} on its own.",
                                        required.join(", ")
                                    ));
                            }
                        });
                    }
                    if shown == 0 {
                        ui.add_space(8.0);
                        ui.label(if self.recipes.is_empty() {
                            "No catalog yet. Choose a workspace and press Load catalog."
                        } else {
                            "No recipes match the search."
                        });
                    }
                });
            });

        if let Some(index) = pick {
            self.select_recipe(index);
        }
        if let Some((id, checked)) = toggle {
            self.set_checked(&id, checked);
        }
    }

    fn detail_ui(&mut self, ui: &mut egui::Ui) {
        let recipe = self.selected_recipe().cloned();
        match &recipe {
            Some(recipe) => {
                ui.heading(&recipe.name);
                ui.horizontal_wrapped(|ui| {
                    ui.monospace(&recipe.id);
                    ui.separator();
                    ui.label(format!(
                        "kind: {}",
                        if recipe.kind.is_empty() {
                            "unknown"
                        } else {
                            &recipe.kind
                        }
                    ));
                    if let Some(language) = &recipe.language {
                        ui.separator();
                        ui.label(format!("language: {language}"));
                    }
                    ui.separator();
                    ui.colored_label(
                        risk_color(&recipe.risk),
                        format!(
                            "risk: {}",
                            if recipe.risk.is_empty() {
                                "unknown"
                            } else {
                                &recipe.risk
                            }
                        ),
                    );
                });
                if !recipe.description.is_empty() {
                    ui.add_space(4.0);
                    ui.label(&recipe.description);
                }
                if !recipe.tags.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for tag in &recipe.tags {
                            ui.label(RichText::new(format!("#{tag}")).small().weak());
                        }
                    });
                }

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                self.options_ui(ui, recipe);
            }
            None => {
                ui.heading("No recipe selected");
                ui.label("Load the catalog, then pick a recipe on the left.");
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        self.actions_ui(ui);
    }

    /// The option fields. They belong to one recipe, so a batch hides them and
    /// says why; a single run shows the options of the recipe it will use.
    fn options_ui(&mut self, ui: &mut egui::Ui, inspected: &Recipe) {
        if self.is_batch() {
            let checked = self.checked_recipes();
            let ids = checked
                .iter()
                .map(|recipe| recipe.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            ui.label(
                RichText::new(format!(
                    "{} recipes checked: they run in one pass, and options are single-recipe only.",
                    checked.len()
                ))
                .strong(),
            );
            ui.label(RichText::new(truncate(&ids, 120)).small().weak())
                .on_hover_text(&ids);
            ui.label(
                RichText::new("Uncheck all but one recipe to set its options.")
                    .small()
                    .weak(),
            );
            return;
        }

        let Some(target) = self.single_target().cloned() else {
            return;
        };
        if target.id != inspected.id {
            ui.label(format!(
                "The run uses the checked recipe `{}`; its options are below.",
                target.id
            ));
            ui.add_space(4.0);
        }
        if target.options.is_empty() {
            ui.label("This recipe takes no options.");
            return;
        }

        let slot = self.option_values_for(&target);
        egui::Grid::new("recipe_options")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                for (name, option) in &target.options {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(name).monospace());
                        if option.required {
                            ui.colored_label(Color32::from_rgb(232, 168, 78), "*")
                                .on_hover_text("Required");
                        }
                    });
                    let mut value = slot
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| option.default_text());
                    ui.vertical(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut value)
                                .desired_width(380.0)
                                .hint_text(option.default_text()),
                        );
                        if !option.description.is_empty() {
                            ui.label(RichText::new(&option.description).small().weak());
                        }
                    });
                    slot.insert(name.clone(), value);
                    ui.end_row();
                }
            });
    }

    fn actions_ui(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        let checked = self.checked.len();
        let can_run = !busy && (checked > 0 || self.selected.is_some());
        let check_label = if checked > 1 {
            format!("Check {checked} recipes")
        } else {
            "Check".to_string()
        };
        let apply_label = if checked > 1 {
            format!("Apply {checked} recipes")
        } else {
            "Apply".to_string()
        };
        let check_hover = if checked > 1 {
            "Report what would change for every checked recipe, in one run. Never writes."
                .to_string()
        } else {
            "Report what would change. Never writes.".to_string()
        };
        let apply_hover = if checked > 1 {
            format!("Rewrite all {checked} checked recipes in one run, then gate once at the end.")
        } else {
            "Rewrite, then run the correctness gate.".to_string()
        };

        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(can_run, egui::Button::new(check_label))
                .on_hover_text(check_hover)
                .clicked()
            {
                self.start_job(Action::Check);
            }
            if ui
                .add_enabled(can_run, egui::Button::new(apply_label))
                .on_hover_text(apply_hover)
                .clicked()
            {
                self.start_job(Action::Apply);
            }
            if busy {
                ui.spinner();
                if ui.button("Cancel").clicked() {
                    self.cancel_job();
                }
            }
            ui.separator();
            ui.add_enabled_ui(!busy, |ui| {
                ui.checkbox(&mut self.flags.test, "Run tests")
                    .on_hover_text("Pass --test to Apply: run the test suite after rewriting.");
                ui.checkbox(&mut self.flags.no_verify, "Skip verification")
                    .on_hover_text(
                        "Pass --no-verify to Apply. The gate is what makes a rewrite safe.",
                    );
                ui.checkbox(&mut self.flags.allow_dirty, "Allow uncommitted changes")
                    .on_hover_text("Pass --allow-dirty to Apply.");
                ui.checkbox(&mut self.flags.warm, "Reuse warm server")
                    .on_hover_text("Keep the analyzer warm between runs. Off passes --cold.");
            });
        });
    }

    fn results_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if self.job.is_some() {
                ui.spinner();
            }
            if !self.status.is_empty() {
                ui.label(&self.status);
            }
            if let Some(summary) = &self.summary {
                ui.separator();
                ui.label(summary_text(summary));
                if let Some(gates) = &summary.gates {
                    for gate in gates {
                        let (label, color) = if gate.ok {
                            ("pass", ADDED)
                        } else {
                            ("FAIL", REMOVED)
                        };
                        ui.separator();
                        ui.colored_label(color, format!("{}: {label}", gate.name));
                    }
                }
            }
        });

        if let Some(message) = self.error.clone() {
            error_banner(ui, &message);
        }

        if !self.log.is_empty() {
            egui::CollapsingHeader::new(format!("Log ({} lines)", self.log.len()))
                .id_salt("stderr_log")
                .default_open(false)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("stderr_scroll")
                        .max_height(120.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &self.log {
                                ui.label(RichText::new(line).monospace());
                            }
                        });
                });
        }

        if self.findings.is_empty() && self.diffs.is_empty() {
            if self.error.is_none() && self.summary.is_none() && self.job.is_none() {
                ui.add_space(4.0);
                ui.label("Run Check or Apply to see results.");
            }
            // Fill the panel so a resizable results area keeps its size when
            // there is nothing to show yet.
            ui.allocate_space(ui.available_size());
            return;
        }

        ui.add_space(4.0);
        let height = (ui.available_height() - 4.0).max(80.0);
        ui.columns(2, |columns| {
            // A resizable table column can be dragged wider than its pane. egui
            // paints children outside their parent's rect unless something
            // clips them, which is how the findings table ended up drawn over
            // the diff. Clip each pane to its own rect so that cannot happen.
            for column in columns.iter_mut() {
                column.set_clip_rect(column.max_rect());
            }
            self.findings_table(&mut columns[0], height);
            self.diff_view(&mut columns[1], height);
        });
        // A resizable panel is sized by its content, so the content has to claim
        // the whole panel. Without this the two panes take only as much height
        // as their rows need, the panel shrinks to fit on the next frame, and
        // the divider walks down the window on every repaint.
        ui.take_available_space();
    }

    fn findings_table(&mut self, ui: &mut egui::Ui, height: f32) {
        ui.label(RichText::new(format!("Findings ({})", self.findings.len())).strong());
        if self.findings.is_empty() {
            ui.label("No findings.");
            return;
        }

        let mut pick = None;
        let selected = self.selected_diff.clone();
        let body_height = (height - 28.0).max(60.0);
        TableBuilder::new(ui)
            .id_salt("findings_table")
            .striped(true)
            .resizable(true)
            .max_scroll_height(body_height)
            .column(Column::initial(150.0).at_least(90.0).clip(true))
            .column(Column::initial(170.0).at_least(90.0).clip(true))
            .column(Column::remainder().at_least(140.0).clip(true))
            .header(20.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Location");
                });
                header.col(|ui| {
                    ui.strong("Recipe");
                });
                header.col(|ui| {
                    ui.strong("Change");
                });
            })
            .body(|body| {
                body.rows(18.0, self.findings.len(), |mut row| {
                    let finding = &self.findings[row.index()];
                    let is_selected = selected.as_deref() == Some(finding.file.as_str());

                    let location = format!("{}:{}", finding.file, finding.line);
                    row.col(|ui| {
                        if ui
                            .selectable_label(is_selected, RichText::new(truncate(&location, 42)))
                            .on_hover_text(&location)
                            .clicked()
                        {
                            pick = Some(finding.file.clone());
                        }
                    });

                    let recipe = finding.recipe.as_deref().unwrap_or("-");
                    row.col(|ui| {
                        if ui
                            .selectable_label(is_selected, RichText::new(truncate(recipe, 36)))
                            .on_hover_text(recipe)
                            .clicked()
                        {
                            pick = Some(finding.file.clone());
                        }
                    });

                    let change = change_text(finding);
                    row.col(|ui| {
                        if ui
                            .selectable_label(is_selected, RichText::new(truncate(&change, 90)))
                            .on_hover_text(&change)
                            .clicked()
                        {
                            pick = Some(finding.file.clone());
                        }
                    });
                });
            });

        if let Some(file) = pick {
            self.selected_diff = Some(file);
        }
    }

    fn diff_view(&mut self, ui: &mut egui::Ui, height: f32) {
        let mut pick = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Diff").strong());
            if self.diffs.is_empty() {
                ui.label("(none)");
                return;
            }
            let selected = self
                .selected_diff
                .clone()
                .unwrap_or_else(|| self.diffs[0].file.clone());
            egui::ComboBox::from_id_salt("diff_file")
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for diff in &self.diffs {
                        let is_selected = self.selected_diff.as_deref() == Some(diff.file.as_str());
                        if ui.selectable_label(is_selected, &diff.file).clicked() {
                            pick = Some(diff.file.clone());
                        }
                    }
                });
        });
        if let Some(file) = pick {
            self.selected_diff = Some(file);
        }

        let Some(file) = self.selected_diff.as_ref() else {
            ui.label("Select a finding to see its file's diff.");
            return;
        };
        let Some(diff) = self.diffs.iter().find(|diff| &diff.file == file) else {
            ui.label(format!("No diff was reported for {file}."));
            return;
        };
        if diff.text.is_empty() {
            ui.label("The diff is empty.");
            return;
        }

        egui::ScrollArea::vertical()
            .id_salt("diff_scroll")
            .max_height((height - 28.0).max(60.0))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in diff.text.lines() {
                    let color = diff_color(ui, line);
                    ui.label(RichText::new(line).monospace().color(color));
                }
            });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label("rewriter binary");
                ui.add(
                    egui::TextEdit::singleline(&mut self.binary)
                        .desired_width(420.0)
                        .hint_text(cli::DEFAULT_BINARY),
                );
                ui.label(
                    RichText::new(
                        "A bare name is looked up on PATH; an absolute path is run directly.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);
                ui.monospace(format!("using: {}", cli::binary_name(&self.binary)));
            });
        self.settings_open = open;
    }
}

impl eframe::App for RustrewriteApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(4.0);
        });

        egui::Panel::left("recipe_panel")
            .resizable(true)
            .default_size(300.0)
            .min_size(220.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                self.recipe_list(ui);
            });

        egui::Panel::bottom("results_panel")
            .resizable(true)
            .default_size(300.0)
            .min_size(140.0)
            .show(ui, |ui| {
                // The panel's own resize edge lives at the top of this rect. A
                // table header flush against it makes a column drag start as a
                // panel drag, which then follows the mouse downward; the gutter
                // keeps the two hit areas apart.
                ui.add_space(10.0);
                self.results_ui(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| self.detail_ui(ui));
        });

        self.settings_window(ui.ctx());
    }
}

fn summary_text(summary: &Summary) -> String {
    let findings = if summary.findings == 1 {
        "finding"
    } else {
        "findings"
    };
    let files = if summary.files == 1 { "file" } else { "files" };
    // A batch reports every selector it ran, comma-joined, so the summary
    // names the run rather than only its totals.
    let ran = match summary.recipe.as_deref() {
        Some(recipe) if !recipe.is_empty() => format!("{} {recipe}", summary.command),
        _ => summary.command.clone(),
    };
    let mut text = format!(
        "{ran}: {} {findings} in {} {files}",
        summary.findings, summary.files
    );
    if summary.applied {
        text.push_str(" · applied and kept");
    } else if summary.changed && summary.command == "check" {
        text.push_str(" · would change");
    } else if summary.changed {
        text.push_str(" · rolled back");
    } else {
        text.push_str(" · no changes");
    }
    text
}

fn change_text(finding: &Finding) -> String {
    let before = one_line(&finding.before);
    match &finding.after {
        None => format!("{before}  (reported only)"),
        Some(after) if after.is_empty() => format!("{before}  (delete)"),
        Some(after) if before.is_empty() => format!("(insert) {}", one_line(after)),
        Some(after) => format!("{before} → {}", one_line(after)),
    }
}

fn one_line(text: &str) -> String {
    text.replace('\n', " ").replace('\r', "")
}

fn truncate(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for (index, character) in text.chars().enumerate() {
        if index >= limit {
            out.push('…');
            break;
        }
        out.push(character);
    }
    out
}

fn risk_color(risk: &str) -> Color32 {
    match risk.to_ascii_lowercase().as_str() {
        "low" => ADDED,
        "medium" | "moderate" => Color32::from_rgb(226, 178, 86),
        "high" => REMOVED,
        _ => Color32::GRAY,
    }
}

fn diff_color(ui: &egui::Ui, line: &str) -> Color32 {
    if line.starts_with("+++") || line.starts_with("---") {
        ui.visuals().weak_text_color()
    } else if line.starts_with("@@") {
        HUNK
    } else if line.starts_with('+') {
        ADDED
    } else if line.starts_with('-') {
        REMOVED
    } else {
        ui.visuals().text_color()
    }
}

fn error_banner(ui: &mut egui::Ui, message: &str) {
    egui::Frame::new()
        .fill(ERROR_BACKGROUND)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .corner_radius(4.0)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(ERROR_TEXT, format!("Error: {message}"));
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(recipe: Option<&str>, command: &str) -> Summary {
        Summary {
            command: command.to_string(),
            recipe: recipe.map(str::to_string),
            findings: 2,
            files: 1,
            changed: true,
            applied: command == "run",
            gates: None,
        }
    }

    #[test]
    fn a_batch_summary_names_every_selector_it_ran() {
        let text = summary_text(&summary(Some("rust.idioms,rust.cleanup"), "run"));
        assert!(text.starts_with("run rust.idioms,rust.cleanup:"));
        assert!(text.contains("2 findings in 1 file"));
    }

    #[test]
    fn a_summary_without_a_recipe_names_the_command_only() {
        let text = summary_text(&summary(None, "check"));
        assert!(text.starts_with("check:"));
    }
}
