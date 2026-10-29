//! Editing the declared return type in a function signature.
//!
//! OpenRewrite's `ChangeMethodInvocationReturnType` changes the type a method
//! invocation is allowed to produce. In Rust that is two separate problems: the
//! declaration's annotation is a text edit here, and every caller that relied on
//! the old type is a compile error the gate already refuses to keep.
//!
//! The edit is deliberately not a blanket string replacement. A signature can
//! already have an arrow, a generic or multi-line return type, a `where` clause,
//! or no annotation at all, and an insertion has to land between the parameters
//! and whichever of those comes next.

use crate::callsites;
use anyhow::{bail, Context, Result};
use ast_grep_language::{LanguageExt, SupportLang};
use std::ops::Range;

/// One edit that gives a function a new declared return type.
#[derive(Debug, Clone)]
pub struct ReturnTypeEdit {
    /// The span to replace. An empty range at the end of the parameter list when
    /// the signature has no `->`.
    pub range: Range<usize>,
    /// The type as it reads today, or `()` when the annotation was implicit.
    pub previous: String,
    /// What replaces `range`.
    pub replacement: String,
}

/// Change the declared return type of the function declaration covering
/// `offset`.
///
/// `name` is checked against the declaration's own name so an anchor that lands
/// on a parameter or a doc comment cannot silently retype a different function.
pub fn set_return_type(
    text: &str,
    offset: usize,
    name: &str,
    new_type: &str,
) -> Result<ReturnTypeEdit> {
    let new_type = new_type.trim();
    if new_type.is_empty() {
        bail!("the new return type is empty");
    }

    let doc = SupportLang::Rust.ast_grep(text);
    let function = callsites::innermost(
        doc.root(),
        offset,
        &["function_item", "function_signature_item"],
    )
    .context("the anchor is not on a function declaration")?;

    let declared = function
        .field("name")
        .map(|node| node.text().to_string())
        .unwrap_or_default();
    if declared != name {
        bail!("the declaration here is `{declared}`, not `{name}`");
    }

    if let Some(current) = function.field("return_type") {
        return Ok(ReturnTypeEdit {
            previous: current.text().to_string(),
            range: current.range(),
            replacement: new_type.to_string(),
        });
    }

    // No arrow, so the declared type is `()`. The annotation goes directly after
    // `)` to stay ahead of a `where` clause, a body, or a trailing `;`.
    let parameters = function
        .field("parameters")
        .context("the function declaration has no parameter list")?;
    let at = parameters.range().end;
    Ok(ReturnTypeEdit {
        range: at..at,
        previous: "()".to_string(),
        replacement: format!(" -> {new_type}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edited(text: &str, name: &str, new_type: &str) -> String {
        let offset = text
            .find(&format!("{name}("))
            .or_else(|| text.find(name))
            .expect("name in snippet");
        let edit = set_return_type(text, offset, name, new_type).expect("return-type edit");
        let mut out = text.to_string();
        out.replace_range(edit.range, &edit.replacement);
        out
    }

    #[test]
    fn replaces_an_existing_return_type() {
        let before = "fn retries() -> usize {\n    3\n}\n";
        assert_eq!(
            edited(before, "retries", "u64"),
            "fn retries() -> u64 {\n    3\n}\n"
        );
    }

    #[test]
    fn inserts_an_arrow_when_the_signature_has_none() {
        let before = "fn forever() {\n    loop {}\n}\n";
        assert_eq!(
            edited(before, "forever", "u64"),
            "fn forever() -> u64 {\n    loop {}\n}\n"
        );
    }

    #[test]
    fn inserts_before_a_where_clause() {
        let before = "fn run()\nwhere\n    u8: Copy,\n{\n}\n";
        assert_eq!(
            edited(before, "run", "u64"),
            "fn run() -> u64\nwhere\n    u8: Copy,\n{\n}\n"
        );
    }

    #[test]
    fn keeps_generics_and_a_where_clause_in_place() {
        let before =
            "fn first<T>(items: &[T]) -> T\nwhere\n    T: Clone,\n{\n    items[0].clone()\n}\n";
        let after = edited(before, "first", "u64");
        assert!(
            after.starts_with("fn first<T>(items: &[T]) -> u64\nwhere\n    T: Clone,"),
            "{after}"
        );
    }

    #[test]
    fn replaces_a_multi_line_return_type() {
        let before = "fn parse(\n    input: &str,\n) -> Result<\n    u32,\n    Error,\n> {\n    todo!()\n}\n";
        let after = edited(before, "parse", "u64");
        assert_eq!(
            after,
            "fn parse(\n    input: &str,\n) -> u64 {\n    todo!()\n}\n"
        );
    }

    #[test]
    fn replaces_a_return_type_that_contains_an_arrow() {
        let before = "fn make() -> impl Fn() -> u8 {\n    || 0\n}\n";
        let after = edited(before, "make", "u64");
        assert_eq!(after, "fn make() -> u64 {\n    || 0\n}\n");
    }

    #[test]
    fn edits_a_trait_signature() {
        let before = "trait Source {\n    fn read(&self) -> Vec<u8>;\n}\n";
        assert_eq!(
            edited(before, "read", "u64"),
            "trait Source {\n    fn read(&self) -> u64;\n}\n"
        );
    }

    #[test]
    fn reports_an_implicit_unit_return_as_the_previous_type() {
        let text = "fn forever() { loop {} }";
        let edit = set_return_type(text, text.find("forever").expect("name"), "forever", "u64")
            .expect("edit");
        assert_eq!(edit.previous, "()");
        assert_eq!(edit.replacement, " -> u64");
    }

    #[test]
    fn refuses_a_name_that_is_not_the_declaration() {
        let text = "fn source() {}";
        let offset = text.find("source").expect("name");
        let error = set_return_type(text, offset, "other", "u64").expect_err("must refuse");
        assert!(
            format!("{error:#}").contains("`source`, not `other`"),
            "{error:#}"
        );
    }

    #[test]
    fn refuses_an_empty_type() {
        let text = "fn source() {}";
        let offset = text.find("source").expect("name");
        assert!(set_return_type(text, offset, "source", "  ").is_err());
    }

    #[test]
    fn refuses_a_position_outside_a_function() {
        let text = "struct Bag;\n";
        assert!(set_return_type(text, 7, "Bag", "u64").is_err());
    }
}
