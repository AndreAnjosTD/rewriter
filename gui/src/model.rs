//! The records `rewriter --json` writes, and the catalog it lists.
//!
//! `type` is the discriminator, and readers are expected to skip a type they
//! do not know so the stream stays additive. The shapes here mirror the README
//! contract; `kind` is sent rather than `mode`.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// One entry of `list --json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Recipe {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub risk: String,
    #[serde(default)]
    pub kind: String,
    /// The language namespace the recipe targets, e.g. `rust`. Absent for a
    /// local recipe written before the catalog was split by language.
    #[serde(default)]
    pub language: Option<String>,
    /// Keyed by option name; `{}` when the recipe takes none.
    #[serde(default)]
    pub options: BTreeMap<String, RecipeOption>,
}

/// A recipe parameter declared in YAML.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RecipeOption {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    /// The declared default, or `null` when the recipe declares none. It is
    /// kept as a JSON value because a YAML default is not always a string.
    #[serde(default)]
    pub default: Option<Value>,
}

impl RecipeOption {
    /// The text a freshly opened option field starts with.
    pub fn default_text(&self) -> String {
        match &self.default {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
        }
    }
}

impl Recipe {
    /// Option values for the detail pane, prefilled with each default.
    pub fn initial_values(&self) -> BTreeMap<String, String> {
        self.options
            .iter()
            .map(|(name, option)| (name.clone(), option.default_text()))
            .collect()
    }

    /// The names this recipe declares as required, in catalog order. One run
    /// shares one `--set` map, so a recipe with any of these can only run on
    /// its own.
    pub fn required_options(&self) -> Vec<&str> {
        self.options
            .iter()
            .filter(|(_, option)| option.required)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Whether the recipe cannot run beside another one because it needs a
    /// required option of its own.
    pub fn needs_sets(&self) -> bool {
        self.options.values().any(|option| option.required)
    }

    /// Whether the search box should keep this recipe. Case-insensitive over
    /// the id, name, tags and description.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        self.id.to_lowercase().contains(&query)
            || self.name.to_lowercase().contains(&query)
            || self.description.to_lowercase().contains(&query)
            || self
                .tags
                .iter()
                .any(|tag| tag.to_lowercase().contains(&query))
    }
}

/// One reported change or hazard.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Finding {
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// `null` for `rename`, which is positional rather than recipe-driven.
    #[serde(default)]
    pub recipe: Option<String>,
    #[serde(default)]
    pub before: String,
    /// `null` for a scan finding that only reports.
    #[serde(default)]
    pub after: Option<String>,
}

/// A unified diff for one changed file, emitted before the summary.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Diff {
    pub file: String,
    pub text: String,
}

/// One correctness gate in the summary.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Gate {
    pub name: String,
    pub ok: bool,
}

/// The line that closes a successful command.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Summary {
    pub command: String,
    #[serde(default)]
    pub recipe: Option<String>,
    #[serde(default)]
    pub findings: usize,
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub changed: bool,
    #[serde(default)]
    pub applied: bool,
    /// Absent when no gate ran, which is not the same as a gate that passed.
    #[serde(default)]
    pub gates: Option<Vec<Gate>>,
}

/// A parsed stdout line.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Catalog(Vec<Recipe>),
    Finding(Finding),
    Diff(Diff),
    Summary(Summary),
    Error(String),
}

/// Parse one stdout line. Blank lines and unknown record types yield `None`;
/// a malformed line, or a known type with the wrong fields, is an error the
/// caller can show in the log.
pub fn parse_record(line: &str) -> Result<Option<Record>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(line).map_err(|err| format!("invalid JSON: {err}"))?;
    let Some(kind) = value.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return Ok(None);
    };
    let record = match kind.as_str() {
        "catalog" => Record::Catalog(decode::<CatalogLine>(value, &kind)?.recipes),
        "finding" => Record::Finding(decode::<Finding>(value, &kind)?),
        "diff" => Record::Diff(decode::<Diff>(value, &kind)?),
        "summary" => Record::Summary(decode::<Summary>(value, &kind)?),
        "error" => Record::Error(decode::<ErrorLine>(value, &kind)?.message),
        _ => return Ok(None),
    };
    Ok(Some(record))
}

#[derive(Deserialize)]
struct CatalogLine {
    recipes: Vec<Recipe>,
}

#[derive(Deserialize)]
struct ErrorLine {
    message: String,
}

fn decode<T: for<'de> Deserialize<'de>>(value: Value, kind: &str) -> Result<T, String> {
    serde_json::from_value(value).map_err(|err| format!("invalid `{kind}` record: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Record {
        parse_record(line)
            .expect("the line should parse")
            .expect("the line should carry a known record type")
    }

    #[test]
    fn parses_a_finding() {
        let record = parsed(
            r#"{"type":"finding","file":"src/main.rs","line":12,"column":5,"recipe":"rust.migrate.change-type","before":"Foo","after":"Bar"}"#,
        );
        match record {
            Record::Finding(finding) => {
                assert_eq!(finding.file, "src/main.rs");
                assert_eq!(finding.line, 12);
                assert_eq!(finding.column, 5);
                assert_eq!(finding.recipe.as_deref(), Some("rust.migrate.change-type"));
                assert_eq!(finding.before, "Foo");
                assert_eq!(finding.after.as_deref(), Some("Bar"));
            }
            other => panic!("unexpected record: {other:?}"),
        }
    }

    #[test]
    fn parses_a_scan_finding_without_recipe_or_replacement() {
        let record = parsed(
            r#"{"type":"finding","file":"src/lib.rs","line":3,"column":9,"recipe":null,"before":"x.unwrap()","after":null}"#,
        );
        match record {
            Record::Finding(finding) => {
                assert_eq!(finding.recipe, None);
                assert_eq!(finding.after, None);
            }
            other => panic!("unexpected record: {other:?}"),
        }
    }

    #[test]
    fn parses_a_diff_and_keeps_its_line_structure() {
        let record = parsed(
            r#"{"type":"diff","file":"src/main.rs","text":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n"}"#,
        );
        match record {
            Record::Diff(diff) => {
                assert_eq!(diff.file, "src/main.rs");
                assert!(diff.text.starts_with("--- a/src/main.rs\n+++"));
                assert_eq!(diff.text.lines().count(), 5);
            }
            other => panic!("unexpected record: {other:?}"),
        }
    }

    #[test]
    fn parses_a_summary_without_gates() {
        let record = parsed(
            r#"{"type":"summary","command":"check","recipe":"rust.idioms","findings":6,"files":1,"changed":true,"applied":false}"#,
        );
        match record {
            Record::Summary(summary) => {
                assert_eq!(summary.command, "check");
                assert_eq!(summary.findings, 6);
                assert_eq!(summary.files, 1);
                assert!(summary.changed);
                assert!(!summary.applied);
                assert_eq!(summary.gates, None);
            }
            other => panic!("unexpected record: {other:?}"),
        }
    }

    #[test]
    fn parses_a_summary_with_gates() {
        let record = parsed(
            r#"{"type":"summary","command":"run","recipe":"rust.idioms","findings":2,"files":1,"changed":true,"applied":true,"gates":[{"name":"format","ok":true},{"name":"compile","ok":false}]}"#,
        );
        match record {
            Record::Summary(summary) => {
                assert_eq!(summary.command, "run");
                assert!(summary.applied);
                let gates = summary.gates.expect("gates should be present");
                assert_eq!(gates.len(), 2);
                assert_eq!(gates[0].name, "format");
                assert!(gates[0].ok);
                assert_eq!(gates[1].name, "compile");
                assert!(!gates[1].ok);
            }
            other => panic!("unexpected record: {other:?}"),
        }
    }

    #[test]
    fn parses_an_error() {
        let record = parsed(r#"{"type":"error","message":"unknown recipe `rust.nope`"}"#);
        match record {
            Record::Error(message) => assert_eq!(message, "unknown recipe `rust.nope`"),
            other => panic!("unexpected record: {other:?}"),
        }
    }

    #[test]
    fn parses_a_catalog_with_options() {
        let record = parsed(
            r#"{"type":"catalog","recipes":[{"id":"rust.migrate.change-type","name":"Change type","description":"Rename a resolved type.","tags":["migrate","type"],"risk":"low","kind":"fix","options":{"file":{"description":"Path to rewrite.","required":true,"default":null},"header":{"description":"Text to insert.","required":false,"default":"// generated"}}}]}"#,
        );
        match record {
            Record::Catalog(recipes) => {
                assert_eq!(recipes.len(), 1);
                let recipe = &recipes[0];
                assert_eq!(recipe.id, "rust.migrate.change-type");
                assert_eq!(recipe.kind, "fix");
                assert_eq!(recipe.risk, "low");
                assert_eq!(recipe.tags, ["migrate", "type"]);
                assert!(recipe.options["file"].required);
                assert_eq!(recipe.options["file"].default_text(), "");
                assert_eq!(recipe.options["header"].default_text(), "// generated");
            }
            other => panic!("unexpected record: {other:?}"),
        }
    }

    #[test]
    fn catalog_options_reach_the_form_prefilled_with_defaults() {
        let recipe: Recipe = serde_json::from_str(
            r#"{"id":"rust.migrate.replace-string-literal","name":"Replace literal","tags":[],"risk":"low","kind":"fix","options":{"from":{"description":"Old value.","required":true,"default":null},"to":{"description":"New value.","required":false,"default":"api.example.com"}}}"#,
        )
        .expect("the recipe should parse");

        let values = recipe.initial_values();
        assert_eq!(values.len(), 2);
        assert_eq!(values["from"], "");
        assert_eq!(values["to"], "api.example.com");
    }

    #[test]
    fn ignores_blank_lines_and_unknown_types() {
        assert_eq!(parse_record("").expect("blank is not an error"), None);
        assert_eq!(
            parse_record("   \t").expect("whitespace is not an error"),
            None
        );
        assert_eq!(
            parse_record(r#"{"type":"progress","percent":40}"#).expect("unknown is not an error"),
            None
        );
        assert_eq!(
            parse_record(r#"{"type":"later","anything":{"else":true}}"#)
                .expect("unknown is not an error"),
            None
        );
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_record("{not json").is_err());
        assert!(parse_record(r#"{"type":"finding"}"#).is_err());
    }

    #[test]
    fn only_required_options_block_a_batch() {
        let required: Recipe = serde_json::from_str(
            r#"{"id":"rust.migrate.change-type","name":"Change type","tags":[],"risk":"low","kind":"fix","options":{"file":{"description":"Path.","required":true,"default":null},"header":{"description":"Text.","required":false,"default":"// generated"}}}"#,
        )
        .expect("the recipe should parse");
        let defaults_only: Recipe = serde_json::from_str(
            r#"{"id":"rust.idioms","name":"Idioms","tags":[],"risk":"low","kind":"fix","options":{"max":{"description":"Limit.","required":false,"default":"10"}}}"#,
        )
        .expect("the recipe should parse");
        let none: Recipe = serde_json::from_str(
            r#"{"id":"rust.cleanup","name":"Cleanup","tags":[],"risk":"low","kind":"composite","options":{}}"#,
        )
        .expect("the recipe should parse");

        assert_eq!(required.required_options(), ["file"]);
        assert!(required.needs_sets());
        assert!(defaults_only.required_options().is_empty());
        assert!(!defaults_only.needs_sets());
        assert!(!none.needs_sets());
    }

    #[test]
    fn search_covers_id_name_tags_and_description() {
        let recipe: Recipe = serde_json::from_str(
            r#"{"id":"rust.cleanup","name":"Cleanup pass","description":"Idioms, then compiler suggestions.","tags":["composite","sweep"],"risk":"medium","kind":"composite","options":{}}"#,
        )
        .expect("the recipe should parse");

        assert!(recipe.matches(""));
        assert!(recipe.matches("  CLEANUP "));
        assert!(recipe.matches("sweep"));
        assert!(recipe.matches("compiler"));
        assert!(!recipe.matches("transmute"));
    }
}
