//! Argument-list rewriting for the semantic actions.
//!
//! `ChangeMethodName` is one symbol and one rename. Changing an argument list is
//! not: the declaration's parameter list and every call site have to move
//! together, or the crate stops compiling. The language server tells us where the
//! calls are; this module turns each location into a concrete text edit.

use anyhow::{bail, Result};
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::Node;
use ast_grep_language::{LanguageExt, SupportLang};
use std::ops::Range;

/// The operation to apply to an argument or parameter list.
#[derive(Debug, Clone)]
pub enum Op {
    Delete { index: usize },
    Add { index: usize, text: String },
    Reorder { order: Vec<usize> },
}

/// A parenthesised list found in the source, with its items already split.
pub struct ListSite {
    pub range: Range<usize>,
    pub items: Vec<String>,
}

/// Locate the call expression whose callee covers `offset`, and return its
/// argument list. `None` means the reference is not a call: the function is used
/// as a value, imported, or referenced some other way.
pub fn call_site(text: &str, offset: usize) -> Option<ListSite> {
    let doc = SupportLang::Rust.ast_grep(text);
    let node = innermost(doc.root(), offset, &["call_expression"])?;
    let callee = node.children().find(|child| child.is_named())?;
    if !covers(callee.range(), offset) {
        return None;
    }
    let arguments = node.children().find(|child| child.kind() == "arguments")?;
    Some(list_site(text, arguments))
}

/// Locate the function whose signature covers `offset`, and return its parameter
/// list. `has_self` reports whether the first parameter is a receiver, which
/// shifts every call-site argument index by one.
pub fn declaration(text: &str, offset: usize) -> Option<(ListSite, bool)> {
    let doc = SupportLang::Rust.ast_grep(text);
    let node = innermost(doc.root(), offset, &["function_item", "closure_expression"])?;
    let parameters = node.children().find(|child| child.kind() == "parameters")?;
    let site = list_site(text, parameters);
    let has_self = site
        .items
        .first()
        .map(|item| {
            let item = item.trim();
            item == "self"
                || item.starts_with("self:")
                || item.starts_with("&self")
                || item.starts_with("&mut self")
                || item.starts_with("mut self")
        })
        .unwrap_or(false);
    Some((site, has_self))
}

/// Replace a list with the operation applied. The whole list is re-rendered and
/// `cargo fmt` reflows it afterwards, which keeps the edit predictable at the
/// cost of a slightly wider diff.
pub fn rewrite(items: &[String], op: &Op, index: usize) -> Result<Vec<String>> {
    let mut items = items.to_vec();
    match op {
        Op::Delete { .. } => {
            if index >= items.len() {
                bail!(
                    "cannot remove argument {index}: the list has {} item(s)",
                    items.len()
                );
            }
            items.remove(index);
        }
        Op::Add { text, .. } => {
            if index > items.len() {
                bail!(
                    "cannot insert argument {index}: the list has {} item(s)",
                    items.len()
                );
            }
            items.insert(index, text.clone());
        }
        Op::Reorder { order } => {
            if order.len() != items.len() {
                bail!(
                    "the reorder list has {} position(s) but the call has {} argument(s)",
                    order.len(),
                    items.len()
                );
            }
            let mut seen = vec![false; items.len()];
            for position in order {
                if *position >= items.len() || seen[*position] {
                    bail!("the reorder list is not a permutation of the arguments");
                }
                seen[*position] = true;
            }
            items = order
                .iter()
                .map(|position| items[*position].clone())
                .collect();
        }
    }
    Ok(items)
}

pub fn render(items: &[String]) -> String {
    format!("({})", items.join(", "))
}

fn list_site(text: &str, node: Node<'_, StrDoc<SupportLang>>) -> ListSite {
    let range = node.range();
    let items = node
        .named_children()
        .map(|child| text[child.range()].to_string())
        .collect();
    ListSite { range, items }
}

pub(crate) fn innermost<'t>(
    root: Node<'t, StrDoc<SupportLang>>,
    offset: usize,
    kinds: &[&str],
) -> Option<Node<'t, StrDoc<SupportLang>>> {
    let mut best: Option<Node<'t, StrDoc<SupportLang>>> = None;
    for node in root.dfs() {
        let kind = node.kind();
        if !kinds.contains(&kind.as_ref()) {
            continue;
        }
        let range = node.range();
        if !covers(range.clone(), offset) {
            continue;
        }
        // Prefer the tightest node: a call inside an argument of another call
        // must resolve to the inner one.
        let better = match &best {
            None => true,
            Some(current) => node.range().len() < current.range().len(),
        };
        if better {
            best = Some(node);
        }
    }
    best
}

fn covers(range: Range<usize>, offset: usize) -> bool {
    range.start <= offset && offset < range.end
}
