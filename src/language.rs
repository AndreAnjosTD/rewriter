//! What makes a language a language for this tool.
//!
//! The engine, the gate, the recipe schema and the report are language-neutral;
//! everything that is not lives here as data: which files to read, which
//! tree-sitter grammar to parse them with, how to tell a project of this
//! language apart from another, and how to prove a rewrite did not break it.
//!
//! A profile is only as good as its gate. A language whose check command is a
//! linter gives a weaker promise than one whose check command compiles, and the
//! profile must say which it is rather than pretend the two are the same.

use anyhow::{bail, Context, Result};
use ast_grep_language::SupportLang;
use std::path::Path;

/// One command the correctness gate runs.
pub struct CommandSpec {
    /// Candidate program prefixes, tried in order until one answers `--version`.
    /// `py -3` is a prefix of two words on Windows, hence the nesting.
    pub programs: &'static [&'static [&'static str]],
    /// Arguments appended to the resolved program.
    pub args: &'static [&'static str],
}

pub struct GateCommands {
    /// Applied rather than asserted: ast-grep replacements are syntactically
    /// correct but not always layout-correct, so a formatter tidies them up.
    pub format: Option<CommandSpec>,
    /// The proof that the rewrite still means something to the compiler.
    pub check: CommandSpec,
    /// Run only when the caller asks for tests.
    pub test: Option<CommandSpec>,
}

pub struct Language {
    /// Namespace recipes use, e.g. `rust` for `rust.idioms`.
    pub id: &'static str,
    /// Human name for messages.
    pub name: &'static str,
    /// Extensions that count as source, without the dot.
    pub extensions: &'static [&'static str],
    /// Files that mark a project root of this language.
    pub markers: &'static [&'static str],
    /// Grammar the structural engine parses with.
    pub ast_grep: SupportLang,
    /// Top-level node kinds that declare an import, for `after-uses` and for
    /// the duplicate check in `add-import`.
    pub import_kinds: &'static [&'static str],
    pub gate: GateCommands,
}

/// Python's syntax check has to be a program of its own: `compileall` writes
/// `__pycache__` into the tree, and a check must not modify the project.
const PYTHON_SYNTAX_CHECK: &str = r#"import ast,pathlib,sys
skip={'.git','.venv','venv','node_modules','__pycache__','target','.tox','.mypy_cache'}
bad=[]
for path in sorted(pathlib.Path('.').rglob('*.py')):
    if skip.intersection(path.parts):
        continue
    try:
        ast.parse(path.read_bytes(), filename=str(path))
    except SyntaxError as err:
        bad.append(f"{path}:{err.lineno}: {err.msg}")
if bad:
    print('\n'.join(bad))
    sys.exit(1)
"#;

pub static RUST: Language = Language {
    id: "rust",
    name: "Rust",
    extensions: &["rs"],
    markers: &["Cargo.toml"],
    ast_grep: SupportLang::Rust,
    import_kinds: &["use_declaration"],
    gate: GateCommands {
        format: Some(CommandSpec {
            programs: &[&["cargo"]],
            args: &["fmt", "--all"],
        }),
        check: CommandSpec {
            programs: &[&["cargo"]],
            args: &["check", "--all-targets", "--quiet"],
        },
        test: Some(CommandSpec {
            programs: &[&["cargo"]],
            args: &["test", "--all-targets", "--quiet"],
        }),
    },
};

pub static PYTHON: Language = Language {
    id: "python",
    name: "Python",
    extensions: &["py", "pyi"],
    markers: &[
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "Pipfile",
        "requirements.txt",
    ],
    ast_grep: SupportLang::Python,
    import_kinds: &[
        "import_statement",
        "import_from_statement",
        "future_import_statement",
    ],
    gate: GateCommands {
        // `ruff` is the formatter most projects have; without it the step is
        // skipped rather than failing a rewrite that is otherwise fine.
        format: Some(CommandSpec {
            programs: &[&["ruff"]],
            args: &["format", "--quiet", "."],
        }),
        // Syntax only: Python has no compile step, so this is the strongest
        // guarantee a compiler-like tool can offer without a type checker.
        check: CommandSpec {
            programs: &[&["py", "-3"], &["python3"], &["python"]],
            args: &["-c", PYTHON_SYNTAX_CHECK],
        },
        test: Some(CommandSpec {
            programs: &[&["pytest"]],
            args: &["-q"],
        }),
    },
};

pub static TYPESCRIPT: Language = Language {
    id: "typescript",
    name: "TypeScript",
    extensions: &["ts", "tsx", "mts", "cts"],
    markers: &["tsconfig.json"],
    ast_grep: SupportLang::TypeScript,
    import_kinds: &["import_statement"],
    gate: GateCommands {
        format: Some(CommandSpec {
            programs: &[&["prettier"]],
            args: &["--write", "--log-level", "warn", "."],
        }),
        // Type checking is the gate. Without `tsc` there is no proof, so this
        // is required and a missing install fails loudly.
        check: CommandSpec {
            programs: &[
                &["tsc"],
                &["node_modules/.bin/tsc"],
                &["node_modules\\\\.bin\\\\tsc.cmd"],
            ],
            args: &["--noEmit"],
        },
        test: None,
    },
};

pub static ALL: &[&Language] = &[&RUST, &PYTHON, &TYPESCRIPT];

/// Look a language up by id.
pub fn by_id(id: &str) -> Result<&'static Language> {
    ALL.iter()
        .copied()
        .find(|language| language.id == id)
        .with_context(|| {
            let known = ALL
                .iter()
                .map(|language| language.id)
                .collect::<Vec<_>>()
                .join(", ");
            format!("unknown language `{id}`; known languages: {known}")
        })
}

/// Decide which language a project is written in.
///
/// Marker files decide when exactly one language claims the root. A repository
/// that carries markers for two languages is genuinely ambiguous, and guessing
/// would run the wrong grammar over the tree, so the caller is asked to say.
/// Only when no marker matches do source-file counts decide.
pub fn detect(root: &Path) -> Result<&'static Language> {
    let mut claimed: Vec<&'static Language> = ALL
        .iter()
        .copied()
        .filter(|language| {
            language
                .markers
                .iter()
                .any(|marker| root.join(marker).exists())
        })
        .collect();
    match claimed.len() {
        1 => return Ok(claimed.remove(0)),
        0 => {}
        _ => {
            let names = claimed
                .iter()
                .map(|language| language.id)
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "{} looks like more than one language ({names}); pass --language <id> to choose",
                root.display()
            );
        }
    }

    let mut counts: Vec<(usize, &'static Language)> = ALL
        .iter()
        .copied()
        .map(|language| (count_sources(root, language), language))
        .collect();
    counts.sort_by_key(|(count, _)| std::cmp::Reverse(*count));
    match counts.first() {
        Some((0, _)) | None => bail!(
            "no source files for a known language under {}; pass --language <id>",
            root.display()
        ),
        Some((_, language)) => Ok(language),
    }
}

fn count_sources(root: &Path, language: &Language) -> usize {
    let mut count = 0usize;
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !is_skipped(entry))
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            if let Some(extension) = entry.path().extension().and_then(|value| value.to_str()) {
                if language.extensions.contains(&extension) {
                    count += 1;
                }
            }
        }
    }
    count
}

/// Directories that hold dependencies, build output or version control, never
/// the project's own source.
pub fn is_skipped(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    entry.file_type().is_dir()
        && matches!(
            name.as_ref(),
            ".git"
                | ".hg"
                | ".svn"
                | "target"
                | "node_modules"
                | "dist"
                | "build"
                | ".venv"
                | "venv"
                | "__pycache__"
                | ".tox"
                | ".mypy_cache"
                | ".ruff_cache"
                | ".next"
                | ".idea"
                | ".vscode"
        )
}

/// Whether a recipe's language claims this project.
///
/// A recipe whose namespace is not a known language is generic: project-local
/// recipes written before this abstraction existed keep working.
pub fn recipe_applies(recipe_language: Option<&str>, language: &Language) -> bool {
    match recipe_language {
        Some(id) => id == language.id,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_has_a_gate_and_a_grammar() {
        for language in ALL {
            assert!(!language.extensions.is_empty(), "{}", language.id);
            assert!(!language.markers.is_empty(), "{}", language.id);
            assert!(!language.gate.check.programs.is_empty(), "{}", language.id);
        }
    }

    #[test]
    fn ids_round_trip() {
        for language in ALL {
            assert_eq!(by_id(language.id).unwrap().id, language.id);
        }
        assert!(by_id("cobol").is_err());
    }

    #[test]
    fn rust_keeps_its_cargo_gate() {
        assert_eq!(RUST.gate.check.programs[0], &["cargo"]);
        assert_eq!(RUST.gate.check.args[0], "check");
    }

    #[test]
    fn python_checks_syntax_without_writing_bytecode() {
        assert!(
            PYTHON.gate.check.args.join(" ").contains("ast.parse"),
            "the python gate must parse, not compileall"
        );
        assert!(!PYTHON.gate.check.args.join(" ").contains("compileall"));
    }

    #[test]
    fn an_unknown_namespace_is_generic() {
        assert!(recipe_applies(None, &PYTHON));
        assert!(recipe_applies(Some("python"), &PYTHON));
        assert!(!recipe_applies(Some("rust"), &PYTHON));
    }
}
