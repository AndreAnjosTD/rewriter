# rustrewrite

Recipe-driven, verified refactoring for Rust: the OpenRewrite idea, built for a language that has no LST.

You describe a rewrite once as a recipe. The tool finds every match across the codebase, applies the rewrite, then compiles the result and puts everything back if the build breaks.
A rewrite that does not compile is not a rewrite, it is a broken build, and `rustrewrite` refuses to leave one behind.

```
rustrewrite check --recipe rust.idioms          # what would change, and where
rustrewrite run   --recipe rust.idioms --apply  # change it, then prove it still builds
rustrewrite run   --recipe rust.idioms --apply --test   # ...and run the tests too
```

## Install

```
cargo install --path .
```

The binary is self-contained: the recipe catalog is compiled into it.

## How it maps onto OpenRewrite

| OpenRewrite | rustrewrite |
| --- | --- |
| Recipe | a YAML file with an id, tags, risk class and steps |
| Declarative recipe | `steps:` of structural rules, no code required |
| Composite recipe | `recipes:` naming other recipe ids |
| `RecipeRun` / dry run | `check` |
| `RewriteRun` | `run --apply` |
| Scan recipe | `mode: scan`, reports without writing |
| LST | tree-sitter parse of the file, edited in place so formatting and comments survive |
| Recipe tests | `examples:` per recipe, executed by `rustrewrite test` |

The one deliberate difference: OpenRewrite trusts type attribution, which is why its Java recipes can rename a type across a codebase.
tree-sitter has no types, so every Rust recipe is scoped to what the syntax proves, and the compile gate is what catches the rest.
That trade is why the gate is mandatory rather than optional.

## Commands

| Command | What it does |
| --- | --- |
| `list [--tag T]` | The catalog. |
| `explain <recipe>` | One recipe, with its examples executed as a live proof. |
| `test` | Run every recipe example. Use this before trusting a new recipe. |
| `check [--recipe R] [--diff] [--json]` | Report findings. Exits 1 if there are any, so CI can hold the line. Never writes. |
| `run [--recipe R] [--apply] [--test] [--diff]` | Apply a recipe, then gate it. Without `--apply` it is a dry run. |
| `rename --at file:line:col --to NAME [--apply]` | Rename one resolved symbol using rust-analyzer's type information. |
| `verify [--test]` | The gate on its own: format, compile, optionally test. |

Recipes that take parameters accept `--set key=value`, repeatable, on `check` and `run`.

`--recipe` accepts an id, a namespace prefix (`rust.idiom` selects the whole pack), or `all`.

## Type-aware renames

Recipes match syntax, and syntax cannot tell a `Vec::len` from a `MyBag::len`. When the change has to hit exactly one of them, ask the compiler-backed server instead:

```
rustrewrite rename --at src/server.rs:42:8 --to spawn          # report
rustrewrite rename --at src/server.rs:42:8 --to spawn --apply  # rename, then gate it
```

It starts `rust-analyzer`, waits for the workspace to load, and asks for a rename at that position. The server returns the edits, this tool applies them, and the compile gate still decides whether they stay. A dry run shows the diff and writes nothing.

What it buys you, demonstrated in `tests/semantic.rs`: with `Vec::len` and a custom `Bag::len` in the same file, renaming `Bag::len` touches the declaration and its one call and leaves `Vec::len` untouched.

Requirements and cost:

- `rust-analyzer` on `PATH`. Install with `rustup component add rust-analyzer` — and for a project that pins a toolchain in `rust-toolchain.toml`, add it for that toolchain too.
- The first request pays for loading the workspace: about 5 s on a small crate, about 30 s on a 40k-line workspace. `--wait` sets the budget (default 180 s).
- The server may refuse a rename. A trait implementation member cannot be renamed, because the trait requires the name. That is reported as a refusal and nothing is written.

### As recipes

The same capability is available as parameterized recipes, which is what makes it composable with the rest of the catalog:

```
rustrewrite check --recipe rust.migrate.change-method-name \
  --set file=src/server.rs \
  --set 'anchor=fn start(&self)' \
  --set from=start --set to=spawn

rustrewrite run --recipe rust.migrate.change-type \
  --set file=src/graph.rs --set 'anchor=struct Bag' \
  --set from=Bag --set to=Sack --apply
```

`anchor` is text on the declaration line. It replaces a line number because any edit above the declaration moves a line number, and an anchor does not.

These are the Rust forms of OpenRewrite's `ChangeMethodName` and `ChangeType`: one reusable primitive, configured per call, rather than a hand-written recipe per migration.

## What the gate does

With `--apply`, in order:

1. Refuses to run on a dirty git tree unless you pass `--allow-dirty`.
2. Snapshots every `.rs` file.
3. Applies the structural steps in memory, writes only files that actually changed.
4. Runs cargo-backed steps (compiler and clippy fixes) on the real tree.
5. Runs `cargo fmt --all`, then `cargo check --all-targets`, then `cargo test --all-targets` if you asked for it.
6. On any failure: restores every snapshot, removes any file the run created, and exits non-zero with the failing output.

A failed gate means the recipe is wrong, not that the code is wrong.

## Writing a recipe

A recipe is a YAML file. The catalog lives in `recipes/`, and a project can add its own under `.rustrewrite/recipes/` (loaded automatically) or any directory passed with `--recipes`.

```yaml
id: rust.idiom.string-len-zero
name: Use is_empty() instead of comparing len() to zero
description: >-
  `len() == 0` predates the is_empty convention.
tags: [idiom, std]
risk: low

steps:
  - rule:
      pattern: $X.len() == 0
    fix: $X.is_empty()

examples:
  - before: if items.len() == 0 { return; }
    after: if items.is_empty() { return; }
    why: The empty case becomes the method that names it.
```

The `rule` object is an [ast-grep rule](https://ast-grep.github.io/reference/rule.html), passed through verbatim: `pattern`, `kind`, `all`/`any`/`not`, `inside`, `has`, `constraints` and `utils` all work.
`constraints` and `utils` are written as siblings of `rule`, mirroring ast-grep's own YAML.
`fix` is a template where `$VAR` and `$$$` metavariables bound by the rule are substituted.

A constraint is what keeps a broad pattern honest. This renames any numeric constant path, but only for the primitive type names, in one rule instead of eighteen:

```yaml
- rule:
    pattern: "std::$T::MAX"
  constraints:
    T:
      regex: "^(i8|i16|i32|i64|i128|isize|u8|u16|u32|u64|u128|usize|f32|f64)$"
  fix: "$T::MAX"
```

Step options:

```yaml
- rule: { pattern: extern crate $NAME; }
  delete: true            # remove the whole line when the match is alone on it
- rule: { pattern: $X.unwrap() }
  message: unwrap() panics on None/Err   # shown by scan recipes
  exclude: ["**/tests/**"]               # path globs, relative to the run root
```

A composite recipe names other recipes and runs them in order:

```yaml
id: rust.idioms
recipes:
  - rust.idiom.string-len-zero
  - rust.idiom.bool-compare
```

A `cargo` step puts a compiler or clippy fixer behind the same gate:

```yaml
steps:
  - kind: cargo
    command: ["cargo", "clippy", "--fix", "--all-targets", "--allow-dirty", "--allow-staged"]
```

A `semantic` step asks rust-analyzer to rename a resolved symbol. It needs `file`, `from` and `to`, plus either `anchor` (text on the declaration line) or `line`:

```yaml
steps:
  - kind: semantic
    action: rename
    file: ${file}
    anchor: ${anchor}
    from: ${from}
    to: ${to}
```

A recipe may declare parameters, the equivalent of OpenRewrite's `@Option` fields, and they are substituted into `${name}`:

```yaml
options:
  file:
    description: Path to the file that declares the method.
    required: true
```

`rustrewrite list` shows which recipes need `--set`, and `explain` describes each option.

A recipe may not mix `structural` and `semantic` steps. The structural pass edits text in memory, so a rename resolved afterwards would be pointed at a file that has already moved. Split such a change into two recipes and run them in order.

Every recipe that rewrites something should carry at least one `example`.
`rustrewrite test` runs them all, so a recipe that silently stops matching fails the build rather than quietly doing nothing.

## The catalog

| Recipe | Kind | What it does |
| --- | --- | --- |
| `rust.idiom.string-len-zero` | fix | `.len() == 0` becomes `.is_empty()` |
| `rust.idiom.bool-compare` | fix | `x == true` becomes `x` |
| `rust.idiom.bool-return` | fix | `if x { true } else { false }` becomes `x` |
| `rust.idiom.double-negation` | fix | `!!x` becomes `x` |
| `rust.idiom.self-assignment` | fix | removes `x = x;` |
| `rust.idiom.string-from-empty` | fix | `String::from("")` becomes `String::new()` |
| `rust.idiom.iter-nth-zero` | fix | `.nth(0)` becomes `.next()` |
| `rust.idiom.redundant-field-shorthand` | fix | `Point { x: x }` becomes `Point { x }` |
| `rust.idiom.explicit-unit-return` | fix | `return ();` becomes `return;` |
| `rust.idiom.lambda-to-fn` | fix | `|x| f(x)` becomes `f` |
| `rust.idiom.debug-macro-removal` | fix | removes statement-position `dbg!(..)` |
| `rust.idioms` | composite | the nine idiom fixes above, in one pass |
| `rust.modernize.extern-crate` | fix | drops redundant `extern crate` declarations |
| `rust.modernize.legacy-numeric-constants` | fix | `std::i32::MAX` becomes `i32::MAX` |
| `rust.modernize` | composite | edition-2018 mechanical migration |
| `rust.migrate.change-method-name` | semantic | rename a resolved method and its true call sites |
| `rust.migrate.change-type` | semantic | rename a resolved type and every reference |
| `rust.migrate.log-to-tracing` | fix | `log::level!` becomes `tracing::level!` |
| `rust.fix.cargo-fix` | cargo | applies compiler suggestions |
| `rust.fix.clippy` | cargo | applies clippy machine-applicable fixes |
| `rust.cleanup` | composite | idiom pack, then compiler suggestions |
| `rust.scan.unwrap` | scan | `unwrap()` outside `tests/` |
| `rust.scan.mem-uninitialized` | scan | deprecated `mem::uninitialized` |
| `rust.scan.transmute` | scan | `mem::transmute` |
| `rust.scan.process-exit` | scan | `process::exit` |
| `rust.scan.hardcoded-secret` | scan | a string literal bound to a secret-looking name |

Read-only recipes exist on purpose: `unwrap` is correct in a main, a prototype, or a test.
The decision belongs to the reviewer, so the tool reports it and writes nothing.

[`docs/openrewrite-mapping.md`](docs/openrewrite-mapping.md) audits the full OpenRewrite catalog against Rust and records which recipes transfer, which need type information, and which were attempted and rejected.

[`docs/type-attribution-scope.md`](docs/type-attribution-scope.md) scopes the type-aware layer that would unlock the recipes needing type information, with measured numbers for each architecture option.

## Known limits

- **Structural recipes are syntax-only.** A `pattern` in type position does not parse, so a type rewrite needs `kind:` plus `regex:` and cannot capture a metavariable. Use `rename` when a change must be type-exact.
- **`rename` is positional, not a recipe.** It renames one symbol at one location. Recipes that drive it are the next phase; see [`docs/type-attribution-scope.md`](docs/type-attribution-scope.md).
- **No type information.** Nothing that needs to know whether an identifier is a trait, a module or a struct can be decided structurally. Those recipes belong in `mode: scan`.
- **The gate needs the crate to compile before you start.** A pre-broken tree fails at the gate and rolls back, which is correct but unsurprising.
- **`cargo fmt` rewrites formatting after a fix.** Replacements are syntactically correct but not always layout-correct; the formatter is the last mile.

## Development

```
cargo test      # recipe examples plus end-to-end CLI tests against a fixture crate
cargo run -- list
```

The integration tests copy `tests/fixtures/demo-crate` to a temp directory for each case, so nothing is shared and nothing in the repo is touched.
