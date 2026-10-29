//! Idempotency: running a fix recipe on its own output must change nothing.
//!
//! `rustrewrite test` proves before -> after for every example. This module
//! proves the other half of the contract, that applying the same recipe to
//! `after` leaves it alone. A recipe that fails here is not safe to re-run
//! against a tree that is already partly migrated, which is exactly what happens
//! after a partial run or a merge.
//!
//! It lives beside the engine rather than in `tests/` because it drives the
//! snippet machinery directly instead of spawning the binary: no fixture copy,
//! no compile gate, no child process. The crate has a single binary target, so a
//! `tests/` file would need a library target just to reach `apply_to_snippet`.

use crate::engine::apply_to_snippet;
use crate::recipes::{apply_options, Catalog, Example, Mode, Resolved};
use std::path::Path;

fn catalog() -> Catalog {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    Catalog::load(&[], root).expect("loading the built-in catalog")
}

/// The example's recipe with its `options` applied, mirroring a real run.
fn resolve_example(catalog: &Catalog, recipe_id: &str, example: &Example) -> Resolved {
    let recipe = catalog
        .get(recipe_id)
        .unwrap_or_else(|| panic!("recipe `{recipe_id}` is in the catalog"));
    let mut resolved = catalog
        .resolve(recipe)
        .unwrap_or_else(|err| panic!("resolving `{recipe_id}`: {err:#}"));
    apply_options(&mut resolved, &example.options)
        .unwrap_or_else(|err| panic!("options for `{recipe_id}`: {err:#}"));
    resolved
}

#[test]
fn every_fix_recipe_example_is_idempotent() {
    let catalog = catalog();
    let mut recipes_checked = 0usize;
    let mut examples_checked = 0usize;
    let mut failures = Vec::new();

    for recipe in catalog
        .all()
        .filter(|recipe| recipe.mode == Mode::Fix && !recipe.examples.is_empty())
    {
        recipes_checked += 1;
        for (index, example) in recipe.examples.iter().enumerate() {
            // Resolving the example the way `test_examples` resolves a recipe
            // flattens a composite into its children's steps.
            let resolved = resolve_example(&catalog, &recipe.id, example);
            let again = apply_to_snippet(&resolved, &example.after).unwrap_or_else(|err| {
                panic!(
                    "applying `{}` example {} again: {err:#}",
                    recipe.id,
                    index + 1
                )
            });
            examples_checked += 1;
            if again != example.after {
                failures.push(format!(
                    "{} example {}:\n  after: {}\n  again: {}",
                    recipe.id,
                    index + 1,
                    example.after.replace('\n', "\n         "),
                    again.replace('\n', "\n         ")
                ));
            }
        }
    }

    assert!(
        recipes_checked > 0 && examples_checked > 0,
        "no fix recipe examples were checked"
    );
    assert!(
        failures.is_empty(),
        "{} of {examples_checked} fix example(s) are not idempotent:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
