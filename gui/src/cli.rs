//! Building the `rustrewrite` command line, running it off the UI thread, and
//! turning its stdout into [`Event`]s.

use crate::model::{parse_record, Record};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

/// Looked up on PATH when the settings field is left empty.
pub const DEFAULT_BINARY: &str = "rustrewrite";

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
    /// The recipe id. `None` is only meaningful for `List`; `check` and `run`
    /// then fall back to the CLI's own default.
    pub recipe: Option<String>,
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

/// `rustrewrite --path <root> <subcommand> ...`.
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
            push_recipe(&mut args, spec);
            args.push("--json".to_string());
            args.push("--diff".to_string());
            push_sets(&mut args, spec);
            push_warmth(&mut args, spec);
        }
        Action::Apply => {
            args.push("run".to_string());
            args.push("--apply".to_string());
            push_recipe(&mut args, spec);
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

fn push_recipe(args: &mut Vec<String>, spec: &JobSpec) {
    if let Some(recipe) = &spec.recipe {
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
            recipe: Some("rust.idioms".to_string()),
            sets: Vec::new(),
            test: false,
            no_verify: false,
            allow_dirty: false,
            warm: true,
        }
    }

    #[test]
    fn list_argv_asks_for_json_and_nothing_else() {
        let mut spec = spec(Action::List);
        spec.recipe = None;
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
    fn an_empty_binary_falls_back_to_the_path_name() {
        assert_eq!(binary_name(""), "rustrewrite");
        assert_eq!(binary_name("   "), "rustrewrite");
        assert_eq!(
            binary_name(" C:/tools/rustrewrite.exe "),
            "C:/tools/rustrewrite.exe"
        );
    }

    #[test]
    fn a_blank_root_is_replaced_with_the_current_directory() {
        let mut spec = spec(Action::Check);
        spec.root = "  ".to_string();
        assert_eq!(build_args(&spec)[1], ".");
    }
}
