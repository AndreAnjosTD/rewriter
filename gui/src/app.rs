//! Application state and layout.
//!
//! The UI never touches the filesystem or a child process itself: a job runs
//! the CLI on background threads, and this module only turns the resulting
//! records into state that the next frame draws.

use crate::cli::{self, Action, Event, Job, JobSpec};
use crate::model::{Diff, Finding, Recipe, Record, Summary};
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::{BTreeMap, VecDeque};

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

pub struct RustrewriteApp {
    ctx: egui::Context,
    binary: String,
    root: String,
    search: String,
    recipes: Vec<Recipe>,
    selected: Option<usize>,
    option_values: BTreeMap<String, String>,
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

    fn select_recipe(&mut self, index: usize) {
        self.selected = Some(index);
        self.option_values = self
            .recipes
            .get(index)
            .map(Recipe::initial_values)
            .unwrap_or_default();
        self.reset_results();
        self.status.clear();
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
        let mut sets = Vec::new();
        for (name, option) in &recipe.options {
            let value = self.option_values.get(name).cloned().unwrap_or_default();
            if option.required && value.trim().is_empty() {
                return Err(format!("Option `{name}` is required."));
            }
            if !value.is_empty() {
                sets.push((name.clone(), value));
            }
        }
        Ok(sets)
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

        let mut recipe = None;
        let mut sets = Vec::new();
        if action != Action::List {
            let Some(selected) = self.selected_recipe() else {
                self.error = Some("Select a recipe first.".to_string());
                return;
            };
            recipe = Some(selected.id.clone());
            match self.collect_sets(selected) {
                Ok(collected) => sets = collected,
                Err(message) => {
                    self.error = Some(message);
                    return;
                }
            }
        }

        let spec = JobSpec {
            binary: self.binary.clone(),
            root,
            action,
            recipe,
            sets,
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
                format!("rustrewrite exited with status {code}: {detail}")
            }
            (Some(code), None) => format!("rustrewrite exited with status {code}."),
            (None, Some(detail)) => format!("rustrewrite stopped: {detail}"),
            (None, None) => "rustrewrite stopped without an exit status.".to_string(),
        }
    }

    fn apply_record(&mut self, record: Record) {
        match record {
            Record::Catalog(recipes) => {
                let previous = self.selected_recipe().map(|recipe| recipe.id.clone());
                self.recipes = recipes;
                self.selected =
                    previous.and_then(|id| self.recipes.iter().position(|recipe| recipe.id == id));
                self.option_values = self
                    .selected
                    .and_then(|index| self.recipes.get(index))
                    .map(Recipe::initial_values)
                    .unwrap_or_default();
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
            if !self.search.is_empty() && ui.small_button("Clear").clicked() {
                self.search.clear();
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

        ui.horizontal(|ui| {
            ui.heading("Recipes");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{} loaded", self.recipes.len()))
                        .small()
                        .weak(),
                );
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
                        let response = ui.selectable_label(selected == Some(index), text);
                        if !recipe.description.is_empty() {
                            response.clone().on_hover_text(&recipe.description);
                        }
                        if response.clicked() {
                            pick = Some(index);
                        }
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
                if recipe.options.is_empty() {
                    ui.label("This recipe takes no options.");
                } else {
                    egui::Grid::new("recipe_options")
                        .num_columns(2)
                        .spacing([16.0, 10.0])
                        .show(ui, |ui| {
                            for (name, option) in &recipe.options {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(name).monospace());
                                    if option.required {
                                        ui.colored_label(Color32::from_rgb(232, 168, 78), "*")
                                            .on_hover_text("Required");
                                    }
                                });
                                let mut value =
                                    self.option_values.get(name).cloned().unwrap_or_default();
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
                                self.option_values.insert(name.clone(), value);
                                ui.end_row();
                            }
                        });
                }
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

    fn actions_ui(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        let can_run = !busy && self.selected.is_some();

        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(can_run, egui::Button::new("Check"))
                .on_hover_text("Report what would change. Never writes.")
                .clicked()
            {
                self.start_job(Action::Check);
            }
            if ui
                .add_enabled(can_run, egui::Button::new("Apply"))
                .on_hover_text("Rewrite, then run the correctness gate.")
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
            self.findings_table(&mut columns[0], height);
            self.diff_view(&mut columns[1], height);
        });
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
            .column(Column::initial(150.0).at_least(90.0))
            .column(Column::initial(170.0).at_least(90.0))
            .column(Column::remainder().at_least(140.0))
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
                ui.label("rustrewrite binary");
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
                ui.add_space(4.0);
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
    let mut text = format!(
        "{}: {} {findings} in {} {files}",
        summary.command, summary.findings, summary.files
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
