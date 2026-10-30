//! Editing a function declaration's return type or visibility.
//!
//! OpenRewrite's `ChangeMethodInvocationReturnType` changes the type a method
//! invocation is allowed to produce. In Rust that is two separate problems: the
//! declaration's annotation is a text edit here, and every caller that relied on
//! the old type is a compile error the gate already refuses to keep.
//!
//! The edit is deliberately not a blanket string replacement. A signature can
//! already have an arrow, a generic or multi-line return type, a `where` clause,
//! or no annotation at all, and an insertion has to land between the parameters
//! and whichever of those comes next. Visibility is the same kind of edit with
//! a different token to place: it goes before every function modifier, because
//! `pub async fn` is the only order Rust accepts.

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

/// One edit that gives a function a new visibility.
#[derive(Debug, Clone)]
pub struct VisibilityEdit {
    /// The span to replace. An empty range before the first modifier or `fn`
    /// when the declaration was private.
    pub range: Range<usize>,
    /// The visibility as it reads today, or empty when the declaration had none.
    pub previous: String,
    /// What replaces `range`; empty removes the visibility.
    pub replacement: String,
}

/// Change the visibility of the function declaration covering `offset`.
///
/// An empty `visibility` removes an existing modifier and leaves a private
/// declaration untouched. `name` is checked against the declaration's own name
/// so an anchor that lands on a parameter or a doc comment cannot silently
/// change a different function.
pub fn set_visibility(
    text: &str,
    offset: usize,
    name: &str,
    visibility: &str,
) -> Result<VisibilityEdit> {
    let visibility = visibility.trim();

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

    if let Some(current) = function
        .children()
        .find(|child| child.kind() == "visibility_modifier")
    {
        let previous = current.text().to_string();
        if visibility.is_empty() {
            // Take the separating whitespace with the modifier so the next
            // token does not move: `pub(crate) fn` becomes `fn`, not ` fn`.
            let mut end = current.range().end;
            while end < text.len() && matches!(text.as_bytes()[end], b' ' | b'\t') {
                end += 1;
            }
            return Ok(VisibilityEdit {
                range: current.range().start..end,
                previous,
                replacement: String::new(),
            });
        }
        return Ok(VisibilityEdit {
            range: current.range(),
            previous,
            replacement: visibility.to_string(),
        });
    }

    // `fn` is an unnamed token, and `function_modifiers` is a named node, so
    // this walks every child rather than only the named ones. Inserting before
    // whichever comes first keeps `pub` ahead of `async`, `const`, `unsafe` and
    // `extern`, which is the order the grammar and the compiler require.
    let at = function
        .children()
        .find(|child| matches!(child.kind().as_ref(), "function_modifiers" | "fn"))
        .map(|child| child.range().start)
        .context("the function declaration has no `fn`")?;
    if visibility.is_empty() {
        return Ok(VisibilityEdit {
            range: at..at,
            previous: String::new(),
            replacement: String::new(),
        });
    }
    Ok(VisibilityEdit {
        range: at..at,
        previous: String::new(),
        replacement: format!("{visibility} "),
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

    fn with_visibility(text: &str, name: &str, visibility: &str) -> String {
        let offset = text
            .find(&format!("{name}("))
            .or_else(|| text.find(name))
            .expect("name in snippet");
        let edit = set_visibility(text, offset, name, visibility).expect("visibility edit");
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

    #[test]
    fn replaces_an_existing_visibility() {
        let before = "pub fn exported() -> u32 {\n    1\n}\n";
        assert_eq!(
            with_visibility(before, "exported", "pub(crate)"),
            "pub(crate) fn exported() -> u32 {\n    1\n}\n"
        );
    }

    #[test]
    fn replaces_a_restricted_visibility() {
        let before = "pub(in crate::module) fn scoped() {}\n";
        assert_eq!(
            with_visibility(before, "scoped", "pub(super)"),
            "pub(super) fn scoped() {}\n"
        );
    }

    #[test]
    fn inserts_before_a_plain_fn() {
        let before = "fn helper() -> u32 {\n    1\n}\n";
        assert_eq!(
            with_visibility(before, "helper", "pub(crate)"),
            "pub(crate) fn helper() -> u32 {\n    1\n}\n"
        );
    }

    #[test]
    fn inserts_below_an_attribute() {
        let before = "#[inline]\nfn fast() {}\n";
        assert_eq!(
            with_visibility(before, "fast", "pub(crate)"),
            "#[inline]\npub(crate) fn fast() {}\n"
        );
    }

    #[test]
    fn inserts_before_an_async_modifier() {
        let before = "async fn work() {}\n";
        assert_eq!(
            with_visibility(before, "work", "pub(crate)"),
            "pub(crate) async fn work() {}\n"
        );
    }

    #[test]
    fn inserts_before_an_unsafe_modifier() {
        let before = "unsafe fn raw() {}\n";
        assert_eq!(
            with_visibility(before, "raw", "pub(crate)"),
            "pub(crate) unsafe fn raw() {}\n"
        );
    }

    #[test]
    fn inserts_before_a_const_modifier() {
        let before = "const fn limit() -> u32 {\n    3\n}\n";
        assert_eq!(
            with_visibility(before, "limit", "pub(crate)"),
            "pub(crate) const fn limit() -> u32 {\n    3\n}\n"
        );
    }

    #[test]
    fn inserts_before_an_extern_modifier() {
        let before = "extern \"C\" fn callback() {}\n";
        assert_eq!(
            with_visibility(before, "callback", "pub(crate)"),
            "pub(crate) extern \"C\" fn callback() {}\n"
        );
    }

    #[test]
    fn edits_a_trait_signature_visibility() {
        let before = "trait Source {\n    fn read(&self) -> u32;\n}\n";
        assert_eq!(
            with_visibility(before, "read", "pub(crate)"),
            "trait Source {\n    pub(crate) fn read(&self) -> u32;\n}\n"
        );
    }

    #[test]
    fn edits_a_method_inside_an_impl() {
        let before = "impl Service {\n    fn hidden(&self) -> u32 {\n        0\n    }\n}\n";
        assert_eq!(
            with_visibility(before, "hidden", "pub(crate)"),
            "impl Service {\n    pub(crate) fn hidden(&self) -> u32 {\n        0\n    }\n}\n"
        );
    }

    #[test]
    fn removes_an_existing_visibility() {
        let before = "pub(crate) fn restricted() -> u32 {\n    2\n}\n";
        assert_eq!(
            with_visibility(before, "restricted", ""),
            "fn restricted() -> u32 {\n    2\n}\n"
        );
    }

    #[test]
    fn leaves_a_private_declaration_alone_when_removing() {
        let before = "fn helper() {}\n";
        let edit = set_visibility(before, before.find("helper").expect("name"), "helper", "")
            .expect("edit");
        assert!(edit.range.is_empty());
        assert_eq!(edit.replacement, "");
        assert_eq!(with_visibility(before, "helper", ""), before);
    }

    #[test]
    fn reports_the_previous_visibility() {
        let text = "pub(crate) fn restricted() {}";
        let edit = set_visibility(
            text,
            text.find("restricted").expect("name"),
            "restricted",
            "pub",
        )
        .expect("edit");
        assert_eq!(edit.previous, "pub(crate)");
        assert_eq!(edit.replacement, "pub");
    }

    #[test]
    fn refuses_a_name_that_is_not_the_visibility_declaration() {
        let text = "fn source() {}";
        let offset = text.find("source").expect("name");
        let error = set_visibility(text, offset, "other", "pub").expect_err("must refuse");
        assert!(
            format!("{error:#}").contains("`source`, not `other`"),
            "{error:#}"
        );
    }
}
