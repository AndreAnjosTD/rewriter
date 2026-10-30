//! Building the `rewriter` command line, running it off the UI thread, and
//! turning its stdout into [`Event`]s.

use crate::model::{parse_record, Recipe, Record};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

/// Looked up on PATH when the settings field is left empty.
pub const DEFAULT_BINARY: &str = "rewriter";

/// There is no separate dry-run command: the CLI's `check` is the dry run of
/// the same recipes `run --apply` executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    List,
    Check,
    Apply,
}

/// Everything one subprocess needs. Kept as data so argv construction is a
/// pure function and can be tested without spawning anything.
#[derive(Debug, Clone, PartialEq)]
pub struct JobSpec {
    pub binary: String,
    pub root: String,
    pub action: Action,
    /// The recipe selectors, each sent as its own `--recipe`, in this order.
    /// Empty is only meaningful for `List`; `check` and `run` then fall back to
    /// the CLI's own default. More than one entry is a batch: one scan and one
    /// gate for the whole invocation.
    pub recipes: Vec<String>,
    pub sets: Vec<(String, String)>,
    pub test: bool,
    pub no_verify: bool,
    pub allow_dirty: bool,
    pub warm: bool,
}

/// The binary to run: the settings value if present, otherwise the PATH name.
pub fn binary_name(binary: &str) -> &str {
    let binary = binary.trim();
    if binary.is_empty() {
        DEFAULT_BINARY
    } else {
        binary
    }
}

/// `rewriter --path <root> <subcommand> ...`.
pub fn build_args(spec: &JobSpec) -> Vec<String> {
    let root = match spec.root.trim() {
        "" => ".",
        root => root,
    };
    let mut args = vec!["--path".to_string(), root.to_string()];
    match spec.action {
        Action::List => {
            args.push("list".to_string());
            args.push("--json".to_string());
        }
        Action::Check => {
            args.push("check".to_string());
            push_recipes(&mut args, spec);
            args.push("--json".to_string());
            args.push("--diff".to_string());
            push_sets(&mut args, spec);
            push_warmth(&mut args, spec);
        }
        Action::Apply => {
            args.push("run".to_string());
            args.push("--apply".to_string());
            push_recipes(&mut args, spec);
            args.push("--json".to_string());
            args.push("--diff".to_string());
            if spec.test {
                args.push("--test".to_string());
            }
            if spec.no_verify {
                args.push("--no-verify".to_string());
            }
            if spec.allow_dirty {
                args.push("--allow-dirty".to_string());
            }
            push_sets(&mut args, spec);
            push_warmth(&mut args, spec);
        }
    }
    args
}

/// One `--recipe` per selector, in the order the caller gave them: the CLI
/// deduplicates, runs them as one job, and gates once at the end.
fn push_recipes(args: &mut Vec<String>, spec: &JobSpec) {
    for recipe in &spec.recipes {
        args.push("--recipe".to_string());
        args.push(recipe.clone());
    }
}

fn push_sets(args: &mut Vec<String>, spec: &JobSpec) {
    for (key, value) in &spec.sets {
        args.push("--set".to_string());
        // One argv item, so an option value with spaces stays intact.
        args.push(format!("{key}={value}"));
    }
}

fn push_warmth(args: &mut Vec<String>, spec: &JobSpec) {
    if !spec.warm {
        args.push("--cold".to_string());
    }
}

/// A recipe that cannot join a batch, and the required option that blocks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchBlocker {
    pub recipe: String,
    pub option: String,
}

impl BatchBlocker {
    /// The message the app shows instead of starting the run.
    pub fn message(&self) -> String {
        format!(
            "`{}` needs `--set {}=…`, and a batch shares one set of options. \
             Uncheck it, or run it on its own.",
            self.recipe, self.option
        )
    }
}

/// The first recipe in `recipes` that a batch cannot include, or `None` when
/// they can all share one invocation.
///
/// A batch sends a single `--set` map for every recipe, so a recipe that
/// declares a required option cannot join one: its option name would have to
/// serve the whole batch, and the next recipe's required name would collide
/// with it. A recipe with only optional defaults, or with no options at all,
/// is fine.
pub fn batch_blocker(recipes: &[&Recipe]) -> Option<BatchBlocker> {
    recipes.iter().find_map(|recipe| {
        recipe
            .required_options()
            .first()
            .map(|option| BatchBlocker {
                recipe: recipe.id.clone(),
                option: (*option).to_string(),
            })
    })
}

/// Something the UI should react to.
#[derive(Debug)]
pub enum Event {
    Record(Record),
    /// One stderr line in JSON mode, where all human text goes.
    Log(String),
    Finished {
        success: bool,
        code: Option<i32>,
    },
}

/// A running child and the stream of events it produces.
pub struct Job {
    pub kind: Action,
    pub receiver: Receiver<Event>,
    child: Arc<Mutex<Child>>,
}

impl Job {
    /// Kill the direct child. The reader threads then see EOF and report the
    /// job as finished with a failure status.
    pub fn cancel(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

/// Start a child on background threads. `wake` is called after every event so
/// the caller can request a repaint without this module knowing about egui.
pub fn spawn(spec: &JobSpec, wake: impl Fn() + Send + Sync + 'static) -> Result<Job, String> {
    let binary = binary_name(&spec.binary).to_string();
    let args = build_args(spec);
    let mut command = Command::new(&binary);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A console child would flash a console window on Windows.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|err| format!("could not run `{binary}`: {err}"))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        return Err(format!("could not read the output of `{binary}`"));
    };

    let child = Arc::new(Mutex::new(child));
    let waiter = Arc::clone(&child);
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
    let (sender, receiver) = mpsc::channel();

    let stderr_reader = {
        let sender = sender.clone();
        let wake = Arc::clone(&wake);
        thread::spawn(move || {
            read_lines(stderr, |line| {
                if sender.send(Event::Log(line)).is_ok() {
                    wake();
                }
            });
        })
    };

    thread::spawn(move || {
        read_lines(stdout, |line| {
            let event = match parse_record(&line) {
                Ok(Some(record)) => Some(Event::Record(record)),
                Ok(None) => None,
                Err(err) => Some(Event::Log(format!("unreadable output: {err}"))),
            };
            if let Some(event) = event {
                if sender.send(event).is_ok() {
                    wake();
                }
            }
        });
        let _ = stderr_reader.join();
        // The pipes are closed by now, so this returns instead of blocking on
        // a child that is still writing.
        let status = waiter.lock().ok().and_then(|mut child| child.wait().ok());
        let (success, code) = status
            .map(|status| (status.success(), status.code()))
            .unwrap_or((false, None));
        if sender.send(Event::Finished { success, code }).is_ok() {
            wake();
        }
    });

    Ok(Job {
        kind: spec.action,
        receiver,
        child,
    })
}

/// Read newline-delimited text. Invalid UTF-8 is replaced rather than treated
/// as the end of the stream, so one bad byte cannot hide the rest of a run.
fn read_lines(stream: impl Read, mut on_line: impl FnMut(String)) {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        while buffer.last() == Some(&b'\n') || buffer.last() == Some(&b'\r') {
            buffer.pop();
        }
        on_line(String::from_utf8_lossy(&buffer).into_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(action: Action) -> JobSpec {
        JobSpec {
            binary: DEFAULT_BINARY.to_string(),
            root: "C:/work/crate".to_string(),
            action,
            recipes: vec!["rust.idioms".to_string()],
            sets: Vec::new(),
            test: false,
            no_verify: false,
            allow_dirty: false,
            warm: true,
        }
    }

    fn recipe(json: &str) -> Recipe {
        serde_json::from_str(json).expect("the recipe should parse")
    }

    #[test]
    fn list_argv_asks_for_json_and_nothing_else() {
        let mut spec = spec(Action::List);
        spec.recipes.clear();
        assert_eq!(
            build_args(&spec),
            vec!["--path", "C:/work/crate", "list", "--json"]
        );
    }

    #[test]
    fn check_argv_carries_path_recipe_json_diff_and_sets() {
        let mut spec = spec(Action::Check);
        spec.sets = vec![
            ("file".to_string(), "src/my file.rs".to_string()),
            ("header".to_string(), "// do not edit".to_string()),
        ];
        spec.warm = false;
        assert_eq!(
            build_args(&spec),
            vec![
                "--path",
                "C:/work/crate",
                "check",
                "--recipe",
                "rust.idioms",
                "--json",
                "--diff",
                "--set",
                "file=src/my file.rs",
                "--set",
                "header=// do not edit",
                "--cold",
            ]
        );
    }

    #[test]
    fn check_does_not_pass_apply_only_flags() {
        let mut spec = spec(Action::Check);
        spec.test = true;
        spec.no_verify = true;
        spec.allow_dirty = true;
        let args = build_args(&spec);
        assert!(!args.iter().any(|arg| arg == "--test"));
        assert!(!args.iter().any(|arg| arg == "--no-verify"));
        assert!(!args.iter().any(|arg| arg == "--allow-dirty"));
        assert!(!args.iter().any(|arg| arg == "--cold"));
    }

    #[test]
    fn apply_argv_enables_the_gate_options_and_apply() {
        let mut spec = spec(Action::Apply);
        spec.test = true;
        spec.no_verify = true;
        spec.allow_dirty = true;
        spec.sets = vec![("to".to_string(), "api.example.com".to_string())];
        assert_eq!(
            build_args(&spec),
            vec![
                "--path",
                "C:/work/crate",
                "run",
                "--apply",
                "--recipe",
                "rust.idioms",
                "--json",
                "--diff",
                "--test",
                "--no-verify",
                "--allow-dirty",
                "--set",
                "to=api.example.com",
            ]
        );
    }

    #[test]
    fn apply_argv_with_defaults_passes_only_apply() {
        let spec = spec(Action::Apply);
        assert_eq!(
            build_args(&spec),
            vec![
                "--path",
                "C:/work/crate",
                "run",
                "--apply",
                "--recipe",
                "rust.idioms",
                "--json",
                "--diff",
            ]
        );
    }

    #[test]
    fn zero_recipes_omit_the_recipe_flag() {
        let mut check = spec(Action::Check);
        check.recipes.clear();
        assert_eq!(
            build_args(&check),
            vec!["--path", "C:/work/crate", "check", "--json", "--diff"]
        );

        let mut apply = spec(Action::Apply);
        apply.recipes.clear();
        assert_eq!(
            build_args(&apply),
            vec![
                "--path",
                "C:/work/crate",
                "run",
                "--apply",
                "--json",
                "--diff",
            ]
        );
    }

    #[test]
    fn several_recipes_become_one_recipe_flag_each_in_order() {
        let mut spec = spec(Action::Apply);
        // Deliberately not alphabetical: the order the caller resolved must
        // survive into argv, because the summary echoes it back.
        spec.recipes = vec![
            "rust.visibility".to_string(),
            "rust.idioms".to_string(),
            "rust.cleanup".to_string(),
        ];
        assert_eq!(
            build_args(&spec),
            vec![
                "--path",
                "C:/work/crate",
                "run",
                "--apply",
                "--recipe",
                "rust.visibility",
                "--recipe",
                "rust.idioms",
                "--recipe",
                "rust.cleanup",
                "--json",
                "--diff",
            ]
        );
        assert_eq!(
            build_args(&spec)
                .iter()
                .filter(|arg| arg.as_str() == "--recipe")
                .count(),
            3
        );
    }

    #[test]
    fn several_recipes_keep_their_order_under_check_too() {
        let mut spec = spec(Action::Check);
        spec.recipes = vec!["b".to_string(), "a".to_string(), "c".to_string()];
        assert_eq!(
            build_args(&spec),
            vec![
                "--path",
                "C:/work/crate",
                "check",
                "--recipe",
                "b",
                "--recipe",
                "a",
                "--recipe",
                "c",
                "--json",
                "--diff",
            ]
        );
    }

    #[test]
    fn a_single_recipe_run_still_carries_its_sets() {
        let mut spec = spec(Action::Apply);
        spec.sets = vec![("file".to_string(), "src/main.rs".to_string())];
        let args = build_args(&spec);
        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == "--recipe").count(),
            1
        );
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--set" && pair[1] == "file=src/main.rs"));
    }

    #[test]
    fn a_batch_refuses_a_recipe_that_declares_a_required_option() {
        let blocked = recipe(
            r#"{"id":"rust.migrate.change-type","name":"Change type","options":{"file":{"required":true,"default":null}}}"#,
        );
        let open = recipe(r#"{"id":"rust.idioms","name":"Idioms","options":{}}"#);

        let blocker = batch_blocker(&[&open, &blocked]).expect("the batch should be refused");
        assert_eq!(blocker.recipe, "rust.migrate.change-type");
        assert_eq!(blocker.option, "file");
        let message = blocker.message();
        assert!(message.contains("rust.migrate.change-type"));
        assert!(message.contains("--set file=…"));
    }

    #[test]
    fn a_batch_accepts_defaults_and_recipes_without_options() {
        let defaults_only = recipe(
            r#"{"id":"rust.idioms","name":"Idioms","options":{"limit":{"required":false,"default":"10"}}}"#,
        );
        let no_options = recipe(r#"{"id":"rust.cleanup","name":"Cleanup","options":{}}"#);
        assert_eq!(batch_blocker(&[&defaults_only, &no_options]), None);
        assert_eq!(batch_blocker(&[]), None);
    }

    #[test]
    fn an_empty_binary_falls_back_to_the_path_name() {
        assert_eq!(binary_name(""), "rewriter");
        assert_eq!(binary_name("   "), "rewriter");
        assert_eq!(
            binary_name(" C:/tools/rewriter.exe "),
            "C:/tools/rewriter.exe"
        );
    }

    #[test]
    fn a_blank_root_is_replaced_with_the_current_directory() {
        let mut spec = spec(Action::Check);
        spec.root = "  ".to_string();
        assert_eq!(build_args(&spec)[1], ".");
    }
}
